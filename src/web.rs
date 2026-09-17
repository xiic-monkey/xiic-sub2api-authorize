//! 本地运维页：用 axum 起一个仅监听 127.0.0.1 的极小服务。
//!
//! 两个能力：
//! 1. CDK 录入（写入 credentials.db）；
//! 2. **一键重新授权**：自动找出报 401 的账号 → 无头浏览器跑接码流程 →
//!    结果按邮箱（忽略大小写）写回 sub2api。后台线程执行，前端每秒轮询进度。

use crate::client;
use crate::commands::reauth;
use crate::config::Config;
use crate::store;
use anyhow::Result;
use axum::{
    extract::{Form, State},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

const PORT: u16 = 7878;

#[derive(Deserialize)]
struct CredForm {
    cdk: String,
}

#[derive(Deserialize)]
struct ReauthForm {
    #[serde(default)]
    yes: String,
    #[serde(default)]
    email: String,
}

/// 后台任务状态（前端轮询）。
#[derive(Clone, Debug, Default, Serialize)]
struct Job {
    running: bool,
    log: Vec<String>,
    result: Option<String>,
    error: Option<String>,
}

#[derive(Clone)]
struct AppState {
    job: Arc<Mutex<Job>>,
    /// config.toml 解析结果；缺失时一键重授权不可用（页面会给出提示）
    cfg: Option<Config>,
    /// 浏览器引擎：None=内置 Chromium（服务器/Docker），Some("chrome")=本机 Chrome
    engine: Option<String>,
}

pub async fn run(engine: Option<&str>) -> Result<()> {
    let cfg = match Config::load(Path::new("config.toml")) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("⚠️  config.toml 不可用：{}（一键重授权将不可用）", e);
            None
        }
    };

    let state = AppState {
        job: Arc::new(Mutex::new(Job::default())),
        cfg,
        engine: engine.map(|s| s.to_string()),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/save", post(save_handler))
        .route("/api/creds", get(api_creds))
        .route("/api/reauth", post(api_reauth))
        .route("/api/reauth/status", get(api_reauth_status))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    println!("🌐 运维页已启动: http://127.0.0.1:{}", PORT);
    println!("   · 上方填 CDK 保存（写入 credentials.db）");
    println!("   · 下方「一键重新授权」：自动处理所有报 401 的账号");
    println!("   按 Ctrl+C 关闭本服务。");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(State(st): State<AppState>) -> Html<String> {
    Html(page_html("", &st))
}

async fn api_creds() -> impl IntoResponse {
    let json = match store::load() {
        Ok(Some(c)) => serde_json::json!({
            "cdk": c.cdk,
            "updated_at": c.updated_at,
        }),
        Ok(None) => serde_json::json!({ "cdk": "", "updated_at": "" }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    axum::Json(json)
}

async fn save_handler(State(st): State<AppState>, Form(f): Form<CredForm>) -> Html<String> {
    let msg = match store::save(&f.cdk) {
        Ok(()) => "✅ 已保存到 credentials.db".to_string(),
        Err(e) => format!("❌ 保存失败: {}", e),
    };
    Html(page_html(&msg, &st))
}

/// 启动一键重授权（后台线程执行，立即返回）。
async fn api_reauth(State(st): State<AppState>, Form(f): Form<ReauthForm>) -> Json<Value> {
    {
        let mut job = st.job.lock().unwrap();
        if job.running {
            return Json(json!({ "ok": false, "error": "已有任务在运行中，请等它结束" }));
        }
        job.running = true;
        job.log.clear();
        job.result = None;
        job.error = None;
    }

    let cfg = match st.cfg.clone() {
        Some(c) => c,
        None => {
            let mut job = st.job.lock().unwrap();
            job.running = false;
            job.error = Some(
                "缺少可用的 config.toml（需含 base_url/email/password）。\
                 请在启动本服务的目录放好 config.toml 后重启。"
                    .to_string(),
            );
            return Json(json!({ "ok": false, "error": job.error.clone().unwrap() }));
        }
    };

    let job = st.job.clone();
    let engine = st.engine.clone();
    let yes = matches!(f.yes.as_str(), "1" | "on" | "true" | "yes");
    let only_email = {
        let e = f.email.trim().to_string();
        if e.is_empty() {
            None
        } else {
            Some(e)
        }
    };

    std::thread::spawn(move || {
        let logger: reauth::Logger = Arc::new({
            let job = job.clone();
            move |line: String| {
                if let Ok(mut j) = job.lock() {
                    j.log.push(line);
                }
            }
        });

        let res = (|| -> anyhow::Result<String> {
            let mut c = client::Sub2ApiClient::login(&cfg)?;
            logger("✅ 已登录 sub2api".to_string());
            let r = reauth::run_once(
                &mut c,
                reauth::OnceOpts {
                    gate_url: &cfg.gate_url,
                    cpa_url: &cfg.cpa_url,
                    max_ms: cfg.max_seconds.saturating_mul(1000),
                    engine: engine.as_deref(),
                    cdk: store::load()?
                        .map(|x| x.cdk)
                        .filter(|s| !s.trim().is_empty()),
                    yes,
                    only_email: only_email.as_deref(),
                },
                &logger,
            )?;
            Ok(summarize(&r, yes))
        })();

        let mut j = job.lock().unwrap();
        j.running = false;
        match res {
            Ok(s) => j.result = Some(s),
            Err(e) => j.error = Some(format!("{}", e)),
        }
    });

    Json(json!({ "ok": true }))
}

async fn api_reauth_status(State(st): State<AppState>) -> Json<Value> {
    let job = st.job.lock().unwrap();
    Json(json!({
        "running": job.running,
        "log": job.log,
        "result": job.result,
        "error": job.error,
    }))
}

fn summarize(r: &reauth::OnceResult, yes: bool) -> String {
    let mut s = String::new();
    s.push_str(&format!("重授权邮箱：{}\n", r.emails.join(", ")));
    s.push_str(&format!("结果长度：{} 字符\n", r.raw_len));
    if yes {
        let ok = r.report.outcomes.iter().filter(|o| o.ok).count();
        s.push_str(&format!(
            "写回结果：成功 {} / 失败 {}\n",
            ok,
            r.report.outcomes.len() - ok
        ));
        for o in &r.report.outcomes {
            s.push_str(&format!(
                "  #{} {} {}\n",
                o.account_id,
                o.email,
                if o.ok {
                    "成功".to_string()
                } else {
                    o.message.clone()
                }
            ));
        }
    } else {
        s.push_str(&format!(
            "（预览）匹配 {} 个账号，未写回\n",
            r.report.plans.len()
        ));
        for p in &r.report.plans {
            s.push_str(&format!("  #{} {}\n", p.account_id, p.email));
        }
    }
    if !r.report.skipped.is_empty() {
        s.push_str(&format!("跳过：{}\n", r.report.skipped.join("；")));
    }
    s
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn page_html(msg: &str, st: &AppState) -> String {
    let cur = store::load().ok().flatten();
    let cdk = cur.as_ref().map(|c| c.cdk.as_str()).unwrap_or("");
    let banner = if msg.is_empty() {
        String::new()
    } else {
        format!("<div class=\"banner\">{}</div>", esc(msg))
    };
    let cfg_warn = if st.cfg.is_none() {
        "<div class=\"banner warn\">⚠️ 未找到可用的 config.toml，一键重授权不可用（需 base_url/email/password）。</div>"
            .to_string()
    } else {
        String::new()
    };
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>sub2api 运维 · 一键重授权</title>
<style>
  body {{ font-family: -apple-system, "PingFang SC", "Microsoft YaHei", sans-serif;
         background:#f5f6f8; color:#1f2329; margin:0; padding:32px; }}
  .card {{ max-width:640px; margin:0 auto 18px; background:#fff; border-radius:12px;
          padding:24px 28px; box-shadow:0 2px 12px rgba(0,0,0,.08); }}
  h1 {{ font-size:19px; margin:0 0 4px; }}
  h2 {{ font-size:15px; margin:0 0 14px; }}
  .sub {{ color:#8a8f99; font-size:12.5px; margin:0 0 18px; }}
  label {{ display:block; font-size:13px; margin:14px 0 6px; font-weight:600; }}
  input[type=text], input[type=password] {{ width:100%; box-sizing:border-box; padding:10px 12px;
          font-size:14px; border:1px solid #d0d3d9; border-radius:8px; }}
  input:focus {{ outline:none; border-color:#3b6cff; }}
  button {{ margin-top:18px; width:100%; padding:12px; font-size:15px; color:#fff;
           background:#3b6cff; border:0; border-radius:8px; cursor:pointer; }}
  button:hover {{ background:#2f59d6; }}
  button:disabled {{ background:#a8b6e8; cursor:not-allowed; }}
  .banner {{ background:#eafaf0; color:#1a7f45; border:1px solid #b6e8c8;
            padding:10px 12px; border-radius:8px; font-size:13px; margin-bottom:10px; }}
  .banner.warn {{ background:#fff7e6; color:#a4610a; border-color:#ffd591; }}
  .row {{ display:flex; align-items:center; gap:8px; font-size:13px; margin-top:12px; font-weight:400; }}
  .row input[type=checkbox] {{ width:16px; height:16px; }}
  pre {{ background:#1e2430; color:#c9d4e5; padding:12px; border-radius:8px; font-size:12px;
        line-height:1.6; max-height:320px; overflow:auto; white-space:pre-wrap; margin:14px 0 0; }}
  .muted {{ color:#8a8f99; font-size:12px; }}
</style>
</head>
<body>
  <div class="card">
    <h1>凭证管理</h1>
    <p class="sub">CDK 本地管理 · 仅本机存储（credentials.db）</p>
    {banner}
    <form method="post" action="/save">
      <label for="cdk">CDK</label>
      <input id="cdk" type="text" name="cdk" value="{cdk}" placeholder="兑换码 / 重授权消耗" autocomplete="off">
      <button type="submit">保存</button>
    </form>
  </div>

  <div class="card">
    <h2>一键重新授权</h2>
    <p class="sub">自动找出报 401 的账号 → 无头浏览器跑接码 → 结果按邮箱写回（忽略大小写）</p>
    {cfg_warn}
    <form id="reauthForm">
      <label for="email">指定邮箱（可空 = 自动处理所有 401 账号）</label>
      <input id="email" type="text" name="email" placeholder="name@example.com" autocomplete="off">
      <div class="row">
        <input id="yes" type="checkbox" name="yes" value="1">
        <label for="yes" style="margin:0; font-weight:400;">真正写回（不勾 = 只预览不写回）</label>
      </div>
      <button id="runBtn" type="submit">🚀 一键重新授权</button>
    </form>
    <pre id="log" class="muted">尚未运行。</pre>
  </div>

<script>
const logEl = document.getElementById('log');
const btn = document.getElementById('runBtn');
let timer = null;

function render(j) {{
  let txt = '';
  if (j.log && j.log.length) txt += j.log.join('\n') + '\n';
  if (j.error) txt += '❌ ' + j.error + '\n';
  if (j.result) txt += '—— 结果 ——\n' + j.result;
  logEl.textContent = txt || '尚未运行。';
  logEl.scrollTop = logEl.scrollHeight;
  btn.disabled = !!j.running;
  btn.textContent = j.running ? '运行中…' : '🚀 一键重新授权';
}}

async function poll() {{
  const r = await fetch('/api/reauth/status');
  const j = await r.json();
  render(j);
  if (j.running) timer = setTimeout(poll, 1000);
}}

document.getElementById('reauthForm').addEventListener('submit', async (e) => {{
  e.preventDefault();
  const fd = new FormData(e.target);
  logEl.textContent = '启动中…';
  btn.disabled = true;
  await fetch('/api/reauth', {{ method: 'POST', body: new URLSearchParams(fd) }});
  clearTimeout(timer);
  poll();
}});
</script>
</body>
</html>"#,
        banner = banner,
        cfg_warn = cfg_warn,
        cdk = esc(cdk),
    )
}
