//! 探针：验证「无头 + 隐身启动参数 + 有机预热」能否通过 Cloudflare 到达 OpenAI 登录页。
//!
//! 用法：
//! ```text
//! cargo run --example stealth_probe -- "<auth_url>" [选项]
//! 选项：
//!   --headed            有头模式（默认无头）
//!   --warmup=none       不做预热（默认）
//!   --warmup=openai     先访问 https://openai.com/ 并模拟人类行为
//!   --warmup=auth       先访问 https://auth.openai.com/ 并模拟人类行为
//!   --seconds=45        目标页观测时长
//!   --keep              结束后保留浏览器（debug 用）
//!   --fast              关闭 UA/指纹伪装（对照实验）
//! ```

use std::path::PathBuf;
use std::time::Instant;

use chromiumoxide::browser::{Browser, HeadlessMode};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType,
};
use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
use chromiumoxide::BrowserConfig;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

const ORGANIC_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";

const STEALTH_ARGS: &[&str] = &[
    // 去掉自动化标识（chromiumoxide 默认参数里带 --enable-automation，已被 disable_default_args 移除）
    "--disable-blink-features=AutomationControlled",
    // 降低 headless 特征
    "--disable-features=IsolateOrigins,VizDisplayCompositor,TranslateUI",
    "--disable-site-isolation-trials",
    "--disable-software-rasterizer",
    "--renderer-process-limit=1",
    // 常规「干净」启动参数（对齐 puppeteer 默认，但剔除 enable-automation）
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
    "--lang=en-US",
    "--window-size=1366,768",
    "--window-position=0,0",
];

/// 指纹伪装：与 xiic-crm 的 headless_stealth_with_warmup 对齐。
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

#[derive(Default, Clone)]
struct Opts {
    headed: bool,
    warmup: String,
    seconds: u64,
    keep: bool,
    fast: bool,
    email: Option<String>,
    code: Option<String>,
    no_ua: bool,
}

fn parse_args() -> (String, Opts) {
    let mut url = String::new();
    let mut o = Opts {
        warmup: "none".into(),
        seconds: 45,
        ..Default::default()
    };
    for a in std::env::args().skip(1) {
        if let Some(v) = a.strip_prefix("--warmup=") {
            o.warmup = v.to_string();
        } else if let Some(v) = a.strip_prefix("--seconds=") {
            o.seconds = v.parse().unwrap_or(45);
        } else if let Some(v) = a.strip_prefix("--email=") {
            o.email = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--code=") {
            o.code = Some(v.to_string());
        } else if a == "--headed" {
            o.headed = true;
        } else if a == "--keep" {
            o.keep = true;
        } else if a == "--fast" {
            o.fast = true;
        } else if a == "--no-ua" {
            o.no_ua = true;
        } else if !a.starts_with("--") {
            url = a;
        }
    }
    (url, o)
}

/// React 受控输入：必须走原生 setter + input 事件，直接改 value 不会触发 onChange。
fn js_set_input(selector: &str, value: &str) -> String {
    let sel = serde_json::to_string(selector).unwrap();
    let val = serde_json::to_string(value).unwrap();
    format!(
        r#"(() => {{
  const el = document.querySelector({sel});
  if (!el) return 'no-el';
  el.focus();
  const d = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value');
  const setter = (d && d.set) || Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
  setter.call(el, {val});
  el.dispatchEvent(new Event('input', {{ bubbles: true }}));
  el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  return 'ok';
}})()"#
    )
}

/// 按可见文本点按钮（支持部分匹配）。
fn js_click_text(text: &str) -> String {
    let t = serde_json::to_string(text).unwrap();
    format!(
        r#"(() => {{
  const want = {t};
  const cands = [...document.querySelectorAll('button, [role=button], input[type=submit], a')];
  const hit = cands.find(e => e.offsetHeight > 0 && (e.textContent || '').trim().includes(want));
  if (!hit) return 'no-btn';
  hit.click();
  return 'clicked:' + (hit.textContent || '').trim().slice(0, 30);
}})()"#
    )
}

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
                .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
        })
}

async fn eval<'a>(page: &chromiumoxide::Page, expr: &'a str) -> Value {
    match page.evaluate_expression(expr).await {
        Ok(r) => r.value().cloned().unwrap_or(Value::Null),
        Err(e) => json!(format!("ERR {e}")),
    }
}

async fn mouse(page: &chromiumoxide::Page, x: f64, y: f64) {
    let p = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseMoved)
        .x(x)
        .y(y)
        .build();
    if let Ok(p) = p {
        let _ = page.execute(p).await;
    }
}

async fn wheel(page: &chromiumoxide::Page, x: f64, y: f64, dy: f64) {
    let p = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(x)
        .y(y)
        .delta_x(0.0)
        .delta_y(dy)
        .build();
    if let Ok(p) = p {
        let _ = page.execute(p).await;
    }
}

/// 有机预热：像人一样逛一圈，给 Cloudflare 留下流量轨迹。
async fn organic_warmup(page: &chromiumoxide::Page, kind: &str, log: &dyn Fn(String)) {
    let target = match kind {
        "auth" => "https://auth.openai.com/",
        _ => "https://openai.com/",
    };
    log(format!("[warmup] 访问 {target}"));
    let _ = tokio::time::timeout(Duration::from_secs(35), page.goto(target)).await;
    sleep(Duration::from_millis(3500)).await;

    // 鼠标轨迹
    for (x, y) in [
        (420.0, 260.0),
        (760.0, 410.0),
        (350.0, 620.0),
        (980.0, 330.0),
    ] {
        mouse(page, x, y).await;
        sleep(Duration::from_millis(650)).await;
    }
    // 滚动
    wheel(page, 680.0, 400.0, 300.0).await;
    sleep(Duration::from_millis(700)).await;
    wheel(page, 680.0, 400.0, 420.0).await;
    sleep(Duration::from_millis(900)).await;
    wheel(page, 680.0, 400.0, -260.0).await;
    sleep(Duration::from_millis(600)).await;

    // 悬停链接
    let _ = eval(
        page,
        "(() => { const a=[...document.querySelectorAll('a')].filter(x=>x.offsetHeight>0); \
         if(a.length){ const t=a[Math.floor(Math.random()*a.length)]; t.dispatchEvent(new MouseEvent('mouseover',{bubbles:true})); t.dispatchEvent(new MouseEvent('mousemove',{bubbles:true})); return t.textContent.trim().slice(0,40);} return 'no-link'; })()",
    )
    .await;
    sleep(Duration::from_millis(800)).await;
    log(format!(
        "[warmup] 完成 url={} title={}",
        eval(page, "location.href").await.as_str().unwrap_or(""),
        eval(page, "document.title").await.as_str().unwrap_or("")
    ));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (url, o) = parse_args();
    if url.is_empty() {
        eprintln!("用法: cargo run --example stealth_probe -- <auth_url> [--headed] [--warmup=none|openai|auth] [--seconds=N] [--keep]");
        std::process::exit(2);
    }
    let log = |m: String| println!("{}", m);
    let exe = find_chrome().expect("找不到 Chrome");

    let profile = std::env::temp_dir().join(format!(
        "sub2op-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&profile)?;

    let mut cfg = BrowserConfig::builder()
        .chrome_executable(exe.clone())
        .user_data_dir(&profile)
        // 关键：丢掉 chromiumoxide 的默认参数（其中含 --enable-automation）
        .disable_default_args();
    for a in STEALTH_ARGS {
        cfg = cfg.arg(*a);
    }
    if !o.fast && !o.no_ua {
        cfg = cfg.arg(format!("--user-agent={}", ORGANIC_UA));
    }
    if o.headed {
        cfg = cfg.headless_mode(HeadlessMode::False);
    } else {
        cfg = cfg.headless_mode(HeadlessMode::New);
    }
    let config = cfg.build().map_err(|e| anyhow::anyhow!("config: {e}"))?;

    log(format!(
        "[launch] headless={} warmup={} fast={} exe={}",
        !o.headed,
        o.warmup,
        o.fast,
        exe.display()
    ));
    let (browser, mut handler) = Browser::launch(config).await?;
    tokio::task::spawn(async move { while let Some(_e) = handler.next().await {} });

    let page = browser.new_page("about:blank").await?;
    if !o.fast {
        page.execute(AddScriptToEvaluateOnNewDocumentParams::new(IDENTITY_JS))
            .await?;
    }

    if !o.fast && !o.no_ua {
        // UA 与 Client Hints 必须一致，否则 Sentinel 判定为伪造
        use chromiumoxide::cdp::browser_protocol::emulation::{
            SetUserAgentOverrideParams, UserAgentBrandVersion, UserAgentMetadata,
        };
        let bv = |b: &str, v: &str| UserAgentBrandVersion {
            brand: b.to_string(),
            version: v.to_string(),
        };
        let meta = UserAgentMetadata {
            brands: Some(vec![
                bv("Not?A_Brand", "24"),
                bv("Chromium", "152"),
                bv("Google Chrome", "152"),
            ]),
            full_version_list: Some(vec![
                bv("Not?A_Brand", "24.0.0.0"),
                bv("Chromium", "152.0.7977.84"),
                bv("Google Chrome", "152.0.7977.84"),
            ]),
            platform: "macOS".to_string(),
            platform_version: "26.5.2".to_string(),
            architecture: "arm".to_string(),
            model: String::new(),
            mobile: false,
            bitness: Some("64".to_string()),
            wow64: Some(false),
        };
        let override_params = SetUserAgentOverrideParams {
            user_agent: ORGANIC_UA.to_string(),
            accept_language: Some("zh-CN,zh;q=0.9,en;q=0.8".to_string()),
            platform: Some("MacIntel".to_string()),
            user_agent_metadata: Some(meta),
        };
        let _ = page.execute(override_params).await;
    }

    if o.warmup != "none" {
        let lg = log.clone();
        organic_warmup(&page, &o.warmup, &move |m| lg(m)).await;
    }

    log(format!("[goto] {url}"));
    let t0 = Instant::now();
    let _ = tokio::time::timeout(Duration::from_secs(60), page.goto(url.clone())).await;

    const JS_SNAP: &str = "(() => ({ u: location.href, t: document.title, \
      body: (document.body ? document.body.innerText : '').replace(/\\s+/g,' ').slice(0,300), \
      inputs: [...document.querySelectorAll('input,textarea')].map(e=>({type:e.type,name:e.name,id:e.id,ph:e.placeholder})), \
      btns: [...document.querySelectorAll('button,[role=button]')].map(e=>e.textContent.trim().slice(0,30)).filter(Boolean), \
      nav: {ua: navigator.userAgent, wd: navigator.webdriver, plat: navigator.platform, ch: (navigator.userAgentData ? navigator.userAgentData.toJSON() : null)}, \
      challenge: !!document.querySelector('iframe[src*=\"challenges.cloudflare.com\"]'), \
      cfTitle: document.title.includes('请稍候') || document.title.includes('Just a moment') }))()";

    let mut timeline: Vec<Value> = Vec::new();
    let mut passed_at: Option<u64> = None;
    let mut email_sent = false;
    let mut code_sent = false;
    let mut callback_code: Option<String> = None;
    let mut signatures: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(o.seconds);

    while Instant::now() < deadline {
        sleep(Duration::from_secs(2)).await;
        let snap = eval(&page, JS_SNAP).await;
        let u = snap
            .get("u")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let title = snap
            .get("t")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let body = snap
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ch = snap
            .get("challenge")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let cf = snap
            .get("cfTitle")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let el = t0.elapsed().as_secs();

        // 每一个「新页面状态」打印一次完整结构，避免刷屏
        let sig = format!(
            "{}|{}|{}",
            u,
            title,
            body.chars().take(80).collect::<String>()
        );
        let is_new = !signatures.iter().any(|s| s == &sig);
        if is_new {
            signatures.push(sig);
            let xhr = eval(
                &page,
                "performance.getEntriesByType('resource').filter(e=>e.initiatorType==='fetch'||e.initiatorType==='xmlhttprequest').map(e=>({n:e.name.slice(-70),s:e.responseStatus,d:Math.round(e.duration)})).slice(-12)",
            )
            .await;
            log(format!(
                "\n----- [{el}s] 新状态 -----\nurl   : {u}\ntitle : {title}\nnav   : {}\nxhr   : {}\nbody  : {}\ninputs: {}\nbtns  : {}",
                snap.get("nav").map(|v| v.to_string()).unwrap_or_default(),
                xhr,
                body.chars().take(300).collect::<String>(),
                snap.get("inputs").map(|v| v.to_string()).unwrap_or_default(),
                snap.get("btns").map(|v| v.to_string()).unwrap_or_default()
            ));
            let _ = page
                .save_screenshot(
                    chromiumoxide::page::ScreenshotParams::builder().build(),
                    format!("/tmp/probe-{el}s.png"),
                )
                .await;
        }

        // Cloudflare 质询超过 15s 未过 → 重载目标页（最多 3 次）
        if (ch || cf) && !url.is_empty() {
            let waited = t0.elapsed().as_secs();
            let retries_done = signatures
                .iter()
                .filter(|x| x.starts_with("__reload__"))
                .count();
            if waited > 15 * (retries_done as u64 + 1) + 5 && retries_done < 3 {
                signatures.push("__reload__".to_string());
                log(format!(
                    "[retry] 质询未过，第 {} 次重载目标页",
                    retries_done + 1
                ));
                let _ = tokio::time::timeout(Duration::from_secs(60), page.goto(url.clone())).await;
            }
        }

        // OAuth 回调：拿到 code
        if u.starts_with("http://localhost:1455") || u.contains("localhost:1455/auth/callback") {
            if let Some(c) = u.split("code=").nth(1).and_then(|s| s.split('&').next()) {
                callback_code = Some(c.to_string());
                passed_at = Some(el);
                log(format!(
                    "== OAuth 回调拿到 code：{} ==",
                    &c[..c.len().min(40)]
                ));
                break;
            }
        }

        // 填邮箱
        if !email_sent {
            if let Some(em) = o.email.clone() {
                let filled = eval(
                    &page,
                    &js_set_input("input[name=email], input[type=email]", &em),
                )
                .await;
                if filled.as_str() == Some("ok") {
                    sleep(Duration::from_millis(400)).await;
                    let clicked = eval(&page, &js_click_text("继续")).await;
                    email_sent = true;
                    log(format!("[act] 填邮箱 {em} -> clicked={clicked}"));
                }
            }
        } else if !code_sent {
            if let Some(cd) = o.code.clone() {
                let sel = "input[name=code], input[inputmode=numeric], input[autocomplete=one-time-code], input[type=text]";
                let has = eval(
                    &page,
                    &format!(
                        "!!document.querySelector({})",
                        serde_json::to_string(sel).unwrap()
                    ),
                )
                .await;
                if has.as_bool() == Some(true) {
                    let filled = eval(&page, &js_set_input(sel, &cd)).await;
                    sleep(Duration::from_millis(400)).await;
                    let clicked = eval(&page, &js_click_text("继续")).await;
                    code_sent = true;
                    log(format!(
                        "[act] 填验证码 {cd} -> filled={filled} clicked={clicked}"
                    ));
                }
            }
        }

        if !ch && !cf {
            if passed_at.is_none()
                && (u.contains("auth.openai.com/log-in")
                    || u.contains("auth.openai.com/authorize")
                    || u.contains("chatgpt.com"))
            {
                log("[ok] 未遇到 Cloudflare 质询，已进入登录/授权页".to_string());
            }
        }
    }

    let last = timeline.last().cloned().unwrap_or(Value::Null);
    println!(
        "\n===== 结果 =====\n{}",
        serde_json::to_string_pretty(&json!({
            "reachedLogin": !signatures.is_empty(),
            "callbackCode": callback_code,
            "email": o.email,
            "code": o.code,
            "warmup": o.warmup,
            "headed": o.headed,
            "fast": o.fast,
            "noUa": o.no_ua,
            "states": signatures.len(),
            "final": last,
        }))?
    );

    if !o.keep {
        drop(page);
        drop(browser);
        sleep(Duration::from_millis(300)).await;
        let _ = std::fs::remove_dir_all(&profile);
    } else {
        log("[keep] 浏览器保留，Ctrl+C 退出".to_string());
        sleep(Duration::from_secs(600)).await;
    }
    Ok(())
}
