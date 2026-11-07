//! 触发判定 —— 群聊仅被 @ 或私聊触发（SPEC 第十五节）。
//!
//! QQ 机器人若对每条群消息都响应，会被风控且扰民。因此本模块是**唯一**的
//! 「这条消息要不要回」决策点，判定逻辑集中、可测。

use yq_onebot::protocol::Event;

/// 触发决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerDecision {
    /// 应当响应
    Respond {
        /// 消息类型（群 / 私聊）
        kind: TriggerKind,
    },
    /// 不响应，附带原因（便于日志）
    Ignore {
        /// 忽略原因
        reason: &'static str,
    },
}

/// 触发类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// 群聊（被 @）
    Group,
    /// 私聊
    Private,
}

/// 触发策略配置。
#[derive(Debug, Clone)]
pub struct TriggerPolicy {
    /// 是否响应私聊（默认 true）
    pub respond_private: bool,
    /// 是否要求群聊必须 @ 机器人才响应（默认 true）
    pub require_mention_in_group: bool,
    /// 命令前缀（可选；如 `"/"`。置空则不用前缀）
    pub command_prefix: Option<String>,
    /// 是否响应以命令前缀开头的群消息（无需 @）
    pub respond_prefix_in_group: bool,
}

impl Default for TriggerPolicy {
    fn default() -> Self {
        Self {
            respond_private: true,
            require_mention_in_group: true,
            command_prefix: Some("/".to_string()),
            respond_prefix_in_group: true,
        }
    }
}

impl TriggerPolicy {
    /// 判断一个事件是否应当触发响应。
    pub fn decide(&self, event: &Event, self_id: i64) -> TriggerDecision {
        if !event.is_message() {
            return TriggerDecision::Ignore {
                reason: "非消息事件",
            };
        }

        // 忽略自己发的消息（防止自回复死循环）
        if event.user_id == Some(self_id) {
            return TriggerDecision::Ignore {
                reason: "机器人自身消息",
            };
        }

        let raw = event.raw_message.as_deref().unwrap_or("");

        if event.is_private() {
            if !self.respond_private {
                return TriggerDecision::Ignore {
                    reason: "私聊响应已关闭",
                };
            }
            return TriggerDecision::Respond {
                kind: TriggerKind::Private,
            };
        }

        if event.is_group() {
            if self.require_mention_in_group {
                if is_mentioned(raw, self_id) {
                    return TriggerDecision::Respond {
                        kind: TriggerKind::Group,
                    };
                }
                if self.respond_prefix_in_group && self.has_command_prefix(raw) {
                    return TriggerDecision::Respond {
                        kind: TriggerKind::Group,
                    };
                }
                return TriggerDecision::Ignore {
                    reason: "群聊未 @ 机器人且无命令前缀",
                };
            }
            return TriggerDecision::Respond {
                kind: TriggerKind::Group,
            };
        }

        TriggerDecision::Ignore {
            reason: "未知消息类型",
        }
    }

    /// 原始消息是否以命令前缀开头。
    fn has_command_prefix(&self, raw: &str) -> bool {
        match &self.command_prefix {
            Some(p) if !p.is_empty() => raw.trim_start().starts_with(p.as_str()),
            _ => false,
        }
    }
}

/// 判断原始消息（CQ 码形式）是否 @ 了指定 QQ。
///
/// OneBot11 中 @ 表现为 `[CQ:at,qq=123456]`。
pub fn is_mentioned(raw: &str, self_id: i64) -> bool {
    let needle = format!("[CQ:at,qq={}", self_id);
    raw.contains(&needle)
}

/// 从原始消息（CQ 码形式）中剥离 @ 机器人的片段与首尾空白。
pub fn strip_mention(raw: &str, self_id: i64) -> String {
    let needle = format!("[CQ:at,qq={}]", self_id);
    let cleaned = raw.replace(&needle, "");
    cleaned.trim().to_string()
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
            message_id: Some(1),
        }
    }

    fn private_event(raw: &str) -> Event {
        Event {
            post_type: "message".to_string(),
            message_type: Some("private".to_string()),
            sub_type: None,
            user_id: Some(10001),
            group_id: None,
            self_id: Some(1905278430),
            message: None,
            raw_message: Some(raw.to_string()),
            message_id: Some(2),
        }
    }

    #[test]
    fn private_message_triggers() {
        let policy = TriggerPolicy::default();
        let d = policy.decide(&private_event("你好"), 1905278430);
        assert_eq!(
            d,
            TriggerDecision::Respond {
                kind: TriggerKind::Private
            }
        );
    }

    #[test]
    fn group_message_without_mention_is_ignored() {
        let policy = TriggerPolicy::default();
        let d = policy.decide(&group_event("大家好啊"), 1905278430);
        assert!(matches!(d, TriggerDecision::Ignore { .. }));
    }

    #[test]
    fn group_message_with_mention_triggers() {
        let policy = TriggerPolicy::default();
        let d = policy.decide(&group_event("[CQ:at,qq=1905278430] 帮我看看"), 1905278430);
        assert_eq!(
            d,
            TriggerDecision::Respond {
                kind: TriggerKind::Group
            }
        );
    }

    #[test]
    fn group_message_with_command_prefix_triggers() {
        let policy = TriggerPolicy::default();
        let d = policy.decide(&group_event("/help"), 1905278430);
        assert_eq!(
            d,
            TriggerDecision::Respond {
                kind: TriggerKind::Group
            }
        );
    }

    #[test]
    fn bot_own_message_is_ignored() {
        let policy = TriggerPolicy::default();
        let mut ev = private_event("我自己说的");
        ev.user_id = Some(1905278430);
        let d = policy.decide(&ev, 1905278430);
        assert!(matches!(d, TriggerDecision::Ignore { .. }));
    }

    #[test]
    fn strip_mention_removes_at_segment() {
        let cleaned = strip_mention("[CQ:at,qq=1905278430] 帮我看看", 1905278430);
        assert_eq!(cleaned, "帮我看看");
    }
}
