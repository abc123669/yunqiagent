//! Access Token 管理 —— 获取与**自动刷新**。
//!
//! ## 为什么不能用静态 Token
//!
//! QQ 开放平台已**废弃**开发者后台里那个静态 `Token` 字段。正确流程是：
//!
//! ```text
//! POST https://bots.qq.com/app/getAppAccessToken
//! {"appId":"...","clientSecret":"..."}
//! → {"access_token":"...","expires_in":"7200"}
//! ```
//!
//! Token **7200 秒（2 小时）过期**，必须在使用前刷新。
//! 本模块在过期前 60 秒自动续期，并在并发场景下保证**只刷一次**
//! （多个任务同时要 token 不会打出一堆请求）。

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::{OfficialConfig, TOKEN_URL};

/// 提前刷新窗口（秒）—— 快过期了就提前换新的。
const REFRESH_MARGIN_SECS: i64 = 60;

/// Token 接口响应。
#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    /// 访问令牌
    #[serde(default)]
    access_token: String,
    /// 有效期（秒）；官方返回的是**字符串**形式
    #[serde(default)]
    expires_in: serde_json::Value,
    /// 错误码（失败时）
    #[serde(default)]
    code: Option<i64>,
    /// 错误信息（失败时）
    #[serde(default)]
    message: Option<String>,
}

impl TokenResponse {
    /// 解析有效期秒数（兼容字符串 / 数字两种返回）。
    fn expires_secs(&self) -> i64 {
        match &self.expires_in {
            serde_json::Value::String(s) => s.trim().parse::<i64>().unwrap_or(7200),
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(7200),
            _ => 7200,
        }
    }
}

/// 缓存的令牌。
#[derive(Debug, Clone)]
struct CachedToken {
    /// 令牌字符串
    value: String,
    /// 过期时间点
    expires_at: DateTime<Utc>,
}

impl CachedToken {
    /// 是否仍然可用（留出提前刷新窗口）。
    fn is_fresh(&self) -> bool {
        Utc::now() + chrono::Duration::seconds(REFRESH_MARGIN_SECS) < self.expires_at
    }
}

/// 令牌错误。
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// HTTP 传输失败
    #[error("请求 Access Token 失败: {0}")]
    Http(#[from] reqwest::Error),
    /// 服务端返回了业务错误
    #[error("Access Token 接口返回错误 code={code:?}: {message}")]
    Api {
        /// 错误码
        code: Option<i64>,
        /// 错误信息
        message: String,
    },
    /// 响应里没有 token
    #[error("Access Token 响应缺少 access_token")]
    Empty,
}

/// 令牌管理器（线程安全，可克隆共享）。
#[derive(Clone)]
pub struct TokenManager {
    http: reqwest::Client,
    app_id: Arc<str>,
    client_secret: Arc<str>,
    cached: Arc<RwLock<Option<CachedToken>>>,
}

impl TokenManager {
    /// 由配置创建。
    pub fn new(config: &OfficialConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(config.timeout).build()?;
        Ok(Self {
            http,
            app_id: Arc::from(config.app_id.as_str()),
            client_secret: Arc::from(config.client_secret.as_str()),
            cached: Arc::new(RwLock::new(None)),
        })
    }

    /// 取一个可用的 Access Token（必要时刷新）。
    pub async fn token(&self) -> Result<String, TokenError> {
        // 快路径：读锁命中未过期的令牌
        {
            let guard = self.cached.read().await;
            if let Some(cached) = guard.as_ref() {
                if cached.is_fresh() {
                    return Ok(cached.value.clone());
                }
            }
        }

        // 慢路径：写锁内刷新（保证并发只刷一次）
        let mut guard = self.cached.write().await;
        // 双重检查：等锁期间可能已被别的任务刷新
        if let Some(cached) = guard.as_ref() {
            if cached.is_fresh() {
                return Ok(cached.value.clone());
            }
        }

        let fresh = self.fetch_token().await?;
        let value = fresh.value.clone();
        *guard = Some(fresh);
        Ok(value)
    }

    /// 强制刷新（收到 401/鉴权失败时调用）。
    pub async fn force_refresh(&self) -> Result<String, TokenError> {
        let mut guard = self.cached.write().await;
        let fresh = self.fetch_token().await?;
        let value = fresh.value.clone();
        *guard = Some(fresh);
        Ok(value)
    }

    /// 实际请求令牌。
    async fn fetch_token(&self) -> Result<CachedToken, TokenError> {
        let body = serde_json::json!({
            "appId": self.app_id.as_ref(),
            "clientSecret": self.client_secret.as_ref(),
        });

        let resp = self
            .http
            .post(TOKEN_URL)
            .json(&body)
            .send()
            .await?
            .json::<TokenResponse>()
            .await?;

        if let Some(code) = resp.code {
            // 官方失败时返回 {code, message}
            if code != 0 {
                return Err(TokenError::Api {
                    code: Some(code),
                    message: resp
                        .message
                        .clone()
                        .unwrap_or_else(|| "(无错误信息)".to_string()),
                });
            }
        }

        if resp.access_token.trim().is_empty() {
            return Err(TokenError::Empty);
        }

        let ttl = resp.expires_secs().max(60);
        tracing::info!("已获取 QQ Access Token（有效期 {} 秒）", ttl);

        Ok(CachedToken {
            value: resp.access_token,
            expires_at: Utc::now() + chrono::Duration::seconds(ttl),
        })
    }

    /// 授权头值：`QQBot {access_token}`。
    ///
    /// **注意**：不是 `Bearer`，也不是已废弃的 `Bot {appid}.{token}`。
    pub async fn auth_header(&self) -> Result<String, TokenError> {
        Ok(format!("QQBot {}", self.token().await?))
    }
}

/// 默认 HTTP 请求超时。
///
/// [`crate::config::OfficialConfig`] 的默认超时以此为准，
/// 避免同一个 30 秒在两处各写一遍、改一处忘另一处。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 供测试：构造一个已预置令牌的管理器（不发网络请求）。
#[cfg(test)]
pub(crate) fn manager_with_token(
    app_id: &str,
    secret: &str,
    token: &str,
    ttl_secs: i64,
) -> TokenManager {
    let http = reqwest::Client::new();
    TokenManager {
        http,
        app_id: Arc::from(app_id),
        client_secret: Arc::from(secret),
        cached: Arc::new(RwLock::new(Some(CachedToken {
            value: token.to_string(),
            expires_at: Utc::now() + chrono::Duration::seconds(ttl_secs),
        }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_token_is_fresh_when_plenty_of_time_left() {
        let t = CachedToken {
            value: "x".into(),
            expires_at: Utc::now() + chrono::Duration::seconds(7200),
        };
        assert!(t.is_fresh());
    }

    #[test]
    fn cached_token_is_stale_inside_refresh_margin() {
        // 只剩 30 秒，小于 60 秒提前窗口 → 应视为过期
        let t = CachedToken {
            value: "x".into(),
            expires_at: Utc::now() + chrono::Duration::seconds(30),
        };
        assert!(!t.is_fresh());
    }

    #[test]
    fn cached_token_is_stale_when_already_expired() {
        let t = CachedToken {
            value: "x".into(),
            expires_at: Utc::now() - chrono::Duration::seconds(10),
        };
        assert!(!t.is_fresh());
    }

    #[test]
    fn expires_secs_parses_string_form() {
        let r = TokenResponse {
            access_token: "t".into(),
            expires_in: serde_json::json!("7200"),
            code: None,
            message: None,
        };
        assert_eq!(r.expires_secs(), 7200);
    }

    #[test]
    fn expires_secs_parses_number_form() {
        let r = TokenResponse {
            access_token: "t".into(),
            expires_in: serde_json::json!(3600),
            code: None,
            message: None,
        };
        assert_eq!(r.expires_secs(), 3600);
    }

    #[test]
    fn expires_secs_defaults_on_garbage() {
        let r = TokenResponse {
            access_token: "t".into(),
            expires_in: serde_json::json!("not-a-number"),
            code: None,
            message: None,
        };
        assert_eq!(r.expires_secs(), 7200);
    }

    #[tokio::test]
    async fn auth_header_uses_qqbot_prefix() {
        let mgr = manager_with_token("123", "sec", "TOKEN123", 3600);
        let h = mgr.auth_header().await.unwrap();
        assert_eq!(h, "QQBot TOKEN123");
        assert!(!h.starts_with("Bearer"));
        assert!(!h.starts_with("Bot "));
    }

    #[tokio::test]
    async fn token_returns_cached_value_without_network() {
        let mgr = manager_with_token("123", "sec", "CACHED", 3600);
        assert_eq!(mgr.token().await.unwrap(), "CACHED");
    }
}
