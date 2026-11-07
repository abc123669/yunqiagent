//! yq-qq —— QQ 业务层
//!
//! **解耦纪律**（SPEC 第十五节）：本 crate 依赖 `yq-onebot`（协议）与 `yq-core`（核心），
//! 但 `yq-core` **绝不依赖本 crate** —— QQ 是接入方式之一，不是核心的一部分。
//!
//! 模块：
//!   · `filter`   触发判定（群聊仅被 @ 或私聊触发）
//!   · `router`   事件 → 会话路由（谁在哪个会话里说话）
//!   · `format`   响应格式化（[心跳] 前缀、≤200 字分片）
//!   · `markdown` Markdown → 纯文本（QQ 消息不支持 Markdown 渲染）
//!   · `commands` 斜杠命令解析（/new /model /usage /compress）
//!   · `session`  会话映射（群/私聊 → agent 会话 ID）

pub mod commands;
pub mod filter;
pub mod format;
pub mod markdown;
pub mod router;
pub mod sanitize;
pub mod session;

pub use commands::{
    COMPRESS_PLACEHOLDER, Command, CommandEffect, ParseResult, compress_done_text,
    compress_empty_text, compress_failed_text, compress_prompt, format_usage, handle_command,
    help_text, help_text_full, looks_like_command, model_switch_failed_text, model_switched_text,
    model_text, needs_agent_state, new_conversation_text, parse, unknown_command_text,
    verbose_off_text, verbose_on_text, verbose_state_text,
};
pub use filter::{TriggerDecision, TriggerPolicy};
pub use format::{
    QQ_MAX_MESSAGE_CHARS, QQ_OFFICIAL_MAX_CHARS, format_heartbeat_message, format_reply,
    format_reply_with_limit, split_for_qq,
};
pub use markdown::markdown_to_plain;
pub use router::{IncomingMessage, MessageKind, Router};
pub use sanitize::{SanitizeVerdict, sanitize_or_replace};
pub use session::SessionMap;
