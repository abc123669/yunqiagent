//! 斜杠命令 —— 会话控制指令
//!
//! 支持的命令（在群/私聊里以 `/` 开头发送）：
//!
//! | 命令 | 作用 |
//! |---|---|
//! | `/new` | 开启新对话（重置会话与历史） |
//! | `/model` | 查看当前模型 |
//! | `/model <名称>` | 切换模型（运行时生效） |
//! | `/usage` | 查看本会话 token 使用量 |
//! | `/compress` | 压缩当前会话历史（保留摘要） |
//! | `/help` | 命令帮助 |
//!
//! 解析与执行分离：`parse()` 是纯函数（可单测），`handle_command()` 也是纯函数，
//! 由上层（yq-cli）完成具体执行。这样 REPL 和 Serve 可以复用同一套逻辑，绝不复制文案。

use std::fmt;

/// 一条命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 开启新对话
    New,
    /// 查看当前模型
    ShowModel,
    /// 切换模型
    SetModel(String),
    /// 查看用量
    Usage,
    /// 压缩会话
    Compress,
    /// 帮助
    Help,
    /// 无法识别的命令（保留原文，便于回提示）
    Unknown(String),
}

/// 命令解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult {
    /// 是一条命令
    Command(Command),
    /// 不是命令（普通消息，应交给 Agent）
    NotACommand,
}

/// 是否为潜在的斜杠命令（以 `/` 开头且长度 > 1）。
///
/// 注意：单独的 `/` 不算命令（可能只是用户打了个斜杠）。
pub fn looks_like_command(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('/') && t.chars().count() > 1
}

/// 解析一条消息为命令。
///
/// **纯函数**：不碰任何状态，便于测试与复用。
pub fn parse(text: &str) -> ParseResult {
    let t = text.trim();
    if !looks_like_command(t) {
        return ParseResult::NotACommand;
    }

    // 去掉开头的 `/`，按空白切分
    let body = t.trim_start_matches('/');
    let mut parts = body.split_whitespace();
    let name = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest: Vec<&str> = parts.collect();

    let cmd = match name.as_str() {
        "new" | "reset" | "clear" => Command::New,
        "model" | "models" | "m" => {
            if rest.is_empty() {
                Command::ShowModel
            } else {
                Command::SetModel(rest.join(" "))
            }
        }
        "usage" | "u" => Command::Usage,
        "compress" | "compact" => Command::Compress,
        "help" | "h" | "" => Command::Help,
        other => Command::Unknown(other.to_string()),
    };

    ParseResult::Command(cmd)
}

/// 命令处理的「效应」——告诉调用方需要做什么动作。
///
/// 这是 REPL 与 Serve 共享的核心设计：把「判断是不是命令」「应该回什么文本」
/// 剥离成纯函数，真正需要操作数据库、发 QQ API、清空历史的侧部分交给调用方。
///
/// 为什么是这样三种效应？
///   - `Reply(String)`：直接回文本给前端，不进 LLM（如/help 的文案固定）
///   - `ResetHistory`：调用方自己决定如何清空历史（REPL 清本地 vec，Serve 清 DB + 内存缓存）
///   - `NotACommand`：交回主流程走 LLM（让模型自己去理解用户的自然语言）
///
/// **注意**：只有「不需要外部状态就能回答」的命令走这里（`/new` `/help`
/// 以及未知命令）。`/model` `/usage` `/compress` 需要 Agent 的实时状态，
/// 由调用方在拿到 `Command` 后用本模块的 `*_text()` 系列函数组装回复 ——
/// 那些函数同样是**唯一文案来源**，REPL 与 Serve 都调它们，不各自抄一份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandEffect {
    /// 直接回复给定文本，不调用 LLM
    Reply(String),
    /// 请求重置历史（调用方自行清空）
    ResetHistory,
    /// 不是命令，交给主流程
    NotACommand,
}

impl fmt::Display for CommandEffect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandEffect::Reply(s) => write!(f, "REPLY:{}", s.len()),
            CommandEffect::ResetHistory => write!(f, "RESET"),
            CommandEffect::NotACommand => write!(f, "NOT_A_CMD"),
        }
    }
}

/// 纯函数：把「不需要外部状态」的命令文本转换为效应。
///
/// **不依赖任何外部状态**，只依赖输入的字符串本身。
/// Serve 与 REPL 共享此实现，保证行为完全一致。
///
/// 需要 Agent 实时状态的命令（`/model` `/usage` `/compress`）在这里返回
/// [`CommandEffect::NotACommand`] **不是**「交回 LLM」，而是明确告知调用方
/// 「这条得你自己拿 Agent 状态来答」。调用方应当先用 [`parse`] 判断，
/// 再对这三个命令调用对应的 `*_text()` 函数。见 [`needs_agent_state`]。
pub fn handle_command(text: &str) -> CommandEffect {
    match parse(text) {
        ParseResult::NotACommand => CommandEffect::NotACommand,

        ParseResult::Command(cmd) => match cmd {
            Command::New => CommandEffect::ResetHistory,
            Command::Help => CommandEffect::Reply(help_text()),
            Command::Unknown(name) => CommandEffect::Reply(unknown_command_text(&name)),

            // 需要 Agent 状态 —— 调用方用 needs_agent_state() 判出来后再走
            // model_text / usage_text / compress 系列。这里返回 NotACommand
            // 会被 needs_agent_state() 拦住，不会被误当普通消息发给模型。
            Command::ShowModel | Command::SetModel(_) | Command::Usage | Command::Compress => {
                CommandEffect::NotACommand
            }
        },
    }
}

/// 该命令是否需要 Agent 的实时状态才能回答。
///
/// REPL 与 Serve 都必须在把文本交给 LLM **之前**调用它 ——
/// 否则 `/model` 会被当成普通消息发给模型，让它编一段关于模型名的胡话。
pub fn needs_agent_state(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::ShowModel | Command::SetModel(_) | Command::Usage | Command::Compress
    )
}

/// 开启新对话的回复。
pub fn new_conversation_text() -> String {
    "已开启新对话。之前的上下文已清空。".to_string()
}

/// 查看当前模型。
pub fn model_text(model: &str) -> String {
    format!("当前模型：{model}\n（用 /model <名称> 切换）")
}

/// 切换模型成功。
pub fn model_switched_text(model: &str) -> String {
    format!("已切换到模型：{model}")
}

/// 切换模型失败。
pub fn model_switch_failed_text(err: &str) -> String {
    format!("切换失败：{err}")
}

/// 未知命令。
pub fn unknown_command_text(name: &str) -> String {
    format!("未知命令：/{name}\n\n{}", help_text())
}

/// 会话没有可压缩的历史。
pub fn compress_empty_text() -> String {
    "当前会话没有可压缩的历史。".to_string()
}

/// 压缩完成。
pub fn compress_done_text(before: usize, summary: &str) -> String {
    format!("已压缩会话：{before} 条历史 → 1 条摘要。\n\n摘要：\n{summary}")
}

/// 压缩失败。
pub fn compress_failed_text(err: &str) -> String {
    format!("压缩失败：{err}")
}

/// 交给模型做压缩的提示词 —— REPL 与 Serve 必须用同一份，
/// 否则两个入口压出来的摘要格式会不一致。
pub fn compress_prompt(transcript: &str) -> String {
    format!(
        "请把下面这段对话压缩成一份**要点摘要**（不超过 200 字），\
         保留：用户的关键信息、已确认的结论、待办事项。不要加任何前言后语。\n\n\
         === 对话 ===\n{transcript}"
    )
}

/// 压缩后替换历史的占位用户消息。
pub const COMPRESS_PLACEHOLDER: &str = "（此前对话已压缩为摘要）";

/// 完整帮助（含只在 serve 路径生效的 `/verbose` `/stop`）。
pub fn help_text_full() -> String {
    format!(
        "{}\n· /verbose on|off —— 详细汇报（连思考片段一起发）\n· /stop —— 中断正在执行的任务",
        help_text()
    )
}

/// `/verbose` 查询当前状态。
pub fn verbose_state_text(on: bool) -> String {
    format!(
        "详细汇报当前：{}\n（/verbose on 开启，/verbose off 关闭）\n\
         开启后每轮会把模型的思考片段一起发给你。",
        if on { "开启" } else { "关闭" }
    )
}

/// `/verbose on` 的回复。
pub fn verbose_on_text() -> String {
    "详细汇报已开启：之后每轮会附带模型思考片段。".to_string()
}

/// `/verbose off` 的回复。
pub fn verbose_off_text() -> String {
    "详细汇报已关闭：之后只报工具调用。".to_string()
}

/// 命令帮助文本（纯文本，可直接发到 QQ 或在 REPL 打印）。
pub fn help_text() -> String {
    [
        "可用命令：",
        "· /new —— 开启新对话（清空上下文）",
        "· /model —— 查看当前模型",
        "· /model <名称> —— 切换模型",
        "· /usage —— 查看本会话 token 用量",
        "· /compress —— 压缩会话历史（保留摘要）",
        "· /help —— 显示这份帮助",
    ]
    .join("\n")
}

/// 格式化 token 用量（纯文本）。
pub fn format_usage(
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    rounds: u64,
) -> String {
    let total = prompt_tokens + completion_tokens;
    // checked_div 明确表达「rounds 为 0 时不做除法」，而不是靠 if 守卫
    let avg = total.checked_div(rounds).unwrap_or(0);
    format!(
        "本会话用量（模型 {model}）：\n\
         · 输入 token：{prompt_tokens}\n\
         · 输出 token：{completion_tokens}\n\
         · 合计：{total}\n\
         · 对话轮数：{rounds}\n\
         · 平均每轮：{avg}",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_slash_command() {
        assert!(looks_like_command("/new"));
        assert!(looks_like_command("  /model gpt"));
        assert!(!looks_like_command("你好"));
        assert!(!looks_like_command("/"), "单个斜杠不算命令");
    }

    #[test]
    fn parses_new_command() {
        assert_eq!(parse("/new"), ParseResult::Command(Command::New));
        assert_eq!(parse("/reset"), ParseResult::Command(Command::New));
        assert_eq!(parse("/clear"), ParseResult::Command(Command::New));
    }

    #[test]
    fn parses_model_without_argument() {
        assert_eq!(parse("/model"), ParseResult::Command(Command::ShowModel));
    }

    #[test]
    fn parses_model_with_argument() {
        assert_eq!(
            parse("/model deepseek-v4-pro"),
            ParseResult::Command(Command::SetModel("deepseek-v4-pro".to_string()))
        );
    }

    #[test]
    fn model_argument_is_trimmed_and_normalized() {
        assert_eq!(
            parse("/model  my  model "),
            ParseResult::Command(Command::SetModel("my model".to_string()))
        );
        assert_eq!(
            parse("/model deepseek-chat"),
            ParseResult::Command(Command::SetModel("deepseek-chat".to_string()))
        );
    }

    #[test]
    fn parses_usage_and_compress() {
        assert_eq!(parse("/usage"), ParseResult::Command(Command::Usage));
        assert_eq!(parse("/compress"), ParseResult::Command(Command::Compress));
        assert_eq!(parse("/compact"), ParseResult::Command(Command::Compress));
    }

    #[test]
    fn parses_help_and_empty_slash() {
        assert_eq!(parse("/help"), ParseResult::Command(Command::Help));
        assert_eq!(parse("/ "), ParseResult::NotACommand);
    }

    #[test]
    fn unknown_command_keeps_name() {
        assert_eq!(
            parse("/foobar abc"),
            ParseResult::Command(Command::Unknown("foobar".to_string()))
        );
    }

    #[test]
    fn plain_message_is_not_a_command() {
        assert_eq!(parse("你好啊"), ParseResult::NotACommand);
        // 关键回归：不能让 "帮我看看 /tmp 目录" 里的 /tmp 被当命令
        assert_eq!(parse("帮我看看 /tmp 目录"), ParseResult::NotACommand);
    }

    #[test]
    fn command_is_case_insensitive() {
        assert_eq!(parse("/NEW"), ParseResult::Command(Command::New));
        assert_eq!(parse("/Model"), ParseResult::Command(Command::ShowModel));
    }

    #[test]
    fn help_text_is_plain_no_markdown() {
        let h = help_text();
        assert!(!h.contains("**"), "帮助文本不应含 Markdown 粗体");
        assert!(!h.contains('|'), "帮助文本不应含表格竖杠");
        assert!(h.contains("/new"));
        assert!(h.contains("/compress"));
    }

    #[test]
    fn format_usage_computes_totals() {
        let s = format_usage("deepseek-chat", 1000, 500, 5);
        assert!(s.contains("1500"), "应含合计");
        assert!(s.contains("300"), "应含平均每轮 300");
        assert!(!s.contains('|'), "不应含表格竖杠");
    }

    #[test]
    fn format_usage_handles_zero_rounds() {
        let s = format_usage("m", 0, 0, 0);
        assert!(s.contains("平均每轮：0"));
    }

    // ---------- CommandEffect 的纯函数映射测试 ----------

    #[test]
    fn handle_new_returns_reset_history() {
        let effect = handle_command("/new");
        assert_eq!(effect, CommandEffect::ResetHistory);
    }

    #[test]
    fn handle_help_returns_reply_with_content() {
        let effect = handle_command("/help");
        if let CommandEffect::Reply(text) = effect {
            assert!(text.contains("/new"));
            assert!(text.contains("/model"));
            assert!(text.contains("/compress"));
            assert!(!text.contains("***"), "不应有 Markdown 粗体");
        } else {
            panic!("Expected Reply，Got {:?}", effect);
        }
    }

    #[test]
    fn stateful_commands_are_flagged_not_answered_with_a_lie() {
        // /model /usage /compress 需要 Agent 实时状态。曾经它们在这里被
        // 硬编码成「尚不支持…请在 serve 模式使用」，一旦 REPL 接上就会
        // 对用户撒谎（功能其实可用）。现在必须由 needs_agent_state 拦下，
        // 交给调用方用真实状态回答。
        for text in ["/model", "/model gpt-4", "/usage", "/compress"] {
            let cmd = match parse(text) {
                ParseResult::Command(c) => c,
                other => panic!("{text} 应解析为命令，实际 {other:?}"),
            };
            assert!(
                needs_agent_state(&cmd),
                "{text} 必须标记为需要 Agent 状态，否则会被当普通消息发给模型"
            );
            // 且 handle_command 不能给出带误导性的答复
            assert_eq!(
                handle_command(text),
                CommandEffect::NotACommand,
                "{text} 不应由纯函数回答"
            );
        }
    }

    #[test]
    fn stateless_commands_are_answered_directly() {
        assert_eq!(handle_command("/new"), CommandEffect::ResetHistory);
        let help = handle_command("/help");
        assert!(matches!(help, CommandEffect::Reply(t) if t.contains("/model")));
        let unknown = handle_command("/nope");
        assert!(matches!(unknown, CommandEffect::Reply(t) if t.contains("未知命令")));
    }

    #[test]
    fn shared_text_builders_are_the_single_source() {
        // 这些文案由 REPL 与 Serve 共用；测试锁住关键信息，防止某一侧被改跑偏
        assert!(new_conversation_text().contains("清空"));
        assert!(model_text("m1").contains("m1"));
        assert!(model_switched_text("m2").contains("m2"));
        assert!(model_switch_failed_text("boom").contains("boom"));
        assert!(compress_empty_text().contains("没有可压缩"));
        assert!(compress_done_text(5, "摘要内容").contains("5 条历史"));
        assert!(compress_done_text(5, "摘要内容").contains("摘要内容"));
        assert!(compress_failed_text("bad").contains("bad"));
        assert!(verbose_state_text(true).contains("开启"));
        assert!(verbose_state_text(false).contains("关闭"));
        // 完整帮助必须包含 serve 专属命令
        let full = help_text_full();
        assert!(full.contains("/verbose") && full.contains("/stop"));
        // 压缩提示词两侧共用同一份
        assert!(compress_prompt("对话内容").contains("对话内容"));
        assert!(compress_prompt("x").contains("200 字"));
    }

    #[test]
    fn handle_unknown_returns_reply() {
        let effect = handle_command("/foobar");
        assert!(matches!(effect, CommandEffect::Reply(_)));
        if let CommandEffect::Reply(text) = effect {
            assert!(text.contains("未知命令：/foobar"));
            assert!(text.contains("/help"), "应附带帮助文本");
        }
    }

    #[test]
    fn plain_input_returns_not_a_command() {
        assert_eq!(handle_command("你好啊"), CommandEffect::NotACommand);
        // 关键回归测试
        assert_eq!(
            handle_command("帮我看看 /tmp 目录"),
            CommandEffect::NotACommand
        );
        assert_eq!(
            handle_command("访问 https://example.com"),
            CommandEffect::NotACommand
        );
    }

    #[test]
    fn case_insensitive_effect() {
        assert_eq!(handle_command("/NEW"), CommandEffect::ResetHistory);
        assert_eq!(handle_command("/New"), CommandEffect::ResetHistory);
        let effect = handle_command("/HELP");
        assert!(matches!(effect, CommandEffect::Reply(text) if !text.is_empty()));
    }
}
