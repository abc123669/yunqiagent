//! OpenAI 兼容的请求 / 响应类型。
//!
//! 所有字段名 **snake_case**，与 OpenAI Chat Completions API 对齐。

use serde::{Deserialize, Serialize};

/// 反序列化 `content` 时容忍 `null`。
///
/// OpenAI 原生协议下，assistant 发起 tool_calls 时 `content` 常为 `null`
/// （而非空字符串）。严格按 `String` 反序列化会直接失败，导致整个响应被丢弃。
fn null_to_empty_string<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<String> = Option::deserialize(d)?;
    Ok(v.unwrap_or_default())
}

/// OpenAI 原生协议中单个工具调用的函数载荷。
///
/// 注意：原生协议里 `arguments` 是 **JSON 字符串**（不是对象），
/// 与文本协议里直接嵌 JSON 对象不同。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FunctionCallPayload {
    /// 工具名（snake_case）
    pub name: String,
    /// 参数的 JSON 字符串，如 `"{\"path\":\"/tmp/x\"}"`
    #[serde(default)]
    pub arguments: String,
}

/// assistant 侧的原生工具调用（OpenAI `tool_calls` 数组成员）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatToolCall {
    /// 调用唯一标识（role=tool 的结果消息用它配对）
    pub id: String,
    /// 固定为 `"function"`
    #[serde(rename = "type", default)]
    pub call_type: String,
    /// 函数名 + 参数
    #[serde(default)]
    pub function: FunctionCallPayload,
}

/// 一条 chat 消息（OpenAI 格式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// 角色：system / user / assistant / tool
    pub role: String,
    /// 文本内容
    #[serde(default, deserialize_with = "null_to_empty_string")]
    pub content: String,
    /// **思维链内容**（推理模型专用，如 LongCat-2.5-Preview / deepseek-reasoner）
    ///
    /// 推理模型把思考过程放在这个字段，正文仍在 `content`。
    /// 只读性字段：我们不回传它，仅用于展示与诊断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// assistant 发起的**原生**工具调用数组（OpenAI 原生协议）。
    ///
    /// 仅在 `role == "assistant"` 且模型走了原生协议时非空；
    /// 序列化时为空则整个字段省略，不影响其它消息的形态。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
    /// `role == "tool"` 的结果消息携带的配对 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// 构造 system 消息。
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// 构造 user 消息。
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// 构造 assistant 消息。
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// 构造带原生 tool_calls 的 assistant 消息（多轮对话回传模型自身调用时用）。
    pub fn assistant_with_calls(
        content: impl Into<String>,
        tool_calls: Vec<ChatToolCall>,
    ) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            reasoning_content: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// 构造 `role=tool` 的结果消息（必须携带配对的 `tool_call_id`）。
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// 工具协议模式。
///
/// - `Native`：走 OpenAI 原生 `tools` / `tool_calls` 字段
/// - `Text`：走固定区文本协议（`<tool_call>{...}</tool_call>`，老路径）
/// - `Auto`：优先原生，原生失败（网关报错 / 模型不支持）降级文本
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProtocol {
    /// 优先原生，失败降级文本（默认）
    #[default]
    Auto,
    /// 强制原生
    Native,
    /// 强制文本协议
    Text,
}

impl ToolProtocol {
    /// 从环境变量 / 字符串解析（非法值回退 Auto）。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "native" => Self::Native,
            "text" => Self::Text,
            _ => Self::Auto,
        }
    }
}

/// Chat Completions 请求体。
///
/// **KV-cache 关键**：`messages[0]` 必须是字节级固定的 system 消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 模型名
    pub model: String,
    /// 消息数组（`messages[0]` = 固定区）
    pub messages: Vec<ChatMessage>,
    /// 采样温度
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 最大生成 token
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 是否流式
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// 流式时是否带 usage 统计
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// 原生协议的工具定义数组（**请求顶层字段**，不是 messages 成员）。
    ///
    /// ## KV-cache 关键
    ///
    /// `tools` 位于请求顶层，不在 messages 前缀内 ——
    /// provider 的 prompt 缓存以 messages 最长公共前缀为单位，
    /// `tools` 只要**自身字节级稳定**（固定顺序 + 字面量 schema），
    /// 就不会引起 messages 前缀的任何漂移。
    ///
    /// 序列化时为空则整个字段省略，文本协议请求形态与旧版完全一致。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// 工具选择策略（原生协议用，通常 `"auto"`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
}

/// OpenAI 原生协议的单个工具定义（请求 `tools` 数组成员）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// 固定为 `"function"`
    #[serde(rename = "type")]
    pub spec_type: String,
    /// 函数定义
    pub function: ToolFunctionSpec,
}

/// 函数级工具定义。
///
/// `parameters_json` 是 **原始 JSON 字符串**（直接取自
/// `ToolDefinition.input_schema` 的编译期字面量），序列化时用
/// `serde_json::value::RawValue` 原样嵌入 —— 全程不经过
/// `serde_json::Value`，杜绝 Map 顺序漂移，保证字节级稳定。
#[derive(Debug, Clone)]
pub struct ToolFunctionSpec {
    /// 工具名
    pub name: String,
    /// 工具描述
    pub description: String,
    /// JSON Schema 的原始字面量字符串
    pub parameters_json: String,
}

impl ToolFunctionSpec {
    /// 从 yq-core 的工具定义构造（零解析：schema 直接透传）。
    pub fn from_definition(def: &yq_core::tools::ToolDefinition) -> Self {
        Self {
            name: def.name.to_string(),
            description: def.description.to_string(),
            parameters_json: def.input_schema.to_string(),
        }
    }
}

impl Serialize for ToolFunctionSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("ToolFunctionSpec", 3)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("description", &self.description)?;
        // 原样嵌入字面量 JSON —— 不解析、不重建，key 顺序与源码字面量完全一致。
        // 构造时（from_definition）已校验过合法性，这里的 map_err 只是防御。
        let raw = serde_json::value::RawValue::from_string(self.parameters_json.clone())
            .map_err(serde::ser::Error::custom)?;
        s.serialize_field("parameters", raw.as_ref())?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for ToolFunctionSpec {
    /// 反序列化路径可以走 `serde_json::Value`（请求方向不涉字节稳定性，
    /// 只有**序列化**才要求字面量直通；且本端几乎不反序列化 tools）。
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Shape {
            name: String,
            #[serde(default)]
            description: String,
            #[serde(default)]
            parameters: serde_json::Value,
        }
        let s = Shape::deserialize(d)?;
        Ok(Self {
            name: s.name,
            description: s.description,
            parameters_json: serde_json::to_string(&s.parameters)
                .map_err(serde::de::Error::custom)?,
        })
    }
}

/// 把工具定义列表渲染成原生协议的 `tools` 数组。
///
/// 顺序严格按传入顺序（与固定区渲染顺序一致），schema 走字面量直通。
pub fn build_tools_array(defs: &[yq_core::tools::ToolDefinition]) -> Vec<ToolSpec> {
    defs.iter()
        .map(|d| ToolSpec {
            spec_type: "function".to_string(),
            function: ToolFunctionSpec::from_definition(d),
        })
        .collect()
}

/// 流式选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    /// 在流末尾附带 usage
    pub include_usage: bool,
}

/// token 用量统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// 输入 token
    #[serde(default)]
    pub prompt_tokens: u32,
    /// 输出 token
    #[serde(default)]
    pub completion_tokens: u32,
    /// 总 token
    #[serde(default)]
    pub total_tokens: u32,
    /// 命中缓存的输入 token（部分 provider 提供）
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// 输入 token 明细（缓存命中统计）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    /// 命中 prompt 缓存的 token 数
    #[serde(default)]
    pub cached_tokens: u32,
}

/// Chat Completions 非流式响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// 响应 ID
    #[serde(default)]
    pub id: String,
    /// 模型名
    #[serde(default)]
    pub model: String,
    /// 生成结果
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// 用量
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// 单条生成结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    /// 序号
    #[serde(default)]
    pub index: u32,
    /// 消息体
    #[serde(default)]
    pub message: Option<ChatMessage>,
    /// 结束原因
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl ChatResponse {
    /// 取出第一个 choice 的文本内容（正文）。
    ///
    /// 推理模型的思考过程在 `reasoning_content`，**不**作为答复返回。
    pub fn first_text(&self) -> Option<String> {
        self.choices
            .first()
            .and_then(|c| c.message.as_ref())
            .map(|m| m.content.clone())
    }

    /// 取出第一个 choice 的思维链（若有）。
    pub fn first_reasoning(&self) -> Option<String> {
        self.choices
            .first()
            .and_then(|c| c.message.as_ref())
            .and_then(|m| m.reasoning_content.clone())
    }

    /// 结束原因（`stop` / `length` / `tool_calls` 等）。
    pub fn finish_reason(&self) -> Option<String> {
        self.choices.first().and_then(|c| c.finish_reason.clone())
    }

    /// 取出第一个 choice 的**原生**工具调用数组（OpenAI 原生协议）。
    ///
    /// 模型未走原生协议（如只输出了文本协议标记）时返回空切片。
    pub fn first_tool_calls(&self) -> &[ChatToolCall] {
        self.choices
            .first()
            .and_then(|c| c.message.as_ref())
            .map(|m| m.tool_calls.as_slice())
            .unwrap_or(&[])
    }

    /// 截断检测：`finish_reason == "length"` 时是否有被截断的工具调用。
    ///
    /// 文本协议下截断的工具调用块只是解析失败（静默丢失）；
    /// 原生协议下我们能**精确知道**哪些调用被截断 ——
    /// 参数字符串不是合法 JSON，即可判定该调用需要模型重发。
    pub fn truncated_tool_call_ids(&self) -> Vec<String> {
        if self.finish_reason().as_deref() != Some("length") {
            return Vec::new();
        }
        self.first_tool_calls()
            .iter()
            .filter(|c| {
                serde_json::from_str::<serde_json::Value>(&c.function.arguments).is_err()
            })
            .map(|c| c.id.clone())
            .collect()
    }
}

/// 流式 SSE 事件（data 段解析结果）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    /// 响应 ID
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    /// 增量结果
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    /// 用量（流末尾）
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// 流式增量选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChoice {
    /// 序号
    #[serde(default)]
    pub index: u32,
    /// 增量
    #[serde(default)]
    pub delta: Option<Delta>,
    /// 结束原因
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// 增量内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    /// 角色（首个 chunk 携带）
    #[serde(default)]
    pub role: Option<String>,
    /// 增量文本（正文）
    #[serde(default)]
    pub content: Option<String>,
    /// 增量思维链（推理模型）
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Native tool arguments arrive incrementally, keyed by the call index.
    #[serde(default)]
    pub tool_calls: Vec<StreamToolCall>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamToolCall {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub call_type: Option<String>,
    #[serde(default)]
    pub function: Option<StreamFunctionCall>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamFunctionCall {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

impl StreamEvent {
    /// 提取增量文本（**正文**，不含思维链）。
    pub fn delta_text(&self) -> Option<&str> {
        self.choices
            .first()
            .and_then(|c| c.delta.as_ref())
            .and_then(|d| d.content.as_deref())
    }

    /// 提取增量思维链（推理模型）。
    pub fn delta_reasoning(&self) -> Option<&str> {
        self.choices
            .first()
            .and_then(|c| c.delta.as_ref())
            .and_then(|d| d.reasoning_content.as_deref())
    }

    /// 是否是最后一个有效 chunk（finish_reason 出现）。
    pub fn is_final(&self) -> bool {
        self.choices
            .first()
            .and_then(|c| c.finish_reason.as_ref())
            .is_some()
    }
}

/// 面向调用方的 token 分片（TUI / REPL 逐 token 渲染用）。
#[derive(Debug, Clone)]
pub struct TokenChunk {
    /// 文本分片
    pub text: String,
    /// 是否最后一片
    pub is_final: bool,
}
