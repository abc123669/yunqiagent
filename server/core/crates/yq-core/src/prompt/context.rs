//! 上下文组装 —— 固定区 + 动态区
//!
//! 动态区构成（顺序固定）：
//!   1. 记忆注入（**只能追加**）
//!   2. 历史对话
//!   3. 当前用户消息
//!
//! `BuiltContext.prefix_sha256` **直接取用** `StaticPrefix.sha256`，
//! 绝不从拼接后的完整字符串截取反推。

use std::sync::Arc;

use crate::message::Message;
use crate::prompt::prefix::StaticPrefix;

/// 组装完成的上下文。
#[derive(Debug, Clone)]
pub struct BuiltContext {
    /// 固定区 SHA256（直接来自 `StaticPrefix`，非反推）
    pub prefix_sha256: String,
    /// 完整上下文文本（固定区 + 动态区）
    pub full_text: String,
}

/// 上下文构建器。
///
/// 内部持有 `Arc<StaticPrefix>`，动态区各部分只能**追加**。
pub struct ContextBuilder {
    prefix: Arc<StaticPrefix>,
    /// 动态区：记忆注入（只能 push，不得前置/插入）
    memories: Vec<String>,
    /// 动态区：历史对话
    history: Vec<Message>,
    /// 动态区：当前用户消息
    current_user: Option<String>,
}

impl ContextBuilder {
    /// 以固定区前缀创建构建器。
    pub fn new(prefix: Arc<StaticPrefix>) -> Self {
        Self {
            prefix,
            memories: Vec::new(),
            history: Vec::new(),
            current_user: None,
        }
    }

    /// 追加记忆注入。
    ///
    /// **只能追加**（`extend`），绝不前置，绝不插入固定区。
    ///
    /// 注：返回 `&mut Self` 需要 `&mut self` 接收者（Rust 借用规则：
    /// 不能由 `&self` 派生 `&mut Self`）。
    pub fn append_memories(&mut self, memories: &[String]) -> &mut Self {
        self.memories.extend(memories.iter().cloned());
        self
    }

    /// 追加历史对话。
    pub fn push_history(&mut self, history: &[Message]) -> &mut Self {
        self.history.extend(history.iter().cloned());
        self
    }

    /// 设置当前用户消息。
    pub fn current_user(&mut self, msg: &str) -> &mut Self {
        self.current_user = Some(msg.to_string());
        self
    }

    /// 构建完整上下文。
    ///
    /// `prefix_sha256` 直接返回 `StaticPrefix` 的 sha256 字段。
    pub fn build(&self) -> BuiltContext {
        let mut full = String::new();

        // ── 固定区 ──
        full.push_str(self.prefix.text());

        // ── 动态区：记忆注入（追加） ──
        full.push_str("[MEMORY_INJECT]\n");
        for m in &self.memories {
            full.push_str(m);
            full.push('\n');
        }
        full.push_str("[/MEMORY_INJECT]\n");

        // ── 动态区：历史对话 ──
        full.push_str("[HISTORY]\n");
        for h in &self.history {
            full.push_str(&h.render());
        }
        full.push_str("[/HISTORY]\n");

        // ── 动态区：当前用户消息 ──
        full.push_str("[CURRENT_USER]\n");
        if let Some(u) = &self.current_user {
            full.push_str(u);
            full.push('\n');
        }
        full.push_str("[/CURRENT_USER]\n");

        BuiltContext {
            // 直接取用固定区指纹，绝不反推
            prefix_sha256: self.prefix.sha256().to_string(),
            full_text: full,
        }
    }

    /// 固定区字节长度（诊断用）。
    pub fn prefix_len(&self) -> usize {
        self.prefix.len()
    }
}
