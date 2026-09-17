use crate::browser;
use crate::client::Sub2ApiClient;
use crate::models::Account;
use crate::store;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

/// 检测并报告需要重授权的 OAuth 账号（与具体渠道无关的第一步）。
pub fn detect(client: &mut Sub2ApiClient) -> anyhow::Result<()> {
    let accounts = client.list_accounts()?;
    let needs: Vec<&Account> = accounts.iter().filter(|a| a.needs_reauth()).collect();

    if needs.is_empty() {
        println!("✅ 当前没有检测到需要重授权的 OAuth 账号。");
        return Ok(());
    }

    println!("⚠️  检测到 {} 个 OAuth 账号可能需要重授权：\n", needs.len());
    for a in &needs {
        println!(
            "  #{:<5} name={:<32} platform={:<12} status={:<9} error={}",
            a.id, a.name, a.platform, a.status, a.error_message
        );
    }
    Ok(())
}

/// `reauth apply` 的选项。
pub struct ApplyOpts<'a> {
    /// 真正发送（否则只 dry-run 预览）
    pub yes: bool,
    /// 从 JSON 文件读结果（优先于系统剪切板）
    pub from: Option<&'a str>,
    /// 只写回该 sub2api 账号 id
    pub only_account: Option<i64>,
    /// 只写回该邮箱对应的账号（忽略大小写）
    pub only_email: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// 可复用核心（CLI 与 Tauri 桌面端共用）
// ---------------------------------------------------------------------------

/// 凭证字段预览：token/secret 类只给长度，不进前端。
#[derive(Debug, Clone, Serialize)]
pub struct FieldPreview {
    pub key: String,
    pub value: String,
    /// 是否为敏感字段（值为长度描述，非真实值）
    pub hidden: bool,
}

/// 单个账号的写回计划。`credentials` 用 `#[serde(skip)]`，**绝不随 JSON 返回给前端**。
#[derive(Debug, Clone, Serialize)]
pub struct PlanItem {
    pub account_id: i64,
    pub account_type: String,
    pub email: String,
    pub preview: Vec<FieldPreview>,
    #[serde(skip)]
    pub credentials: Value,
    #[serde(skip)]
    pub extra: Value,
}

/// 单个账号的写回结果。
#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
    pub account_id: i64,
    pub email: String,
    pub ok: bool,
    pub message: String,
}

/// 单个账号的删除结果（用于 fetch 检测到封禁/停用时）。
#[derive(Debug, Clone, Serialize)]
pub struct DeleteOutcome {
    pub account_id: i64,
    pub email: String,
    pub ok: bool,
    pub message: String,
}

/// 写回报告（Tauri 返回给前端的结构）。
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub dry_run: bool,
    pub plans: Vec<PlanItem>,
    pub skipped: Vec<String>,
    /// 仅 `dry_run=false` 时有内容
    pub outcomes: Vec<ApplyOutcome>,
}

/// 把一个结果项里的「账号邮箱」取出来，优先级：`email` → `credentials.email` → `name`。
/// CPA 输出的 `accounts[].name` 就是邮箱，所以必须覆盖 name。
fn account_email(item: &Value) -> Option<String> {
    item.get("email")
        .and_then(|v| v.as_str())
        .or_else(|| {
            item.get("credentials")
                .and_then(|c| c.get("email"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| item.get("name").and_then(|v| v.as_str()))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 在 sub2api 账号列表里按邮箱找目标账号——**忽略大小写**，同时比对 `name` 与 `credentials.email`。
fn find_account<'a>(accounts: &'a [Account], email: &str) -> Option<&'a Account> {
    let eq = |s: &str| s.eq_ignore_ascii_case(email);
    accounts.iter().find(|a| {
        eq(&a.name)
            || a.credentials
                .as_ref()
                .and_then(|c| c.get("email"))
                .and_then(|v| v.as_str())
                .map(eq)
                .unwrap_or(false)
    })
}

/// 合并凭证：先铺账号现有值作兜底，再用结果值覆盖同名键。
fn merge_credentials(clip: &Value, existing: &Value) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(obj) = existing.as_object() {
        for (k, v) in obj {
            m.insert(k.clone(), v.clone());
        }
    }
    if let Some(obj) = clip.as_object() {
        for (k, v) in obj {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

/// 生成凭证预览（敏感字段只给长度）。
fn build_preview(creds: &Value) -> Vec<FieldPreview> {
    let mut out = Vec::new();
    if let Some(obj) = creds.as_object() {
        for (k, v) in obj {
            let kl = k.to_ascii_lowercase();
            if kl.contains("token") || kl.contains("secret") {
                let len = v.as_str().map(|s| s.len()).unwrap_or(0);
                out.push(FieldPreview {
                    key: k.clone(),
                    value: format!("<{} 字符>", len),
                    hidden: true,
                });
            } else {
                out.push(FieldPreview {
                    key: k.clone(),
                    value: match v {
                        Value::String(s) => s.clone(),
                        Value::Null => "null".to_string(),
                        other => other.to_string(),
                    },
                    hidden: false,
                });
            }
        }
    }
    out
}

/// 解析结果 JSON 文本，归一成「账号项」列表。
/// 支持：`{accounts:[...]}`（CPA 输出）/ 顶层数组 / 单条 `{email,credentials}`。
fn parse_items(raw: &str) -> Result<Vec<Value>> {
    let val: Value = serde_json::from_str(raw.trim())
        .context("结果不是合法 JSON（期望 CPA 输出的 accounts[] 结构）")?;
    let items: Vec<Value> = if let Some(arr) = val.get("accounts").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = val.as_array() {
        arr.clone()
    } else if val.get("credentials").is_some() {
        vec![val.clone()]
    } else {
        return Err(anyhow!(
            "无法识别的结果结构：既没有 accounts[]，也不是单条含 credentials 的对象"
        ));
    };
    if items.is_empty() {
        return Err(anyhow!("结果里的账号列表为空，没有可写回的内容"));
    }
    Ok(items)
}

/// 核心：结果 JSON → 写回计划（纯函数，不打印、不发请求）。
///
/// 匹配规则：**以邮箱为键、忽略大小写**。结果里有几个账号就规划几个（批量）：
///   结果项邮箱 → 在 sub2api 账号列表里找 `name` / `credentials.email` 相等的账号 → 写回它。
/// 找不到对应账号的项会被**跳过并列出**，绝不误写。
pub fn build_plan(
    accounts: &[Account],
    raw: &str,
    only_account: Option<i64>,
    only_email: Option<&str>,
) -> Result<ApplyReport> {
    let items = parse_items(raw)?;
    let mut plans: Vec<PlanItem> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for item in &items {
        let email = match account_email(item) {
            Some(e) => e,
            None => {
                skipped.push("(某项缺 email/name，无法匹配)".to_string());
                continue;
            }
        };

        if let Some(oe) = only_email {
            if !email.eq_ignore_ascii_case(oe) {
                continue;
            }
        }

        let target = match find_account(accounts, &email) {
            Some(t) => t,
            None => {
                skipped.push(format!("{}（sub2api 里没有这个邮箱的账号）", email));
                continue;
            }
        };

        if let Some(oid) = only_account {
            if target.id != oid {
                continue;
            }
        }

        let creds = match item.get("credentials").cloned() {
            Some(c) if c.is_object() => c,
            _ => {
                skipped.push(format!("{}（该项没有 credentials 对象）", email));
                continue;
            }
        };

        let merged = merge_credentials(&creds, target.credentials.as_ref().unwrap_or(&Value::Null));
        let extra = target
            .extra
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default()));

        plans.push(PlanItem {
            account_id: target.id,
            account_type: if target.account_type.is_empty() {
                "oauth".to_string()
            } else {
                target.account_type.clone()
            },
            email,
            preview: build_preview(&merged),
            credentials: merged,
            extra,
        });
    }

    Ok(ApplyReport {
        dry_run: true,
        plans,
        skipped,
        outcomes: Vec::new(),
    })
}

/// 「删除被封禁账号」的执行结果。
#[derive(Debug, Clone, Serialize)]
pub struct BannedSweep {
    /// 实际执行过的删除（成功/失败）
    pub outcomes: Vec<DeleteOutcome>,
    /// 是否因为「疑似封禁数 ≥ 本次总数」而被拦下、**未自动删除**
    pub suppressed: bool,
    /// 被拦下、等待人工确认的邮箱（前端据此展示「确认删除」）
    pub pending: Vec<String>,
}

impl BannedSweep {
    pub fn skipped() -> Self {
        Self {
            outcomes: Vec::new(),
            suppressed: false,
            pending: Vec::new(),
        }
    }
}

/// 邮箱集合归一化：trim + 转小写 + 丢空串 + 排序去重。
fn normalize_emails(list: &[String]) -> Vec<String> {
    let mut v: Vec<String> = list
        .iter()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// 裸删除执行体：按邮箱精确匹配账号后逐个删除。**内部不做任何闸**，调用方必须先过闸。
///
/// 全仓库只有这里发 `DELETE /api/v1/admin/accounts/:id`，供两条路径复用：
/// - 自动清理：`delete_banned_accounts`（过闸后调用）
/// - 人工确认删除：`delete_accounts_by_email`（用户点「确认删除」后直接调用）
///
/// 空列表一律什么都不做。
pub fn delete_accounts_by_email(
    client: &mut Sub2ApiClient,
    accounts: &[Account],
    emails: &[String],
    log: &Logger,
) -> Vec<DeleteOutcome> {
    let mut out = Vec::new();
    for email in &normalize_emails(emails) {
        let target = match find_account(accounts, email) {
            Some(a) => a,
            None => {
                log(format!("未找到对应 sub2api 账号，跳过删除：{}", email));
                continue;
            }
        };
        match client.delete_account(target.id) {
            Ok(_) => {
                log(format!("已删除账号 #{} {}", target.id, email));
                out.push(DeleteOutcome {
                    account_id: target.id,
                    email: email.clone(),
                    ok: true,
                    message: "已删除".to_string(),
                });
            }
            Err(e) => {
                log(format!("删除账号 #{} {} 失败：{}", target.id, email, e));
                out.push(DeleteOutcome {
                    account_id: target.id,
                    email: email.clone(),
                    ok: false,
                    message: format!("{}", e),
                });
            }
        }
    }
    out
}

/// 一键流程辅助：把 fetch 检测到的被封禁/停用邮箱从 sub2api 删除。
///
/// ## 三道防误删闸（血泪事故，删除不可逆，务必保留）
///
/// 真实事故：结果页提取逻辑曾把**包住全部结果的祖先容器**当成单条结果
/// （门页 `#jobs` 的 class 字面量就是 `"jobs"`，含子串 `job`；外层还有 `div.panel`）。
/// 于是**只要任意一条**失败项的日志里出现封禁词，整页邮箱全被判封禁
/// → 连**成功**的账号一起从 sub2api 删光。
///
/// 闸 0（fail-closed）：`scope` 为空（或全是空串）→ **一个都不删**。
///                    没给范围不等于不受限，这个函数不能依赖调用方替它保证非空。
/// 闸 1（`scope`）：只删**本次送进浏览器的邮箱**。结果页可能残留别的 job，
///                  删到范围外属于越权。
/// 闸 2（比例）：疑似封禁数 ≥ 本次总数 → **不自动删**，转人工确认（结果落在 `pending`）。
///              真·全批封禁概率极低，而误判全删真实发生过。**这里刻意不设「本次 ≥2」的
///              下限**：单账号批次（1/1 命中）同样是 100% 命中，无法与误报区分，
///              也必须走人工确认，否则闸 2 在最窄的场景下恰好失效。
///
/// 提取侧的对应修复见 `browser::JS_EXTRACT_BANNED`。
pub fn delete_banned_accounts(
    client: &mut Sub2ApiClient,
    accounts: &[Account],
    banned: &[String],
    scope: &[String],
    log: &Logger,
) -> BannedSweep {
    // ── 闸 0 + 闸 1：scope 必须非空，且只删本次范围 ──────
    // 注意：**scope 为空必须 fail-closed**。绝不能写成 `allowed.is_empty() || contains(e)`
    // ——那等于「没给范围就无限制」，会让闸 1 和闸 2 同时失效（闸 2 依赖 allowed.len()），
    // 正好退回历史事故的形状：整页邮箱一起删。
    let allowed: std::collections::HashSet<String> = normalize_emails(scope).into_iter().collect();
    if allowed.is_empty() {
        log("本次没有提供邮箱范围（scope 为空），为防误删一律不删除任何账号".to_string());
        return BannedSweep::skipped();
    }
    let banned_norm = normalize_emails(banned);
    let list: Vec<String> = banned_norm
        .iter()
        .filter(|e| allowed.contains(*e))
        .cloned()
        .collect();
    let dropped = banned_norm.len().saturating_sub(list.len());
    if dropped > 0 {
        log(format!(
            "已过滤掉 {} 个不属于本次范围的疑似封禁邮箱（不删）",
            dropped
        ));
    }

    if list.is_empty() {
        return BannedSweep::skipped();
    }

    // ── 闸 2：全批命中 → 不自动删，转人工确认 ────────────
    if list.len() >= allowed.len() {
        log(format!(
            "⚠️ 疑似封禁 {} 个，已达本次提交总数 {} —— 不再自动删除，请人工核对后确认。",
            list.len(),
            allowed.len()
        ));
        return BannedSweep {
            outcomes: Vec::new(),
            suppressed: true,
            pending: list,
        };
    }

    let outcomes = delete_accounts_by_email(client, accounts, &list, log);
    BannedSweep {
        outcomes,
        suppressed: false,
        pending: Vec::new(),
    }
}

/// 核心：执行写回（逐项 POST `apply-oauth-credentials`，成功后恢复调度开关）。
///
/// 上游在报 401 时会顺手把 `schedulable` 置 false（调度开关关闭、状态「暂停」），
/// 而 `apply-oauth-credentials` 只清错误不恢复调度——所以写回成功后必须补一刀
/// `POST /accounts/:id/schedulable {"schedulable":true}`，否则账号换完凭证仍然是暂停的。
pub fn execute_plan(client: &mut Sub2ApiClient, plans: &[PlanItem]) -> Vec<ApplyOutcome> {
    let mut out = Vec::new();
    for p in plans {
        let body = serde_json::json!({
            "type": p.account_type,
            "credentials": p.credentials,
            "extra": p.extra,
        });
        match client.apply_oauth_credentials(p.account_id, &body) {
            Ok(_) => {
                // 恢复调度开关；失败不算写回失败，但要在结果里点名
                let sched_msg = match client.set_schedulable(p.account_id, true) {
                    Ok(_) => "调度已恢复".to_string(),
                    Err(e) => format!("⚠️ 调度开关恢复失败（{}），请到后台手动打开", e),
                };
                out.push(ApplyOutcome {
                    account_id: p.account_id,
                    email: p.email.clone(),
                    ok: true,
                    message: format!("写回成功，{}", sched_msg),
                });
            }
            Err(e) => out.push(ApplyOutcome {
                account_id: p.account_id,
                email: p.email.clone(),
                ok: false,
                message: format!("{}", e),
            }),
        }
    }
    out
}

/// 一站式入口（供 Tauri 调用）：解析 → 建计划 → （可选）执行。
/// `yes=false` 时只返回计划（dry-run）；`yes=true` 时返回计划 + 执行结果。
pub fn apply_value(
    client: &mut Sub2ApiClient,
    raw: &str,
    yes: bool,
    only_account: Option<i64>,
    only_email: Option<&str>,
) -> Result<ApplyReport> {
    let accounts = client.list_accounts()?;
    let mut report = build_plan(&accounts, raw, only_account, only_email)?;
    if report.plans.is_empty() {
        let tail = if report.skipped.is_empty() {
            String::new()
        } else {
            format!(
                "（跳过 {} 项：{}）",
                report.skipped.len(),
                report.skipped.join("；")
            )
        };
        return Err(anyhow!("没有匹配到任何可写回的账号。{}", tail));
    }
    if yes {
        report.outcomes = execute_plan(client, &report.plans);
        report.dry_run = false;
    }
    Ok(report)
}

/// `reauth apply`：把结果 JSON（CPA 输出 / sub2api 导入格式）里的 oauth 凭证写回 sub2api 对应账号。
///
/// 凭证来源：`--from <file>` 优先，否则读系统剪切板。
/// 默认 **dry-run**：打印「结果邮箱 → sub2api 账号 id」的匹配计划和将写回的字段（token 隐藏）；
/// 加 `--yes` 才逐个 POST `apply-oauth-credentials`。
pub fn apply(client: &mut Sub2ApiClient, opts: ApplyOpts) -> Result<()> {
    // 1) 读结果 JSON
    let raw = match opts.from {
        Some(p) => {
            std::fs::read_to_string(p).with_context(|| format!("读取 --from 文件失败：{}", p))?
        }
        None => read_clipboard()?,
    };
    if raw.trim().is_empty() {
        return Err(anyhow!(
            "没有可写回的结果（剪切板为空）。请先：401 页「复制全部」→ CPA 页贴入左侧 → 点「复制输出」，再跑本命令；或用 --from 指定 JSON 文件。"
        ));
    }

    let report = apply_value(client, &raw, opts.yes, opts.only_account, opts.only_email)?;

    // 2) 预览匹配计划 + 将写回的字段
    println!("共匹配到 {} 个账号待写回：\n", report.plans.len());
    for p in &report.plans {
        println!("▶ 账号 #{}  ← 结果邮箱 {}", p.account_id, p.email);
        for f in &p.preview {
            if f.hidden {
                println!("    - {:<22} {}", f.key, format!("{}，已隐藏", f.value));
            } else {
                println!("    - {:<22} {}", f.key, f.value);
            }
        }
        println!();
    }
    if !report.skipped.is_empty() {
        println!(
            "（跳过 {} 项：{}）\n",
            report.skipped.len(),
            report.skipped.join("；")
        );
    }

    if report.dry_run {
        println!("（dry-run）未发送。核对无误后加 `--yes` 真正写回。");
        return Ok(());
    }

    // 3) 汇总执行结果
    let ok = report.outcomes.iter().filter(|o| o.ok).count();
    let fail = report.outcomes.len() - ok;
    for o in &report.outcomes {
        if o.ok {
            println!("✅ #{} {} 写回成功", o.account_id, o.email);
        } else {
            println!("❌ #{} {} 写回失败：{}", o.account_id, o.email, o.message);
        }
    }
    println!("\n完成：成功 {} / 失败 {}", ok, fail);
    if fail > 0 {
        anyhow::bail!("有 {} 个账号写回失败", fail);
    }
    Ok(())
}

/// 日志回调：一键流程把进度实时交给调用方（CLI 打印 / web 控制台轮询展示）。
pub type Logger = Arc<dyn Fn(String) + Send + Sync>;

/// 一键重授权的参数。
pub struct OnceOpts<'a> {
    /// 接码门页地址
    pub gate_url: &'a str,
    /// CPA 转换页地址（fetch 完成后打开，把结果转成 sub2api 凭证）
    pub cpa_url: &'a str,
    /// 浏览器轮询最长等待（毫秒）
    pub max_ms: u64,
    /// 浏览器引擎：None=内置 Chromium（服务器/Docker）
    pub engine: Option<&'a str>,
    /// CDK（None 时从 credentials.db 读）
    pub cdk: Option<String>,
    /// 真正写回；false = 只做 dry-run 预览
    pub yes: bool,
    /// 只处理该邮箱（忽略大小写）；None = 所有需要重授权的账号
    pub only_email: Option<&'a str>,
}

/// 一键重授权的结果。
pub struct OnceResult {
    /// 本次送进去重授权的邮箱
    pub emails: Vec<String>,
    /// 拿到的结果文本长度（字符）
    pub raw_len: usize,
    /// 写回报告（dry_run / plans / outcomes / skipped）
    pub report: ApplyReport,
    /// fetch 结果页检测到的被封禁/停用邮箱
    pub banned_emails: Vec<String>,
    /// 已执行的删除结果
    pub deleted: Vec<DeleteOutcome>,
}

/// 一键重授权：**自动找出需要重授权的账号 → 无头浏览器跑接码 → 结果按邮箱写回**。
///
/// 步骤：登录后的 client 拉全量账号 → 过滤 `needs_reauth()`（或只处理 `--email`）→
/// `browser::fetch_stream` 走门页/邮箱/获取/CPA 转换 → 取 `cpaPage.output`（没有则退回剪切板）→
/// `apply_value` 按邮箱忽略大小写匹配写回。
pub fn run_once(client: &mut Sub2ApiClient, opts: OnceOpts, log: &Logger) -> Result<OnceResult> {
    let accounts = client.list_accounts()?;

    let targets: Vec<&Account> = match opts.only_email {
        Some(e) => match find_account(&accounts, e) {
            Some(a) => vec![a],
            None => bail!("sub2api 里没有邮箱为 {} 的账号", e),
        },
        None => accounts.iter().filter(|a| a.needs_reauth()).collect(),
    };
    if targets.is_empty() {
        bail!("当前没有需要重授权的账号（都没报 401），无需操作");
    }

    let emails: Vec<String> = targets.iter().map(|a| a.name.clone()).collect();
    log(format!(
        "待重授权 {} 个账号：{}",
        emails.len(),
        emails.join(", ")
    ));

    let cdk = match opts.cdk {
        Some(c) if !c.trim().is_empty() => c,
        _ => store::load()?
            .map(|c| c.cdk)
            .filter(|s| !s.trim().is_empty())
            .context("credentials.db 里没有 CDK，请先在页面保存 CDK 再执行")?,
    };

    log(format!(
        "启动无头浏览器：门页 → 填邮箱 → 获取令牌（最长等待 {}s）",
        opts.max_ms / 1000
    ));
    let logger = Arc::clone(log);
    let out = browser::fetch_stream(
        opts.gate_url,
        &emails,
        Some(&cdk),
        opts.max_ms,
        Some(opts.cpa_url),
        opts.engine,
        Arc::new(move |l| logger(l)),
    )?;

    // 结果优先取 CPA 转换输出（已是 sub2api 凭证，含 refresh_token），否则退回剪切板原文
    let cpa = out
        .get("cpaPage")
        .and_then(|c| c.get("output"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let clip = out
        .get("clipboard")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let raw = if !cpa.is_empty() { cpa } else { clip };
    if raw.is_empty() {
        bail!("浏览器流程没有拿到结果（CPA 输出与剪切板都为空）");
    }
    log(format!(
        "拿到结果（{} 字符），开始按邮箱匹配写回",
        raw.len()
    ));

    // 先处理被封禁/停用的账号：从 sub2api 直接删除，避免继续写回或调度。
    // 防误删的两道闸在 `delete_banned_accounts` 内部（提取侧的修复见 JS_EXTRACT_BANNED）。
    let banned_emails: Vec<String> = out
        .get("bannedEmails")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_lowercase()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let deleted = if !banned_emails.is_empty() {
        delete_banned_accounts(client, &accounts, &banned_emails, &emails, log).outcomes
    } else {
        Vec::new()
    };

    let report = apply_value(client, &raw, opts.yes, None, opts.only_email)?;

    if opts.yes {
        let ok = report.outcomes.iter().filter(|o| o.ok).count();
        let fail = report.outcomes.len() - ok;
        for o in &report.outcomes {
            if o.ok {
                log(format!("✅ #{} {} 写回成功", o.account_id, o.email));
            } else {
                log(format!(
                    "❌ #{} {} 写回失败：{}",
                    o.account_id, o.email, o.message
                ));
            }
        }
        log(format!("完成：成功 {} / 失败 {}", ok, fail));
    } else {
        log(format!(
            "（预览）匹配到 {} 个账号，未真正写回。勾选「真正写回」再执行一次。",
            report.plans.len()
        ));
    }
    for s in &report.skipped {
        log(format!("跳过：{}", s));
    }

    Ok(OnceResult {
        emails,
        raw_len: raw.len(),
        report,
        banned_emails,
        deleted,
    })
}

/// 读系统剪切板。macOS 用 `pbpaste`；其它平台给出明确提示。
fn read_clipboard() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("pbpaste")
            .output()
            .context("读取剪切板失败（需要 macOS 的 pbpaste）")?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(anyhow!(
            "当前平台不支持自动读剪切板（仅 macOS pbpaste）。请用 --from 指定结果 JSON 文件。"
        ))
    }
}

#[cfg(test)]
mod banned_sweep_tests {
    use super::*;

    fn acct(id: i64, email: &str) -> Account {
        Account {
            id,
            name: email.to_string(),
            platform: "openai".to_string(),
            account_type: "oauth".to_string(),
            status: "error".to_string(),
            error_message: "401 Unauthorized".to_string(),
            expires_at: None,
            schedulable: false,
            priority: None,
            credentials: None,
            extra: None,
        }
    }

    /// 指向必然拒绝连接的端口：任何真的发起的删除都会以 `ok:false` 落进 `outcomes`，
    /// 于是「`outcomes` 为空」就等价于「一个删除都没发起」——不用起假服务就能断言。
    fn offline_client() -> Sub2ApiClient {
        Sub2ApiClient::new("http://127.0.0.1:9".to_string(), "t".to_string(), None)
    }

    fn quiet() -> Logger {
        Arc::new(|_m: String| {})
    }

    fn emails(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 闸 0：scope 为空绝不能退化成「无限制删除」。
    #[test]
    fn empty_scope_deletes_nothing() {
        let accounts = vec![acct(1, "a@x.com"), acct(2, "b@x.com")];
        let mut client = offline_client();
        let sweep = delete_banned_accounts(
            &mut client,
            &accounts,
            &emails(&["a@x.com", "b@x.com"]),
            &[],
            &quiet(),
        );
        assert!(sweep.outcomes.is_empty(), "空 scope 不得删除任何账号");
        assert!(!sweep.suppressed);

        // 全是空串 / 纯空白，同样算空 scope
        let mut client = offline_client();
        let sweep = delete_banned_accounts(
            &mut client,
            &accounts,
            &emails(&["a@x.com"]),
            &emails(&["", "   "]),
            &quiet(),
        );
        assert!(sweep.outcomes.is_empty(), "空串 scope 不得删除任何账号");
    }

    /// 闸 1：范围外的邮箱一律不删（结果页可能残留别的 job）。
    #[test]
    fn out_of_scope_is_never_deleted() {
        let accounts = vec![acct(1, "a@x.com"), acct(9, "other@x.com")];
        let mut client = offline_client();
        let sweep = delete_banned_accounts(
            &mut client,
            &accounts,
            &emails(&["other@x.com"]),
            &emails(&["a@x.com", "b@x.com"]),
            &quiet(),
        );
        assert!(sweep.outcomes.is_empty(), "范围外的邮箱不得被删除");
    }

    /// 闸 2：疑似封禁数 ≥ 本次总数 → 不自动删，转人工确认（历史全删事故的形状）。
    #[test]
    fn full_batch_match_is_suppressed() {
        let accounts = vec![acct(1, "a@x.com"), acct(2, "b@x.com")];
        let mut client = offline_client();
        let sweep = delete_banned_accounts(
            &mut client,
            &accounts,
            &emails(&["a@x.com", "b@x.com"]),
            &emails(&["a@x.com", "b@x.com"]),
            &quiet(),
        );
        assert!(sweep.suppressed, "全批命中应转人工确认");
        assert!(sweep.outcomes.is_empty(), "全批命中时不得自动删除任何账号");
        assert_eq!(sweep.pending, emails(&["a@x.com", "b@x.com"]));
    }

    /// 单账号批次（1/1 命中）同样只待确认：100% 命中无法与误报区分，
    /// 闸 2 不能因为「本次只有 1 个」就放行。
    #[test]
    fn single_account_batch_is_held_for_confirmation() {
        let accounts = vec![acct(1, "a@x.com")];
        let mut client = offline_client();
        let sweep = delete_banned_accounts(
            &mut client,
            &accounts,
            &emails(&["a@x.com"]),
            &emails(&["a@x.com"]),
            &quiet(),
        );
        assert!(sweep.suppressed);
        assert!(sweep.outcomes.is_empty(), "1/1 命中不得自动删除");
        assert_eq!(sweep.pending, emails(&["a@x.com"]));
    }

    /// 人工确认删除走裸执行体：不受闸 2 影响，但空列表仍然什么都不做。
    #[test]
    fn confirmed_delete_bypasses_ratio_gate_but_not_empty() {
        let accounts = vec![acct(1, "a@x.com"), acct(2, "b@x.com")];
        let mut client = offline_client();
        let out = delete_accounts_by_email(
            &mut client,
            &accounts,
            &emails(&["a@x.com", "b@x.com"]),
            &quiet(),
        );
        assert_eq!(out.len(), 2, "确认后应逐个尝试删除");
        assert_eq!(out[0].account_id, 1);
        assert_eq!(out[1].account_id, 2);

        let mut client = offline_client();
        let out = delete_accounts_by_email(&mut client, &accounts, &[], &quiet());
        assert!(out.is_empty(), "空列表不得发起删除");

        let mut client = offline_client();
        let out = delete_accounts_by_email(&mut client, &accounts, &emails(&["  "]), &quiet());
        assert!(out.is_empty(), "空串列表不得发起删除");
    }

    /// 人工确认删除遇到不存在的邮箱：跳过、不报错、也不删别人。
    #[test]
    fn confirmed_delete_skips_unknown_email() {
        let accounts = vec![acct(1, "a@x.com")];
        let mut client = offline_client();
        let out = delete_accounts_by_email(
            &mut client,
            &accounts,
            &emails(&["nobody@x.com", "a@x.com"]),
            &quiet(),
        );
        assert_eq!(out.len(), 1, "只有真实存在的账号会被尝试删除");
        assert_eq!(out[0].account_id, 1);
    }

    /// 正常路径：只有一部分被判封禁 → 只删那一个，且落在正确的 id 上。
    #[test]
    fn partial_match_deletes_only_flagged_account() {
        let accounts = vec![acct(1, "a@x.com"), acct(2, "b@x.com"), acct(3, "c@x.com")];
        let mut client = offline_client();
        let sweep = delete_banned_accounts(
            &mut client,
            &accounts,
            &emails(&["c@x.com"]),
            &emails(&["a@x.com", "b@x.com", "c@x.com"]),
            &quiet(),
        );
        assert!(!sweep.suppressed);
        assert_eq!(sweep.outcomes.len(), 1, "只应对被判封禁的那一个发起删除");
        assert_eq!(sweep.outcomes[0].account_id, 3);
        assert_eq!(sweep.outcomes[0].email, "c@x.com");
    }

    /// 邮箱匹配忽略大小写（scope 与被封禁列表两侧都要）。
    #[test]
    fn scope_and_banned_matching_ignores_case() {
        let accounts = vec![acct(1, "a@x.com")];
        let mut client = offline_client();
        let sweep = delete_banned_accounts(
            &mut client,
            &accounts,
            &emails(&["A@X.COM"]),
            &emails(&["a@x.com", "b@x.com"]),
            &quiet(),
        );
        assert_eq!(sweep.outcomes.len(), 1);
        assert_eq!(sweep.outcomes[0].account_id, 1);
    }
}
