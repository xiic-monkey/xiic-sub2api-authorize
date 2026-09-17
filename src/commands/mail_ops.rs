//! 收码站编排：把 sub2api 账号列表里的邮箱一键导入收码站。
//!
//! 背景：非 CDK 授权路径里，OpenAI 会把登录验证码发到账号邮箱，
//! 只有把邮箱绑定到收码站才能读到验证码。收码站按邮箱**匹配它的总库**，
//! 命中即回填真实凭证；已绑定过的会返回 `already_bound`，天然去重。

use anyhow::Result;

use crate::client::Sub2ApiClient;
use crate::mail::{BindReport, MailClient, MailConfig};
use crate::models::Account;

/// 从账号列表里收集可当收码邮箱用的地址。
///
/// 取 `credentials.email` 优先，回落到 `name`（sub2api 的 OpenAI 账号 name 就是邮箱）。
/// `only_openai=true` 时只收 openai 平台的账号。
pub fn collect_emails(accounts: &[Account], only_openai: bool) -> Vec<String> {
    let mut out = Vec::new();
    for a in accounts {
        if only_openai && !a.platform.eq_ignore_ascii_case("openai") {
            continue;
        }
        let email = a
            .credentials
            .as_ref()
            .and_then(|c| c.get("email"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| a.name.clone());
        let e = email.trim();
        if e.contains('@') {
            out.push(e.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 一键导入：拉账号列表 → 提取邮箱 → 绑定到收码站。
///
/// `only_openai` 默认 true（只导 OpenAI 账号的邮箱）。
pub fn import_emails(
    client: &mut Sub2ApiClient,
    mail_cfg: &MailConfig,
    only_openai: bool,
    log: &dyn Fn(String),
) -> Result<BindReport> {
    if !mail_cfg.ready() {
        anyhow::bail!("未配置收码站（请在左侧「收码站」里填地址、用户名、密码并保存）");
    }
    log("读取 sub2api 账号列表…".to_string());
    let accounts = client.list_accounts()?;
    let emails = collect_emails(&accounts, only_openai);
    if emails.is_empty() {
        anyhow::bail!("账号列表里没有可导入的邮箱");
    }
    log(format!(
        "共 {} 个账号，提取出 {} 个邮箱，开始绑定到收码站…",
        accounts.len(),
        emails.len()
    ));
    let mc = MailClient::login(mail_cfg)?;
    let report = mc.bind_emails(&emails)?;
    log(format!(
        "绑定完成：新增 {} · 已存在 {} · 总库未命中 {}（共 {}）",
        report.bound, report.already_bound, report.not_found, report.total
    ));
    if !report.missing.is_empty() {
        let head: Vec<&str> = report.missing.iter().take(8).map(|s| s.as_str()).collect();
        log(format!(
            "以下邮箱不在收码站总库里，需等总库更新：{}{}",
            head.join("、"),
            if report.missing.len() > head.len() {
                " …"
            } else {
                ""
            }
        ));
    }
    Ok(report)
}
