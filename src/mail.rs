//! 收码站（花火邮箱助手 mail.kyon888.xyz）客户端。
//!
//! 用途：非 CDK 授权路径里，OpenAI 会把登录验证码发到账号邮箱，
//! 我们通过本客户端把「sub2api 账号列表里的邮箱」绑到收码站，再读取验证码。
//!
//! ## 已核实契约（2026-09-17 对真实服务实测）
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | POST | `/api/auth/login` | `{username,password}` → `{token,user}` |
//! | POST | `/api/mail-pool/batch-bind` | `{emails:[...]}` → `{results,summary,total}` |
//! | GET  | `/api/emails` | 已绑定邮箱**全量裸数组**（不分页） |
//! | POST | `/api/emails/:id/check` | **触发收信**（202，异步） |
//! | POST | `/api/emails/batch_check` | `{email_ids:[...]}` 批量触发 |
//! | GET  | `/api/emails/:id/mail_records` | 该邮箱收件记录 |
//!
//! ## 两个踩过的坑（别再犯）
//!
//! 1. **收信必须显式触发。** 实测所有邮箱的 `enable_realtime_check=0`，
//!    站点不会自己去拉 IMAP；不调 `/emails/:id/check`，`mail_records`
//!    **永远是空数组**。只轮询记录 = 永远等不到码。
//! 2. **取码必须先剥 HTML。** `content` 是整封 HTML，里面含 CSS 颜色
//!    （`color:#202123`）。直接在原始 HTML 里找「独立 6 位数字」会先命中
//!    **颜色值**，取到错码。真实验证码邮件实测：正解 `225179`，
//!    原始 HTML 会取到 `202123`。
//!
//! 安全：站点地址/账号/密码全部由用户在设置里填写并本地保存，代码中不内置。

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client as HttpClient;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// 浏览器 UA。实测收码站前置有 WAF，会 403 掉 `python-urllib` 这类 UA，
/// 统一伪装成 Chrome 更稳。
const BROWSER_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

/// 两次「触发收信」之间的最小间隔。
///
/// 站点是异步拉 IMAP，一次要几十秒；间隔太短只会一直撞 `409 邮箱正在处理中`。
const CHECK_EVERY: Duration = Duration::from_secs(15);

// ===================================================================
// 配置
// ===================================================================

/// 收码站连接配置（用户填写，本地保存）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    /// 站点地址，如 https://mail.kyon888.xyz（可带 /api 后缀也可不带）
    pub base_url: String,
    pub username: String,
    pub password: String,
}

impl MailConfig {
    pub fn ready(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.username.trim().is_empty()
            && !self.password.is_empty()
    }

    /// 规整成 API 根：确保以 /api 结尾且无多余斜杠。
    fn api_root(&self) -> String {
        let mut b = self.base_url.trim().trim_end_matches('/').to_string();
        if b.ends_with("/api") {
            b
        } else {
            b.push_str("/api");
            b
        }
    }
}

// ===================================================================
// 导入邮箱
// ===================================================================

/// 单个邮箱的绑定结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindItem {
    pub email: String,
    #[serde(default)]
    pub email_id: Option<i64>,
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub message: String,
    /// bound | already_bound | not_found
    pub status: String,
}

/// 绑定汇总。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BindSummary {
    #[serde(default)]
    pub bound: i64,
    #[serde(default)]
    pub already_bound: i64,
    #[serde(default)]
    pub not_found: i64,
}

/// 一键导入的结果（跨批次聚合）。
#[derive(Debug, Clone, Serialize)]
pub struct BindReport {
    /// 提交的邮箱总数（去重后）
    pub total: usize,
    pub bound: i64,
    pub already_bound: i64,
    pub not_found: i64,
    /// 新绑定的邮箱
    pub newly_bound: Vec<String>,
    /// 总库里没有、暂不可绑定的邮箱
    pub missing: Vec<String>,
    /// 批次数量
    pub batches: usize,
    /// 已绑定邮箱的 email_id（email → id），供后续触发收信直接用
    pub email_ids: std::collections::BTreeMap<String, i64>,
}

// ===================================================================
// 收件记录
// ===================================================================

/// 一条收件记录（按真实响应字段建模）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailRecord {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub email_id: i64,
    #[serde(default)]
    pub sender: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub received_time: String,
    #[serde(default)]
    pub folder: String,
}

impl MailRecord {
    fn from_value(v: &Value) -> Self {
        let s = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        Self {
            id: v.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
            email_id: v.get("email_id").and_then(|x| x.as_i64()).unwrap_or(0),
            sender: s("sender"),
            subject: s("subject"),
            content: v
                .get("content")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            received_time: s("received_time"),
            folder: s("folder"),
        }
    }

    /// 收信时间（epoch 秒）。解析不出来返回 None。
    pub fn received_epoch(&self) -> Option<i64> {
        parse_time_epoch(&self.received_time)
    }

    /// 是不是「验证码邮件」。实测特征：
    /// - `ChatGPT <otp@tm1.openai.com>` / `<noreply@tm.openai.com>`
    ///     主题 `Your temporary ChatGPT verification code`
    /// - 干扰项：`trustandsafety@tm.openai.com`（封号）、
    ///     `noreply@email.openai.com`（营销）
    pub fn looks_like_code_mail(&self) -> bool {
        mentions_code(&format!("{} {}", self.sender, self.subject))
    }

    /// 是不是 OpenAI 发来的（范围比上面宽）。
    pub fn looks_like_openai(&self) -> bool {
        let s = format!("{} {}", self.sender, self.subject).to_lowercase();
        s.contains("openai") || s.contains("chatgpt")
    }
}

/// 收件记录水位线：只认「它之后」到达的新邮件，避免拿历史旧码。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailBaseline {
    pub email_id: i64,
    /// 快照时最大的记录 id
    pub max_record_id: i64,
    /// 快照时最新的收信时间（epoch 秒）
    pub max_received_epoch: i64,
    /// 快照时已有多少封历史邮件（用于日志提示）
    pub existing: usize,
}

impl MailBaseline {
    /// 这条记录是不是水位线之后的新邮件。
    pub fn is_new(&self, r: &MailRecord) -> bool {
        // 记录 id 单调递增，id 更大的一定是新的
        if self.max_record_id > 0 && r.id > self.max_record_id {
            return true;
        }
        if let Some(e) = r.received_epoch() {
            if self.max_received_epoch > 0 && e > self.max_received_epoch {
                return true;
            }
        }
        false
    }
}

/// 解析收码站的时间串。实测格式 `2026-09-16 08:29:50+00:00`。
pub fn parse_time_epoch(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%:z",
        "%Y-%m-%dT%H:%M:%S%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
    ] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(s, fmt) {
            return Some(dt.timestamp());
        }
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(ndt.and_utc().timestamp());
        }
    }
    None
}

// ===================================================================
// 取码
// ===================================================================

/// 把 HTML 邮件体剥成纯文本。
///
/// 关键：整块丢掉 `<script>` / `<style>`，否则 CSS 里的颜色值
/// （`color:#202123`）会被当成验证码。实测踩过。
pub fn html_to_text(src: &str) -> String {
    let cs: Vec<char> = src.chars().collect();
    let n = cs.len();
    let mut out = String::with_capacity(n);
    let mut i = 0usize;
    let mut skip_end: Option<Vec<char>> = None;

    while i < n {
        // 1) 正在跳过 <script>/<style> 整块
        if let Some(end) = skip_end.as_ref() {
            if i + end.len() <= n {
                let hit = end
                    .iter()
                    .enumerate()
                    .all(|(x, c)| cs[i + x].to_ascii_lowercase() == *c);
                if hit {
                    let mut k = i;
                    while k < n && cs[k] != '>' {
                        k += 1;
                    }
                    i = (k + 1).min(n);
                    skip_end = None;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        // 2) 标签：整段丢掉（用空格代替，避免把相邻文字粘一起）
        if cs[i] == '<' {
            let mut j = i + 1;
            if j < n && cs[j] == '/' {
                j += 1;
            }
            let mut name = String::new();
            while j < n && cs[j].is_ascii_alphanumeric() {
                name.push(cs[j].to_ascii_lowercase());
                j += 1;
            }
            let mut k = i;
            while k < n && cs[k] != '>' {
                k += 1;
            }
            let tag_end = (k + 1).min(n);
            if name == "script" || name == "style" {
                skip_end = Some(format!("</{}", name).chars().collect());
            }
            out.push(' ');
            i = tag_end;
            continue;
        }

        out.push(cs[i]);
        i += 1;
    }

    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 解码常见 HTML 实体（够用就行）。
fn decode_entities(s: &str) -> String {
    let mut out = s.replace("&nbsp;", " ").replace("&amp;", "&");
    for (k, v) in [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&rsquo;", "'"),
        ("&lsquo;", "'"),
        ("&hellip;", "…"),
        ("&mdash;", "—"),
        ("&ndash;", "–"),
        ("&copy;", "©"),
    ] {
        out = out.replace(k, v);
    }
    // 数字实体 &#123; / &#x7b;
    if out.contains("&#") {
        let mut r = String::with_capacity(out.len());
        let cs: Vec<char> = out.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            if cs[i] == '&' && i + 2 < cs.len() && cs[i + 1] == '#' {
                let mut j = i + 2;
                let hex = j < cs.len() && (cs[j] == 'x' || cs[j] == 'X');
                if hex {
                    j += 1;
                }
                let st = j;
                while j < cs.len() && cs[j].is_ascii_hexdigit() {
                    j += 1;
                }
                if j > st && j < cs.len() && cs[j] == ';' {
                    let digits: String = cs[st..j].iter().collect();
                    if let Ok(num) = u32::from_str_radix(&digits, if hex { 16 } else { 10 }) {
                        if let Some(ch) = char::from_u32(num) {
                            r.push(ch);
                            i = j + 1;
                            continue;
                        }
                    }
                }
            }
            r.push(cs[i]);
            i += 1;
        }
        out = r;
    }
    out
}

/// 找出第一段「独立」的 6 位数字（前后不接其他数字，避免命中长数字串）。
pub fn first_six_digit(s: &str) -> Option<String> {
    let cs: Vec<char> = s.chars().collect();
    first_six_chars(&cs)
}

fn first_six_chars(cs: &[char]) -> Option<String> {
    let mut i = 0usize;
    while i < cs.len() {
        if cs[i].is_ascii_digit() {
            let st = i;
            while i < cs.len() && cs[i].is_ascii_digit() {
                i += 1;
            }
            if i - st == 6 {
                return Some(cs[st..i].iter().collect());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// 在关键词附近找 6 位数字。比「全篇第一个 6 位数字」准得多。
fn code_near_keyword(text: &str) -> Option<String> {
    let cs: Vec<char> = text.chars().collect();
    // 只折叠 ASCII 大小写，保证下标与 cs 一一对应
    let lc: Vec<char> = cs.iter().map(|c| c.to_ascii_lowercase()).collect();
    const BEFORE: usize = 24;
    const AFTER: usize = 150;
    for kw in [
        "verification code",
        "verification",
        "code",
        "验证码",
        "校验码",
        "验证代码",
    ] {
        let k: Vec<char> = kw.chars().collect();
        if k.is_empty() || lc.len() < k.len() {
            continue;
        }
        let mut i = 0usize;
        while i + k.len() <= lc.len() {
            if lc[i..i + k.len()] == k[..] {
                let a = i.saturating_sub(BEFORE);
                let b = (i + k.len() + AFTER).min(cs.len());
                if let Some(c) = first_six_chars(&cs[a..b]) {
                    return Some(c);
                }
                i += k.len();
            } else {
                i += 1;
            }
        }
    }
    None
}

/// 主题/发件人里是否明确提到「验证码」。
///
/// 用于给宽松兜底上闸：只有确认是验证码邮件，才允许用
/// [`site_reference_extract`] 这种「逮到 5~8 位数字就算」的规则。
/// 否则封号邮件（`OpenAI - Access Deactivated`）里的订单号会被当成验证码。
fn mentions_code(s: &str) -> bool {
    let s = s.to_lowercase();
    s.contains("verification code")
        || s.contains("temporary code")
        || s.contains("login code")
        || s.contains("one-time code")
        || s.contains("your code")
        || s.contains("verification")
        || s.contains("验证码")
        || s.contains("校验码")
}

/// 从「主题 + 邮件体」里提取验证码。
///
/// 顺序：剥 HTML → 关键词邻域找码（最准）→ （仅主题确认为验证码邮件时）站点同款兜底。
///
/// 兜底规则对齐收码站前端 `EmailsView`：
/// `text.match(/\b\d{5,8}\b/) ?? (\b\d{4}\b 且非 19xx/20xx ? : "")`
pub fn extract_code_from_text(subject: &str, content: &str) -> Option<String> {
    let text = if content.contains('<') {
        html_to_text(content)
    } else {
        content.to_string()
    };
    let hay = format!("{} {}", subject, text);

    // 1) 关键词邻域（最准：验证码旁边就是「code/验证码」）
    if let Some(c) = code_near_keyword(&hay) {
        return Some(c);
    }

    // 2) 站点同款兜底，但只对「主题就是验证码邮件」的邮件放开
    if mentions_code(subject) {
        if let Some(c) = site_reference_extract(&hay) {
            return Some(c);
        }
    }
    None
}

/// 收码站前端的取码规则（原样对齐，用于兜底与交叉验证）。
///
/// ```js
/// const e = `${mail.subject} ${stripHtml(content)}`;
/// const o = e.match(/\b\d{5,8}\b/)?.[0];   // 先 5~8 位
/// if (o) return o;
/// const g = e.match(/\b\d{4}\b/)?.[0];     // 再 4 位
/// return !g || /^20\d{2}$/.test(g) || /^19\d{2}$/.test(g) ? "" : g;
/// ```
pub fn site_reference_extract(text: &str) -> Option<String> {
    if let Some(c) = token_digits(text, 5, 8) {
        return Some(c);
    }
    if let Some(c) = token_digits(text, 4, 4) {
        // 排除 19xx / 20xx 年份
        if !(c.starts_with("19") || c.starts_with("20")) {
            return Some(c);
        }
    }
    None
}

/// 取第一段「完整 token 的连续数字」，长度落在 [min,max]。
/// `\b` 语义：数字串前后不能还是数字。
fn token_digits(s: &str, min: usize, max: usize) -> Option<String> {
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < cs.len() {
        if cs[i].is_ascii_digit() {
            let st = i;
            while i < cs.len() && cs[i].is_ascii_digit() {
                i += 1;
            }
            let n = i - st;
            if n >= min && n <= max {
                return Some(cs[st..i].iter().collect());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// 从一组收件记录里挑出验证码。
///
/// 优先「明确是验证码邮件」的，其次 OpenAI 来的，最新优先。
/// 返回 `(验证码, 邮件主题)`。
pub fn pick_code<'a, I>(records: I) -> Option<(String, String)>
where
    I: IntoIterator<Item = &'a MailRecord> + Clone,
{
    let mut all: Vec<&MailRecord> = records.into_iter().collect();
    // 最新优先
    all.sort_by(|a, b| {
        b.received_epoch()
            .unwrap_or(0)
            .cmp(&a.received_epoch().unwrap_or(0))
            .then(b.id.cmp(&a.id))
    });

    for r in all.iter().filter(|r| r.looks_like_code_mail()) {
        if let Some(c) = extract_code_from_text(&r.subject, &r.content) {
            return Some((c, r.subject.clone()));
        }
    }
    for r in all.iter().filter(|r| r.looks_like_openai()) {
        if let Some(c) = extract_code_from_text(&r.subject, &r.content) {
            return Some((c, r.subject.clone()));
        }
    }
    None
}

// ===================================================================
// 客户端
// ===================================================================

/// 收码站客户端（阻塞式，与 sub2api client 风格一致）。
pub struct MailClient {
    http: HttpClient,
    root: String,
    token: String,
}

impl MailClient {
    fn headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(USER_AGENT, HeaderValue::from_static(BROWSER_UA));
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token)).context("token 含非法字符")?,
        );
        Ok(h)
    }

    /// 用用户名密码登录，拿到 token。
    pub fn login(cfg: &MailConfig) -> Result<Self> {
        let root = cfg.api_root();
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(40))
            .user_agent(BROWSER_UA)
            .build()
            .context("构建 HTTP client 失败")?;
        let url = format!("{}/auth/login", root);
        let resp = http
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, BROWSER_UA)
            .json(&serde_json::json!({
                "username": cfg.username.trim(),
                "password": cfg.password,
            }))
            .send()
            .with_context(|| format!("请求收码站登录失败：{}", url))?;
        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(anyhow!("收码站登录失败 ({})：{}", status, val));
        }
        let token = val
            .get("token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                val.get("data")
                    .and_then(|d| d.get("token"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .context("收码站登录未返回 token（检查用户名/密码）")?;
        Ok(Self { http, root, token })
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.root, path);
        let resp = self
            .http
            .get(&url)
            .headers(self.headers()?)
            .send()
            .with_context(|| format!("请求失败：{}", url))?;
        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(anyhow!("{} 返回 {}：{}", path, status, val));
        }
        Ok(val)
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.root, path);
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(body)
            .send()
            .with_context(|| format!("请求失败：{}", url))?;
        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);
        // 触发收信返回 202，也算成功
        if !status.is_success() {
            return Err(anyhow!("{} 返回 {}：{}", path, status, val));
        }
        Ok(val)
    }

    // ---------------------------------------------------------------
    // 一键导入邮箱
    // ---------------------------------------------------------------

    /// 批量把邮箱绑定到收码站（按 email 匹配站点总库）。
    ///
    /// 服务端自带去重：已绑定过的返回 `already_bound`，不会重复占用。
    /// 单批上限 100，超出自动分批。
    pub fn bind_emails(&self, emails: &[String]) -> Result<BindReport> {
        // 本地先去重，减少请求量
        let mut seen = BTreeSet::new();
        let uniq: Vec<String> = emails
            .iter()
            .map(|e| e.trim().to_lowercase())
            .filter(|e| !e.is_empty() && seen.insert(e.clone()))
            .collect();

        let mut report = BindReport {
            total: uniq.len(),
            bound: 0,
            already_bound: 0,
            not_found: 0,
            newly_bound: Vec::new(),
            missing: Vec::new(),
            batches: 0,
            email_ids: Default::default(),
        };

        for chunk in uniq.chunks(100) {
            report.batches += 1;
            let val = self.post_json(
                "/mail-pool/batch-bind",
                &serde_json::json!({ "emails": chunk }),
            )?;
            let items: Vec<BindItem> =
                serde_json::from_value(val.get("results").cloned().unwrap_or(Value::Array(vec![])))
                    .unwrap_or_default();
            for it in items {
                let key = it.email.trim().to_lowercase();
                if let Some(id) = it.email_id {
                    report.email_ids.insert(key.clone(), id);
                }
                match it.status.as_str() {
                    "bound" => {
                        report.bound += 1;
                        report.newly_bound.push(it.email.clone());
                    }
                    "already_bound" => report.already_bound += 1,
                    _ => {
                        report.not_found += 1;
                        report.missing.push(it.email.clone());
                    }
                }
            }
            // 服务端没回 results 时按 summary 兜底
            if let Some(s) = val.get("summary") {
                if let Ok(s) = serde_json::from_value::<BindSummary>(s.clone()) {
                    if report.batches == 1 && report.bound == 0 && report.not_found == 0 {
                        report.bound = s.bound;
                        report.already_bound = s.already_bound;
                        report.not_found = s.not_found;
                    }
                }
            }
        }
        Ok(report)
    }

    // ---------------------------------------------------------------
    // 读信 / 取验证码
    // ---------------------------------------------------------------

    /// 已绑定的邮箱列表（裸数组，全量）。
    pub fn list_emails(&self) -> Result<Vec<Value>> {
        Ok(as_array(self.get_json("/emails")?))
    }

    /// 按邮箱地址找收码站里的邮箱 id。
    pub fn mailbox_id(&self, email: &str) -> Result<Option<i64>> {
        let want = email.trim().to_lowercase();
        for m in self.list_emails()? {
            let e = m
                .get("email")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if e == want {
                return Ok(m.get("id").and_then(|v| v.as_i64()));
            }
        }
        Ok(None)
    }

    /// **触发收信**。站点 `enable_realtime_check=0`，不调这个就永远读不到新邮件。
    ///
    /// 服务端异步执行，返回 202；重复调用**安全**：
    /// 上一轮还在跑时会返回 `409 邮箱正在处理中`，这属于正常，不当错误。
    /// 返回 `true` 表示本次真的启动了检查，`false` 表示上一轮还没跑完。
    pub fn check(&self, email_id: i64) -> Result<bool> {
        let url = format!("{}/emails/{}/check", self.root, email_id);
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(&serde_json::json!({}))
            .send()
            .with_context(|| format!("请求失败：{}", url))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(true);
        }
        if status.as_u16() == 409 {
            // 邮箱正在处理中 —— 等下一轮
            return Ok(false);
        }
        let val: Value = resp.json().unwrap_or(Value::Null);
        Err(anyhow!(
            "/emails/{}/check 返回 {}：{}",
            email_id,
            status,
            val
        ))
    }

    /// 批量触发收信。单个邮箱撞 409（正在处理中）不影响其它邮箱。
    pub fn batch_check(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        for chunk in ids.chunks(100) {
            let url = format!("{}/emails/batch_check", self.root);
            let resp = self
                .http
                .post(&url)
                .headers(self.headers()?)
                .json(&serde_json::json!({ "email_ids": chunk }))
                .send()
                .with_context(|| format!("请求失败：{}", url))?;
            let status = resp.status();
            // 409 = 有邮箱正在处理，同样可接受
            if !status.is_success() && status.as_u16() != 409 {
                let val: Value = resp.json().unwrap_or(Value::Null);
                return Err(anyhow!("批量触发收信返回 {}：{}", status, val));
            }
        }
        Ok(())
    }

    /// 某邮箱的收件记录（结构化）。
    pub fn records(&self, email_id: i64) -> Result<Vec<MailRecord>> {
        let val = self.get_json(&format!("/emails/{}/mail_records", email_id))?;
        Ok(as_array(val).iter().map(MailRecord::from_value).collect())
    }

    /// 某邮箱的原始收件记录（排查用）。
    pub fn mail_records(&self, email_id: i64) -> Result<Vec<Value>> {
        Ok(as_array(
            self.get_json(&format!("/emails/{}/mail_records", email_id))?,
        ))
    }

    /// 打一份「水位线」快照：只认它之后到达的新邮件。
    ///
    /// **必须在触发发信之前调用**（即提交邮箱给 OpenAI 之前），
    /// 否则可能把历史旧码当成新码。
    pub fn snapshot(&self, email: &str) -> Result<Option<MailBaseline>> {
        match self.mailbox_id(email)? {
            Some(id) => Ok(Some(self.snapshot_for(id)?)),
            None => Ok(None),
        }
    }

    fn snapshot_for(&self, email_id: i64) -> Result<MailBaseline> {
        let recs = self.records(email_id)?;
        Ok(MailBaseline {
            email_id,
            max_record_id: recs.iter().map(|r| r.id).max().unwrap_or(0),
            max_received_epoch: recs
                .iter()
                .filter_map(|r| r.received_epoch())
                .max()
                .unwrap_or(0),
            existing: recs.len(),
        })
    }

    /// 轮询等待某邮箱收到**新**验证码。
    ///
    /// - `baseline`：传入「触发发信之前」的快照；传 None 则以本次进入时的状态为准。
    /// - 每 [`CHECK_EVERY`] 秒显式触发一次收信。
    /// - 只接受水位线之后的新邮件，历史旧码不会被误用。
    pub fn wait_code(
        &self,
        email: &str,
        baseline: Option<&MailBaseline>,
        timeout: Duration,
        poll: Duration,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        log: &dyn Fn(String),
    ) -> Result<Option<String>> {
        use std::sync::atomic::Ordering;

        let id = match self.mailbox_id(email)? {
            Some(id) => id,
            None => {
                log(format!(
                    "收码站里没有邮箱 {}（请先在账号列表执行「导入邮箱」）",
                    email
                ));
                return Ok(None);
            }
        };
        let base = match baseline.filter(|b| b.email_id == id) {
            Some(b) => b.clone(),
            None => self.snapshot_for(id)?,
        };
        log(format!(
            "收码站 mailbox_id={}，历史邮件 {} 封，只认之后到达的新邮件",
            id, base.existing
        ));

        let start = Instant::now();
        // 置成「很久以前」→ 首轮立刻触发一次收信
        let mut last_check = start - CHECK_EVERY - Duration::from_secs(1);
        let mut told_checking = false;
        let mut told_waiting = false;
        let mut seen: BTreeSet<i64> = BTreeSet::new();

        while start.elapsed() < timeout {
            if let Some(c) = cancel {
                if c.load(Ordering::SeqCst) {
                    log("已取消，停止等待验证码".to_string());
                    return Ok(None);
                }
            }

            // 1) 显式触发收信（站点不会自己拉 IMAP）
            if last_check.elapsed() >= CHECK_EVERY {
                match self.check(id) {
                    Ok(true) => {
                        if !told_checking {
                            log("已触发收信（收码站只在被触发时才去拉邮件）".to_string());
                            told_checking = true;
                        }
                    }
                    // 409：上一轮还在处理，等下一轮再说，不算错误
                    Ok(false) => {}
                    Err(e) => log(format!("触发收信失败：{:#}", e)),
                }
                last_check = Instant::now();
            }

            // 2) 读记录，只挑新邮件
            match self.records(id) {
                Ok(recs) => {
                    let fresh: Vec<&MailRecord> = recs.iter().filter(|r| base.is_new(r)).collect();
                    if fresh.is_empty() {
                        if !told_waiting {
                            log(format!(
                                "已触发收信，等待新邮件…（当前 {} 封，暂无新件）",
                                recs.len()
                            ));
                            told_waiting = true;
                        }
                    } else {
                        for r in &fresh {
                            if seen.insert(r.id) {
                                log(format!("新邮件：{} | {}", r.sender, r.subject));
                            }
                        }
                        if let Some((code, subj)) = pick_code(fresh.iter().copied()) {
                            log(format!("从「{}」取到验证码：{}", subj, code));
                            return Ok(Some(code));
                        }
                        if !told_waiting {
                            log(format!("收到 {} 封新邮件，但还没解析出验证码", fresh.len()));
                            told_waiting = true;
                        }
                    }
                }
                Err(e) => log(format!("读取收件记录失败：{:#}", e)),
            }

            std::thread::sleep(poll);
        }
        log(format!("等待验证码超时（{}s）", timeout.as_secs()));
        Ok(None)
    }
}

/// 响应可能是裸数组，也可能包在 `data` 里。
fn as_array(v: Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a,
        Value::Object(o) => o
            .get("data")
            .and_then(|x| x.as_array())
            .cloned()
            .or_else(|| o.get("results").and_then(|x| x.as_array()).cloned())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 真实夹具：花火收码站抓下来的原文
    const REAL_CODE_HTML: &str = include_str!("../tests/fixtures/openai_code_email.html");
    const REAL_CODE_SUBJECT: &str = include_str!("../tests/fixtures/openai_code_email.subject");
    const REAL_BAN_HTML: &str = include_str!("../tests/fixtures/openai_ban_email.html");

    fn rec(sender: &str, subject: &str, content: &str, id: i64, t: &str) -> MailRecord {
        MailRecord {
            id,
            email_id: 1,
            sender: sender.into(),
            subject: subject.into(),
            content: content.into(),
            received_time: t.into(),
            folder: "Inbox".into(),
        }
    }

    /// 回归：真实验证码邮件必须取到 225179，不能取到 CSS 颜色 202123。
    #[test]
    fn real_code_email_extracts_correct_code() {
        let code = extract_code_from_text(REAL_CODE_SUBJECT, REAL_CODE_HTML);
        assert_eq!(code.as_deref(), Some("225179"), "必须取到真实验证码");

        // 反证：直接扫原始 HTML 会命中 CSS 颜色值 —— 这就是曾经取错码的原因
        let naive = first_six_digit(REAL_CODE_HTML).unwrap();
        assert_eq!(naive, "202123", "原始 HTML 第一个 6 位数字是 CSS 颜色");
        assert_ne!(naive, "225179");
    }

    /// 剥 HTML 后不应残留 style/script 里的内容。
    #[test]
    fn html_to_text_drops_style_and_script() {
        let t = html_to_text(REAL_CODE_HTML);
        assert!(!t.contains("@font-face"));
        assert!(!t.contains("#202123"));
        assert!(t.contains("225179"));
        assert!(t.contains("verification code"));
    }

    /// 封号邮件里没有验证码，不能瞎编一个。
    ///
    /// 注意：收码站前端那套「逮到 5~8 位数字就算」的规则在这封邮件上是**会误报**的，
    /// 所以我们对宽松兜底加了「主题必须像验证码邮件」的闸。
    ///
    /// 夹具里的收件地址已脱敏（真实地址换成了 `example-user6989@example.com`），
    /// 但保留了「地址里带 4 位非年份数字」这个**结构特征** —— 站点规则正是被它误触的。
    #[test]
    fn ban_email_yields_no_code() {
        assert!(extract_code_from_text("OpenAI - Access Deactivated", REAL_BAN_HTML).is_none());
        // 反证：不加闸的站点同款兜底确实会误报
        let loose = site_reference_extract(&html_to_text(REAL_BAN_HTML));
        assert!(
            loose.is_some(),
            "站点同款规则在封号邮件上会误报，所以才需要加闸"
        );
    }

    /// 交叉验证：站点前端的取码规则和我们的实现，在真邮件上结果一致。
    #[test]
    fn site_reference_agrees_on_real_code_mail() {
        let text = format!("{} {}", REAL_CODE_SUBJECT, html_to_text(REAL_CODE_HTML));
        assert_eq!(site_reference_extract(&text).as_deref(), Some("225179"));
        assert_eq!(
            extract_code_from_text(REAL_CODE_SUBJECT, REAL_CODE_HTML).as_deref(),
            Some("225179")
        );
    }

    /// 宽松兜底：主题像验证码邮件时，5~8 位/4 位数字也能捞出来。
    #[test]
    fn site_reference_fallback_shapes() {
        assert_eq!(
            site_reference_extract("your code is 12345").as_deref(),
            Some("12345")
        );
        assert_eq!(
            site_reference_extract("code 12345678").as_deref(),
            Some("12345678")
        );
        // 4 位：排除年份
        assert_eq!(site_reference_extract("code 4821").as_deref(), Some("4821"));
        assert_eq!(site_reference_extract("copyright 2026").as_deref(), None);
        assert_eq!(site_reference_extract("copyright 1999").as_deref(), None);
    }

    #[test]
    fn ignores_long_numbers_and_dates() {
        assert_eq!(first_six_digit("id 1234567890"), None);
        assert_eq!(first_six_digit("code: 123456"), Some("123456".to_string()));
        assert_eq!(first_six_digit("2026-09-17"), None);
    }

    #[test]
    fn picks_code_mail_over_marketing() {
        let marketing = rec(
            "ChatGPT <noreply@email.openai.com>",
            "Writing help, whenever you need it",
            "<html><body>Save 123456 today!</body></html>",
            1,
            "2026-09-16 09:00:00+00:00",
        );
        let code_mail = rec(
            "ChatGPT <otp@tm1.openai.com>",
            "Your temporary ChatGPT verification code",
            REAL_CODE_HTML,
            2,
            "2026-09-16 08:29:50+00:00",
        );
        let got = pick_code([&marketing, &code_mail]).unwrap();
        assert_eq!(got.0, "225179");
        assert!(got.1.contains("verification code"));
    }

    /// 历史旧码不得被当成新码。
    #[test]
    fn baseline_filters_old_mail() {
        let base = MailBaseline {
            email_id: 1,
            max_record_id: 100,
            max_received_epoch: parse_time_epoch("2026-09-16 08:00:00+00:00").unwrap(),
            existing: 1,
        };
        let old = rec("a", "b", "c", 99, "2026-09-16 07:00:00+00:00");
        let new = rec("a", "b", "c", 101, "2026-09-16 09:00:00+00:00");
        assert!(!base.is_new(&old));
        assert!(base.is_new(&new));
        // id 没变但时间更新也算新（兜底）
        let newer_same_id = rec("a", "b", "c", 100, "2026-09-16 09:30:00+00:00");
        assert!(base.is_new(&newer_same_id));
    }

    #[test]
    fn parses_real_time_format() {
        let e = parse_time_epoch("2026-09-16 08:29:50+00:00").unwrap();
        assert_eq!(
            e,
            chrono::DateTime::parse_from_rfc3339("2026-09-16T08:29:50+00:00")
                .unwrap()
                .timestamp()
        );
        assert!(parse_time_epoch("").is_none());
        assert!(parse_time_epoch("not a time").is_none());
    }

    #[test]
    fn detects_code_mail_signature() {
        let m = rec(
            "ChatGPT <otp@tm1.openai.com>",
            "Your temporary ChatGPT verification code",
            "",
            1,
            "",
        );
        assert!(m.looks_like_code_mail() && m.looks_like_openai());
        let ban = rec(
            "OpenAI <trustandsafety@tm.openai.com>",
            "OpenAI - Access Deactivated",
            "",
            2,
            "",
        );
        assert!(!ban.looks_like_code_mail());
        assert!(ban.looks_like_openai());
    }

    #[test]
    fn api_root_normalizes() {
        let c = MailConfig {
            base_url: "https://mail.kyon888.xyz/".into(),
            ..Default::default()
        };
        assert_eq!(c.api_root(), "https://mail.kyon888.xyz/api");
        let c2 = MailConfig {
            base_url: "https://mail.kyon888.xyz/api".into(),
            ..Default::default()
        };
        assert_eq!(c2.api_root(), "https://mail.kyon888.xyz/api");
    }

    /// 结构化解析：字段名跟真实响应一致。
    #[test]
    fn parses_real_record_shape() {
        let v = json!({
            "id": 81376, "email_id": 81033, "folder": "Inbox",
            "sender": "ChatGPT <otp@tm1.openai.com>",
            "subject": "Your temporary ChatGPT verification code",
            "content": REAL_CODE_HTML,
            "received_time": "2026-09-16 08:29:50+00:00",
            "has_attachments": false
        });
        let r = MailRecord::from_value(&v);
        assert_eq!(r.id, 81376);
        assert_eq!(r.email_id, 81033);
        assert!(r.looks_like_code_mail());
    }
}
