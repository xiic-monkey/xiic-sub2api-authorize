//! 收信侧实测探针：验证 `MailClient` 能真的从收码站取到验证码。
//!
//! 与 `stealth_probe` 配合使用（那个负责「发信」——把邮箱提交给 OpenAI）：
//!
//! ```text
//! # 1) 发信：打开授权链接 + 提交邮箱，让 OpenAI 把码发到该邮箱
//! cargo run --example stealth_probe -- "<auth_url>" --email=xxx@outlook.com --seconds=90
//!
//! # 2) 收信：本探针从收码站把码取回来
//! cargo run --example mail_wait_probe -- xxx@outlook.com --seconds=180
//! ```
//!
//! 走的是库里的正式实现（`MailClient::snapshot` / `check` / `wait_code`），
//! 不是另写一套，所以它通过 = 产品代码通过。

use anyhow::Result;
use std::time::Duration;
use sub2api_operator::mail::{MailClient, MailConfig};

struct Args {
    email: String,
    base: String,
    user: String,
    pass: String,
    seconds: u64,
    /// 忽略水位线：直接对邮箱里**现有**记录取码（诊断用，别用在正式流程）
    any: bool,
}

fn parse_args() -> Option<Args> {
    let mut a = Args {
        email: String::new(),
        base: "https://mail.kyon888.xyz".into(),
        user: String::new(),
        pass: String::new(),
        seconds: 180,
        any: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        if let Some(v) = x.strip_prefix("--base=") {
            a.base = v.to_string();
        } else if let Some(v) = x.strip_prefix("--user=") {
            a.user = v.to_string();
        } else if let Some(v) = x.strip_prefix("--pass=") {
            a.pass = v.to_string();
        } else if let Some(v) = x.strip_prefix("--seconds=") {
            a.seconds = v.parse().unwrap_or(180);
        } else if x == "--any" {
            a.any = true;
        } else if !x.starts_with("--") && a.email.is_empty() {
            a.email = x;
        }
    }
    // 缺省账号从环境变量兜底，避免写进代码
    if a.user.is_empty() {
        a.user = std::env::var("MAIL_USER").unwrap_or_default();
    }
    if a.pass.is_empty() {
        a.pass = std::env::var("MAIL_PASS").unwrap_or_default();
    }
    (!a.email.is_empty() && !a.user.is_empty() && !a.pass.is_empty()).then_some(a)
}

fn main() -> Result<()> {
    let Some(a) = parse_args() else {
        eprintln!(
            "用法: MAIL_USER=... MAIL_PASS=... cargo run --example mail_wait_probe -- <email> \
             [--base=https://mail.kyon888.xyz] [--seconds=180]"
        );
        std::process::exit(2);
    };

    let cfg = MailConfig {
        base_url: a.base.clone(),
        username: a.user.clone(),
        password: a.pass.clone(),
    };
    println!("[1] 登录收码站 {}", a.base);
    let client = MailClient::login(&cfg)?;
    println!("    登录成功");

    println!("[2] 记录水位线（只认之后到达的新邮件）");
    let base = client.snapshot(&a.email)?;
    match &base {
        Some(b) => println!(
            "    mailbox_id={} 历史邮件 {} 封 max_record_id={} max_time={}",
            b.email_id, b.existing, b.max_record_id, b.max_received_epoch
        ),
        None => {
            println!(
                "    ❌ 收码站里没有 {} —— 先去账号列表点「导入邮箱」",
                a.email
            );
            std::process::exit(1);
        }
    }

    // 诊断模式：直接拿现有记录验证「取码」逻辑（不涉及新邮件）
    if a.any {
        let b = base.as_ref().unwrap();
        let recs = client.records(b.email_id)?;
        println!("[诊断] 邮箱现有 {} 封记录，逐封尝试取码：", recs.len());
        for r in &recs {
            let hit = sub2api_operator::mail::extract_code_from_text(&r.subject, &r.content);
            println!(
                "  - folder={:?} [{}] {} | {}  → 取码={:?}",
                r.folder, r.received_time, r.sender, r.subject, hit
            );
        }
        match sub2api_operator::mail::pick_code(recs.iter()) {
            Some((c, s)) => println!("[诊断] 整箱取码结果: {} （来自「{}」）", c, s),
            None => println!("[诊断] 整箱取不出码"),
        }
        return Ok(());
    }

    println!("[3] 等待验证码（内部会自动触发收信）");
    let code = client.wait_code(
        &a.email,
        base.as_ref(),
        Duration::from_secs(a.seconds),
        Duration::from_secs(4),
        None,
        &|m: String| println!("    [mail] {}", m),
    )?;

    match code {
        Some(c) => {
            println!("\n✅ 取到验证码: {}", c);
            // 二次确认：把记录里的候选也打出来，方便人工核对
            if let Some(b) = base {
                let recs = client.records(b.email_id)?;
                let fresh: Vec<_> = recs.iter().filter(|r| b.is_new(r)).collect();
                println!("新邮件 {} 封：", fresh.len());
                for r in fresh {
                    println!("  - [{}] {} | {}", r.received_time, r.sender, r.subject);
                }
            }
        }
        None => {
            println!("\n❌ 未取到验证码（超时或没有新邮件）");
            std::process::exit(1);
        }
    }
    Ok(())
}
