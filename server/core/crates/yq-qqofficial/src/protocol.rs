//! WebSocket 协议定义 —— opcode、intents、事件类型、载荷。
//!
//! 协议要点（源自 QQ 开放平台官方文档 / botgo / botpy）：
//!   · 连接后服务端先发 `op=10 Hello`（含 heartbeat_interval）
//!   · 客户端发 `op=2 Identify`（`d.token` 必须是 `QQBot {access_token}`）
//!   · 之后按间隔发 `op=1 Heartbeat`
//!   · 事件推送为 `op=0 Dispatch`（带 `t` 事件名与 `s` 序号）
//!   · 断线可 `op=6 Resume`（带 session_id + seq）

use serde::{Deserialize, Serialize};

/// WebSocket 操作码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    /// 服务端事件推送
    Dispatch = 0,
    /// 客户端心跳
    Heartbeat = 1,
    /// 客户端鉴权
    Identify = 2,
    /// 客户端更新状态
    PresenceUpdate = 3,
    /// 客户端恢复连接
    Resume = 6,
    /// 服务端要求重连
    Reconnect = 7,
    /// 服务端要求清空会话后重连
    InvalidSession = 9,
    /// 服务端欢迎消息
    Hello = 10,
    /// 心跳确认（ACK）
    HeartbeatAck = 11,
}

impl OpCode {
    /// 由 u8 解析。
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => OpCode::Dispatch,
            1 => OpCode::Heartbeat,
            2 => OpCode::Identify,
            3 => OpCode::PresenceUpdate,
            6 => OpCode::Resume,
            7 => OpCode::Reconnect,
            9 => OpCode::InvalidSession,
            10 => OpCode::Hello,
            11 => OpCode::HeartbeatAck,
            _ => return None,
        })
    }
}

/// 事件订阅意图位图。
///
/// **群聊机器人**必需 `GROUP_MESSAGES`（`1 << 25`）——
/// 它覆盖 `GROUP_AT_MESSAGE_CREATE`、`C2C_MESSAGE_CREATE` 等事件。
pub struct Intents;

impl Intents {
    /// 频道（guild）相关事件 —— **不是**群聊
    pub const GUILDS: u32 = 1 << 0;
    /// 频道消息事件
    pub const GUILD_MESSAGES: u32 = 1 << 9;
    /// 频道私信事件
    pub const DIRECT_MESSAGE: u32 = 1 << 12;
    /// **群聊/C2C 消息事件**（`GROUP_AT_MESSAGE_CREATE` / `C2C_MESSAGE_CREATE`）
    pub const GROUP_MESSAGES: u32 = 1 << 25;
    /// 互动事件（按钮回调等）
    pub const INTERACTION: u32 = 1 << 26;
    /// 频道 @ 消息事件
    pub const GUILD_AT_MESSAGES: u32 = 1 << 30;

    /// 纯群聊机器人（群 @ + C2C 私聊）
    pub const GROUP_ONLY: u32 = Self::GROUP_MESSAGES;
    /// 群聊 + C2C（默认）
    pub const GROUP_AND_C2C: u32 = Self::GROUP_MESSAGES;
    /// 群聊 + 频道 + 互动（覆盖面最广的保守组合）
    pub const BROAD: u32 = Self::GROUP_MESSAGES | Self::GUILD_AT_MESSAGES | Self::INTERACTION;
}

/// WebSocket 上行/下行载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    /// 操作码
    pub op: u8,
    /// 事件数据（可选；心跳可能为空）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
    /// 事件序号（`op=0` 时携带，用于 Resume）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s: Option<i64>,
    /// 事件类型名（`op=0` 时携带，如 `GROUP_AT_MESSAGE_CREATE`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
}

impl Payload {
    /// 构造 Identify 载荷。
    ///
    /// **关键**：`token` 必须是 `QQBot {access_token}`（与 HTTP 头同格式）。
    pub fn identify(access_token: &str, intents: u32, shard: (u32, u32)) -> Self {
        Self {
            op: OpCode::Identify as u8,
            d: Some(serde_json::json!({
                "token": format!("QQBot {}", access_token),
                "intents": intents,
                "shard": [shard.0, shard.1],
                "properties": {
                    "$os": std::env::consts::OS,
                    "$browser": "yunqiagent",
                    "$device": "yunqiagent"
                }
            })),
            s: None,
            t: None,
        }
    }

    /// 构造心跳载荷（`d` 为最新序号，可为 null）。
    pub fn heartbeat(seq: Option<i64>) -> Self {
        Self {
            op: OpCode::Heartbeat as u8,
            d: Some(serde_json::json!(seq)),
            s: None,
            t: None,
        }
    }

    /// 构造 Resume 载荷。
    pub fn resume(access_token: &str, session_id: &str, seq: i64) -> Self {
        Self {
            op: OpCode::Resume as u8,
            d: Some(serde_json::json!({
                "token": format!("QQBot {}", access_token),
                "session_id": session_id,
                "seq": seq
            })),
            s: None,
            t: None,
        }
    }

    /// 解析 opcode（未知返回 None）。
    pub fn opcode(&self) -> Option<OpCode> {
        OpCode::from_u8(self.op)
    }
}

/// 已知事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// 群聊 @机器人 消息（旧版 API 的唯一群事件）
    GroupAtMessageCreate,
    /// 群聊消息（**公域群机器人**发送全部群消息）
    GroupMessageCreate,
    /// C2C 私聊消息
    C2cMessageCreate,
    /// 频道 @消息
    AtMessageCreate,
    /// 互动事件（按钮回调）
    InteractionCreate,
    /// 其他/未识别
    Other,
}

impl EventType {
    /// 由事件名解析（未识别一律归入 `EventType::Other`，不返回错误）。
    ///
    /// 命名说明：刻意不叫 `from_str` —— 那是 `std::str::FromStr::from_str`
    /// 的约定名，而那个 trait 要求返回 `Result`。这里返回 `Self`（总有兜底
    /// 分支），同名会让调用方误以为可以 `?` 传播错误。
    pub fn from_event_name(s: &str) -> Self {
        match s {
            "GROUP_AT_MESSAGE_CREATE" => EventType::GroupAtMessageCreate,
            "GROUP_MESSAGE_CREATE" => EventType::GroupMessageCreate,
            "C2C_MESSAGE_CREATE" => EventType::C2cMessageCreate,
            "AT_MESSAGE_CREATE" => EventType::AtMessageCreate,
            "INTERACTION_CREATE" => EventType::InteractionCreate,
            _ => EventType::Other,
        }
    }

    /// 是否是需要回复的消息事件。
    pub fn is_message(self) -> bool {
        matches!(
            self,
            EventType::GroupAtMessageCreate
                | EventType::GroupMessageCreate
                | EventType::C2cMessageCreate
                | EventType::AtMessageCreate
        )
    }

    /// 是否群聊消息。
    pub fn is_group(self) -> bool {
        matches!(
            self,
            EventType::GroupAtMessageCreate | EventType::GroupMessageCreate
        )
    }

    /// 是否私聊消息。
    pub fn is_private(self) -> bool {
        matches!(self, EventType::C2cMessageCreate)
    }

    /// 事件名（原始字符串）。
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::GroupAtMessageCreate => "GROUP_AT_MESSAGE_CREATE",
            EventType::GroupMessageCreate => "GROUP_MESSAGE_CREATE",
            EventType::C2cMessageCreate => "C2C_MESSAGE_CREATE",
            EventType::AtMessageCreate => "AT_MESSAGE_CREATE",
            EventType::InteractionCreate => "INTERACTION_CREATE",
            EventType::Other => "OTHER",
        }
    }
}

/// 服务端 Hello 载荷（`op=10` 的 `d`）。
#[derive(Debug, Clone, Deserialize)]
pub struct HelloData {
    /// 心跳间隔（毫秒）
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
}

/// 默认心跳间隔（毫秒）。
fn default_heartbeat_interval() -> u64 {
    41250
}

impl HelloData {
    /// 心跳间隔（用 `Duration` 表示）。
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.heartbeat_interval.max(5000))
    }
}

/// WebSocket 关闭码（协议层语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCode {
    /// 鉴权失败（token 无效）
    Unauthorized,
    /// 认证失败（需刷新 token）
    AuthFailed,
    /// 会话超时（**必须重新 Identify，不可 Resume**）
    SessionTimeout,
    /// 机器人已下线（不可重连）
    BotOffline,
    /// 机器人被封禁（不可重连）
    BotBanned,
    /// 其他
    Other(u16),
}

impl CloseCode {
    /// 由数值解析。
    pub fn from_u16(v: u16) -> Self {
        match v {
            4001 => CloseCode::Unauthorized,
            4008 => CloseCode::AuthFailed,
            4009 => CloseCode::SessionTimeout,
            4914 => CloseCode::BotOffline,
            4915 => CloseCode::BotBanned,
            other => CloseCode::Other(other),
        }
    }

    /// 是否**不可重连**（致命错误）。
    pub fn is_fatal(self) -> bool {
        matches!(self, CloseCode::BotOffline | CloseCode::BotBanned)
    }

    /// 是否应当**清除会话并重新 Identify**（而不是 Resume）。
    ///
    /// 注意：4009 常被误当作"可 Resume" —— 实际上它表示会话已过期，
    /// **必须**丢弃 session_id/seq 重新 Identify，否则会无限 Resume 失败。
    pub fn must_reidentify(self) -> bool {
        matches!(
            self,
            CloseCode::SessionTimeout | CloseCode::Unauthorized | CloseCode::AuthFailed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_roundtrip() {
        assert_eq!(OpCode::from_u8(0), Some(OpCode::Dispatch));
        assert_eq!(OpCode::from_u8(10), Some(OpCode::Hello));
        assert_eq!(OpCode::from_u8(11), Some(OpCode::HeartbeatAck));
        assert_eq!(OpCode::from_u8(99), None);
    }

    #[test]
    fn identify_token_has_qqbot_prefix() {
        let p = Payload::identify("abc123", Intents::GROUP_ONLY, (0, 1));
        assert_eq!(p.op, 2);
        let d = p.d.unwrap();
        assert_eq!(d["token"], "QQBot abc123");
        assert_eq!(d["intents"], 33554432);
        assert_eq!(d["shard"][0], 0);
        assert_eq!(d["shard"][1], 1);
    }

    #[test]
    fn group_intent_value_matches_official_docs() {
        // 官方文档：群聊意图 = 1 << 25 = 33554432
        assert_eq!(Intents::GROUP_MESSAGES, 33_554_432);
        assert_eq!(Intents::GROUP_ONLY, 33_554_432);
    }

    #[test]
    fn heartbeat_payload_carries_seq() {
        let p = Payload::heartbeat(Some(42));
        assert_eq!(p.op, 1);
        assert_eq!(p.d.unwrap(), serde_json::json!(42));

        let p_null = Payload::heartbeat(None);
        assert!(p_null.d.unwrap().is_null());
    }

    #[test]
    fn resume_payload_shape() {
        let p = Payload::resume("tk", "sess-1", 7);
        assert_eq!(p.op, 6);
        let d = p.d.unwrap();
        assert_eq!(d["token"], "QQBot tk");
        assert_eq!(d["session_id"], "sess-1");
        assert_eq!(d["seq"], 7);
    }

    #[test]
    fn event_type_parsing_and_classification() {
        assert_eq!(
            EventType::from_event_name("GROUP_AT_MESSAGE_CREATE"),
            EventType::GroupAtMessageCreate
        );
        assert_eq!(
            EventType::from_event_name("GROUP_MESSAGE_CREATE"),
            EventType::GroupMessageCreate
        );
        assert_eq!(
            EventType::from_event_name("C2C_MESSAGE_CREATE"),
            EventType::C2cMessageCreate
        );
        assert_eq!(EventType::from_event_name("WHATEVER"), EventType::Other);

        assert!(EventType::GroupAtMessageCreate.is_message());
        assert!(EventType::GroupMessageCreate.is_group());
        assert!(EventType::C2cMessageCreate.is_private());
        assert!(!EventType::InteractionCreate.is_message());
    }

    #[test]
    fn close_code_semantics() {
        assert!(CloseCode::from_u16(4914).is_fatal());
        assert!(CloseCode::from_u16(4915).is_fatal());
        assert!(!CloseCode::from_u16(4009).is_fatal());
        // 4009 必须重新 Identify（不是 Resume）
        assert!(CloseCode::from_u16(4009).must_reidentify());
        assert!(CloseCode::from_u16(4001).must_reidentify());
        assert!(!CloseCode::from_u16(1000).must_reidentify());
    }

    #[test]
    fn hello_data_interval_has_floor() {
        let h = HelloData {
            heartbeat_interval: 1, // 异常小的值应被抬到 5s 下限
        };
        assert_eq!(h.interval().as_secs(), 5);

        let h2 = HelloData {
            heartbeat_interval: 41250,
        };
        assert_eq!(h2.interval().as_millis(), 41250);
    }

    #[test]
    fn payload_deserializes_server_dispatch() {
        let raw = r#"{"op":0,"s":12,"t":"GROUP_AT_MESSAGE_CREATE","d":{"id":"msg1"}}"#;
        let p: Payload = serde_json::from_str(raw).unwrap();
        assert_eq!(p.op, 0);
        assert_eq!(p.s, Some(12));
        assert_eq!(p.t.as_deref(), Some("GROUP_AT_MESSAGE_CREATE"));
        assert_eq!(p.opcode(), Some(OpCode::Dispatch));
    }
}
