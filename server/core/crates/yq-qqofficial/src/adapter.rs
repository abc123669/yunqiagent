//! 适配层 —— 把官方事件载荷解析为通用消息模型。
//!
//! ## 为什么单独一层
//!
//! `yq-qqofficial` **不依赖** `yq-qq`（避免两个接入层互相耦合）。
//! 本模块产出中立的 `OfficialMessage`，由上层（yq-cli）决定如何送进 Agent。
//!
//! ## 字段来源（官方事件 `d`）
//!
//! ```json
//! {
//!   "id": "消息ID",
//!   "content": "消息内容",
//!   "group_openid": "群 openid（群消息）",
//!   "author": { "member_openid": "发送者 openid", "id": "..." },
//!   "timestamp": "2026-10-05T12:00:00+08:00",
//!   "mentions": [ { "id": "...", "bot": true } ]
//! }
//! ```
//!
//! **注意**：`group_openid` 是编码串，**不是 QQ 群号**，
//! 只能从事件中获得，无法用群号反查。

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::protocol::EventType;

/// 一次 @提及。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Mention {
    /// 被提及者 ID
    #[serde(default)]
    pub id: String,
    /// **是否机器人**（字段名是 `bot`，不是 `is_bot`）
    #[serde(default)]
    pub bot: bool,
}

/// 解析后的官方消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfficialMessage {
    /// 所属会话类型
    pub scene: Scene,
    /// 消息 ID（被动回复必需）
    pub message_id: String,
    /// 文本内容
    pub content: String,
    /// 群 openid（仅群聊）
    pub group_openid: Option<String>,
    /// 发送者 openid（私聊时用于回复）
    pub user_openid: String,
    /// 发送时间
    pub timestamp: Option<DateTime<FixedOffset>>,
    /// @提及列表
    pub mentions: Vec<Mention>,
}

/// 会话场景。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scene {
    /// 群聊
    Group,
    /// C2C 私聊
    C2c,
    /// 频道
    Guild,
}

impl OfficialMessage {
    /// 从事件载荷解析。
    ///
    /// 非消息类事件（互动、系统事件等）返回 `None`。
    pub fn from_event(event_type: EventType, data: &serde_json::Value) -> Option<Self> {
        if !event_type.is_message() {
            return None;
        }

        let scene = if event_type.is_group() {
            Scene::Group
        } else if event_type.is_private() {
            Scene::C2c
        } else {
            Scene::Guild
        };

        let message_id = data
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // 内容字段：消息事件用 `content`；频道事件可能用 `content` 或需要拼接
        let content = extract_content(data);

        let group_openid = data
            .get("group_openid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // 发送者：群/C2C 在 author.member_openid；频道在 author.id
        let user_openid = data
            .get("author")
            .and_then(|a| {
                a.get("member_openid")
                    .or_else(|| a.get("id"))
                    .or_else(|| a.get("user_openid"))
            })
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let timestamp = data
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok());

        let mentions = data
            .get("mentions")
            .and_then(|v| serde_json::from_value::<Vec<Mention>>(v.clone()).ok())
            .unwrap_or_default();

        Some(Self {
            scene,
            message_id,
            content,
            group_openid,
            user_openid,
            timestamp,
            mentions,
        })
    }

    /// 是否 @了机器人。
    ///
    /// 用 `mentions[].bot` 精确判断 —— **不要**用 `<@` 字符串匹配
    /// （@ 群里的普通用户同样会产生 `<@xxx>`，会误触发）。
    pub fn mentions_bot(&self) -> bool {
        self.mentions.iter().any(|m| m.bot && !m.id.is_empty())
    }

    /// 会话键（用于 session 映射）。
    pub fn session_key(&self) -> String {
        match self.scene {
            Scene::Group => format!("qqgroup:{}", self.group_openid.clone().unwrap_or_default()),
            Scene::C2c | Scene::Guild => format!("qqc2c:{}", self.user_openid),
        }
    }

    /// 剥离内容里的 @机器人 标记（官方格式为 `<@!id>` 或 `<@id>`）。
    pub fn stripped_content(&self) -> String {
        strip_mentions(&self.content).trim().to_string()
    }

    /// 是否为空文本（只 @ 了机器人没说别的）。
    pub fn is_empty_text(&self) -> bool {
        self.stripped_content().is_empty()
    }
}

/// 从事件载荷里取文本内容。
///
/// 官方群/C2C 消息在 `content`；部分场景把内容放在 `content` 之外的
/// `message.content`（兼容处理）。
fn extract_content(data: &serde_json::Value) -> String {
    if let Some(s) = data.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(s) = data
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
    {
        return s.to_string();
    }
    String::new()
}

/// 剥离 `<@...>` 形式的提及标记。
///
/// 官方 @ 格式是 `<@BOT_ID>`（不是 `@bot_name`）。
pub fn strip_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            // 尝试消费到 '>' 且形如 <@...>
            let mut buf = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '>' {
                    closed = true;
                    break;
                }
                buf.push(c);
                if buf.len() > 64 {
                    // 防御：过长说明不是提及标记
                    break;
                }
            }
            let is_mention = closed && buf.trim_start().starts_with('@');
            if !is_mention {
                out.push('<');
                out.push_str(&buf);
                if closed {
                    out.push('>');
                }
            }
        } else {
            out.push(ch);
        }
    }

    out
}

/// 判断一条官方消息是否应当触发 Agent。
///
/// 规则：
///   · 群聊：仅当 `mentions` 里包含机器人时响应
///     （公域群机器人会推送**所有**群消息，必须过滤）
///   · 私聊：直接响应
pub fn should_respond(msg: &OfficialMessage) -> bool {
    match msg.scene {
        Scene::Group => msg.mentions_bot(),
        Scene::C2c => true,
        Scene::Guild => msg.mentions_bot(),
    }
}

/// 便捷转换：官方事件 → 可直接处理的 `OfficialMessage`（含触发判定）。
pub fn official_to_incoming(
    event_type: EventType,
    data: &serde_json::Value,
) -> Option<OfficialMessage> {
    OfficialMessage::from_event(event_type, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group_payload() -> serde_json::Value {
        json!({
            "id": "ROBOT1.0_abc",
            "content": "<@!1234567890> 帮我看看服务器状态",
            "group_openid": "F7E0840EC8EC1BC6B19AF8DBEE1EEEA5",
            "author": { "member_openid": "USER_OPENID_1", "id": "u1" },
            "timestamp": "2026-10-05T12:00:00+08:00",
            "mentions": [ { "id": "1234567890", "bot": true } ]
        })
    }

    #[test]
    fn parses_group_message() {
        let msg = OfficialMessage::from_event(EventType::GroupAtMessageCreate, &group_payload())
            .expect("应解析成功");
        assert_eq!(msg.scene, Scene::Group);
        assert_eq!(msg.message_id, "ROBOT1.0_abc");
        assert_eq!(
            msg.group_openid.as_deref(),
            Some("F7E0840EC8EC1BC6B19AF8DBEE1EEEA5")
        );
        assert_eq!(msg.user_openid, "USER_OPENID_1");
        assert!(msg.timestamp.is_some());
        assert!(msg.mentions_bot());
    }

    #[test]
    fn ignored_for_non_message_events() {
        let ev = OfficialMessage::from_event(EventType::InteractionCreate, &json!({}));
        assert!(ev.is_none());
    }

    #[test]
    fn strips_at_mention_from_content() {
        let msg =
            OfficialMessage::from_event(EventType::GroupAtMessageCreate, &group_payload()).unwrap();
        assert_eq!(msg.stripped_content(), "帮我看看服务器状态");
    }

    #[test]
    fn strip_mentions_leaves_normal_angle_brackets() {
        // 形如 <abc> 的非提及内容不应被破坏
        assert_eq!(strip_mentions("<abc> 保留"), "<abc> 保留");
        assert_eq!(strip_mentions("<@!1> 去掉"), " 去掉");
        assert_eq!(strip_mentions("a <@2> b"), "a  b");
    }

    #[test]
    fn group_message_without_bot_mention_should_not_respond() {
        let data = json!({
            "id": "m1",
            "content": "大家好啊",
            "group_openid": "G1",
            "author": { "member_openid": "U1" },
            "mentions": [ { "id": "OTHER_USER", "bot": false } ]
        });
        let msg = OfficialMessage::from_event(EventType::GroupMessageCreate, &data).unwrap();
        assert!(!msg.mentions_bot());
        assert!(!should_respond(&msg), "公域群消息未 @机器人时不应响应");
    }

    #[test]
    fn c2c_message_always_responds() {
        let data = json!({
            "id": "m2",
            "content": "你好",
            "author": { "user_openid": "U2" }
        });
        let msg = OfficialMessage::from_event(EventType::C2cMessageCreate, &data).unwrap();
        assert_eq!(msg.scene, Scene::C2c);
        assert!(should_respond(&msg));
    }

    #[test]
    fn session_key_distinguishes_scenes() {
        let g =
            OfficialMessage::from_event(EventType::GroupAtMessageCreate, &group_payload()).unwrap();
        assert_eq!(g.session_key(), "qqgroup:F7E0840EC8EC1BC6B19AF8DBEE1EEEA5");

        let c2c = OfficialMessage::from_event(
            EventType::C2cMessageCreate,
            &json!({ "id": "m", "content": "hi", "author": { "user_openid": "U9" } }),
        )
        .unwrap();
        assert_eq!(c2c.session_key(), "qqc2c:U9");
    }

    #[test]
    fn empty_text_after_mention_detected() {
        let data = json!({
            "id": "m3",
            "content": "<@!1234567890>",
            "group_openid": "G1",
            "author": { "member_openid": "U1" },
            "mentions": [ { "id": "1234567890", "bot": true } ]
        });
        let msg = OfficialMessage::from_event(EventType::GroupAtMessageCreate, &data).unwrap();
        assert!(msg.is_empty_text());
    }

    #[test]
    fn content_falls_back_to_message_content() {
        let data = json!({
            "id": "m4",
            "message": { "content": "备用字段内容" },
            "author": { "user_openid": "U1" }
        });
        let msg = OfficialMessage::from_event(EventType::C2cMessageCreate, &data).unwrap();
        assert_eq!(msg.content, "备用字段内容");
    }

    #[test]
    fn malformed_mentions_do_not_panic() {
        let data = json!({
            "id": "m5",
            "content": "x",
            "author": { "member_openid": "U1" },
            "mentions": "不是数组"
        });
        let msg = OfficialMessage::from_event(EventType::GroupMessageCreate, &data).unwrap();
        assert!(msg.mentions.is_empty());
        assert!(!msg.mentions_bot());
    }

    #[test]
    fn official_to_incoming_wrapper_works() {
        let m = official_to_incoming(EventType::GroupAtMessageCreate, &group_payload());
        assert!(m.is_some());
    }
}
