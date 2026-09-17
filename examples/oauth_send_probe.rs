//! 非 CDK 授权路径的**产品代码**端到端探针。
//!
//! 与 `stealth_probe` 的区别：那个是独立小工具（自带一套填表实现），
//! 本探针直接调库里的正式流程 [`sub2api_operator::browser::openai_oauth`]，
//! 所以它跑通 = 产品代码跑通（发信 + 收信 + 拿 code 全在里面）。
//!
//! ```text
//! MAIL_USER=... MAIL_PASS=... \
//! cargo run --example oauth_send_probe -- "<auth_url>" <email> [--headed] [--seconds=180]
//! ```

use anyhow::Result;
use std::sync::Arc;
use sub2api_operator::browser::{openai_oauth, BrowserMode, OAuthHooks};
use sub2api_operator::mail::MailConfig;

struct Args {
    auth_url: String,
    email: String,
    headed: bool,
    seconds: u64,
    mail: MailConfig,
}

fn parse_args() -> Option<Args> {
    let mut auth_url = String::new();
    let mut email = String::new();
    let mut headed = false;
    let mut seconds = 180u64;
    for a in std::env::args().skip(1) {
        if a == "--headed" {
            headed = true;
        } else if let Some(v) = a.strip_prefix("--seconds=") {
            seconds = v.parse().unwrap_or(180);
        } else if a.starts_with("--") {
            // ignore
        } else if auth_url.is_empty() {
            auth_url = a;
        } else if email.is_empty() {
            email = a;
        }
    }
    if auth_url.is_empty() || email.is_empty() {
        return None;
    }
    let mail = MailConfig {
        base_url: std::env::var("MAIL_BASE")
            .unwrap_or_else(|_| "https://mail.kyon888.xyz".to_string()),
        username: std::env::var("MAIL_USER").unwrap_or_default(),
        password: std::env::var("MAIL_PASS").unwrap_or_default(),
    };
    Some(Args {
        auth_url,
        email,
        headed,
        seconds,
        mail,
    })
}

fn main() -> Result<()> {
    let Some(a) = parse_args() else {
        eprintln!(
            "用法: MAIL_USER=... MAIL_PASS=... cargo run --example oauth_send_probe -- \
             \"<auth_url>\" <email> [--headed] [--seconds=180]"
        );
        std::process::exit(2);
    };

    let mode = if a.headed {
        BrowserMode::StealthHeaded
    } else {
        BrowserMode::Stealth
    };
    println!(
        "[run] mode={} email={} max={}s",
        if a.headed {
            "有头隐身"
        } else {
            "无头隐身"
        },
        a.email,
        a.seconds
    );

    let hooks = OAuthHooks {
        log: Arc::new(|m: String| println!("    [flow] {}", m)),
        step: Arc::new(|s: &'static str, m: String| println!("  >> [{}] {}", s, m)),
    };

    let outcome = openai_oauth(
        &a.auth_url,
        &a.email,
        &a.mail,
        mode,
        None,
        a.seconds,
        None,
        &hooks,
    )?;

    println!("\n===== 结果 =====");
    println!("status      : {}", outcome.status);
    println!("needs_human : {}", outcome.needs_human);
    println!("message     : {}", outcome.message);
    match &outcome.code {
        Some(c) => {
            let head: String = c.chars().take(30).collect();
            println!("OAuth code  : {}…（len={}）", head, c.len());
            println!("\n✅ 发信 + 收信 + 回调 全链路打通");
        }
        None => {
            println!("OAuth code  : 无");
            println!("\n❌ 未拿到 code（见上方日志定位卡在哪一步）");
        }
    }
    println!("\n--- 流程日志 ---");
    for l in &outcome.log {
        println!("  {}", l);
    }
    if outcome.code.is_none() {
        std::process::exit(1);
    }
    Ok(())
}
