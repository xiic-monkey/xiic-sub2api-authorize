# sub2api-operator

sub2api（订阅转换网关）的**外部运维工具**：不二开、不爬页面，纯靠 admin API + 无头浏览器完成 OAuth 账号的重授权。

两种用法：
- **服务器 / CLI**：本仓库的 `sub2api-operator` 二进制（`cargo build --release`，也提供 Dockerfile）。
- **本地 GUI**：配套桌面端 [`xiic-sub2api-auth-desktop`](https://github.com/xiic-monkey/xiic-sub2api-auth-desktop)（Tauri 2），
  以 path 依赖复用本仓库的 lib，同一套核心逻辑。

## 一键重新授权（web 页面）

```bash
cp config.example.toml config.toml   # 填 base_url / email / password
cargo run -- creds web               # 打开 http://127.0.0.1:7878
```

页面上填 CDK 保存（写入 `credentials.db`），然后点「一键重新授权」：

1. 登录 sub2api，拉全量账号，筛出报 401 的（也可指定单个邮箱）；
2. 无头浏览器走接码门页：填 CDK 进入 → 填邮箱 → 点获取 → 每 5 秒轮询 → 取结果 →
   打开 CPA 转换页转成 sub2api 凭证（含 refresh_token）；
3. 结果**按邮箱忽略大小写**匹配账号，写回 `POST /api/v1/admin/accounts/:id/apply-oauth-credentials`。

不勾选「真正写回」时为 dry-run，只预览不写回。进度在页面日志区实时滚动。

## 常用命令

```bash
cargo run -- login                      # 验证登录
cargo run -- accounts list              # 列出账号
cargo run -- accounts list --401        # 只看报 401 的
cargo run -- reauth detect              # 检测需要重授权的账号
cargo run -- browser fetch <门页URL> --emails-file emails.txt   # 单跑接码流程
```

浏览器引擎：默认用 playwright 内置 Chromium（服务器/Docker 适用）；
加 `--browser chrome` 直接唤醒本机已装的 Google Chrome，不下载 Chromium。

## 配置

`config.toml`（已 gitignore，别提交）：

| 键 | 说明 |
| --- | --- |
| `base_url` / `email` / `password` | sub2api 实例地址与管理员账号 |
| `totp_secret` | 后台开了 2FA 才填（base32） |
| `verify_tls` | 自签证书设 `false` |
| `gate_url` | 接码门页地址 |
| `cpa_url` | CPA/Sub2API 转换页 |
| `max_seconds` | 浏览器轮询最长等待秒数 |

## 部署

```bash
docker build -t sub2api-operator .
docker run --rm -v $(pwd)/config.toml:/app/config.toml sub2api-operator -- accounts list
```

## 许可

MIT
