use clap::{Parser, Subcommand};

use sub2api_operator::{browser, client, commands, config, store, web};

#[derive(Parser)]
#[command(name = "sub2op", about = "sub2api 外部运维工具（不修改上游，纯 admin API 驱动）")]
struct Cli {
    /// 配置文件路径
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// 浏览器引擎：`chrome`=唤醒本机 Google Chrome；缺省=自动探测 chromium/chrome（服务器装 chromium 即可）
    #[arg(long, global = true)]
    browser: Option<String>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 测试登录，打印 token 前缀
    Login,
    /// 账号相关
    Accounts {
        #[command(subcommand)]
        action: AccountsCmd,
    },
    /// 检测/修复需要重授权的 OAuth 账号
    Reauth {
        #[command(subcommand)]
        action: ReauthCmd,
    },
    /// CDK 本地 SQLite 管理（重授权流程用）
    Creds {
        #[command(subcommand)]
        action: CredsCmd,
    },
    /// 无头浏览器自动化（自动检测系统 Chromium/Chrome 引擎）
    Browser {
        #[command(subcommand)]
        action: BrowserCmd,
    },
}

#[derive(Subcommand)]
enum AccountsCmd {
    /// 列出账号。默认全部 + 汇总；`--401` 只列报 401 的账号；`--emails` 只逐行打邮箱
    List {
        /// 只显示 error_message 含 "401" 的账号
        #[arg(long = "401")]
        r401: bool,
        /// 只逐行打印账号邮箱（name 字段），跳过表格与汇总
        #[arg(long = "emails")]
        emails: bool,
    },
}

#[derive(Subcommand)]
enum ReauthCmd {
    /// 检测需要重授权的 OAuth 账号
    Detect,
    /// 把 CPA 输出（sub2api 凭证，含 rt）按**邮箱**写回账号（默认 dry-run，支持多个结果批量）
    Apply {
        /// 只写回该 sub2api 账号 id（不填则按结果里的邮箱自动匹配）
        #[arg(long)]
        account: Option<i64>,
        /// 只写回该邮箱对应的账号（忽略大小写）
        #[arg(long)]
        email: Option<String>,
        /// 真正发送写回（默认仅预览，不发送）
        #[arg(long)]
        yes: bool,
        /// 从 JSON 文件读结果（优先于剪切板）
        #[arg(long)]
        from: Option<String>,
    },
}

#[derive(Subcommand)]
enum CredsCmd {
    /// 启动本地凭证管理页（http://127.0.0.1:7878），浏览器录入后写入 credentials.db
    Web,
    /// 在终端打印当前已存凭证（密码脱敏）
    Show,
}

#[derive(Subcommand)]
enum BrowserCmd {
    /// 打开 URL 并截图（验证自动检测 + 冒烟测试）
    Open {
        /// 目标 URL
        url: String,
        /// 截图输出路径
        #[arg(short, long, default_value = "/tmp/sub2op-shot.png")]
        out: String,
    },
    /// 接码平台门页流程：检测 未进入/已进入 状态，未进入时填 CDK 并点进入
    Gate {
        /// 门页 URL
        url: String,
        /// 直接给 CDK（不填则从 credentials.db 读取）
        #[arg(long)]
        cdk: Option<String>,
        /// 截图/HTML 输出目录
        #[arg(long = "out-dir", default_value = "/tmp")]
        out_dir: String,
    },
    /// 获取令牌（重授权）→ 转 sub2api 凭证：确保已进入 → 填邮箱 → 点获取 → 每 5s 轮询 →
    /// 点复制全部取剪切板 → 打开 CPA 页贴 #session-input → 读 #output 的 sub2api 凭证（含 rt）
    Fetch {
        /// 门页 URL
        url: String,
        /// 邮箱清单文件（每行一个邮箱）
        #[arg(long = "emails-file")]
        emails_file: String,
        /// 直接给 CDK（不填则从 credentials.db 读取）
        #[arg(long)]
        cdk: Option<String>,
        /// 最长等待毫秒（默认 150000，即 2.5 分钟）
        #[arg(long, default_value_t = 150000)]
        max: u64,
        /// 完成后打开的页（默认老板记下的 CPA/Sub2API 入口）
        #[arg(long = "then-open")]
        then_open: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let engine = cli.browser.clone();
    let engine = engine.as_deref();
    let cfg = config::Config::load(std::path::Path::new(&cli.config))?;

    match cli.command {
        Cmd::Login => {
            let c = client::Sub2ApiClient::login(&cfg)?;
            let head = c.token_prefix();
            println!("✅ 登录成功。access_token 前缀: {}", head);
        }
        Cmd::Accounts { action } => {
            let mut c = client::Sub2ApiClient::login(&cfg)?;
            match action {
                AccountsCmd::List { r401, emails } => {
                    commands::accounts::list(&mut c, r401, emails)?
                }
            }
        }
        Cmd::Reauth { action } => {
            let mut c = client::Sub2ApiClient::login(&cfg)?;
            match action {
                ReauthCmd::Detect => commands::reauth::detect(&mut c)?,
                ReauthCmd::Apply {
                    account,
                    email,
                    yes,
                    from,
                } => commands::reauth::apply(
                    &mut c,
                    commands::reauth::ApplyOpts {
                        yes,
                        from: from.as_deref(),
                        only_account: account,
                        only_email: email.as_deref(),
                    },
                )?,
            }
        }
        Cmd::Creds { action } => match action {
            CredsCmd::Web => {
                tokio::runtime::Runtime::new()?.block_on(web::run(engine))?;
            }
            CredsCmd::Show => match store::load()? {
                Some(c) => println!("CDK={}\n更新于={}", c.cdk, c.updated_at),
                None => println!("（credentials.db 还没有任何凭证，先跑 `creds web` 录入）"),
            },
        },
        Cmd::Browser { action } => match action {
            BrowserCmd::Open { url, out } => {
                browser::open_and_shoot(&url, std::path::Path::new(&out), engine)?;
            }
            BrowserCmd::Gate { url, cdk, out_dir } => {
                // CDK 优先级：命令行 --cdk > credentials.db
                let cdk = match cdk {
                    Some(c) => Some(c),
                    None => store::load()?.map(|c| c.cdk).filter(|s| !s.is_empty()),
                };
                if cdk.is_none() {
                    println!("⚠️ 未提供 CDK 且 credentials.db 为空：仅做门页状态检测，不填表/不点击");
                }
                browser::gate(&url, cdk.as_deref(), std::path::Path::new(&out_dir), engine)?;
                println!("✅ 门页流程完成。截图与 HTML 在：{}", out_dir);
            }
            BrowserCmd::Fetch {
                url,
                emails_file,
                cdk,
                max,
                then_open,
            } => {
                // CDK 优先级：命令行 --cdk > credentials.db
                let cdk = match cdk {
                    Some(c) => Some(c),
                    None => store::load()?.map(|c| c.cdk).filter(|s| !s.is_empty()),
                };
                if cdk.is_none() {
                    println!("⚠️ 未提供 CDK 且 credentials.db 为空：门页将无法进入");
                }
                browser::fetch(
                    &url,
                    std::path::Path::new(&emails_file),
                    cdk.as_deref(),
                    max,
                    then_open.as_deref(),
                    engine,
                )?;
                println!("✅ 获取流程结束。结果已从剪切板读出，并完成 CPA 页探测（见上方 JSON）");
            }
        },
    }
    Ok(())
}
