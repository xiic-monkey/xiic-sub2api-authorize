//! 非 CDK 授权路径（走 sub2api 自带授权链接）。
//!
//! 与 CDK 路径的区别：不再去第三方门页消耗 CDK，而是
//! 1. 让 sub2api 生成一条 OpenAI 授权链接（`POST /admin/openai/generate-auth-url`）；
//! 2. 用有机隐身浏览器打开它，自动填账号邮箱、自动从收码站取验证码；
//! 3. 授权完成后浏览器会跳到 `http://localhost:1455/auth/callback?code=...`，
//!    从 URL 里抠出 `code`；
//! 4. `POST /admin/accounts/exchange-code {session_id, code}` 换回凭证；
//! 5. 按账号 id 写回 `apply-oauth-credentials` 并恢复调度开关。
//!
//! **已知边界**：OpenAI 的 Cloudflare + Sentinel 会拦截部分自动化登录。
//! 走到需要人工（质询点验证 / 要密码）时，本模块会如实返回 `needs_human`，
//! 有头模式下用户可在窗口里手动完成，流程继续。

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::browser::{self, BrowserMode, OAuthHooks};
use crate::client::Sub2ApiClient;
use crate::commands::mail_ops;
use crate::commands::reauth::{execute_plan, ApplyOutcome, PlanItem};
use crate::config::Config;
use crate::mail::BindReport;

/// 非 CDK 单账号授权结果。
#[derive(Debug, Clone, Serialize)]
pub struct OAuthAccountOutcome {
    pub account_id: i64,
    pub email: String,
    pub ok: bool,
    /// 停在哪个阶段：done | need_code | need_password | blocked | timeout | cancelled | error
    pub stage: String,
    pub message: String,
}

/// 非 CDK 一键授权报告。
#[derive(Debug, Clone, Serialize)]
pub struct OAuthReport {
    /// 本次要重置的邮箱
    pub emails: Vec<String>,
    /// 邮箱导入收码站的结果（失败为 None）
    pub imported: Option<BindReport>,
    /// 导入失败原因（不阻断主流程）
    pub import_error: Option<String>,
    /// 逐账号结果
    pub outcomes: Vec<OAuthAccountOutcome>,
    /// 写回结果（逐账号）
    pub applied: Vec<ApplyOutcome>,
}

/// 非 CDK 授权选项。
#[derive(Debug, Clone)]
pub struct OAuthOptions {
    /// 只处理这些邮箱（空 / None = 处理全部 401 账号）
    pub only_emails: Option<Vec<String>>,
    /// 是否真的写回（false = 只跑到拿 code 为止，不落库）
    pub yes: bool,
    /// 有头窗口
    pub headed: bool,
    /// 单账号最长等待秒数
    pub max_seconds: u64,
    /// 浏览器引擎
    pub engine: Option<String>,
}

impl OAuthOptions {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            only_emails: None,
            yes: true,
            headed: cfg.openai_headed,
            max_seconds: cfg.openai_max_seconds,
            engine: None,
        }
    }
}

/// 非 CDK 一键授权主流程。
pub fn run_once(
    config: &Config,
    opts: &OAuthOptions,
    cancel: Option<&AtomicBool>,
    hooks: &OAuthHooks,
) -> Result<OAuthReport> {
    let log = |m: String| (hooks.log)(m);

    let mut client = Sub2ApiClient::login(config)?;
    let accounts = client.list_accounts()?;

    // 选目标：指定邮箱 > 全部 401 账号
    let want: Option<Vec<String>> = opts
        .only_emails
        .as_ref()
        .map(|v| v.iter().map(|s| s.trim().to_lowercase()).collect())
        .filter(|v: &Vec<String>| !v.is_empty());
    let targets: Vec<(i64, String, String)> = accounts
        .iter()
        .filter_map(|a| {
            let email = a
                .credentials
                .as_ref()
                .and_then(|c| c.get("email"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| a.name.clone());
            let em = email.trim().to_string();
            if let Some(list) = want.as_ref() {
                if !list.contains(&em.to_lowercase()) {
                    return None;
                }
            } else if !a.has_401() {
                return None;
            }
            if !em.contains('@') {
                return None;
            }
            Some((a.id, em, a.account_type.clone()))
        })
        .collect();

    if targets.is_empty() {
        anyhow::bail!(
            "没有需要授权的账号{}。",
            if want.is_some() {
                "（勾选的邮箱都没匹配到）"
            } else {
                "（当前没有 401/异常账号）"
            }
        );
    }
    let emails: Vec<String> = targets.iter().map(|t| t.1.clone()).collect();
    log(format!("本次需要授权的邮箱：{}", emails.join("、")));

    // 先确保这些邮箱在收码站里（否则读不到验证码）
    let mail_cfg = config.mail_config();
    let (imported, import_error) = if mail_cfg.ready() {
        (hooks.step)("import", "导入邮箱到收码站".to_string());
        match mail_ops::import_emails(&mut client, &mail_cfg, false, &log) {
            Ok(r) => (Some(r), None),
            Err(e) => {
                let msg = format!("{:#}", e);
                log(format!("⚠️ 邮箱导入失败（继续尝试授权）：{}", msg));
                (None, Some(msg))
            }
        }
    } else {
        log("⚠️ 未配置收码站，验证码将无法自动获取".to_string());
        (None, Some("未配置收码站".to_string()))
    };

    let mode = if opts.headed {
        BrowserMode::StealthHeaded
    } else {
        BrowserMode::Stealth
    };

    let mut outcomes = Vec::new();
    let mut plans: Vec<PlanItem> = Vec::new();

    for (idx, (account_id, email, account_type)) in targets.iter().enumerate() {
        if let Some(c) = cancel {
            if c.load(std::sync::atomic::Ordering::SeqCst) {
                log("已取消".to_string());
                break;
            }
        }
        log(format!(
            "—— [{}/{}] 处理 {} ——",
            idx + 1,
            targets.len(),
            email
        ));
        (hooks.step)("open", format!("生成 {} 的授权链接", email));

        let url_resp = match client.generate_openai_auth_url() {
            Ok(v) => v,
            Err(e) => {
                outcomes.push(OAuthAccountOutcome {
                    account_id: *account_id,
                    email: email.clone(),
                    ok: false,
                    stage: "error".into(),
                    message: format!("生成授权链接失败：{:#}", e),
                });
                continue;
            }
        };
        let auth_url = url_resp
            .get("auth_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = url_resp
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if auth_url.is_empty() || session_id.is_empty() {
            outcomes.push(OAuthAccountOutcome {
                account_id: *account_id,
                email: email.clone(),
                ok: false,
                stage: "error".into(),
                message: format!("授权链接响应异常：{}", url_resp),
            });
            continue;
        }
        log("已生成授权链接，启动浏览器…".to_string());

        let res = browser::openai_oauth(
            &auth_url,
            email,
            &mail_cfg,
            mode,
            opts.engine.as_deref(),
            opts.max_seconds,
            cancel,
            hooks,
        );

        let outcome = match res {
            Err(e) => OAuthAccountOutcome {
                account_id: *account_id,
                email: email.clone(),
                ok: false,
                stage: "error".into(),
                message: format!("浏览器流程失败：{:#}", e),
            },
            Ok(o) => {
                let mut stage = o.status.clone();
                let mut message = o.message.clone();
                let mut creds: Option<Value> = None;

                if let Some(code) = o.code.as_ref() {
                    (hooks.step)("exchange", "换取凭证".to_string());
                    match client.exchange_code(&session_id, code) {
                        Ok(v) => {
                            creds = Some(v);
                            message = "已换回凭证".to_string();
                        }
                        Err(e) => {
                            stage = "exchange_failed".into();
                            message = format!("换取凭证失败：{:#}", e);
                        }
                    }
                }

                if let Some(c) = creds {
                    // 复用 reauth 的写回逻辑（含恢复调度开关）
                    let extra = accounts
                        .iter()
                        .find(|a| a.id == *account_id)
                        .and_then(|a| a.extra.clone())
                        .unwrap_or(Value::Null);
                    plans.push(PlanItem {
                        account_id: *account_id,
                        account_type: account_type.clone(),
                        email: email.clone(),
                        preview: Vec::new(),
                        credentials: c,
                        extra,
                    });
                    OAuthAccountOutcome {
                        account_id: *account_id,
                        email: email.clone(),
                        ok: true,
                        stage: "done".into(),
                        message,
                    }
                } else {
                    OAuthAccountOutcome {
                        account_id: *account_id,
                        email: email.clone(),
                        ok: false,
                        stage,
                        message,
                    }
                }
            }
        };
        log(format!(
            "{} → {}{}",
            email,
            if outcome.ok { "✓ " } else { "✗ " },
            outcome.message
        ));
        outcomes.push(outcome);
    }

    let applied = if opts.yes && !plans.is_empty() {
        (hooks.step)("apply", format!("写回 {} 个账号", plans.len()));
        execute_plan(&mut client, &plans)
    } else {
        if !opts.yes {
            log("已跳过写回（dry-run）".to_string());
        }
        Vec::new()
    };

    Ok(OAuthReport {
        emails,
        imported,
        import_error,
        outcomes,
        applied,
    })
}
