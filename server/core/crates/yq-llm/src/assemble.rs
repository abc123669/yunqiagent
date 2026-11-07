//! KV-cache 友好组装 —— 把固定区 + 动态区组装为 OpenAI 兼容请求。
//!
//! ## 机关所在
//!
//! provider 的自动前缀缓存以 **messages 数组的最长公共前缀**为复用单位。
//! 因此：
//!
//! ```text
//! messages[0]   = system(固定区全文)   ← 字节级固定，永不变化 → 缓存必然命中
//! messages[1..] = 动态区（记忆 / 历史 / 当前消息）             ← 随便变，不影响缓存
//! ```
//!
//! **纪律**：
//!   · `messages[0]` 只能来自 `StaticPrefix::text()`，不得拼接任何运行时数据
//!   · 记忆注入只能出现在 index ≥ 1（追加语义）
//!   · 时间戳 / UUID / 随机数 一律不得进入 `messages[0]`

use yq_core::message::Message;
use yq_core::prompt::StaticPrefix;

use crate::config::LlmConfig;
use crate::types::{ChatMessage, ChatRequest, StreamOptions, ToolSpec};

/// 组装结果（含诊断信息）。
#[derive(Debug, Clone)]
pub struct AssembledRequest {
    /// 组装好的请求体
    pub request: ChatRequest,
    /// 固定区 SHA256（直接取用 `StaticPrefix`，非反推）
    pub prefix_sha256: String,
    /// 固定区字节长度
    pub prefix_bytes: usize,
    /// 动态区消息条数（不含 `messages[0]`）
    pub dynamic_messages: usize,
}

/// 记忆注入块的标签（固定字符串，便于 provider 侧观察）。
pub const MEMORY_BLOCK_HEADER: &str = "【相关记忆（自动注入，仅供你参考）】";

/// 组装一次 chat 请求。
///
/// 参数：
///   · `prefix`  固定区前缀（**只读**，其 `text()` 原样成为 `messages[0]`）
///   · `memories` 本轮检索到的记忆（追加到动态区首部）
///   · `history`  历史对话
///   · `user`     当前用户消息
///   · `config`   LLM 配置（模型 / 采样参数）
///   · `stream`   是否流式
///   · `tools`    原生协议的工具定义（空 = 走文本协议，请求不含 `tools` 字段）
pub fn build_chat_request(
    prefix: &StaticPrefix,
    memories: &[String],
    history: &[Message],
    user: &str,
    config: &LlmConfig,
    stream: bool,
    tools: Vec<ToolSpec>,
) -> AssembledRequest {
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(2 + history.len());

    // ── messages[0]：固定区全文。字节级固定，绝不掺入动态内容。 ──
    messages.push(ChatMessage::system(prefix.text()));

    // ── messages[1]：记忆注入（仅在有记忆时插入，且只能追加） ──
    if !memories.is_empty() {
        let mut block = String::from(MEMORY_BLOCK_HEADER);
        block.push('\n');
        for m in memories {
            block.push_str("- ");
            block.push_str(m);
            block.push('\n');
        }
        messages.push(ChatMessage::system(block));
    }

    // ── 动态区：历史对话（顺序保持） ──
    for h in history {
        // 原生协议：把 yq-core 消息里的工具调用 / 工具结果映射成 OpenAI 结构。
        // 文本协议下这些字段为空，映射产物与旧版字节一致。
        let msg = if h.role == yq_core::message::Role::Tool {
            if let Some(res) = &h.tool_result {
                ChatMessage::tool_result(res.tool_call_id.clone(), h.content.clone())
            } else {
                ChatMessage {
                    role: h.role.as_str().to_string(),
                    content: h.content.clone(),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                }
            }
        } else if !h.tool_calls.is_empty() {
            let calls = h
                .tool_calls
                .iter()
                .map(|tc| crate::types::ChatToolCall {
                    id: tc.tool_call_id.clone(),
                    call_type: "function".to_string(),
                    function: crate::types::FunctionCallPayload {
                        name: tc.tool_name.clone(),
                        arguments: serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                })
                .collect();
            ChatMessage::assistant_with_calls(h.content.clone(), calls)
        } else {
            ChatMessage {
                role: h.role.as_str().to_string(),
                content: h.content.clone(),
                reasoning_content: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
            }
        };
        messages.push(msg);
    }

    // ── 动态区末位：当前用户消息 ──
    // ── messages[1..]：动态区 ──
    //
    // 注意：`user` 为**空**时跳过追加 —— agent 循环只在首轮注入原始任务
    // （任务已在 `history` 里），后续轮次不应重复追加，
    // 否则模型每轮都被末尾的重复任务"重置"，陷入反复重述计划的循环。
    if !user.is_empty() {
        messages.push(ChatMessage::user(user));
    }

    let dynamic_messages = messages.len() - 1;
    // 原生协议：tools 是请求**顶层**字段，不进 messages，
    // 因此不参与前缀缓存计算；只要 tools 自身字节稳定即可。
    //
    // 注意：`tools` 在下面被 move 进请求体，所以 `tool_choice` 的判断
    // 必须在 move 之前完成（否则 E0382：值已被移动）。
    let tool_choice = if tools.is_empty() {
        None
    } else {
        Some("auto".to_string())
    };

    let request = ChatRequest {
        model: config.model.clone(),
        messages,
        temperature: Some(config.temperature),
        max_tokens: Some(config.max_tokens),
        stream: if stream { Some(true) } else { None },
        stream_options: if stream {
            Some(StreamOptions {
                include_usage: true,
            })
        } else {
            None
        },
        tools,
        tool_choice,
    };

    AssembledRequest {
        request,
        // 直接取用固定区指纹，绝不从拼接串反推
        prefix_sha256: prefix.sha256().to_string(),
        prefix_bytes: prefix.len(),
        dynamic_messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yq_core::persona::Persona;
    use yq_core::prompt::{STATIC_RULES, SYSTEM_PROMPT};
    use yq_core::tools::ToolRegistry;

    fn make_prefix() -> StaticPrefix {
        let registry = ToolRegistry::new_default();
        let tools = registry.render_for_prefix();
        StaticPrefix::build(SYSTEM_PROMPT, &tools, &Persona::default(), STATIC_RULES)
    }

    fn make_config() -> LlmConfig {
        LlmConfig {
            base_url: "http://127.0.0.1:3000/v1".to_string(),
            api_key: "test".to_string(),
            model: "deepseek-chat".to_string(),
            ..LlmConfig::default()
        }
    }

    /// 核心断言：无论动态区如何变化，`messages[0]` 必须逐字节相同。
    #[test]
    fn first_message_is_byte_stable_across_rounds() {
        let prefix = make_prefix();
        let cfg = make_config();

        let mut firsts: Vec<String> = Vec::new();
        for round in 1..=10 {
            let memories: Vec<String> = (0..round)
                .map(|i| format!("记忆[{i}] 第 {round} 轮内容 rand={}", round * 7919))
                .collect();
            let history: Vec<Message> = (0..round)
                .map(|i| Message::user(format!("历史 {i} 轮")))
                .collect();
            let user = format!("第 {round} 轮提问 ts={}", round * 104729);

            let assembled = build_chat_request(&prefix, &memories, &history, &user, &cfg, false, Vec::new());

            // 首个消息必须是固定区
            assert_eq!(assembled.request.messages[0].role, "system");
            firsts.push(assembled.request.messages[0].content.clone());

            // 固定区指纹必须与 prefix 一致
            assert_eq!(assembled.prefix_sha256, prefix.sha256());
        }

        for (i, f) in firsts.iter().enumerate() {
            assert_eq!(
                f,
                &firsts[0],
                "第 {} 轮的 messages[0] 与首轮不一致 —— KV-cache 前缀漂移！",
                i + 1
            );
        }
    }

    /// 固定区一旦变化（改 persona），`messages[0]` 必须随之变化。
    #[test]
    fn first_message_changes_when_persona_changes() {
        let registry = ToolRegistry::new_default();
        let tools = registry.render_for_prefix();
        let cfg = make_config();

        let mut persona_a = Persona::default();
        let prefix_a = StaticPrefix::build(SYSTEM_PROMPT, &tools, &persona_a, STATIC_RULES);
        let req_a = build_chat_request(&prefix_a, &[], &[], "你好", &cfg, false, Vec::new());

        persona_a.tone = "严肃克制".to_string();
        let prefix_b = StaticPrefix::build(SYSTEM_PROMPT, &tools, &persona_a, STATIC_RULES);
        let req_b = build_chat_request(&prefix_b, &[], &[], "你好", &cfg, false, Vec::new());

        assert_ne!(
            req_a.request.messages[0].content,
            req_b.request.messages[0].content
        );
        assert_ne!(req_a.prefix_sha256, req_b.prefix_sha256);
    }

    /// 动态区内容不同时，前缀指纹必须保持不变。
    #[test]
    fn prefix_hash_unaffected_by_dynamic_content() {
        let prefix = make_prefix();
        let cfg = make_config();

        let a = build_chat_request(&prefix, &[], &[], "短", &cfg, false, Vec::new());
        let long_memories: Vec<String> = (0..50).map(|i| format!("记忆 {i}")).collect();
        let long_history: Vec<Message> = (0..30)
            .map(|i| Message::assistant(format!("回复 {i}")))
            .collect();
        let b = build_chat_request(
            &prefix,
            &long_memories,
            &long_history,
            "一条长得多的用户消息，含时间戳 2026-10-05T12:34:56",
            &cfg,
            false,
            Vec::new(),
        );

        assert_eq!(a.prefix_sha256, b.prefix_sha256);
        assert_ne!(
            a.request.messages.len(),
            b.request.messages.len(),
            "动态消息条数应不同"
        );
        assert_eq!(a.request.messages[0].content, b.request.messages[0].content);
    }

    /// 记忆为空时不应插入空的记忆块（避免无意义消息）。
    #[test]
    fn empty_memories_insert_no_block() {
        let prefix = make_prefix();
        let cfg = make_config();
        let assembled = build_chat_request(&prefix, &[], &[], "你好", &cfg, false, Vec::new());
        assert_eq!(assembled.request.messages.len(), 2); // system + user
        assert_eq!(assembled.request.messages[1].role, "user");
    }
}
