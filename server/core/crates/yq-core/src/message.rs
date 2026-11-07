//! 消息模型 —— 对话上下文的基本单元
//!
//! 重要约束：
//!   · 本模块的类型出现在**动态区**（历史对话 / 工具结果）
//!   · **绝不进入固定区前缀**，因此允许含时间戳等可变字段
//!   · 所有字段名 snake_case

use serde::{Deserialize, Serialize};

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// 系统（仅用于固定区的系统提示，一般不作为动态消息）
    System,
    /// 用户
    User,
    /// 助手
    Assistant,
    /// 工具结果
    Tool,
}

impl Role {
    /// 转为数据库 / 序列化使用的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次工具调用请求（助手发出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// 调用唯一标识（用于与 ToolResult 配对；属动态区，不进前缀）
    pub tool_call_id: String,
    /// 工具名（snake_case，如 "read"、"delegate"）
    pub tool_name: String,
    /// 参数（运行时解析后的 JSON；属动态区）
    pub arguments: serde_json::Value,
}

/// 一次工具执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// 对应 ToolCall 的 id
    pub tool_call_id: String,
    /// 工具名
    pub tool_name: String,
    /// 结果文本（进动态区，不污染前缀）
    pub content: String,
    /// 是否失败
    pub is_error: bool,
}

/// 一条对话消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// 角色
    pub role: Role,
    /// 文本内容
    pub content: String,
    /// 助手发起的工具调用（可选）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// 工具结果（role=Tool 时使用；可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResult>,
}

impl Message {
    /// 构造一条用户消息。
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_result: None,
        }
    }

    /// 构造一条助手消息。
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_result: None,
        }
    }

    /// 构造一条工具结果消息。
    pub fn tool(result: ToolResult) -> Self {
        Self {
            role: Role::Tool,
            content: result.content.clone(),
            tool_calls: Vec::new(),
            tool_result: Some(result),
        }
    }

    /// 渲染为动态区文本片段。
    ///
    /// 格式固定：`[role]\ncontent\n`
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push('[');
        s.push_str(self.role.as_str());
        s.push_str("]\n");
        s.push_str(&self.content);
        s.push('\n');
        s
    }
}
