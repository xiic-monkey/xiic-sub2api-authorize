//! 无头浏览器封装（Playwright，与 xiic-crm 同构）。
//!
//! 本项目是 Rust 运维二进制，浏览器自动化交给同目录的 Node worker
//! （`browser-worker/worker.js`，基于 playwright-core），由它驱动浏览器。
//! 这样与 xiic-crm 技术栈一致，后期维护靠 playwright 生态。
//!
//! - 不需要虚拟桌面：worker 内部 `headless: true`。
//! - 引擎二选一（worker 的 `--browser`）：
//!   - 默认 / `chromium`：playwright 内置 Chromium（由 `playwright-core install chromium` 管理，服务器/Docker 用）；
//!   - `chrome`：直接唤醒本机已装的 Google Chrome（`channel: 'chrome'`），
//!     不下载 Chromium，适合本地单机版（Tauri 桌面端默认用它）。

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

/// worker 所在目录：优先 `SUB2OP_ROOT`（容器/桌面端由外部注入），否则当前工作目录。
/// 运维约定：从项目根目录运行本工具，worker 在 `browser-worker/worker.js`。
pub fn worker_dir() -> PathBuf {
    if let Ok(p) = std::env::var("SUB2OP_ROOT") {
        return PathBuf::from(p);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 定位 `browser-worker/worker.js`。Tauri 桌面端也用它，避免重复实现路径逻辑。
pub fn worker_script() -> Result<PathBuf> {
    let p = worker_dir().join("browser-worker/worker.js");
    if !p.exists() {
        anyhow::bail!(
            "找不到浏览器 worker：{}\n提示：设 SUB2OP_ROOT=<含 browser-worker/ 的目录>；\
             并在 browser-worker/ 下执行 npm install（playwright-core）。",
            p.display()
        );
    }
    Ok(p)
}

/// 追加浏览器引擎参数。`None` 时不下发，worker 用默认（内置 Chromium）。
fn append_engine(cmd: &mut Command, engine: Option<&str>) {
    if let Some(e) = engine {
        if !e.is_empty() {
            cmd.arg("--browser").arg(e);
        }
    }
}

fn spawn(mut cmd: Command) -> Result<()> {
    let status = cmd.status().with_context(|| {
        "启动 node worker 失败（确认 node 已安装、browser-worker 依赖已装：npm install）"
    })?;
    if !status.success() {
        anyhow::bail!("浏览器 worker 退出异常：{:?}", status.code());
    }
    Ok(())
}

/// 无头打开 URL、截图、打印页面结构（实际由 node worker 执行）。
pub fn open_and_shoot(url: &str, out: &Path, engine: Option<&str>) -> Result<()> {
    let worker = worker_script()?;
    let mut cmd = Command::new("node");
    cmd.arg(&worker).arg("open").arg(url).arg("--out").arg(out);
    append_engine(&mut cmd, engine);
    spawn(cmd)
}

/// 门页流程：检测 未进入/已进入 状态，若未进入且给了 CDK 则填 `#gateCdk` 并点 `#gateEnter`。
/// `cdk` 为 None 时仅做状态检测（不填表、不点击），用于核对门页结构。
pub fn gate(url: &str, cdk: Option<&str>, out_dir: &Path, engine: Option<&str>) -> Result<()> {
    let worker = worker_script()?;
    let mut cmd = Command::new("node");
    cmd.arg(&worker).arg("gate").arg(url).arg("--out-dir").arg(out_dir);
    if let Some(c) = cdk {
        cmd.arg("--cdk").arg(c);
    }
    append_engine(&mut cmd, engine);
    spawn(cmd)
}

/// 接码平台「获取令牌（重授权）→ 转 sub2api 凭证」完整链路：确保已进入 → 填邮箱(#emails) →
/// 点获取(#go) → 每 5s 轮询 → 点「复制全部」(#copyAll) 从剪切板读结果 →
/// 打开 CPA/Sub2API 页把结果贴进 #session-input → 读右边 #output 的 sub2api 凭证。
/// **不落盘**：结果只经 worker 的 stdout JSON 返回，由调用方（终端/后续命令）消费。
pub fn fetch(
    url: &str,
    emails_file: &Path,
    cdk: Option<&str>,
    max_ms: u64,
    then_open: Option<&str>,
    engine: Option<&str>,
) -> Result<()> {
    let worker = worker_script()?;
    if !emails_file.exists() {
        anyhow::bail!("邮箱清单文件不存在：{}", emails_file.display());
    }

    let mut cmd = Command::new("node");
    cmd.arg(&worker)
        .arg("fetch")
        .arg(url)
        .arg("--emails-file")
        .arg(emails_file)
        .arg("--max")
        .arg(max_ms.to_string());
    if let Some(c) = cdk {
        cmd.arg("--cdk").arg(c);
    }
    if let Some(t) = then_open {
        cmd.arg("--then-open").arg(t);
    }
    append_engine(&mut cmd, engine);
    spawn(cmd)
}

/// 与 `fetch` 相同的链路，但**捕获 worker 输出**而不是打到终端：
/// - 强制加 `--progress`，逐行读 NDJSON；进度通过 `on_log` 回调交给调用方（web 控制台日志）；
/// - 返回 `done` 事件里的 result（`clipboard` 与 `cpaPage.output`），供后续按邮箱写回直接消费。
///
/// 邮箱直接以 `--emails` 换行串下发，不写临时文件。
pub fn fetch_stream(
    url: &str,
    emails: &[String],
    cdk: Option<&str>,
    max_ms: u64,
    then_open: Option<&str>,
    engine: Option<&str>,
    on_log: &dyn Fn(String),
) -> Result<Value> {
    let worker = worker_script()?;
    if emails.is_empty() {
        anyhow::bail!("邮箱列表为空，无法执行获取流程");
    }

    let mut cmd = Command::new("node");
    cmd.arg(&worker)
        .arg("fetch")
        .arg(url)
        .arg("--emails")
        .arg(emails.join("\n"))
        .arg("--max")
        .arg(max_ms.to_string())
        .arg("--progress")
        .stdout(Stdio::piped());
    if let Some(c) = cdk {
        cmd.arg("--cdk").arg(c);
    }
    if let Some(t) = then_open {
        cmd.arg("--then-open").arg(t);
    }
    append_engine(&mut cmd, engine);

    let mut child = cmd.spawn().with_context(|| {
        "启动 node worker 失败（确认 node 已安装、browser-worker 依赖已装：npm install）"
    })?;
    let stdout = child
        .stdout
        .take()
        .context("无法读取 worker 的 stdout（应为 piped）")?;

    let mut result: Option<Value> = None;
    let mut err_msg: Option<String> = None;
    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(t) {
            Ok(ev) => match ev.get("event").and_then(|v| v.as_str()).unwrap_or("") {
                "done" => result = ev.get("result").cloned(),
                "error" => {
                    let m = ev
                        .get("msg")
                        .and_then(|v| v.as_str())
                        .unwrap_or("未知错误")
                        .to_string();
                    err_msg = Some(m.clone());
                    on_log(format!("❌ {}", m));
                }
                _ => {
                    let msg = ev.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                    if !msg.is_empty() {
                        let step = ev.get("step").and_then(|v| v.as_str()).unwrap_or("");
                        if step.is_empty() {
                            on_log(msg.to_string());
                        } else {
                            on_log(format!("[{}] {}", step, msg));
                        }
                    }
                }
            },
            Err(_) => on_log(t.to_string()),
        }
    }

    let status = child.wait().context("等待浏览器 worker 结束失败")?;
    if let Some(r) = result {
        return Ok(r);
    }
    if !status.success() {
        let tail = err_msg.map(|e| format!("：{}", e)).unwrap_or_default();
        anyhow::bail!(
            "浏览器 worker 退出异常（code={:?}){}",
            status.code(),
            tail
        );
    }
    Err(anyhow!("浏览器 worker 没有返回结果 JSON"))
}
