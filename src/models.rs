use serde::Deserialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// 账号（对应 sub2api DTO 的 Account，只保留运维需要的字段）。
///
/// 真实字段名来自 backend/internal/handler/dto/types.go:
/// - status: string（"active"/"error"/"paused"/"expired" ...）
/// - error_message: string（上游报错就在这里，401 藏在里面）
/// - expires_at: *int64（epoch 秒或毫秒）
/// - schedulable: bool
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(rename = "type", default)]
    pub account_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, rename = "error_message")]
    pub error_message: String,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub schedulable: bool,
    /// 调度优先级（sub2api 原生字段，越小越优先）。缺失则为 None。
    #[serde(default, rename = "priority")]
    pub priority: Option<i64>,
    /// 原始 OAuth 凭证（apply 写回时作兜底，缺字段时保留现有值）。缺失则为 None。
    #[serde(default)]
    pub credentials: Option<Value>,
    /// 账号扩展信息（apply 写回时原样保留，避免丢用户设置）。缺失则为 None。
    #[serde(default)]
    pub extra: Option<Value>,
}

impl Account {
    /// 是否为 OAuth 类型账号
    pub fn is_oauth(&self) -> bool {
        self.account_type.eq_ignore_ascii_case("oauth")
    }

    /// 是否有上游错误标记
    pub fn has_error(&self) -> bool {
        !self.error_message.trim().is_empty()
    }

    /// 错误里是否含 401（上游鉴权失效的典型信号）
    pub fn has_401(&self) -> bool {
        self.error_message.contains("401")
    }

    /// OAuth 且已有上游报错 → 需要重授权
    pub fn needs_reauth(&self) -> bool {
        self.is_oauth() && self.has_error()
    }

    /// expires_at 距今天数（正数=还有 N 天，负数=已过期 N 天）。None=无该字段。
    pub fn expires_in_days(&self) -> Option<i64> {
        let raw = self.expires_at?;
        // 秒（~1.7e9）与毫秒（~1.7e12）区分：大于 1e12 视为毫秒
        let ms = if raw > 1_000_000_000_000 { raw } else { raw * 1000 };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Some((ms - now) / 86_400_000)
    }
}
