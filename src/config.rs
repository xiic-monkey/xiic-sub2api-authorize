use serde::Deserialize;
use std::path::Path;

/// 运维工具的配置。从 config.toml 读取，绝不写死任何凭据。
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// sub2api 实例地址，不含 /api/v1 后缀
    pub base_url: String,
    /// 管理员邮箱
    pub email: String,
    /// 管理员密码
    pub password: String,
    /// 若后台开启 2FA(TOTP)，填 base32 密钥
    #[serde(default)]
    pub totp_secret: Option<String>,
    /// 是否校验证书；false 时跳过（自签证书用）。默认 true
    #[serde(default = "default_verify_tls")]
    pub verify_tls: bool,
    /// 接码门页地址（一键重授权用）
    #[serde(default = "default_gate_url")]
    pub gate_url: String,
    /// CPA/Sub2API 转换页：把接码结果转成 sub2api 凭证（含 refresh_token）
    #[serde(default = "default_cpa_url")]
    pub cpa_url: String,
    /// 浏览器轮询最长等待秒数（一键重授权用）
    #[serde(default = "default_max_seconds")]
    pub max_seconds: u64,

    // ---- 非 CDK 授权路径（走 sub2api 自带授权链接 + 收码站取验证码）----
    /// 收码站地址（如 https://mail.kyon888.xyz）
    #[serde(default)]
    pub mail_base_url: String,
    /// 收码站用户名
    #[serde(default)]
    pub mail_username: String,
    /// 收码站密码
    #[serde(default)]
    pub mail_password: String,
    /// 非 CDK 授权是否使用有头窗口（Cloudflare 升级为交互式质询时可人工点一下）
    #[serde(default)]
    pub openai_headed: bool,
    /// 非 CDK 单账号授权最长等待秒数
    #[serde(default = "default_openai_seconds")]
    pub openai_max_seconds: u64,
}

/// 组装收码站配置（供 mail 模块直接用）。
impl Config {
    pub fn mail_config(&self) -> crate::mail::MailConfig {
        crate::mail::MailConfig {
            base_url: self.mail_base_url.clone(),
            username: self.mail_username.clone(),
            password: self.mail_password.clone(),
        }
    }
}

fn default_openai_seconds() -> u64 {
    240
}

fn default_verify_tls() -> bool {
    true
}

fn default_gate_url() -> String {
    "https://401.kyon888.xyz".to_string()
}

fn default_cpa_url() -> String {
    "https://zh.kyon888.xyz/CPAandSub2API/".to_string()
}

fn default_max_seconds() -> u64 {
    150
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取配置文件 {} 失败: {}", path.display(), e))?;
        let cfg: Config =
            toml::from_str(&raw).map_err(|e| anyhow::anyhow!("解析配置失败: {}", e))?;
        if cfg.base_url.is_empty() || cfg.email.is_empty() || cfg.password.is_empty() {
            anyhow::bail!("config.toml 缺少 base_url / email / password");
        }
        Ok(cfg)
    }
}
