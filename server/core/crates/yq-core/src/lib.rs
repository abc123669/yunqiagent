//! yq-core —— yunqiagent 核心
//!
//! 模块：
//!   · `message`  对话消息模型
//!   · `error`    统一错误类型
//!   · `persona`  persona 设定（固定区，保序）
//!   · `tools`    五工具（固定顺序）+ 注册表
//!   · `prompt`   KV-cache 前缀组装（一等设计约束）
//!   · `text`     文本工具（按字符截断，错误/日志路径共用）

pub mod error;
pub mod message;
pub mod ops;
pub mod persona;
pub mod prompt;
pub mod text;
pub mod tools;

pub use error::{CoreError, ToolError};
pub use message::{Message, Role, ToolCall, ToolResult};
pub use ops::{BackupConfig, BackupQueue, DangerOp, OpType};
pub use persona::Persona;
pub use prompt::{BuiltContext, ContextBuilder, STATIC_RULES, SYSTEM_PROMPT, StaticPrefix};
pub use text::{truncate, truncate_ellipsis, truncate_with_marker};
pub use tools::{Tool, ToolDefinition, ToolRegistry};
