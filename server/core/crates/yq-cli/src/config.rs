//! 应用配置 —— 照 Hermes 的章法：配置与代码分离，住 `~/.yunqiagent/config.toml`。
//!
//! 设计要求：
//!   · 密钥**绝不进项目目录**，只住用户家目录下的配置文件（权限 600）
//!   · 文件缺失时回退环境变量，保证 CI / 临时调试也能跑
//!   · 解析失败**不静默吞掉**，而是告警 + 回退，避免"配置写错却毫无提示"

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use yq_llm::LlmConfig;

/// 数据库配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL 连接串
    pub url: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            // 占位默认值；实际凭据请通过 `yunqiagent config --init` 写入
            // ~/.yunqiagent/config.toml 的 [database].url，或用 DATABASE_URL 环境变量覆盖。
            url: "postgres://<用户名>:<密码>@127.0.0.1:5432/yunqiagent".to_string(),
        }
    }
}

/// QQ / OneBot11 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QqConfig {
    /// 连接模式：forward | reverse
    pub mode: String,
    /// 端点
    pub endpoint: String,
    /// 心跳间隔（如 "30m"，"0m" 禁用）
    pub heartbeat: String,
    /// 心跳投递目标群号（可选）
    pub heartbeat_group: Option<i64>,
}

impl Default for QqConfig {
    fn default() -> Self {
        Self {
            mode: "reverse".to_string(),
            endpoint: "0.0.0.0:6700".to_string(),
            heartbeat: "30m".to_string(),
            heartbeat_group: None,
        }
    }
}

/// QQ **开放平台官方机器人**配置。
///
/// 与 `QqConfig`（OneBot11 / NapCat）是两条**并列**的接入路径：
///   · OneBot11 —— 第三方客户端，需要跑 QQ 桌面端
///   · 官方平台  —— 腾讯服务端 API，无需客户端
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QqOfficialConfig {
    /// 机器人 AppID
    pub app_id: String,
    /// ClientSecret（**不是**已废弃的 Token）
    pub client_secret: String,
    /// 是否沙箱环境
    #[serde(default)]
    pub sandbox: bool,
}

impl QqOfficialConfig {
    /// 是否有可用凭据。
    pub fn is_configured(&self) -> bool {
        !self.app_id.trim().is_empty() && !self.client_secret.trim().is_empty()
    }

    /// 转换为 `yq-qqofficial` 的配置。
    pub fn to_official(&self) -> yq_qqofficial::OfficialConfig {
        yq_qqofficial::OfficialConfig::new(&self.app_id, &self.client_secret)
            .with_sandbox(self.sandbox)
    }
}

/// 顶层应用配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// 数据库
    pub database: DatabaseConfig,
    /// 模型层
    pub llm: LlmConfig,
    /// QQ 接入（OneBot11）
    #[serde(default)]
    pub qq: QqConfig,
    /// QQ 官方机器人接入
    #[serde(default)]
    pub qq_official: QqOfficialConfig,
    /// Kenkin Chat 接入（好友自研聊天软件，OneBot v11 反向 WS）
    #[serde(default)]
    pub kenkin: KenkinConfig,
}

/// Kenkin Chat 接入配置。
///
/// KEN 是 OICQ 生态的 OneBot v11 服务端：
///   · 我们**主动连**它的反向 WS 端点（如 `/bot/v11/ws/3`）
///   · 用 `Authorization: Bearer <token>` 鉴权
///   · **路径编号 = 机器人槽位**，同编号后到踢先到，不同编号可同时在线
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KenkinConfig {
    /// 是否启用
    #[serde(default)]
    pub enabled: bool,
    /// 机器人名称（仅用于日志/展示）
    #[serde(default = "default_kenkin_name")]
    pub name: String,
    /// 接入令牌（KEN 平台发放）
    #[serde(default)]
    pub token: String,
    /// 反向 WS 端点。**建议带路径编号**（如 `/bot/v11/ws/3`）——
    /// 不带编号会让服务端把你路由到默认槽位，容易与别的客户端互踢。
    #[serde(default = "default_kenkin_ws")]
    pub ws_url: String,
}

fn default_kenkin_name() -> String {
    "YQBot".to_string()
}

fn default_kenkin_ws() -> String {
    "wss://chat.kenkin.top:65533/bot/v11/ws".to_string()
}

impl Default for KenkinConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            name: default_kenkin_name(),
            token: String::new(),
            ws_url: default_kenkin_ws(),
        }
    }
}

impl KenkinConfig {
    /// 配置是否完整可用。
    pub fn is_ready(&self) -> bool {
        self.enabled && !self.token.trim().is_empty() && !self.ws_url.trim().is_empty()
    }
}

impl AppConfig {
    /// yunqiagent 配置目录：`~/.yunqiagent`。
    pub fn home_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".yunqiagent")
    }

    /// 配置文件路径：`~/.yunqiagent/config.toml`。
    pub fn default_path() -> PathBuf {
        Self::home_dir().join("config.toml")
    }

    /// 加载配置：优先配置文件，缺失/失败时回退环境变量。
    ///
    /// 回退顺序：
    ///   1. `~/.yunqiagent/config.toml`（若存在）
    ///   2. 环境变量（`DATABASE_URL` / `YQ_LLM_*`）
    pub fn load() -> Self {
        Self::load_from(&Self::default_path())
    }

    /// 从指定路径加载。
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<AppConfig>(&text) {
                Ok(mut cfg) => {
                    tracing::info!("已加载配置: {}", path.display());
                    // 允许环境变量覆盖个别项（便于临时切换模型）
                    cfg.apply_env_overrides();
                    cfg
                }
                Err(e) => {
                    tracing::warn!(
                        "配置解析失败（{}），回退环境变量。请检查 {} 的 TOML 语法",
                        e,
                        path.display()
                    );
                    Self::from_env()
                }
            },
            Err(_) => {
                tracing::debug!("未找到 {}，使用环境变量配置", path.display());
                Self::from_env()
            }
        }
    }

    /// 全部来自环境变量。
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(url) = std::env::var("DATABASE_URL") {
            if !url.trim().is_empty() {
                cfg.database.url = url.trim().to_string();
            }
        }

        cfg.llm = LlmConfig::from_env();
        cfg.apply_env_overrides();
        cfg
    }

    /// 环境变量覆盖（用于临时调试，不落盘）。
    fn apply_env_overrides(&mut self) {
        if let Ok(url) = std::env::var("DATABASE_URL") {
            if !url.trim().is_empty() {
                self.database.url = url.trim().to_string();
            }
        }
        if let Ok(m) = std::env::var("YQ_LLM_MODEL") {
            if !m.trim().is_empty() {
                self.llm.model = m.trim().to_string();
            }
        }
        if let Ok(k) = std::env::var("YQ_LLM_API_KEY") {
            if !k.trim().is_empty() {
                self.llm.api_key = k.trim().to_string();
            }
        }
        if let Ok(b) = std::env::var("YQ_LLM_BASE_URL") {
            if !b.trim().is_empty() {
                self.llm.base_url = b.trim().to_string();
            }
        }
    }

    /// 校验配置可用性。
    pub fn validate(&self) -> Result<()> {
        if self.database.url.trim().is_empty() {
            anyhow::bail!("database.url 为空");
        }
        self.llm
            .validate()
            .context("LLM 配置校验失败（检查 config.toml 的 [llm] 段）")?;
        Ok(())
    }

    /// 生成默认配置文件内容（首次运行时写盘）。
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|_| String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let mut cfg = AppConfig::default();
        cfg.llm.api_key = "test-key".to_string();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn missing_api_key_fails_validation() {
        let cfg = AppConfig::default();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn roundtrip_toml() {
        let mut cfg = AppConfig::default();
        cfg.llm.api_key = "roundtrip-key".to_string();
        cfg.llm.model = "deepseek-chat".to_string();
        let text = cfg.to_toml_string();
        let parsed: AppConfig = toml::from_str(&text).expect("应能解析回来");
        assert_eq!(parsed.llm.model, "deepseek-chat");
        assert_eq!(parsed.llm.api_key, "roundtrip-key");
        assert_eq!(parsed.database.url, cfg.database.url);
    }

    #[test]
    fn nonexistent_path_falls_back_to_env() {
        let cfg = AppConfig::load_from(Path::new("/nonexistent/path/config.toml"));
        // 回退成功即可（不 panic）
        assert!(!cfg.database.url.is_empty());
    }
}
