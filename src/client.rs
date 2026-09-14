use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client as HttpClient;
use reqwest::header;
use std::time::Duration;

use crate::config::Config;
use crate::models::Account;

/// sub2api admin API 客户端。负责登录、2FA、token 自动刷新，以及所有受保护请求。
pub struct Sub2ApiClient {
    http: HttpClient,
    base_url: String,
    access_token: String,
    refresh_token: Option<String>,
}

fn extract_data(v: serde_json::Value) -> serde_json::Value {
    if let Some(data) = v.get("data") {
        if !data.is_null() {
            return data.clone();
        }
    }
    v
}

impl Sub2ApiClient {
    pub fn new(base_url: String, access_token: String, refresh_token: Option<String>) -> Self {
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("构建 HTTP client 失败");
        Self {
            http,
            base_url,
            access_token,
            refresh_token,
        }
    }

    /// 用管理员邮箱+密码登录。开了 2FA 时自动用 totp_secret 完成第二步。
    pub fn login(config: &Config) -> Result<Self> {
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(config.verify_tls == false)
            .build()
            .context("构建 HTTP client 失败")?;

        let url = format!("{}/api/v1/auth/login", config.base_url.trim_end_matches('/'));
        let body = serde_json::json!({ "email": config.email, "password": config.password });
        let resp = http.post(&url).json(&body).send().context("登录请求失败")?;
        let status = resp.status();
        let val: serde_json::Value = resp.json().context("解析登录响应失败")?;
        if !status.is_success() {
            return Err(anyhow!("登录失败 ({}): {}", status, val));
        }
        let data = extract_data(val);

        // 2FA 分支
        if data.get("requires_2fa").and_then(|v| v.as_bool()) == Some(true) {
            let temp = data
                .get("temp_token")
                .and_then(|v| v.as_str())
                .context("服务端要求 2FA 但未返回 temp_token")?;
            let secret = config
                .totp_secret
                .as_ref()
                .context("后台已开启 2FA，但 config.toml 未填 totp_secret")?;
            let code = crate::totp::generate_current(secret)?;
            let url2 = format!("{}/api/v1/auth/login/2fa", config.base_url.trim_end_matches('/'));
            let body2 = serde_json::json!({ "temp_token": temp, "totp_code": code });
            let resp2 = http.post(&url2).json(&body2).send().context("2FA 请求失败")?;
            let val2: serde_json::Value = resp2.json().context("解析 2FA 响应失败")?;
            let d2 = extract_data(val2);
            let at = d2
                .get("access_token")
                .and_then(|v| v.as_str())
                .context("2FA 后未返回 access_token")?
                .to_string();
            let rt = d2.get("refresh_token").and_then(|v| v.as_str()).map(str::to_string);
            return Ok(Self::new(config.base_url.clone(), at, rt));
        }

        let at = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .context("登录未返回 access_token")?
            .to_string();
        let rt = data.get("refresh_token").and_then(|v| v.as_str()).map(str::to_string);
        Ok(Self::new(config.base_url.clone(), at, rt))
    }

    /// 仅供调试输出：access_token 前 8 位。
    pub fn token_prefix(&self) -> &str {
        &self.access_token[..self.access_token.len().min(8)]
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/api/v1/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// 用 refresh_token 换新 token 对。
    fn refresh(&mut self) -> Result<()> {
        let rt = self.refresh_token.clone().context("无 refresh_token 可刷新")?;
        let body = serde_json::json!({ "refresh_token": rt });
        let resp = self
            .http
            .post(self.url("auth/refresh"))
            .json(&body)
            .send()
            .context("刷新 token 请求失败")?;
        let val: serde_json::Value = resp.json().context("解析刷新响应失败")?;
        let data = extract_data(val);
        self.access_token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .context("刷新后未返回 access_token")?
            .to_string();
        if let Some(r) = data.get("refresh_token").and_then(|v| v.as_str()) {
            self.refresh_token = Some(r.to_string());
        }
        Ok(())
    }

    /// 通用受保护请求。401 且持有 refresh_token 时自动刷新并重试一次。
    pub fn request(
        &mut self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        for attempt in 0..2 {
            let mut rb = self
                .http
                .request(method.clone(), self.url(path))
                .header(header::AUTHORIZATION, format!("Bearer {}", self.access_token))
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(b) = body {
                rb = rb.json(b);
            }
            let resp = rb.send().context("请求失败")?;
            let status = resp.status();
            if status == 401 && attempt == 0 && self.refresh_token.is_some() {
                self.refresh()?;
                continue;
            }
            let text = resp.text().unwrap_or_default();
            if !status.is_success() {
                return Err(anyhow!("HTTP {} {}: {}", status, path, text));
            }
            let val: serde_json::Value =
                serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            return Ok(extract_data(val));
        }
        Err(anyhow!("请求重试耗尽"))
    }

    /// 把 OAuth 凭证写回指定账号（等价于「手动输入 rt 重新授权」）。
    /// 端点：`POST /api/v1/admin/accounts/:id/apply-oauth-credentials`，body `{type, credentials, extra}`。
    /// 服务端会清错误标记 + 失效 token 缓存，且不会新建重复账号。
    pub fn apply_oauth_credentials(
        &mut self,
        account_id: i64,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let path = format!("admin/accounts/{}/apply-oauth-credentials", account_id);
        self.request(reqwest::Method::POST, &path, Some(body))
    }

    /// 设置账号是否可调度（= 管理页的「调度」开关）。
    /// 端点：`POST /api/v1/admin/accounts/:id/schedulable`，body `{"schedulable": bool}`。
    /// 上游 401 时 `SetError` 会顺手把 schedulable 置 false，重授权写回后需要把它打开。
    pub fn set_schedulable(&mut self, account_id: i64, schedulable: bool) -> Result<serde_json::Value> {
        let path = format!("admin/accounts/{}/schedulable", account_id);
        let body = serde_json::json!({ "schedulable": schedulable });
        self.request(reqwest::Method::POST, &path, Some(&body))
    }

    /// 删除指定账号（用于账号已被封禁/停用等无法继续使用的场景）。
    /// 端点：`DELETE /api/v1/admin/accounts/:id`。
    pub fn delete_account(&mut self, account_id: i64) -> Result<serde_json::Value> {
        let path = format!("admin/accounts/{}", account_id);
        self.request(reqwest::Method::DELETE, &path, None)
    }

    /// 列出全部账号。用响应里的 `total` 字段翻页拉全，避免账号数超过单页上限被截断。
    pub fn list_accounts(&mut self) -> Result<Vec<Account>> {
        let mut all: Vec<Account> = Vec::new();
        let mut page: u32 = 1;
        const PAGE_SIZE: u32 = 1000; // 服务端上限
        loop {
            let path = format!("admin/accounts?page={}&page_size={}", page, PAGE_SIZE);
            let val = self.request(reqwest::Method::GET, &path, None)?;
            // 响应信封：{ success, data:{ items, total, page, page_size, pages } }
            let items = val
                .get("items")
                .and_then(|x| x.as_array())
                .cloned()
                .ok_or_else(|| anyhow!("意外的账号列表响应: {}", val))?;
            let total = val
                .get("total")
                .and_then(|t| t.as_i64())
                .unwrap_or(items.len() as i64);
            for it in &items {
                if let Ok(a) = serde_json::from_value::<Account>(it.clone()) {
                    all.push(a);
                }
            }
            if all.len() as i64 >= total || items.is_empty() {
                break;
            }
            page += 1;
            if page > 1000 {
                // 安全阀：理论上不会到这（total 上限受 DB 限制），防止死循环
                break;
            }
        }
        Ok(all)
    }
}
