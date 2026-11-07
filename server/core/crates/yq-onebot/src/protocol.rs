//! OneBot11 协议类型定义
//!
//! 覆盖最小可用子集：消息事件 + 发送消息动作 + API 响应。
//!
//! 所有字段名 snake_case，与 OneBot11 官方 JSON 字段严格对齐。

use serde::{Deserialize, Serialize};

/// OneBot11 事件（仅解析本阶段需要的字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// 事件类型（message / notice / request / meta_event）
    pub post_type: String,

    /// 消息类型（private / group），仅 post_type=message 时有
    #[serde(default)]
    pub message_type: Option<String>,

    /// 消息子类型
    #[serde(default)]
    pub sub_type: Option<String>,

    /// 发送者 QQ 号
    #[serde(default)]
    pub user_id: Option<i64>,

    /// 群号（群消息时存在）
    #[serde(default)]
    pub group_id: Option<i64>,

    /// 机器人自身 QQ 号
    #[serde(default)]
    pub self_id: Option<i64>,

    /// 消息内容（数组或字符串，本阶段按原始 JSON 保留）
    #[serde(default)]
    pub message: Option<serde_json::Value>,

    /// 原始消息文本（CQ 码形式）
    #[serde(default)]
    pub raw_message: Option<String>,

    /// 消息 ID
    #[serde(default)]
    pub message_id: Option<i64>,
}

impl Event {
    /// 是否为消息事件。
    pub fn is_message(&self) -> bool {
        self.post_type == "message"
    }

    /// 是否群消息。
    pub fn is_group(&self) -> bool {
        self.message_type.as_deref() == Some("group")
    }

    /// 是否私聊消息。
    pub fn is_private(&self) -> bool {
        self.message_type.as_deref() == Some("private")
    }
}

/// 发送群消息动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendGroupMsg {
    /// 群号
    pub group_id: i64,
    /// 消息内容（纯字符串）
    pub message: String,
    /// 是否忽略机器人自身
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_escape: Option<bool>,
}

/// 发送私聊消息动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendPrivateMsg {
    /// 目标 QQ 号
    pub user_id: i64,
    /// 消息内容
    pub message: String,
    /// 是否忽略机器人自身
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_escape: Option<bool>,
}

/// 通用动作封装（OneBot11 action 格式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    /// 动作名，如 send_group_msg
    pub action: String,
    /// 参数
    pub params: serde_json::Value,
    /// 回执标识（便于匹配响应）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo: Option<String>,
}

impl Action {
    /// 构造 `send_group_msg` 动作。
    pub fn send_group_msg(group_id: i64, message: impl Into<String>) -> Self {
        let p = SendGroupMsg {
            group_id,
            message: message.into(),
            auto_escape: Some(false),
        };
        Self {
            action: "send_group_msg".to_string(),
            params: serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            echo: None,
        }
    }

    /// 构造 `send_private_msg` 动作。
    pub fn send_private_msg(user_id: i64, message: impl Into<String>) -> Self {
        let p = SendPrivateMsg {
            user_id,
            message: message.into(),
            auto_escape: Some(false),
        };
        Self {
            action: "send_private_msg".to_string(),
            params: serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            echo: None,
        }
    }

    /// 附加 echo。
    pub fn with_echo(mut self, echo: impl Into<String>) -> Self {
        self.echo = Some(echo.into());
        self
    }
}

/// OneBot11 API 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse {
    /// 状态：ok / failed
    pub status: String,
    /// 返回码，0 表示成功
    #[serde(default)]
    pub retcode: i32,
    /// 数据
    #[serde(default)]
    pub data: serde_json::Value,
    /// 回执标识
    #[serde(default)]
    pub echo: Option<String>,
}

impl ApiResponse {
    /// 是否成功。
    pub fn is_ok(&self) -> bool {
        self.status == "ok" && self.retcode == 0
    }
}

/// 单条消息最大字符数（QQ 风控）。
pub const MAX_MESSAGE_CHARS: usize = 200;

/// 把长消息按 ≤`max_chars` 分片。
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return vec![text.to_string()];
    }
    chars
        .chunks(max_chars)
        .map(|c| c.iter().collect::<String>())
        .collect()
}
