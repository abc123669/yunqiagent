//! REST API —— 发送消息、查询机器人信息。
//!
//! 权威端点（QQ 开放平台 v2）：
//!
//! | 用途 | 方法 | 路径 |
//! |---|---|---|
//! | 发群消息 | POST | `/v2/groups/{group_openid}/messages` |
//! | 发私聊 | POST | `/v2/users/{user_openid}/messages` |
//! | 机器人信息 | GET | `/users/@me` |
//! | 网关地址 | GET | `/gateway/bot` |
//!
//! ## 关键坑
//!
//! - **`group_openid` 不是 QQ 群号** —— 它是事件里带回来的编码串，
//!   只能从收到的消息事件中获取（无法用群号反查）。
//! - **鉴权头是 `QQBot {access_token}`**，不是 `Bearer`。
//! - 被动回复在 **5 分钟**内有效；主动推送受频控限制。

use serde::{Deserialize, Serialize};

use crate::auth::{TokenError, TokenManager};
use crate::config::{API_BASE, OfficialConfig};

/// REST 调用错误。
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 令牌获取失败
    #[error("令牌错误: {0}")]
    Token(#[from] TokenError),
    /// HTTP 传输失败
    #[error("HTTP 请求失败: {0}")]
    Http(#[from] reqwest::Error),
    /// 服务端业务错误
    #[error("API 返回错误 code={code:?}: {message}")]
    Api {
        /// 错误码
        code: Option<i64>,
        /// 错误信息
        message: String,
    },
    /// 群 openid 缺失
    #[error("group_openid 为空：官方 API 需要从消息事件中获取，不能使用 QQ 群号")]
    MissingGroupOpenid,
}

/// 服务端通用响应（成功或失败）。
#[derive(Debug, Clone, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    id: Option<String>,
    /// 服务端时间戳（保留字段：反序列化兼容，暂未使用）
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: Option<serde_json::Value>,
}

impl ApiResponse {
    /// 检查业务错误。
    fn check(self) -> Result<Self, ApiError> {
        if let Some(code) = self.code {
            if code != 0 {
                return Err(ApiError::Api {
                    code: Some(code),
                    message: self
                        .message
                        .clone()
                        .unwrap_or_else(|| "(无错误信息)".to_string()),
                });
            }
        }
        Ok(self)
    }

    /// 消息 ID（发送成功后返回）。
    fn message_id(&self) -> Option<String> {
        self.id.clone()
    }
}

/// 发送消息的请求体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageBody {
    /// 文本内容（`content` 与 `media` 二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 富媒体（图片/文件）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<serde_json::Value>,
    /// 被动回复时携带的消息 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    /// 消息序号（防重复，1~10000）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<u32>,
}

impl SendMessageBody {
    /// 纯文本消息（主动推送）。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            media: None,
            msg_id: None,
            msg_seq: None,
        }
    }

    /// 被动回复（携带原消息 ID + 随机序号防重复）。
    pub fn reply(content: impl Into<String>, msg_id: impl Into<String>, msg_seq: u32) -> Self {
        Self {
            content: Some(content.into()),
            media: None,
            msg_id: Some(msg_id.into()),
            msg_seq: Some(msg_seq.clamp(1, 10_000)),
        }
    }
}

/// 机器人信息。
#[derive(Debug, Clone, Deserialize)]
pub struct BotIdentity {
    /// 机器人 ID
    #[serde(default)]
    pub id: String,
    /// 机器人名称
    #[serde(default)]
    pub username: String,
    /// 分享链接（含 robot_uin，可用于重新邀请机器人进群）
    #[serde(default)]
    pub share_url: String,
}

/// 官方 REST API 客户端。
#[derive(Clone)]
pub struct OfficialApi {
    http: reqwest::Client,
    tokens: TokenManager,
    base: String,
}

impl OfficialApi {
    /// 由配置创建。
    pub fn new(config: &OfficialConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(config.timeout).build()?;
        Ok(Self {
            http,
            tokens: TokenManager::new(config)?,
            base: API_BASE.to_string(),
        })
    }

    /// 供测试：注入自定义 base 与令牌管理器。
    #[cfg(test)]
    pub(crate) fn with_parts(base: impl Into<String>, tokens: TokenManager) -> Self {
        Self {
            http: reqwest::Client::new(),
            tokens,
            base: base.into(),
        }
    }

    /// 带鉴权的 Authorization 头值。
    pub async fn auth_header(&self) -> Result<String, ApiError> {
        Ok(self.tokens.auth_header().await?)
    }

    /// 发送群消息。
    ///
    /// `group_openid` 必须是**事件里带回来的**那个编码串，不是 QQ 群号。
    pub async fn send_group_message(
        &self,
        group_openid: &str,
        body: &SendMessageBody,
    ) -> Result<Option<String>, ApiError> {
        if group_openid.trim().is_empty() {
            return Err(ApiError::MissingGroupOpenid);
        }

        let url = format!(
            "{}/v2/groups/{}/messages",
            self.base.trim_end_matches('/'),
            group_openid
        );

        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header().await?)
            .json(body)
            .send()
            .await?
            .json::<ApiResponse>()
            .await?;

        Ok(resp.check()?.message_id())
    }

    /// 发送 C2C 私聊消息。
    pub async fn send_c2c_message(
        &self,
        user_openid: &str,
        body: &SendMessageBody,
    ) -> Result<Option<String>, ApiError> {
        let url = format!(
            "{}/v2/users/{}/messages",
            self.base.trim_end_matches('/'),
            user_openid
        );

        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header().await?)
            .json(body)
            .send()
            .await?
            .json::<ApiResponse>()
            .await?;

        Ok(resp.check()?.message_id())
    }

    /// 查询机器人自身信息（`GET /users/@me`）。
    pub async fn bot_identity(&self) -> Result<BotIdentity, ApiError> {
        let url = format!("{}/users/@me", self.base.trim_end_matches('/'));

        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header().await?)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError::Api {
                code: Some(status.as_u16() as i64),
                message: yq_core::truncate_ellipsis(&text, 300),
            });
        }

        Ok(resp.json::<BotIdentity>().await?)
    }

    /// 获取 WebSocket 网关地址（`GET /gateway/bot`）。
    pub async fn gateway_url(&self) -> Result<String, ApiError> {
        #[derive(Deserialize)]
        struct GatewayResp {
            #[serde(default)]
            url: String,
            #[serde(default)]
            code: Option<i64>,
            #[serde(default)]
            message: Option<String>,
        }

        let url = format!("{}/gateway/bot", self.base.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header().await?)
            .send()
            .await?
            .json::<GatewayResp>()
            .await?;

        if let Some(code) = resp.code {
            if code != 0 {
                return Err(ApiError::Api {
                    code: Some(code),
                    message: resp.message.unwrap_or_default(),
                });
            }
        }

        if resp.url.trim().is_empty() {
            return Err(ApiError::Api {
                code: None,
                message: "gateway 响应缺少 url".to_string(),
            });
        }

        Ok(resp.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_body_has_no_reply_fields() {
        let b = SendMessageBody::text("你好");
        assert_eq!(b.content.as_deref(), Some("你好"));
        assert!(b.msg_id.is_none());
        assert!(b.msg_seq.is_none());
        assert!(b.media.is_none());
    }

    #[test]
    fn reply_body_carries_msg_id_and_clamps_seq() {
        let b = SendMessageBody::reply("回复", "msg-1", 99_999);
        assert_eq!(b.msg_id.as_deref(), Some("msg-1"));
        // 官方要求 1~10000
        assert_eq!(b.msg_seq, Some(10_000));

        let b2 = SendMessageBody::reply("回复", "msg-1", 0);
        assert_eq!(b2.msg_seq, Some(1));
    }

    #[test]
    fn api_response_flags_nonzero_code() {
        let r = ApiResponse {
            code: Some(11244),
            message: Some("频控限制".to_string()),
            id: None,
            timestamp: None,
        };
        assert!(r.check().is_err());
    }

    #[test]
    fn api_response_accepts_code_zero_and_missing() {
        let ok0 = ApiResponse {
            code: Some(0),
            message: None,
            id: Some("m1".to_string()),
            timestamp: None,
        };
        assert!(ok0.check().is_ok());

        let none = ApiResponse {
            code: None,
            message: None,
            id: None,
            timestamp: None,
        };
        assert!(none.check().is_ok());
    }

    #[tokio::test]
    async fn group_message_rejects_empty_openid_before_network() {
        let tokens = crate::auth::manager_with_token("1", "s", "tk", 3600);
        let api = OfficialApi::with_parts("https://example.invalid", tokens);
        let err = api
            .send_group_message("", &SendMessageBody::text("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::MissingGroupOpenid));
    }
}
