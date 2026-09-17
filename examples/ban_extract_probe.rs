//! 回归探针：验证「封禁邮箱提取」不会把聚合容器整锅端。
//!
//! 背景（真实事故）：旧版 `JS_EXTRACT_BANNED` 用 `[class*="job"]` / `#work > *`
//! 这类宽泛选择器扫容器，而真实门页里 `#jobs` 的 class 字面量就是 `"jobs"`、
//! 并且被 `div.panel` 包着。于是**只要任意一条失败项的日志里出现封禁词**，
//! 这两个祖先容器的文本就整体命中 → 容器内**全部邮箱**（含成功的）被判封禁
//! → sub2api 里被删光。删除不可逆。
//!
//! 用法：
//! ```text
//! # 静态夹具（期望：只输出 bad5@example.com）
//! cargo run --example ban_extract_probe -- tests/fixtures/jobs_page_repro.html
//!
//! # 真实页面（需已进入结果区；只打印，不断言）
//! cargo run --example ban_extract_probe -- "https://401.kyon888.xyz/"
//! ```

use std::path::PathBuf;

use chromiumoxide::browser::{Browser, HeadlessMode};
use chromiumoxide::BrowserConfig;
use futures::StreamExt;
use serde_json::{json, Value};
use sub2api_operator::browser::JS_EXTRACT_BANNED;
use tokio::time::{sleep, Duration};

fn find_chrome() -> Option<PathBuf> {
    let cands = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
    ];
    cands
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| {
            std::process::Command::new("which")
                .arg("google-chrome")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
                .filter(|p| p.exists())
        })
}

async fn eval(page: &chromiumoxide::Page, expr: &str) -> Value {
    match page.evaluate_expression(expr).await {
        Ok(r) => r.value().cloned().unwrap_or(Value::Null),
        Err(e) => json!(format!("ERR {e}")),
    }
}

/// 夹具里唯一「应该」被判封禁的邮箱。
const EXPECTED: &[&str] = &["bad5@example.com"];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg.is_empty() {
        eprintln!(
            "用法: cargo run --example ban_extract_probe -- <本地html路径|url>\n\
             例:   cargo run --example ban_extract_probe -- tests/fixtures/jobs_page_repro.html"
        );
        std::process::exit(2);
    }

    let remote = arg.starts_with("http://") || arg.starts_with("https://");
    let url = if remote {
        arg.clone()
    } else {
        let p =
            std::fs::canonicalize(&arg).map_err(|e| anyhow::anyhow!("找不到文件 {arg}: {e}"))?;
        format!("file://{}", p.display())
    };

    let exe = find_chrome().ok_or_else(|| anyhow::anyhow!("找不到 Chrome"))?;
    let profile = std::env::temp_dir().join(format!(
        "sub2op-banprobe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&profile)?;

    let config = BrowserConfig::builder()
        .chrome_executable(exe)
        .user_data_dir(&profile)
        .disable_default_args()
        .headless_mode(HeadlessMode::New)
        .build()
        .map_err(|e| anyhow::anyhow!("config: {e}"))?;

    let (mut browser, mut handler) = Browser::launch(config).await?;
    tokio::task::spawn(async move { while let Some(_e) = handler.next().await {} });

    let page = browser.new_page("about:blank").await?;
    page.goto(&url).await?;
    sleep(Duration::from_millis(600)).await;

    println!("[url] {url}");
    let got = eval(&page, JS_EXTRACT_BANNED).await;
    println!("[JS_EXTRACT_BANNED] {}", serde_json::to_string(&got)?);

    // 顺带把页面里「含封禁词」的元素打出来，方便肉眼核对是不是又扫到聚合容器
    let diag = eval(
        &page,
        r#"(() => {
             const kws = ['封禁','停用','deactivated','banned','suspended','disabled'];
             const re = /[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}/g;
             const out = [];
             for (const el of document.querySelectorAll('*')) {
               const t = el.innerText || '';
               if (!kws.some(k => t.toLowerCase().includes(k.toLowerCase()))) continue;
               out.push((el.id ? '#'+el.id : el.tagName.toLowerCase()+(el.className?'.'+el.className:'')) +
                        ' → ' + ((t.match(re)||[]).length) + ' 个邮箱');
             }
             return out;
           })()"#,
    )
    .await;
    println!("[页面中含封禁词的元素] {}", serde_json::to_string(&diag)?);

    if remote {
        println!("[断言] 跳过（远程页面无期望值，仅打印）");
    } else {
        let list: Vec<String> = got
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut sorted = list.clone();
        sorted.sort();
        let mut exp: Vec<String> = EXPECTED.iter().map(|s| s.to_string()).collect();
        exp.sort();
        println!("[断言] 期望 {exp:?}");
        println!("[断言] 实际 {sorted:?}");
        if sorted == exp {
            println!("✅ 通过：聚合容器没有整锅端");
        } else {
            println!("❌ 失败：提取结果与期望不符 —— 有账号会被误删！");
            std::process::exit(1);
        }
    }

    let _ = browser.close().await;
    let _ = std::fs::remove_dir_all(&profile);
    Ok(())
}
