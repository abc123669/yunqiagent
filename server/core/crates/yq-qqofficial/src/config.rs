//! 官方机器人配置。
//!
//! 凭据来源（优先级从高到低）：
//!   1. 环境变量 `QQ_OPEN_APPID` / `QQ_OPEN_SECRET`
//!   2. 环境变量 `QQBOT_APP_ID` / `QQBOT_CLIENT_SECRET`（Hermes 兼容命名）
//!
//! **注意**：官方平台的 `Token` 字段已废弃，必须用 AppID + ClientSecret 换 Access Token。

use std::time::Duration;

use crate::auth::DEFAULT_TIMEOUT;
use crate::protocol::Intents;

/// 官方机器人 API 基础地址（REST）。
pub const API_BASE: &str = "https://api.sgroup.qq.com";

/// Access Token 获取地址。
pub const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// 官方机器人配置。
#[derive(Debug, Clone)]
pub struct OfficialConfig {
    /// 机器人 AppID
    pub app_id: String,
    /// 机器人 ClientSecret（不是已废弃的 Token）
    pub client_secret: String,
    /// 订阅的事件意图位图
    pub intents: u32,
    /// 是否沙箱环境（沙箱网关地址不同）
    pub sandbox: bool,
    /// HTTP 请求超时
    pub timeout: Duration,
}

impl OfficialConfig {
    /// 从环境变量构建。
    pub fn from_env() -> anyhow::Result<Self> {
        let app_id = std::env::var("QQ_OPEN_APPID")
            .or_else(|_| std::env::var("QQBOT_APP_ID"))
            .map_err(|_| anyhow::anyhow!("未找到 QQ_OPEN_APPID / QQBOT_APP_ID 环境变量"))?;

        let client_secret = std::env::var("QQ_OPEN_SECRET")
            .or_else(|_| std::env::var("QQBOT_CLIENT_SECRET"))
            .map_err(|_| anyhow::anyhow!("未找到 QQ_OPEN_SECRET / QQBOT_CLIENT_SECRET 环境变量"))?;

        let sandbox = std::env::var("QQ_OPEN_SANDBOX")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Ok(Self {
            app_id: app_id.trim().to_string(),
            client_secret: client_secret.trim().to_string(),
            // 默认订阅群聊 + C2C 私聊事件
            intents: Intents::GROUP_AND_C2C,
            sandbox,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// 显式构造。
    pub fn new(app_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            client_secret: client_secret.into(),
            intents: Intents::GROUP_AND_C2C,
            sandbox: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 设置订阅的事件意图。
    pub fn with_intents(mut self, intents: u32) -> Self {
        self.intents = intents;
        self
    }

    /// 设置为沙箱环境。
    pub fn with_sandbox(mut self, sandbox: bool) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// 校验配置。
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.app_id.trim().is_empty() {
            anyhow::bail!("app_id 为空");
        }
        if self.client_secret.trim().is_empty() {
            anyhow::bail!("client_secret 为空");
        }
        Ok(())
    }

    /// 机器人 AppID 的数值形式（心跳日志里用）。
    pub fn app_id_numeric(&self) -> Option<u64> {
        self.app_id.parse::<u64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_config_is_valid() {
        let cfg = OfficialConfig::new("1905278430", "secret-value");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.app_id_numeric(), Some(1905278430));
    }

    #[test]
    fn empty_secret_fails_validation() {
        let cfg = OfficialConfig::new("1905278430", "   ");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn with_intents_overrides_default() {
        let cfg = OfficialConfig::new("1", "s").with_intents(Intents::GROUP_ONLY);
        assert_eq!(cfg.intents, Intents::GROUP_ONLY);
    }
}
