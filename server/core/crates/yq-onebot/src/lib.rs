//! yq-onebot —— OneBot11 协议层（正向 + 反向 WebSocket）
//!
//! 只做协议收发，不含 QQ 业务逻辑（见 yq-qq）。

pub mod protocol;
pub mod ws;

pub use protocol::{
    Action, ApiResponse, Event, MAX_MESSAGE_CHARS, SendGroupMsg, SendPrivateMsg, split_message,
};
pub use ws::{ActionStream, EventStream, Mode, OneBotConnection, connect};
