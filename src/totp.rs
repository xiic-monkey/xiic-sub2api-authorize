use anyhow::{anyhow, Result};
use totp_rs::{Algorithm, Secret, TOTP};

/// 用 base32 密钥算当前 TOTP 码（默认 SHA1 / 6 位 / 30s 步长，匹配 Google Authenticator）。
pub fn generate_current(secret_base32: &str) -> Result<String> {
    let secret = Secret::Encoded(secret_base32.trim().to_uppercase());
    let raw = secret
        .to_bytes()
        .map_err(|e| anyhow!("无效的 TOTP 密钥: {:?}", e))?;
    let totp = TOTP::new(Algorithm::SHA1, 6, 1, 30, raw)
        .map_err(|e| anyhow!("初始化 TOTP 失败: {}", e))?;
    Ok(totp.generate_current()?)
}
