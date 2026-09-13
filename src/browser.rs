//! 原生浏览器自动化：纯 Rust CDP（chromiumoxide），**零 Node 依赖**。
//!
//! 直接以 headless 启动系统 Chrome / Chromium，走 DevTools 协议完成：
//! - `open_and_shoot`：打开页面 + 截图 + 抽表单结构；
//! - `gate`：门页状态检测（未进入 / 已记住会话），可填 CDK 进入；
//! - `fetch` / `fetch_stream`：接码全流程——确保进入 → 填邮箱 → 点获取 →
//!   每 5s 轮询 → 点「复制全部」读剪切板 → CPA 页转成 sub2api 凭证。
//!
//! 引擎定位：`SUB2OP_BROWSER` 环境变量 > `engine` 参数（"chrome"=本机 Chrome）>
//! 依次探测 chromium / google-chrome / edge 等常见安装位置。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chromiumoxide::browser::Browser;
use chromiumoxide::BrowserConfig;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

/// fetch 过程的进度回调：`log` 收人类可读日志，`step` 收流程阶段（step 名与前端时间线对齐）。
/// Arc 持有、Send + Sync，方便调用方放进线程池。
#[derive(Clone)]
pub struct FetchHooks {
    pub log: Arc<dyn Fn(String) + Send + Sync>,
    pub step: Arc<dyn Fn(&'static str, String) + Send + Sync>,
}

// ---------------------------------------------------------------------------
// 浏览器可执行文件定位
// ---------------------------------------------------------------------------

fn which(name: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("which").arg(name).output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    None
}

/// 按引擎偏好给出候选浏览器路径（从优到劣）。
fn browser_candidates(engine: Option<&str>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    // 显式指定优先
    if let Ok(p) = std::env::var("SUB2OP_BROWSER") {
        let p = p.trim();
        if !p.is_empty() {
            v.push(PathBuf::from(p));
        }
    }

    let e = engine.unwrap_or("").to_lowercase();
    let chrome = |v: &mut Vec<PathBuf>| {
        if let Some(p) = which("google-chrome") {
            v.push(p);
        }
        if let Some(p) = which("google-chrome-stable") {
            v.push(p);
        }
        v.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
        v.push(PathBuf::from("/usr/bin/google-chrome"));
    };
    let chromium = |v: &mut Vec<PathBuf>| {
        for name in ["chromium", "chromium-browser"] {
            if let Some(p) = which(name) {
                v.push(p);
            }
        }
        v.push(PathBuf::from("/usr/bin/chromium"));
        v.push(PathBuf::from("/usr/bin/chromium-browser"));
        v.push(PathBuf::from("/snap/bin/chromium"));
        v.push(PathBuf::from(
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ));
    };
    let edge = |v: &mut Vec<PathBuf>| {
        if let Some(p) = which("microsoft-edge") {
            v.push(p);
        }
        v.push(PathBuf::from(
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ));
    };

    match e.as_str() {
        "chrome" => chrome(&mut v),
        "chromium" | "" => {
            chromium(&mut v);
            chrome(&mut v);
        }
        "msedge" | "edge" => edge(&mut v),
        other => {
            if let Some(p) = which(other) {
                v.push(p);
            }
            chromium(&mut v);
            chrome(&mut v);
        }
    }
    v
}

/// 找到第一个真实存在的浏览器可执行文件。
pub fn detect_executable(engine: Option<&str>) -> Result<PathBuf> {
    let cands = browser_candidates(engine);
    if let Some(p) = cands.iter().find(|p| p.exists()) {
        return Ok(p.clone());
    }
    bail!(
        "找不到可用的浏览器引擎（engine={:?}）。已尝试：{}。\
         可设置环境变量 SUB2OP_BROWSER=<浏览器可执行文件路径> 指定。",
        engine.unwrap_or(""),
        cands.iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("、")
    );
}

/// 引擎可用性检测（桌面端「检测」按钮 / 服务器自检用）。
pub fn check_engines() -> Value {
    let mut out = serde_json::Map::new();
    for (name, engine) in [("chrome", Some("chrome")), ("chromium", Some("chromium"))] {
        match detect_executable(engine) {
            Ok(p) => {
                out.insert(name.to_string(), Value::Bool(true));
                out.insert(format!("{}Path", name), json!(p.display().to_string()));
                out.insert(format!("{}Error", name), Value::Null);
            }
            Err(e) => {
                out.insert(name.to_string(), Value::Bool(false));
                out.insert(format!("{}Path", name), Value::Null);
                out.insert(format!("{}Error", name), json!(e.to_string()));
            }
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// CDP 基础设施
// ---------------------------------------------------------------------------

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("创建 tokio runtime 失败")
}

/// 启动浏览器，返回 (Browser, 本次专用的 user-data-dir)。
///
/// **每次运行用独立临时目录**：Chrome 对 user-data-dir 有单例锁（SingletonLock），
/// 共用固定目录时，上一次没退干净的残留进程会让下一次启动直接自杀
/// （"File exists ... ProcessSingleton ... Aborting"）。独立目录 + 用完即删根治。
/// 异常路径可能残留目录，但位于系统临时目录下、目录名唯一，无害且会被系统定期清理。
async fn launch(engine: Option<&str>) -> Result<(Browser, PathBuf)> {
    let exe = detect_executable(engine)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let user_data_dir = std::env::temp_dir().join(format!("sub2op-chrome-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&user_data_dir)
        .with_context(|| format!("创建临时 profile 目录失败：{}", user_data_dir.display()))?;
    let config = BrowserConfig::builder()
        .chrome_executable(exe.clone())
        .user_data_dir(&user_data_dir)
        .arg("--no-sandbox")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-gpu")
        .window_size(1366, 900)
        .build()
        .map_err(|e| anyhow!("浏览器配置失败：{}", e))?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .with_context(|| format!("启动浏览器失败：{}", exe.display()))?;
    // handler 负责泵 CDP 事件，必须常驻驱动，否则页面操作会挂起
    tokio::task::spawn(async move {
        use futures::StreamExt;
        while let Some(_event) = handler.next().await {
            // 事件仅驱动内部状态，无需处理
        }
    });
    Ok((browser, user_data_dir))
}

/// 浏览器会话结束后的收尾：确保浏览器已关闭并删除本次的临时 profile 目录（best-effort）。
fn cleanup_browser(browser: Browser, dir: &Path) {
    drop(browser);
    // Chrome 退出是异步的，稍等一下再删，删不掉就留给系统清理
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = std::fs::remove_dir_all(dir);
}

async fn goto(page: &chromiumoxide::Page, url: &str, errors: &mut Vec<String>) {
    match tokio::time::timeout(Duration::from_secs(30), page.goto(url)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => errors.push(format!("goto: {}", e)),
        Err(_) => errors.push(format!("goto: 超时（30s）：{}", url)),
    }
}

async fn eval_value(page: &chromiumoxide::Page, expr: &str) -> Result<Value> {
    let res = page
        .evaluate_expression(expr)
        .await
        .map_err(|e| anyhow!("执行页面脚本失败：{}", e))?;
    Ok(res.into_value::<Value>().unwrap_or(Value::Null))
}

async fn eval_bool(page: &chromiumoxide::Page, expr: &str) -> bool {
    matches!(eval_value(page, expr).await, Ok(Value::Bool(true)))
}

async fn eval_string(page: &chromiumoxide::Page, expr: &str) -> String {
    match eval_value(page, expr).await {
        Ok(Value::String(s)) => s,
        _ => String::new(),
    }
}

fn js_visible(sel: &str) -> String {
    format!(
        "(() => {{ const el = document.querySelector({s}); if (!el) return false; \
         if (el.hasAttribute('hidden')) return false; \
         const r = el.getBoundingClientRect(); \
         const style = getComputedStyle(el); \
         return !!(r.width || r.height) && style.display !== 'none' && style.visibility !== 'hidden'; }})()",
        s = json!(sel)
    )
}

async fn visible(page: &chromiumoxide::Page, sel: &str) -> bool {
    eval_bool(page, &js_visible(sel)).await
}

async fn js_fill(page: &chromiumoxide::Page, sel: &str, value: &str) -> bool {
    let expr = format!(
        "(() => {{ const el = document.querySelector({s}); if (!el) return false; \
         el.value = {v}; \
         el.dispatchEvent(new Event('input', {{ bubbles: true }})); \
         el.dispatchEvent(new Event('change', {{ bubbles: true }})); \
         return true; }})()",
        s = json!(sel),
        v = json!(value)
    );
    eval_bool(page, &expr).await
}

async fn js_click(page: &chromiumoxide::Page, sel: &str) -> bool {
    let expr = format!(
        "(() => {{ const el = document.querySelector({s}); if (!el) return false; el.click(); return true; }})()",
        s = json!(sel)
    );
    eval_bool(page, &expr).await
}

async fn js_click_sub2api_channel(page: &chromiumoxide::Page) -> bool {
    let expr = "(() => { const b = [...document.querySelectorAll('button')] \
                .find(x => /sub2api/i.test(x.textContent || '')); \
                if (!b) return false; b.click(); return true; })()";
    eval_bool(page, expr).await
}

async fn js_text(page: &chromiumoxide::Page, sel: &str) -> String {
    let expr = format!(
        "(() => {{ const el = document.querySelector({s}); return el ? (el.textContent || '') : ''; }})()",
        s = json!(sel)
    );
    eval_string(page, &expr).await
}

async fn js_input_value(page: &chromiumoxide::Page, sel: &str) -> String {
    let expr = format!(
        "(() => {{ const el = document.querySelector({s}); return el ? (el.value || '') : ''; }})()",
        s = json!(sel)
    );
    eval_string(page, &expr).await
}

async fn js_enabled(page: &chromiumoxide::Page, sel: &str) -> bool {
    let expr = format!(
        "(() => {{ const el = document.querySelector({s}); return !!el && !el.disabled; }})()",
        s = json!(sel)
    );
    eval_bool(page, &expr).await
}

const JS_MARKERS: &str = "(() => { const ids = ['gate','gateCdk','gateEnter','mergeBtn','mergeBanner','stat','jobs','work','go','left','newCdk','newLeft']; \
                          const o = {}; for (const id of ids) { const el = document.getElementById(id); \
                          let v = false; if (el) { const r = el.getBoundingClientRect(); \
                          v = !!(r.width || r.height) && getComputedStyle(el).visibility !== 'hidden'; } o[id] = v; } return o; })()";

const JS_FIELDS: &str = "(() => { const els = [...document.querySelectorAll('input,button,select,textarea,a[href]')]; \
                         return els.map(e => ({ tag: e.tagName, type: e.getAttribute('type'), name: e.getAttribute('name'), \
                         id: e.id, placeholder: e.getAttribute('placeholder'), text: (e.textContent || '').trim().slice(0, 48) })) \
                         .filter(f => f.text || f.placeholder || f.type || f.tag === 'INPUT' || f.tag === 'BUTTON') \
                         .slice(0, 80); })()";

/// 探测「进入后」状态：正向标记（stat/work/go/left）任一可见，或门控件已隐藏。
async fn is_entered(page: &chromiumoxide::Page) -> Result<(bool, Value)> {
    let m = eval_value(page, JS_MARKERS).await?;
    let truthy = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
    let marked = truthy("stat") || truthy("work") || truthy("go") || truthy("left");
    let gate = visible(page, "#gateCdk").await && visible(page, "#gateEnter").await;
    Ok((marked || !gate, m))
}

async fn screenshot(page: &chromiumoxide::Page, path: &Path) -> Result<()> {
    let params = chromiumoxide::page::ScreenshotParams::builder()
        .full_page(true)
        .build();
    let bytes = page
        .screenshot(params)
        .await
        .with_context(|| format!("截图失败：{}", path.display()))?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

async fn grant_clipboard(page: &chromiumoxide::Page) {
    use chromiumoxide::cdp::browser_protocol::browser::{GrantPermissionsParams, PermissionType};
    let Ok(params) = GrantPermissionsParams::builder()
        .permissions(vec![
            PermissionType::ClipboardReadWrite,
            PermissionType::ClipboardSanitizedWrite,
        ])
        .build()
    else {
        return;
    };
    let _ = page.execute(params).await;
}

/// 读带回 Promise 的表达式（clipboard.readText 这类异步 API 用）。
async fn eval_promise_string(page: &chromiumoxide::Page, expr: &str) -> String {
    use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
    let Ok(params) = EvaluateParams::builder()
        .expression(expr)
        .await_promise(true)
        .return_by_value(true)
        .build()
    else {
        return String::new();
    };
    if let Ok(r) = page.execute(params).await {
        if let Some(Value::String(s)) = r.result.result.value {
            return s;
        }
    }
    String::new()
}

#[cfg(target_os = "macos")]
fn system_clipboard() -> String {
    std::process::Command::new("pbpaste")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn system_clipboard() -> String {
    for cmd in [
        vec!["xclip", "-selection", "clipboard", "-o"],
        vec!["xsel", "-bo"],
    ] {
        if let Ok(o) = std::process::Command::new(cmd[0]).args(&cmd[1..]).output() {
            if o.status.success() {
                return String::from_utf8_lossy(&o.stdout).to_string();
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// 门页进入（gate / fetch 共用）
// ---------------------------------------------------------------------------

async fn ensure_entered(
    page: &chromiumoxide::Page,
    cdk: Option<&str>,
    errors: &mut Vec<String>,
    log: &dyn Fn(String),
) -> Result<(bool, bool)> {
    let gate_visible = visible(page, "#gateCdk").await && visible(page, "#gateEnter").await;
    let (pre_entered, _) = is_entered(page).await?;
    let at_gate = gate_visible && !pre_entered;

    if at_gate {
        match cdk.filter(|c| !c.trim().is_empty()) {
            Some(c) => {
                log("检测到门页：填入 CDK 并点击进入".to_string());
                if !js_fill(page, "#gateCdk", c).await {
                    errors.push("fill-gate: 填入 #gateCdk 失败".to_string());
                }
                if !js_click(page, "#gateEnter").await {
                    errors.push("click-gate: 点击 #gateEnter 失败".to_string());
                }
                sleep(Duration::from_millis(3500)).await;
            }
            None => errors.push("no-cdk: 在门页但未提供 CDK，无法进入".to_string()),
        }
    } else if pre_entered {
        log("页面记住了会话，已是进入后状态，跳过门页".to_string());
    }

    let (post_entered, _) = is_entered(page).await?;
    Ok((at_gate, post_entered))
}

// ---------------------------------------------------------------------------
// open：打开 + 截图 + 抽结构
// ---------------------------------------------------------------------------

async fn open_native(url: &str, out: &Path, engine: Option<&str>) -> Result<()> {
    let (browser, profile_dir) = launch(engine).await?;
    let res: Result<()> = async {
    let page = browser.new_page("about:blank").await?;
    let mut errors = Vec::new();
    goto(&page, url, &mut errors).await;
    sleep(Duration::from_millis(2500)).await;

    screenshot(&page, out).await?;
    let content = page.content().await.unwrap_or_default();
    let fields = eval_value(&page, JS_FIELDS).await.unwrap_or(Value::Null);
    let title = eval_string(&page, "document.title").await;

    let result = json!({
        "ok": true,
        "url": eval_string(&page, "location.href").await,
        "title": title,
        "screenshot": out.display().to_string(),
        "htmlBytes": content.len(),
        "formFields": fields,
        "errors": errors,
    });
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
    }
    .await;
    cleanup_browser(browser, &profile_dir);
    res
}

pub fn open_and_shoot(url: &str, out: &Path, engine: Option<&str>) -> Result<()> {
    rt().block_on(open_native(url, out, engine))
}

// ---------------------------------------------------------------------------
// gate：门页状态检测（± CDK 进入）
// ---------------------------------------------------------------------------

async fn gate_native(
    url: &str,
    cdk: Option<&str>,
    out_dir: &Path,
    engine: Option<&str>,
    log: &dyn Fn(String),
) -> Result<Value> {
    std::fs::create_dir_all(out_dir).ok();
    log(format!("打开门页：{}", url));
    let (browser, profile_dir) = launch(engine).await?;
    let res: Result<Value> = async {
    let page = browser.new_page("about:blank").await?;
    let mut errors = Vec::new();
    goto(&page, url, &mut errors).await;
    sleep(Duration::from_millis(2500)).await;

    let gate_visible = visible(&page, "#gateCdk").await && visible(&page, "#gateEnter").await;
    let pre_shot = out_dir.join("gate-pre.png");
    screenshot(&page, &pre_shot).await?;
    let pre_fields = eval_value(&page, JS_FIELDS).await.unwrap_or(Value::Null);
    let pre_html = page.content().await.unwrap_or_default();
    let pre_html_path = out_dir.join("gate-pre.html");
    let _ = std::fs::write(&pre_html_path, &pre_html);
    let pre_title = eval_string(&page, "document.title").await;
    let pre_markers = eval_value(&page, JS_MARKERS).await.unwrap_or(Value::Null);
    let truthy = |v: &Value, k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let pre_entered = truthy(&pre_markers, "stat")
        || truthy(&pre_markers, "work")
        || truthy(&pre_markers, "go")
        || truthy(&pre_markers, "left");
    let has_gate = gate_visible && !pre_entered;

    log(if has_gate {
        "当前未进入（在门页）".to_string()
    } else {
        "当前已进入（页面记住了会话）".to_string()
    });

    let mut entered_with_cdk = false;
    if has_gate {
        match cdk.filter(|c| !c.trim().is_empty()) {
            Some(c) => {
                log("填入 CDK 并点击进入".to_string());
                if !js_fill(&page, "#gateCdk", c).await {
                    errors.push("fill: 填入 #gateCdk 失败".to_string());
                }
                if !js_click(&page, "#gateEnter").await {
                    errors.push("click: 点击 #gateEnter 失败".to_string());
                }
                entered_with_cdk = true;
                sleep(Duration::from_millis(3500)).await;
            }
            None => errors.push("no-cdk: 未进入且未提供 CDK，跳过填表/点击".to_string()),
        }
    }

    let post_gate_visible =
        visible(&page, "#gateCdk").await && visible(&page, "#gateEnter").await;
    let post_markers = eval_value(&page, JS_MARKERS).await.unwrap_or(Value::Null);
    let post_entered = truthy(&post_markers, "stat")
        || truthy(&post_markers, "work")
        || truthy(&post_markers, "go")
        || truthy(&post_markers, "left");
    let already_entered = post_entered || !post_gate_visible;

    let post_shot = out_dir.join("gate-post.png");
    screenshot(&page, &post_shot).await?;
    let post_fields = eval_value(&page, JS_FIELDS).await.unwrap_or(Value::Null);
    let post_html = page.content().await.unwrap_or_default();
    let post_html_path = out_dir.join("gate-post.html");
    let _ = std::fs::write(&post_html_path, &post_html);
    let post_title = eval_string(&page, "document.title").await;

    Ok(json!({
        "ok": true,
        "url": eval_string(&page, "location.href").await,
        "detection": {
            "preEntry": has_gate,
            "postEntry": already_entered,
            "enteredWithCdk": entered_with_cdk,
        },
        "pre": {
            "title": pre_title,
            "screenshot": pre_shot.display().to_string(),
            "htmlFile": pre_html_path.display().to_string(),
            "htmlBytes": pre_html.len(),
            "fields": pre_fields,
            "markers": pre_markers,
        },
        "post": {
            "title": post_title,
            "screenshot": post_shot.display().to_string(),
            "htmlFile": post_html_path.display().to_string(),
            "htmlBytes": post_html.len(),
            "fields": post_fields,
            "markers": post_markers,
        },
        "errors": errors,
    }))
    }
    .await;
    cleanup_browser(browser, &profile_dir);
    res
}

pub fn gate(url: &str, cdk: Option<&str>, out_dir: &Path, engine: Option<&str>) -> Result<()> {
    let val = rt().block_on(gate_native(url, cdk, out_dir, engine, &|l| {
        eprintln!("{}", l)
    }))?;
    println!("{}", serde_json::to_string_pretty(&val)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// fetch：接码全流程（进入 → 邮箱 → 轮询 → 复制 → CPA 转换）
// ---------------------------------------------------------------------------

async fn fetch_native(
    url: &str,
    emails: &[String],
    cdk: Option<&str>,
    max_ms: u64,
    then_open: &str,
    engine: Option<&str>,
    hooks: &FetchHooks,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    let log = |l: String| (hooks.log)(l);
    if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
        bail!("已取消");
    }
    if emails.is_empty() {
        bail!("邮箱列表为空，无法执行获取流程");
    }

    (hooks.step)("open", format!("打开门页：{}", url));
    log("启动无头浏览器…".to_string());
    let (browser, profile_dir) = launch(engine).await?;
    let res: Result<Value> = async {
    let page = browser.new_page("about:blank").await?;
    let mut errors = Vec::new();
    goto(&page, url, &mut errors).await;
    sleep(Duration::from_millis(2500)).await;

    // 1) 确保已进入
    let (pre_entry, post_entry) =
        ensure_entered(&page, cdk, &mut errors, &|l| (hooks.log)(l)).await?;
    if !post_entry {
        bail!("未能进入页面（CDK 无效或页面结构变化）");
    }
    let entered = json!({ "preEntry": pre_entry, "postEntry": post_entry });

    // 2) 填邮箱
    (hooks.step)("emails", format!("填入 {} 个邮箱", emails.len()));
    log(format!("填入 {} 个邮箱", emails.len()));
    if !js_fill(&page, "#emails", &emails.join("\n")).await
    {
        errors.push("fill-emails: 填入 #emails 失败".to_string());
    }

    // 3) 点「获取令牌」
    (hooks.step)("go", "已点击「获取令牌」，开始轮询（每 5 秒）".to_string());
    log("已点击「获取令牌」，开始轮询（每 5 秒）".to_string());
    if !js_click(&page, "#go").await {
        errors.push("click-go: 点击 #go 失败".to_string());
    }
    let started = std::time::Instant::now();

    // 4) 轮询：完成信号 = #stat 出完成字样，或 #dlAll/#copyAll 同时可用，或 #stat 连续 3 次不变
    let mut poll_log: Vec<Value> = Vec::new();
    let mut last_stat = String::new();
    let mut stable = 0usize;
    let mut done = false;
    let mut final_state = Value::Null;

    while started.elapsed().as_millis() < max_ms as u128 {
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            bail!("已取消");
        }
        sleep(Duration::from_millis(5000)).await;
        let stat = js_text(&page, "#stat").await.trim().to_string();
        let err_tx = js_text(&page, "#err").await.trim().to_string();
        let emails_val = js_input_value(&page, "#emails").await;
        let dl = js_enabled(&page, "#dlAll").await;
        let cp = js_enabled(&page, "#copyAll").await;
        let copy_visible = visible(&page, "#copyAll").await;
        let elapsed = started.elapsed().as_secs();

        // 完成判定：必须同时满足
        // 1) #stat 文案里「共 N 个」的 N > 0（批次确实在跑）
        // 2) #stat 文案里「进行中 Z」的 Z == 0（没有还在跑的子任务）
        // 3) #copyAll 按钮可见（去掉 hidden，而不是 disabled；一开始按钮 hidden，完成后才显示）
        // 注意：stat 文案固定为「共 N 个 · 成功 X · 失败 Y · 进行中 Z」，「成功」二字永远存在——
        // 绝不能用关键词 contains 判定（第一次轮询「进行中 1」也会命中「成功」，之前就栽在这）。
        // 也不能只看 enabled：按钮由 hidden 控制，初始就 enabled。
        let num_after = |key: &str| -> Option<u64> {
            let idx = stat.find(key)?;
            let rest = stat[idx + key.len()..].trim_start();
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        };
        let total = num_after("共");
        let ok_n = num_after("成功");
        let err_n = num_after("失败");
        let live = num_after("进行中");

        let snap = json!({
            "t": format!("{}s", elapsed),
            "stat": stat.chars().take(200).collect::<String>(),
            "err": err_tx.chars().take(200).collect::<String>(),
            "emailsLen": emails_val.chars().count(),
            "total": total,
            "ok": ok_n,
            "fail": err_n,
            "live": live,
            "dlAll": dl,
            "copyAll": cp,
            "copyAllVisible": copy_visible,
        });
        poll_log.push(snap.clone());
        log(format!(
            "[poll {}s] stat={} copyAllVisible={} live={:?}",
            elapsed,
            stat.chars().take(120).collect::<String>(),
            copy_visible,
            live
        ));

        if total.unwrap_or(0) > 0 && live == Some(0) && copy_visible {
            done = true;
            final_state = snap;
            log(format!(
                "批次结束：成功 {} / 失败 {} / 共 {}",
                ok_n.unwrap_or(0),
                err_n.unwrap_or(0),
                total.unwrap_or(0)
            ));
            break;
        }
        if !stat.is_empty() && stat == last_stat {
            stable += 1;
        } else {
            stable = 0;
        }
        last_stat = stat;
        // 兜底：stat 连续 3 次不变，且批次确实在跑（total>0）且「复制全部」按钮已显示。
        // 这里不再要求 live 必须解析不到，避免模板没变、主条件已满足时由于抖动错过。
        if stable >= 3 && total.unwrap_or(0) > 0 && copy_visible {
            done = true;
            final_state = snap;
            log("#stat 连续 3 次不变且复制全部按钮已显示，视为完成".to_string());
            break;
        }
    }
    if !done {
        final_state = json!({
            "note": "超时未检测到完成信号",
            "elapsed": format!("{}s", started.elapsed().as_secs()),
        });
    }
    (hooks.step)("poll-done", if done { "检测到完成信号" } else { "超时结束" }.to_string());
    log(if done {
        "检测到完成信号".to_string()
    } else {
        "超时结束".to_string()
    });

    // 5) 点「复制全部」→ 读剪切板
    (hooks.step)("copy", "点击「复制全部」并读取剪切板".to_string());
    log("点击「复制全部」并读取剪切板".to_string());
    grant_clipboard(&page).await;
    let copy_clicked = js_click(&page, "#copyAll").await;
    sleep(Duration::from_millis(800)).await;
    let mut clipboard = eval_promise_string(&page, "navigator.clipboard.readText()").await;
    if clipboard.trim().is_empty() {
        clipboard = system_clipboard();
    }
    if clipboard.trim().is_empty() {
        clipboard = js_input_value(&page, "#emails").await;
        if !clipboard.trim().is_empty() {
            errors.push("clipboard-empty: 回退 #emails 值".to_string());
        }
    }

    // 6) CPA 页转换：贴 #session-input → 读 #output
    (hooks.step)("cpa", "打开 CPA 页并转换（贴入 → 读取输出）".to_string());
    log("打开 CPA 页并转换（贴入 → 读取输出）".to_string());
    let mut cpa_page: Value = Value::Null;
    match browser.new_page("about:blank").await {
        Ok(p2) => {
            grant_clipboard(&p2).await;
            goto(&p2, then_open, &mut errors).await;
            sleep(Duration::from_millis(2500)).await;
            if !js_click_sub2api_channel(&p2).await {
                errors.push("cpa-channel: 未找到 sub2api 渠道按钮".to_string());
            }
            if clipboard.trim().is_empty() {
                errors.push("cpa: 剪切板为空，无法贴入 #session-input".to_string());
            } else if !js_fill(&p2, "#session-input", &clipboard).await
            {
                errors.push("cpa-fill: 填入 #session-input 失败".to_string());
            }
            let mut cpa_out = String::new();
            let t0 = std::time::Instant::now();
            while t0.elapsed().as_millis() < 15000 {
                sleep(Duration::from_millis(1500)).await;
                cpa_out = js_input_value(&p2, "#output").await;
                let low = cpa_out.to_lowercase();
                if low.contains("refresh_token") || low.contains("rt-") {
                    break;
                }
            }
            let _ = js_click(&p2, "#copy-output").await;
            cpa_page = json!({
                "url": eval_string(&p2, "location.href").await,
                "title": eval_string(&p2, "document.title").await,
                "output": cpa_out,
            });
            let _ = p2.close().await;
        }
        Err(e) => errors.push(format!("cpa: {}", e)),
    }

    Ok(json!({
        "ok": true,
        "url": eval_string(&page, "location.href").await,
        "entered": entered,
        "emailsCount": emails.len(),
        "done": done,
        "finalState": final_state,
        "copyClicked": copy_clicked,
        "clipboard": clipboard,
        "cpaPage": cpa_page,
        "pollLog": poll_log,
        "errors": errors,
    }))
    }
    .await;
    cleanup_browser(browser, &profile_dir);
    res
}

/// CLI 版 `fetch`：结果 JSON 打到 stdout。
pub fn fetch(
    url: &str,
    emails_file: &Path,
    cdk: Option<&str>,
    max_ms: u64,
    then_open: Option<&str>,
    engine: Option<&str>,
) -> Result<()> {
    if !emails_file.exists() {
        bail!("邮箱清单文件不存在：{}", emails_file.display());
    }
    let raw = std::fs::read_to_string(emails_file)?;
    let emails: Vec<String> = raw
        .split(|c| c == '\n' || c == '\r' || c == ',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if emails.is_empty() {
        bail!("邮箱清单为空：{}", emails_file.display());
    }
    let then = then_open
        .map(|s| s.to_string())
        .unwrap_or_else(|| "https://zh.kyon888.xyz/CPAandSub2API/".to_string());
    let val = rt().block_on(fetch_native(
        url,
        &emails,
        cdk,
        max_ms,
        &then,
        engine,
        &FetchHooks {
            log: Arc::new(|l| eprintln!("{}", l)),
            step: Arc::new(|_, _| {}),
        },
        None,
    ))?;
    println!("{}", serde_json::to_string_pretty(&val)?);
    Ok(())
}

/// 流式版 `fetch`：只有日志回调（web 控制台用）。
pub fn fetch_stream(
    url: &str,
    emails: &[String],
    cdk: Option<&str>,
    max_ms: u64,
    then_open: Option<&str>,
    engine: Option<&str>,
    on_log: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<Value> {
    let hooks = FetchHooks {
        log: on_log,
        step: Arc::new(|_, _| {}),
    };
    fetch_stream_hooks(url, emails, cdk, max_ms, then_open, engine, &hooks, None)
}

/// 全功能版 `fetch`：日志 + 阶段回调 + 取消标记（Tauri 桌面端用）。
/// 取消后下一次轮询前退出（最多 5 秒），浏览器随 Browser drop 一并关闭。
pub fn fetch_stream_hooks(
    url: &str,
    emails: &[String],
    cdk: Option<&str>,
    max_ms: u64,
    then_open: Option<&str>,
    engine: Option<&str>,
    hooks: &FetchHooks,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    let then = then_open
        .map(|s| s.to_string())
        .unwrap_or_else(|| "https://zh.kyon888.xyz/CPAandSub2API/".to_string());
    rt().block_on(fetch_native(
        url, emails, cdk, max_ms, &then, engine, hooks, cancel,
    ))
}
