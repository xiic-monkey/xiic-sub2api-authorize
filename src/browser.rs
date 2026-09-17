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
    let out = std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()?;
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
        cands
            .iter()
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

/// 浏览器运行模式。
///
/// 门页 / CPA 页没有反爬风控，用 [`BrowserMode::Headless`] 最快。
/// OpenAI（`auth.openai.com`）前面有 Cloudflare + Sentinel，必须用有机隐身模式：
/// - 丢掉 chromiumoxide 默认参数（其中带 `--enable-automation`，这是最容易被识别的标志）
/// - 指纹伪装（webdriver / plugins / screen / canvas / WebGL）
/// - UA 与 Client Hints 必须彼此一致，否则被判为伪造
/// - 先在同域做一次「有机预热」（带鼠标轨迹/滚动）再进目标页
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMode {
    /// 纯无头（追求速度，无反爬站点用）
    Headless,
    /// 有机隐身（无头）
    Stealth,
    /// 有机隐身 + 有头窗口（Cloudflare 升级成交互式质询时可人工点一下）
    StealthHeaded,
}

impl BrowserMode {
    fn headed(self) -> bool {
        matches!(self, BrowserMode::StealthHeaded)
    }
    fn stealth(self) -> bool {
        matches!(self, BrowserMode::Stealth | BrowserMode::StealthHeaded)
    }
    fn organic(self) -> bool {
        matches!(self, BrowserMode::Stealth | BrowserMode::StealthHeaded)
    }
}

/// 有机模式的启动参数。对齐 xiic-crm `headless_stealth_with_warmup` 并做了 macOS 适配。
const STEALTH_ARGS: &[&str] = &[
    "--disable-blink-features=AutomationControlled",
    "--disable-features=IsolateOrigins,VizDisplayCompositor,TranslateUI",
    "--disable-site-isolation-trials",
    "--disable-software-rasterizer",
    "--renderer-process-limit=1",
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-extensions",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--no-sandbox",
    "--password-store=basic",
    "--use-mock-keychain",
    "--window-size=1366,768",
    "--window-position=0,0",
];

/// 指纹伪装脚本（页面每次加载前注入）。
const IDENTITY_JS: &str = r#"(() => {
  const VW = 1366, VH = 768, LANG = 'en-US';
  const def = (t, p, g) => { try { Object.defineProperty(t, p, { configurable: true, get: g }); } catch (e) {} };
  def(Navigator.prototype, 'webdriver', () => undefined);
  window.chrome = window.chrome || { runtime: {} };
  def(Navigator.prototype, 'languages', () => [LANG, 'en']);
  def(Navigator.prototype, 'plugins', () => [
    { name: 'Chrome PDF Plugin', filename: 'internal-pdf-viewer', description: 'Portable Document Format' },
    { name: 'Chrome PDF Viewer', filename: 'mhjfbmdgcfjbbpaeojofohoefgiehjai', description: '' },
    { name: 'Native Client', filename: 'internal-nacl-plugin', description: '' },
  ]);
  try {
    const oq = navigator.permissions && navigator.permissions.query;
    if (oq) {
      navigator.permissions.query = (p) => (p && p.name === 'notifications')
        ? Promise.resolve({ state: Notification.permission, onchange: null, addEventListener(){}, removeEventListener(){}, dispatchEvent(){ return false; } })
        : oq.call(navigator.permissions, p);
    }
  } catch (e) {}
  def(window, 'outerWidth', () => VW);
  def(window, 'outerHeight', () => VH);
  def(screen, 'width', () => VW);
  def(screen, 'height', () => VH);
  def(screen, 'availWidth', () => VW);
  def(screen, 'availHeight', () => VH);
  def(screen, 'colorDepth', () => 24);
  def(screen, 'pixelDepth', () => 24);
  def(Navigator.prototype, 'hardwareConcurrency', () => 8);
  def(Navigator.prototype, 'deviceMemory', () => 8);
  def(Navigator.prototype, 'maxTouchPoints', () => 0);
  def(Navigator.prototype, 'connection', () => ({ effectiveType: '4g', rtt: 50, downlink: 10, saveData: false }));
  try {
    const otd = HTMLCanvasElement.prototype.toDataURL;
    HTMLCanvasElement.prototype.toDataURL = function () {
      const ctx = this.getContext('2d');
      if (ctx) {
        try {
          const d = ctx.getImageData(0, 0, this.width, this.height);
          if (d && d.data.length) {
            for (let i = 0; i < d.data.length; i += 4) d.data[i] += Math.floor(Math.random() * 2);
            ctx.putImageData(d, 0, 0);
          }
        } catch (e) {}
      }
      return otd.apply(this, arguments);
    };
  } catch (e) {}
  try {
    if (window.WebGLRenderingContext) {
      const og = WebGLRenderingContext.prototype.getParameter;
      WebGLRenderingContext.prototype.getParameter = function (p) {
        if (p === 37445) return 'Apple';
        if (p === 37446) return 'Apple M5';
        return og.call(this, p);
      };
    }
  } catch (e) {}
})();"#;

/// 启动浏览器，返回 (Browser, 本次专用的 user-data-dir)。
///
/// **每次运行用独立临时目录**：Chrome 对 user-data-dir 有单例锁（SingletonLock），
/// 共用固定目录时，上一次没退干净的残留进程会让下一次启动直接自杀
/// （"File exists ... ProcessSingleton ... Aborting"）。独立目录 + 用完即删根治。
/// 异常路径可能残留目录，但位于系统临时目录下、目录名唯一，无害且会被系统定期清理。
async fn launch_ex(engine: Option<&str>, mode: BrowserMode) -> Result<(Browser, PathBuf)> {
    let exe = detect_executable(engine)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let user_data_dir =
        std::env::temp_dir().join(format!("sub2op-chrome-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&user_data_dir)
        .with_context(|| format!("创建临时 profile 目录失败：{}", user_data_dir.display()))?;

    let mut builder = BrowserConfig::builder()
        .chrome_executable(exe.clone())
        .user_data_dir(&user_data_dir);
    if mode.stealth() {
        // 关键：丢弃 chromiumoxide 默认参数（含 --enable-automation）
        builder = builder.disable_default_args();
        for a in STEALTH_ARGS {
            builder = builder.arg(*a);
        }
        builder = builder.arg(format!("--user-agent={}", organic_user_agent()));
        if mode.headed() {
            builder = builder.with_head();
        } else {
            builder = builder.new_headless_mode();
        }
    } else {
        builder = builder
            .arg("--no-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-gpu")
            .window_size(1366, 900);
    }
    let config = builder
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

async fn launch(engine: Option<&str>) -> Result<(Browser, PathBuf)> {
    launch_ex(engine, BrowserMode::Headless).await
}

/// 本机 Chrome 的真实版本号（用于让 UA 与 Client Hints 一致）。
fn chrome_version(exe: &Path) -> Option<(String, String)> {
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let v = s.split_whitespace().last()?.trim().to_string();
    let full = v.clone();
    let major = v.split('.').next()?.to_string();
    Some((major, full))
}

fn organic_user_agent() -> String {
    let (major, full) = detect_executable(None)
        .ok()
        .and_then(|p| chrome_version(&p))
        .unwrap_or_else(|| ("152".to_string(), "152.0.0.0".to_string()));
    let _ = major;
    format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{} Safari/537.36",
        full.split('.').take(3).collect::<Vec<_>>().join(".")
    )
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

/// 有界的页面脚本求值。
///
/// **必须有界**：CDP 的 `Runtime.evaluate` 在页面正处于导航/质询/渲染进程被挂起时
/// 可能永远不回包。一旦无界 await 卡住，外层循环的 deadline 就再也检查不到，
/// 整个流程会永久挂死（实测卡过 23 小时）。超时一律按 `Null` 处理，让循环继续转。
async fn eval_value_bounded(page: &chromiumoxide::Page, expr: &str, secs: u64) -> Value {
    match tokio::time::timeout(Duration::from_secs(secs), eval_value(page, expr)).await {
        Ok(Ok(v)) => v,
        _ => Value::Null,
    }
}

/// 有界的页面脚本求值（字符串版）。
async fn eval_string_bounded(page: &chromiumoxide::Page, expr: &str, secs: u64) -> String {
    match eval_value_bounded(page, expr, secs).await {
        Value::String(s) => s,
        _ => String::new(),
    }
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

async fn extract_banned_emails(page: &chromiumoxide::Page) -> Vec<String> {
    match eval_value(page, JS_EXTRACT_BANNED).await {
        Ok(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_lowercase()))
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
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

/// 从 401 结果页提取「账号已被封禁/停用」的邮箱列表。
///
/// ## 为什么写得这么啰嗦（血泪史）
///
/// 旧版用宽泛选择器（`#work > *`、`[class*="job"]` …）扫容器，
/// **只要容器文本里有封禁词，就把容器内所有邮箱判为封禁**。
/// 真实门页的结构是：
///
/// ```html
/// <div id="work">
///   <div class="panel">          <!-- #work > * -->
///     <div id="jobs" class="jobs"> <!-- 也命中 [class*="job"]！ -->
///       <div class="card">… ok1@x.com</div>
///       <div class="card">… ok2@x.com</div>
///       <div class="card">… bad3@x.com  ← 只有这条的 log 含 "deactivated"</div>
/// ```
///
/// `#jobs` 的 `class="jobs"` 含子串 `job`，于是 `[class*="job"]` 命中了
/// **包住全部结果的祖先容器**，`div.panel` 同理。结果：只要**任意一条**失败项
/// 带封禁词，整页邮箱全被判封禁 → sub2api 里**全删**（含成功账号）。
///
/// 现在只认「单条结果卡片」，且强制「该容器恰好只含一个邮箱」：
/// 一旦发现容器里邮箱数 > 1，说明它是聚合容器，直接丢弃。
/// 宁可漏判（账号留着，下次再说），绝不误判（删号不可逆）。
///
/// 验证脚本：`cargo run --example ban_extract_probe -- tests/fixtures/jobs_page_repro.html`
pub const JS_EXTRACT_BANNED: &str = r#"(() => {
  const keywords = ['封禁','停用','deactivated','banned','suspended','disabled'];
  const emailRe = /[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}/g;
  const hasBanWord = (s) => !!s && keywords.some(k => s.toLowerCase().includes(k.toLowerCase()));
  const banned = new Set();

  // 只取「单条结果」容器：#jobs 的直接子元素，或结果区里的卡片。
  // 注意 [class~="card"] 是整词匹配，不会像 [class*="card"] 那样误伤 cardlist。
  const boxes = document.querySelectorAll('#jobs > *, #work .card, .jobs > *, [class~="card"]');

  for (const el of boxes) {
    const text = el.innerText || '';
    const own = text.match(emailRe) || [];

    // 该条状态：门页的 <span class="st ok|error|queued|running">。
    // 已成功的条目绝不可能「被封禁」——直接跳过，避免日志里出现 disabled 之类
    // 无关字样（例如「2FA disabled」）被算成封禁。
    const stEl = el.querySelector ? el.querySelector('.st') : null;
    if (stEl) {
      const stCls = (stEl.className || '').toLowerCase();
      const stTxt = (stEl.textContent || '').trim().toLowerCase();
      if (/\bok\b/.test(stCls) || stTxt === 'ok' || stTxt === '成功') continue;
    }

    // 结构性取邮箱：卡片里的 .mail 就是这一条的邮箱
    const mailEl = el.querySelector ? el.querySelector('.mail') : null;
    const mail = (mailEl && (mailEl.textContent || '').trim()) || '';

    // 拿不到结构化邮箱时，才退化成「容器里恰好一个邮箱」的推断；
    // 一旦 >1 就说明这是聚合容器，必须丢弃。
    let email = mail;
    if (!email) {
      if (own.length !== 1) continue;
      email = own[0];
    }

    // 双保险：即使拿到了 .mail，若卡片内还混着别的邮箱，也不认（结构异常）
    if (own.length > 1) continue;

    if (hasBanWord(text)) banned.add(email.toLowerCase());
  }

  return [...banned];
})()"#;

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
        if !js_fill(&page, "#emails", &emails.join("\n")).await {
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

            // 完成判定：批次已开始（total>0）且没有仍在进行的子任务（live==0）即可。
            // 不需要成功 > 0，也不需要 #copyAll 可见——页面可能是全部失败，
            // 此时 copyAll 仍 hidden，但任务已结束，应该继续下一步（读剪切板 / CPA 转换）。
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

            if total.unwrap_or(0) > 0 && live == Some(0) {
                done = true;
                final_state = snap;
                log(format!(
                    "批次结束：成功 {} / 失败 {} / 共 {}（copyAll 可见={}）",
                    ok_n.unwrap_or(0),
                    err_n.unwrap_or(0),
                    total.unwrap_or(0),
                    copy_visible
                ));
                break;
            }
            if !stat.is_empty() && stat == last_stat {
                stable += 1;
            } else {
                stable = 0;
            }
            last_stat = stat;
            // 兜底：stat 连续 3 次不变，批次在跑（total>0），且没有仍在进行的子任务（live==0 或解析不到）。
            // 绝不能在 live==Some(>0) 时触发，否则进行中 1 会被误判为完成。
            let no_live = live == Some(0) || live.is_none();
            if stable >= 3 && total.unwrap_or(0) > 0 && no_live {
                done = true;
                final_state = snap;
                log("#stat 连续 3 次不变且已无进行中任务，视为完成".to_string());
                break;
            }
        }
        if !done {
            final_state = json!({
                "note": "超时未检测到完成信号",
                "elapsed": format!("{}s", started.elapsed().as_secs()),
            });
        }
        (hooks.step)(
            "poll-done",
            if done {
                "检测到完成信号"
            } else {
                "超时结束"
            }
            .to_string(),
        );
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

        // 6) 提取可能被封禁/停用的账号邮箱
        let banned_emails = extract_banned_emails(&page).await;
        if !banned_emails.is_empty() {
            log(format!(
                "检测到被封禁/停用账号：{}",
                banned_emails.join(", ")
            ));
        }

        // 7) CPA 页转换：贴 #session-input → 读 #output
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
                } else if !js_fill(&p2, "#session-input", &clipboard).await {
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
            "bannedEmails": banned_emails,
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

// ---------------------------------------------------------------------------
// CDK 查询次数
// ---------------------------------------------------------------------------

async fn check_cdk_native(
    url: &str,
    cdk: &str,
    engine: Option<&str>,
    hooks: &FetchHooks,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    let log = |l: String| (hooks.log)(l);
    if cdk.trim().is_empty() {
        bail!("未保存 CDK，请先填写并保存 CDK");
    }
    if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
        bail!("已取消");
    }

    (hooks.step)("open", format!("打开门页：{}", url));
    log("启动无头浏览器…".to_string());
    let (browser, profile_dir) = launch(engine).await?;
    let res: Result<Value> = async {
        let page = browser.new_page("about:blank").await?;
        let mut errors = Vec::new();
        goto(&page, url, &mut errors).await;
        sleep(Duration::from_millis(2500)).await;

        let (_, entered) =
            ensure_entered(&page, Some(cdk), &mut errors, &|l| (hooks.log)(l)).await?;
        if !entered {
            bail!("未能进入页面（CDK 无效或页面结构变化）");
        }

        // 确保查询框里的 CDK 就是我们要查的那张
        js_fill(&page, "#cdk", cdk).await;
        (hooks.step)("check", format!("查询 CDK 剩余次数：{}", cdk));
        log(format!("查询 CDK 剩余次数：{}", cdk));
        if !js_click(&page, "#chk").await {
            bail!("点击 #chk（查询次数）失败");
        }

        let t0 = std::time::Instant::now();
        let mut left_text = String::new();
        let mut err_text = String::new();
        while t0.elapsed().as_millis() < 30000 {
            if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
                bail!("已取消");
            }
            sleep(Duration::from_millis(1000)).await;
            left_text = js_text(&page, "#left").await.trim().to_string();
            err_text = js_text(&page, "#err").await.trim().to_string();
            if !left_text.is_empty() || !err_text.is_empty() {
                break;
            }
        }

        if !err_text.is_empty() {
            bail!("查询失败：{}", err_text);
        }
        if left_text.is_empty() {
            bail!("查询超时，未拿到剩余次数");
        }

        // 解析 "剩余 N / M 次" 或 "剩余 N 次"
        let parse_nums = |s: &str| -> (Option<u64>, Option<u64>) {
            let rest = s.trim().strip_prefix("剩余").unwrap_or(s).trim();
            let rest = rest.strip_suffix("次").unwrap_or(rest).trim();
            let mut parts = rest.split('/');
            let a = parts.next().and_then(|x| x.trim().parse::<u64>().ok());
            let b = parts.next().and_then(|x| x.trim().parse::<u64>().ok());
            (a, b)
        };
        let (remaining, quota) = parse_nums(&left_text);

        (hooks.step)("check-done", format!("{}：{}", cdk, left_text));
        log(format!("查询结果：{}", left_text));

        Ok(json!({
            "ok": true,
            "cdk": cdk,
            "remaining": remaining,
            "quota": quota,
            "left_text": left_text,
            "errors": errors,
        }))
    }
    .await;
    cleanup_browser(browser, &profile_dir);
    res
}

/// 查询单张 CDK 的剩余次数。
pub fn check_cdk_left(
    url: &str,
    cdk: &str,
    engine: Option<&str>,
    hooks: &FetchHooks,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    rt().block_on(check_cdk_native(url, cdk, engine, hooks, cancel))
}

// ---------------------------------------------------------------------------
// CDK 合并
// ---------------------------------------------------------------------------

async fn merge_cdk_native(
    url: &str,
    codes: &[String],
    engine: Option<&str>,
    hooks: &FetchHooks,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    let log = |l: String| (hooks.log)(l);
    if codes.len() < 2 {
        bail!("至少需要两张 CDK 才能合并");
    }
    if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
        bail!("已取消");
    }

    let first = codes[0].trim();
    let other: Vec<String> = codes
        .iter()
        .skip(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    (hooks.step)("open", format!("打开门页：{}", url));
    log("启动无头浏览器…".to_string());
    let (browser, profile_dir) = launch(engine).await?;
    let res: Result<Value> = async {
        let page = browser.new_page("about:blank").await?;
        let mut errors = Vec::new();
        goto(&page, url, &mut errors).await;
        sleep(Duration::from_millis(2500)).await;

        // 用第一张 CDK 进入；进入后页面会自动把它填入 #cdk/#gateCdk
        let (_, entered) =
            ensure_entered(&page, Some(first), &mut errors, &|l| (hooks.log)(l)).await?;
        if !entered {
            bail!("未能进入页面（CDK 无效或页面结构变化）");
        }

        // 把其余 CDK 填入合并输入框；页面 mergeBtn 会自动加上 #cdk 和 #gateCdk 的值
        let merge_input = other.join("\n");
        (hooks.step)("merge", format!("合并 {} 张 CDK", codes.len()));
        log(format!(
            "填入合并 CDK（共 {} 张）：\\n{}",
            codes.len(),
            merge_input.replace('\n', ", ")
        ));
        if !js_fill(&page, "#mergeCodes", &merge_input).await {
            bail!("填入 #mergeCodes 失败");
        }

        if !js_click(&page, "#mergeBtn").await {
            bail!("点击 #mergeBtn 失败");
        }

        let t0 = std::time::Instant::now();
        let mut new_cdk = String::new();
        let mut new_left = String::new();
        while t0.elapsed().as_millis() < 60000 {
            if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
                bail!("已取消");
            }
            sleep(Duration::from_millis(1000)).await;
            new_cdk = js_text(&page, "#newCdk").await.trim().to_string();
            new_left = js_text(&page, "#newLeft").await.trim().to_string();
            let merge_err = js_text(&page, "#mergeErr").await.trim().to_string();
            let banner_visible = visible(&page, "#mergeBanner").await;
            if banner_visible && !new_cdk.is_empty() {
                break;
            }
            if !merge_err.is_empty() {
                bail!("合并失败：{}", merge_err);
            }
        }

        if new_cdk.is_empty() {
            bail!("合并超时，未拿到新 CDK");
        }

        (hooks.step)("merge-done", format!("新 CDK：{}{}", new_cdk, new_left));
        log(format!("合并完成，新 CDK：{}{}", new_cdk, new_left));

        Ok(json!({
            "ok": true,
            "new_cdk": new_cdk,
            "new_left": new_left,
            "errors": errors,
        }))
    }
    .await;
    cleanup_browser(browser, &profile_dir);
    res
}

/// 合并多张 CDK 为一张新 CDK。
pub fn merge_cdk(
    url: &str,
    codes: &[String],
    engine: Option<&str>,
    hooks: &FetchHooks,
    cancel: Option<&AtomicBool>,
) -> Result<Value> {
    rt().block_on(merge_cdk_native(url, codes, engine, hooks, cancel))
}

// ---------------------------------------------------------------------------
// 有机隐身模式 + OpenAI OAuth 授权（非 CDK 路径）
// ---------------------------------------------------------------------------

/// 注入指纹伪装 + 对齐 UA/Client Hints。
///
/// `auth.openai.com` 上，UA 与 Client Hints 不一致会被 Sentinel 判为伪造，
/// 所以这里用本机 Chrome 的真实版本号拼 UA，并用 CDP 覆盖 UA-CH，两侧完全一致。
async fn apply_identity(page: &chromiumoxide::Page) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::emulation::{
        SetUserAgentOverrideParams, UserAgentBrandVersion, UserAgentMetadata,
    };
    use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;

    page.execute(AddScriptToEvaluateOnNewDocumentParams::new(IDENTITY_JS))
        .await
        .context("注入指纹伪装脚本失败")?;

    let exe = detect_executable(None)?;
    let (major, full) = chrome_version(&exe).unwrap_or_else(|| ("152".into(), "152.0.0.0".into()));
    let display = {
        let parts: Vec<&str> = full.split('.').collect();
        let mut v: Vec<String> = parts.iter().take(3).map(|s| s.to_string()).collect();
        while v.len() < 3 {
            v.push("0".to_string());
        }
        v.join(".")
    };
    let ua = format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{} Safari/537.36",
        display
    );
    let bv = |b: &str, v: &str| UserAgentBrandVersion {
        brand: b.to_string(),
        version: v.to_string(),
    };
    let meta = UserAgentMetadata {
        brands: Some(vec![
            bv("Not?A_Brand", "24"),
            bv("Chromium", &major),
            bv("Google Chrome", &major),
        ]),
        full_version_list: Some(vec![
            bv("Not?A_Brand", "24.0.0.0"),
            bv("Chromium", &full),
            bv("Google Chrome", &full),
        ]),
        platform: "macOS".to_string(),
        platform_version: "15.0.0".to_string(),
        architecture: "arm".to_string(),
        model: String::new(),
        mobile: false,
        bitness: Some("64".to_string()),
        wow64: Some(false),
    };
    let _ = page
        .execute(SetUserAgentOverrideParams {
            user_agent: ua,
            accept_language: Some("en-US,en;q=0.9".to_string()),
            platform: Some("MacIntel".to_string()),
            user_agent_metadata: Some(meta),
        })
        .await;
    Ok(())
}

async fn mouse_move(page: &chromiumoxide::Page, x: f64, y: f64) {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchMouseEventParams, DispatchMouseEventType,
    };
    if let Ok(p) = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseMoved)
        .x(x)
        .y(y)
        .build()
    {
        let _ = page.execute(p).await;
    }
}

async fn mouse_wheel(page: &chromiumoxide::Page, x: f64, y: f64, dy: f64) {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchMouseEventParams, DispatchMouseEventType,
    };
    if let Ok(p) = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(x)
        .y(y)
        .delta_x(0.0)
        .delta_y(dy)
        .build()
    {
        let _ = page.execute(p).await;
    }
}

/// 有机预热：先在目标同域「像人一样逛一圈」，再进目标页。
///
/// xiic-crm 的实测结论（17TRACK + Cloudflare）：无头模式下**必须**有有机浏览
/// 才能通过风控；仅靠 flags + 指纹伪装不够。这里沿用同一思路，
/// 预热落在 `auth.openai.com` 同域，让 cf_clearance 覆盖目标页。
async fn organic_warmup(page: &chromiumoxide::Page, warm_url: &str, log: &dyn Fn(String)) {
    log(format!("有机预热：先访问 {}（模拟人类浏览）", warm_url));
    let _ = tokio::time::timeout(Duration::from_secs(35), page.goto(warm_url)).await;
    sleep(Duration::from_millis(2500)).await;

    for (x, y) in [
        (430.0, 265.0),
        (770.0, 415.0),
        (360.0, 610.0),
        (960.0, 335.0),
    ] {
        mouse_move(page, x, y).await;
        sleep(Duration::from_millis(600)).await;
    }
    mouse_wheel(page, 680.0, 400.0, 300.0).await;
    sleep(Duration::from_millis(600)).await;
    mouse_wheel(page, 680.0, 400.0, 420.0).await;
    sleep(Duration::from_millis(700)).await;
    mouse_wheel(page, 680.0, 400.0, -240.0).await;
    sleep(Duration::from_millis(500)).await;
    log(format!(
        "有机预热完成：{}",
        eval_string(page, "location.href").await
    ));
}

const JS_OAUTH_STATE: &str = "(() => ({ \
  u: location.href, t: document.title, \
  body: (document.body ? document.body.innerText : '').replace(/\\s+/g,' ').slice(0,260), \
  email: !!document.querySelector('input[name=email], input[type=email]'), \
  code: !!document.querySelector('input[name=code], input[autocomplete=one-time-code], input[inputmode=numeric]'), \
  pwd: !!document.querySelector('input[type=password]'), \
  challenge: !!document.querySelector('iframe[src*=\"challenges.cloudflare.com\"]'), \
  cfTitle: document.title.includes('请稍候') || document.title.includes('Just a moment') \
}))()";

/// JS 直接赋值填输入框（**不产生可信事件**）。
///
/// 曾用于 OpenAI 登录页，已改用 [`type_into`]（真实键击）——
/// Sentinel 能识别 `isTrusted:false` 的合成 input 事件。
/// 保留它是给「真实键击打不进去」的普通页面（门页 / CPA 页）兜底。
#[allow(dead_code)]
fn js_set_input(selector: &str, value: &str) -> String {
    let sel = json!(selector);
    let val = json!(value);
    format!(
        "(() => {{ const el = document.querySelector({sel}); if (!el) return 'no-el'; el.focus(); \
          const d = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value'); \
          const setter = (d && d.set) || Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set; \
          setter.call(el, {val}); el.dispatchEvent(new Event('input', {{ bubbles: true }})); \
          el.dispatchEvent(new Event('change', {{ bubbles: true }})); return 'ok'; }})()"
    )
}

fn js_click_text(text: &str) -> String {
    let t = json!(text);
    format!(
        "(() => {{ const want = {t}; \
          const els = [...document.querySelectorAll('button, [role=button], input[type=submit]')]; \
          const hit = els.find(e => e.offsetHeight > 0 && (e.textContent || '').trim().includes(want)); \
          if (!hit) return 'no-btn'; hit.click(); return 'clicked'; }})()"
    )
}

/// 「像人一样」输入文本：真实鼠标点击 + CDP 真实键击。
///
/// 为什么不用 [`js_set_input`]：直接给 `el.value` 赋值只产生 `isTrusted:false`
/// 的合成事件；再叠加「页面刚出来 1 秒内就填好邮箱并提交」，
/// 这是典型机器人特征 —— OpenAI Sentinel 据此把
/// `POST /api/accounts/authorize/continue` 判成 403（返回 HTML）。
///
/// 这里改走 CDP `Input.dispatchKeyEvent`（`Element::type_str`），
/// 事件是可信的，节奏也按人手速度来（带抖动 + `@`/`.` 处稍停）。
async fn type_into(page: &chromiumoxide::Page, selector: &str, text: &str) -> bool {
    // find_element 只吃单个选择器，取第一个
    let sel = selector
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(selector);
    let el = match page.find_element(sel).await {
        Ok(el) => el,
        Err(_) => return false,
    };
    // 真鼠标点击（顺便聚焦）
    if el.click().await.is_err() {
        return false;
    }
    sleep(Duration::from_millis(180)).await;
    // 清掉可能存在的预填值
    let _ = eval_string(
        page,
        &format!(
            "(() => {{ const e = document.querySelector({}); if (e) {{ e.value = ''; \
             e.dispatchEvent(new Event('input', {{ bubbles: true }})); }} return 'ok'; }})()",
            json!(sel)
        ),
    )
    .await;
    // 逐字敲
    for (i, c) in text.chars().enumerate() {
        let mut buf = [0u8; 4];
        let s: &str = c.encode_utf8(&mut buf);
        if el.type_str(s).await.is_err() {
            return false;
        }
        let base: u64 = match c {
            '@' | '.' | '_' | '-' => 105,
            _ => 38,
        };
        sleep(Duration::from_millis(base + (i as u64 * 37) % 55)).await;
    }
    // 确认落值（有些页面会做受控组件回写）
    let got = eval_string(
        page,
        &format!(
            "(() => {{ const e = document.querySelector({}); return e ? String(e.value || '') : ''; }})()",
            json!(sel)
        ),
    )
    .await;
    got.trim() == text.trim()
}

/// 有界的真实键击输入。逐字符 dispatch 期间若渲染进程被挂起，
/// CDP 可能不回包 —— 超时即放弃，交给外层循环重试。
async fn type_into_bounded(
    page: &chromiumoxide::Page,
    selector: &str,
    text: &str,
    secs: u64,
) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(secs), type_into(page, selector, text)).await,
        Ok(true)
    )
}

/// 线程安全的日志收集器：既推给上层回调，也留一份在结果里。
#[derive(Clone)]
struct Logger {
    logs: Arc<std::sync::Mutex<Vec<String>>>,
    out: Arc<dyn Fn(String) + Send + Sync>,
}

impl Logger {
    fn new(out: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        Self {
            logs: Arc::new(std::sync::Mutex::new(Vec::new())),
            out,
        }
    }
    fn log(&self, m: String) {
        if let Ok(mut g) = self.logs.lock() {
            g.push(m.clone());
        }
        (self.out)(m);
    }
    fn take(&self) -> Vec<String> {
        self.logs.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// OpenAI OAuth 授权的进度回调。
#[derive(Clone)]
pub struct OAuthHooks {
    pub log: Arc<dyn Fn(String) + Send + Sync>,
    pub step: Arc<dyn Fn(&'static str, String) + Send + Sync>,
}

/// OpenAI OAuth 授权结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthOutcome {
    /// 换到的授权码（拿它去 POST /admin/accounts/exchange-code）
    pub code: Option<String>,
    /// 停在哪个阶段：done | need_code | need_password | blocked | timeout | cancelled
    pub status: String,
    /// 是否需要人工介入（有头模式下让用户点一下）
    pub needs_human: bool,
    /// 人类可读说明
    pub message: String,
    /// 过程中产生的日志
    pub log: Vec<String>,
}

/// 驱动浏览器完成 OpenAI 授权，拿到回调里的 `code`。
///
/// 流程：有机预热 → 打开授权链接 → 过 Cloudflare → 填邮箱 → 点继续 →
/// 从收码站轮询验证码 → 填入 → 等重定向到 `localhost:1455/auth/callback?code=...`。
/// 任何一步需要人工（Cloudflare 交互式质询 / 要密码）都会如实返回，交由上层提示。
#[allow(clippy::too_many_arguments)]
async fn openai_oauth_native(
    auth_url: &str,
    email: &str,
    mail_cfg: &crate::mail::MailConfig,
    mode: BrowserMode,
    engine: Option<&str>,
    max_seconds: u64,
    cancel: Option<&AtomicBool>,
    hooks: &OAuthHooks,
) -> Result<OAuthOutcome> {
    use std::sync::atomic::Ordering;

    let lg = Logger::new(hooks.log.clone());
    let push = |m: String| lg.log(m);

    // 收码「水位线」：必须在把邮箱提交给 OpenAI **之前**打。
    // 之后到达的才算本次的验证码，历史旧码一律不认。
    let mail_baseline: Option<crate::mail::MailBaseline> = if mail_cfg.ready() {
        let cfg = mail_cfg.clone();
        let em = email.to_string();
        let lg2 = lg.clone();
        tokio::task::block_in_place(move || {
            match crate::mail::MailClient::login(&cfg).and_then(|c| c.snapshot(&em)) {
                Ok(Some(b)) => {
                    lg2.log(format!(
                        "收码水位线已记录：mailbox_id={}，邮箱里已有 {} 封历史邮件（只认之后的新件）",
                        b.email_id, b.existing
                    ));
                    Some(b)
                }
                Ok(None) => {
                    lg2.log(format!(
                        "⚠️ 收码站里没有 {}，稍后取不到码（先在账号列表点「导入邮箱」）",
                        em
                    ));
                    None
                }
                Err(e) => {
                    lg2.log(format!("⚠️ 记录收码水位线失败：{:#}", e));
                    None
                }
            }
        })
    } else {
        None
    };

    push(format!(
        "启动浏览器（模式：{}）",
        match mode {
            BrowserMode::Headless => "无头",
            BrowserMode::Stealth => "有机隐身（无头）",
            BrowserMode::StealthHeaded => "有机隐身（有头）",
        }
    ));
    let (browser, profile_dir) = launch_ex(engine, mode).await?;
    // 硬超时兜底：循环内的 CDP 调用虽然都改成了有界的，但浏览器/网络层仍可能
    // 出现意料之外的阻塞。这一层保证「无论如何流程一定会收尾」——包括 cancel。
    let hard = Duration::from_secs(max_seconds + 180);
    let res: Result<OAuthOutcome> = match tokio::time::timeout(
        hard,
        async {
        let page = browser.new_page("about:blank").await?;
        if mode.stealth() {
            apply_identity(&page).await?;
        }

        if mode.organic() {
            (hooks.step)("warmup", "有机预热（同域浏览，降低风控）".to_string());
            organic_warmup(&page, "https://auth.openai.com/", &|m: String| lg.log(m)).await;
        }

        (hooks.step)("open", "打开授权链接".to_string());
        let _ = tokio::time::timeout(Duration::from_secs(60), page.goto(auth_url)).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(max_seconds);
        let mut email_filled = false;
        let mut code_filled = false;
        let mut challenge_seen_at: Option<std::time::Instant> = None;
        let mut reloads = 0;
        let mut code_wait_announced = false;
        // 邮箱框首次出现的时间：别一出现就秒填，先让页面「稳」一下（拟人）
        let mut email_seen_at: Option<std::time::Instant> = None;

        loop {
            if let Some(c) = cancel {
                if c.load(Ordering::SeqCst) {
                    return Ok(OAuthOutcome {
                        code: None,
                        status: "cancelled".into(),
                        needs_human: false,
                        message: "已取消".into(),
                        log: lg.take(),
                    });
                }
            }
            if std::time::Instant::now() > deadline {
                return Ok(OAuthOutcome {
                    code: None,
                    status: "timeout".into(),
                    needs_human: true,
                    message: format!("等待授权超时（{}s）", max_seconds),
                    log: lg.take(),
                });
            }
            sleep(Duration::from_secs(2)).await;

            let st = eval_value_bounded(&page, JS_OAUTH_STATE, 8).await;
            let url = st.get("u").and_then(Value::as_str).unwrap_or("").to_string();

            // 1) 回调命中 —— 成功
            if url.starts_with("http://localhost:1455") || url.contains("localhost:1455/auth/callback") {
                let code = url
                    .split("code=")
                    .nth(1)
                    .and_then(|s| s.split('&').next())
                    .map(str::to_string);
                push(format!(
                    "已拿到 OAuth 回调{}",
                    if code.is_some() { "（含 code）" } else { "（无 code，可能被拒绝）" }
                ));
                return Ok(OAuthOutcome {
                    code,
                    status: "done".into(),
                    needs_human: false,
                    message: "授权完成".into(),
                    log: lg.take(),
                });
            }

            // 2) Cloudflare 质询：给它时间自解；解不开就重载，再不行交人工
            let challenged = st.get("challenge").and_then(Value::as_bool).unwrap_or(false)
                || st.get("cfTitle").and_then(Value::as_bool).unwrap_or(false);
            if challenged {
                let since = *challenge_seen_at.get_or_insert_with(std::time::Instant::now);
                let waited = since.elapsed().as_secs();
                if waited > 12 && reloads < 2 {
                    reloads += 1;
                    challenge_seen_at = None;
                    push(format!("质询未过，第 {} 次重载授权链接", reloads));
                    let _ = tokio::time::timeout(Duration::from_secs(60), page.goto(auth_url)).await;
                } else if waited > 40 {
                    return Ok(OAuthOutcome {
                        code: None,
                        status: "blocked".into(),
                        needs_human: true,
                        message: "Cloudflare 要求人工验证，请在有头窗口里点一下「验证您是真人」".into(),
                        log: lg.take(),
                    });
                }
                continue;
            }
            challenge_seen_at = None;

            // 3) 要密码 —— 这些号没有密码，只能人工
            if st.get("pwd").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(OAuthOutcome {
                    code: None,
                    status: "need_password".into(),
                    needs_human: true,
                    message: "该账号要求输入密码，请在浏览器里手动完成".into(),
                    log: lg.take(),
                });
            }

            // 4) 填邮箱 —— 用真实键击，且先等页面「稳」住再动手（拟人）
            if !email_filled && st.get("email").and_then(Value::as_bool).unwrap_or(false) {
                let seen = *email_seen_at.get_or_insert_with(std::time::Instant::now);
                // 页面出现后先停 1.5~2.6s，别瞬间填完提交
                let settle = 1500 + (email.len() as u64 * 11) % 1100;
                if seen.elapsed().as_millis() < settle as u128 {
                    // 等待期间做点像人的小动作
                    let _ = eval_string(
                        &page,
                        "(() => { window.dispatchEvent(new MouseEvent('mousemove', { bubbles: true, clientX: 400 + Math.random() * 300, clientY: 200 + Math.random() * 200 })); return 'ok'; })()",
                    )
                    .await;
                    continue;
                }
                if type_into_bounded(&page, "input[name=email], input[type=email]", email, 20).await {
                    push(format!("已用真实键击输入邮箱 {}", email));
                    sleep(Duration::from_millis(500)).await;
                    let _ = eval_string_bounded(&page, &js_click_text("继续"), 5).await;
                    email_filled = true;
                    push(format!("已提交邮箱 {}", email));
                }
            }

            // 5) 验证码框出现 —— 去收码站取码并填
            if email_filled
                && !code_filled
                && st.get("code").and_then(Value::as_bool).unwrap_or(false)
            {
                if !code_wait_announced {
                    code_wait_announced = true;
                    (hooks.step)("code", "等待邮箱验证码".to_string());
                    push(format!("等待 {} 的验证码（收码站）", email));
                }
                if mail_cfg.ready() {
                    let cfg = mail_cfg.clone();
                    let mail_log = lg.clone();
                    let em = email.to_string();
                    let base = mail_baseline.clone();
                    // blocking 的 reqwest 不能直接在 async 里跑
                    let got = tokio::task::block_in_place(move || {
                        let client = crate::mail::MailClient::login(&cfg)?;
                        client.wait_code(
                            &em,
                            base.as_ref(),
                            Duration::from_secs(120),
                            Duration::from_secs(4),
                            cancel,
                            &|m: String| mail_log.log(m),
                        )
                    });
                    match got {
                        Ok(Some(code)) => {
                            // 验证码同样用真实键击
                            let ok = type_into_bounded(
                                &page,
                                "input[name=code], input[autocomplete=one-time-code], input[inputmode=numeric], input[type=text]",
                                &code,
                                20,
                            )
                            .await;
                            if ok {
                                sleep(Duration::from_millis(500)).await;
                                let _ = eval_string_bounded(&page, &js_click_text("继续"), 5).await;
                                code_filled = true;
                                push(format!("已提交验证码 {}", code));
                                (hooks.step)("consent", "等待授权确认".to_string());
                            } else {
                                push("验证码填不进去，稍后重试".to_string());
                            }
                        }
                        Ok(None) => {
                            push("暂未取到验证码，继续等待".to_string());
                        }
                        Err(e) => {
                            push(format!("收码站取码失败：{:#}", e));
                        }
                    }
                } else {
                    return Ok(OAuthOutcome {
                        code: None,
                        status: "need_code".into(),
                        needs_human: true,
                        message: "已到验证码输入页，但未配置收码站，请手动输入验证码".into(),
                        log: lg.take(),
                    });
                }
            }
        }
        },
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            push(format!("⚠️ 流程硬超时（{}s），强制收尾", hard.as_secs()));
            Ok(OAuthOutcome {
                code: None,
                status: "timeout".into(),
                needs_human: true,
                message: format!("流程硬超时（{}s）", hard.as_secs()),
                log: lg.take(),
            })
        }
    };
    cleanup_browser(browser, &profile_dir);
    res
}

/// 非 CDK 路径：打开 sub2api 生成的授权链接，自动填邮箱/验证码，换回授权码。
#[allow(clippy::too_many_arguments)]
pub fn openai_oauth(
    auth_url: &str,
    email: &str,
    mail_cfg: &crate::mail::MailConfig,
    mode: BrowserMode,
    engine: Option<&str>,
    max_seconds: u64,
    cancel: Option<&AtomicBool>,
    hooks: &OAuthHooks,
) -> Result<OAuthOutcome> {
    rt().block_on(openai_oauth_native(
        auth_url,
        email,
        mail_cfg,
        mode,
        engine,
        max_seconds,
        cancel,
        hooks,
    ))
}
