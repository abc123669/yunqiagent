//! yq-llm —— 模型层（OpenAI 兼容 + 流式 + KV-cache 友好组装）
//!
//! 模块：
//!   · `config`  LLM 配置（base_url / api_key / model / 采样参数）
//!   · `types`   OpenAI 兼容的请求 / 响应类型
//!   · `client`  HTTP 客户端（非流式 + SSE 流式）
//!   · `assemble` 把 `StaticPrefix` + 动态区组装为 KV-cache 友好请求
//!
//! ## KV-cache 友好的核心机关
//!
//! OpenAI 兼容 provider 的 prompt 缓存是**自动前缀缓存**（automatic prefix caching）：
//! 它把请求体里 **messages 数组的最长公共前缀**作为可复用缓存段。
//!
//! 因此组装遵循 SPEC 第四节：
//! ```text
//! messages[0] = system  ← 固定区全文（系统提示 + 工具定义 + persona + 静态指令），字节级固定
//! messages[1..] = 动态区（本轮记忆注入 + 历史对话 + 当前用户消息），只追加
//! ```
//! **固定区必须整个塞进 `messages[0]` 且永不变化** —— 只要它不变，provider 就能命中缓存。

pub mod assemble;
pub mod client;
pub mod config;
pub mod types;

pub use assemble::{AssembledRequest, build_chat_request};
pub use client::LlmClient;
pub use config::LlmConfig;
pub use types::{
    ChatMessage, ChatRequest, ChatResponse, ChatToolCall, FunctionCallPayload, ToolFunctionSpec,
    ToolProtocol, ToolSpec, StreamEvent, TokenChunk, Usage, build_tools_array,
};
