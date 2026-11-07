//! 会话路由 —— 把 OneBot 事件映射为「谁在哪个会话里说了什么」。
//!
//! 群聊 / 私聊各自映射到一个稳定的**会话 ID**（见 `session.rs`），
//! 同一会话的多轮对话共享历史与记忆作用域。

use yq_onebot::protocol::Event;

use crate::filter::{TriggerKind, strip_mention};

/// 消息来源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// 群聊
    Group,
    /// 私聊
    Private,
}

impl From<TriggerKind> for MessageKind {
    fn from(k: TriggerKind) -> Self {
        match k {
            TriggerKind::Group => MessageKind::Group,
            TriggerKind::Private => MessageKind::Private,
        }
    }
}

/// 一条待处理的消息（已完成触发判定与文本清洗）。
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// 来源类型
    pub kind: MessageKind,
    /// 群号（私聊为 None）
    pub group_id: Option<i64>,
    /// 发送者 QQ 号
    pub user_id: i64,
    /// 机器人自身 QQ 号
    pub self_id: i64,
    /// 清洗后的用户文本（已剥离 @ 片段）
    pub text: String,
    /// 原始消息 ID（便于引用回复）
    pub message_id: Option<i64>,
}

impl IncomingMessage {
    /// 会话键（用于 `SessionMap`）。
    ///
    /// 群聊以群为单位（群内共享上下文），私聊以用户为单位。
    pub fn session_key(&self) -> String {
        match self.kind {
            MessageKind::Group => format!("group:{}", self.group_id.unwrap_or(0)),
            MessageKind::Private => format!("private:{}", self.user_id),
        }
    }

    /// 是否群聊。
    pub fn is_group(&self) -> bool {
        self.kind == MessageKind::Group
    }
}

/// 事件路由器（无状态；由 `TriggerPolicy` 完成判定后调用）。
pub struct Router;

impl Router {
    /// 由已通过触发判定的事件构造 `IncomingMessage`。
    pub fn build(event: &Event, kind: MessageKind) -> Option<IncomingMessage> {
        let self_id = event.self_id?;
        let user_id = event.user_id?;
        let raw = event.raw_message.as_deref().unwrap_or("");

        let text = match kind {
            MessageKind::Group => strip_mention(raw, self_id),
            MessageKind::Private => raw.trim().to_string(),
        };

        Some(IncomingMessage {
            kind,
            group_id: event.group_id,
            user_id,
            self_id,
            text,
            message_id: event.message_id,
        })
    }

    /// 是否需要忽略空文本（如只 @ 了机器人没说别的）。
    pub fn is_empty_text(msg: &IncomingMessage) -> bool {
        msg.text.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_event(raw: &str) -> Event {
        Event {
            post_type: "message".to_string(),
            message_type: Some("group".to_string()),
            sub_type: None,
            user_id: Some(10001),
            group_id: Some(931409187),
            self_id: Some(1905278430),
            message: None,
            raw_message: Some(raw.to_string()),
            message_id: Some(7),
        }
    }

    #[test]
    fn group_session_key_is_group_scoped() {
        let ev = group_event("[CQ:at,qq=1905278430] 你好");
        let msg = Router::build(&ev, MessageKind::Group).unwrap();
        assert_eq!(msg.session_key(), "group:931409187");
        assert_eq!(msg.text, "你好");
    }

    #[test]
    fn private_session_key_is_user_scoped() {
        let mut ev = group_event("你好");
        ev.message_type = Some("private".to_string());
        ev.group_id = None;
        let msg = Router::build(&ev, MessageKind::Private).unwrap();
        assert_eq!(msg.session_key(), "private:10001");
    }

    #[test]
    fn empty_text_after_mention_is_detected() {
        let ev = group_event("[CQ:at,qq=1905278430]");
        let msg = Router::build(&ev, MessageKind::Group).unwrap();
        assert!(Router::is_empty_text(&msg));
    }
}
