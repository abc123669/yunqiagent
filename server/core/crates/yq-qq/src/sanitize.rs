//! 输出净化 —— 拦截模型"说漏嘴"的框架内部元信息
//!
//! ## 问题（2026-10-06 实测）
//!
//! 用户问「我的 MC PVP 服务器怎么炸掉了？」，yunqiagent 回了一段：
//!
//! ```text
//! [CONTEXT_COMPACTION — REFERENCE ONLY] Earlier turns were compacted
//! into the summary below...
//! Goal
//! User is building out their Minecraft server infrastructure...
//! ```
//!
//! —— 这是 **Hermes 框架**的会话压缩摘要格式，而 yunqiagent 根本没有这套东西。
//! 模型（LongCat-2.5-Preview，推理模型）的**训练数据里见过类似的 agent 框架产物**，
//! 在个别情况下会"回忆"出来当成自己的输出。这是**偶发幻觉**（复现不出），
//! 但对用户来说就是「bot 突然说起莫名其妙的话」，观感极差。
//!
//! ## 本模块做什么
//!
//! 在**发送前**检测这类元输出，替换成一句诚实的说明 + 建议重试。
//!
//! ## 设计取舍
//!
//! · **保守**：只拦特征明确的框架标记，避免误杀正常内容
//!   （比如用户自己发来的日志里含这类字样，不该被拦——但那是**输入**路径，
//!   本模块只作用于**模型输出**）
//! · **可解释**：拦截后告诉用户发生了什么，不静默吞掉
//! · **可扩展**：特征以常量数组维护，加新特征只需加一行

/// 特征模式：命中任一即判定为"框架元输出"。
///
/// 每条都是 (特征子串, 人类可读的原因)。
const META_PATTERNS: &[(&str, &str)] = &[
    ("[CONTEXT_COMPACTION", "会话压缩摘要（框架内部产物）"),
    ("[... REFERENCE ONLY]", "框架内部引用标记"),
    ("Earlier turns were compacted", "会话压缩措辞"),
    ("compacted into the summary below", "会话压缩措辞"),
    ("REFERENCE ONLY]", "框架内部引用标记"),
    ("<system-reminder>", "系统提示标记"),
    ("</system-reminder>", "系统提示标记"),
    ("[INST]", "指令模板标记"),
    ("<<SYS>>", "指令模板标记"),
    ("### HUMAN:", "对话模板标记"),
    ("### ASSISTANT:", "对话模板标记"),
];

/// 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SanitizeVerdict {
    /// 内容正常，原样发送
    Clean,
    /// 命中元输出特征，需要替换
    MetaOutput {
        /// 命中的特征
        matched: String,
        /// 可读原因
        reason: String,
    },
    /// 内容整体为空/无意义
    Empty,
}

impl SanitizeVerdict {
    /// 是否应当替换。
    pub fn should_replace(&self) -> bool {
        !matches!(self, SanitizeVerdict::Clean)
    }
}

/// 检测模型输出是否为"框架元输出"。
///
/// 只在**开头附近**匹配（前 500 字符）—— 框架元输出总是出现在回复起始处；
/// 若正文中间偶然提到这些词（例如用户在讨论 agent 框架），不应误杀。
pub fn inspect(text: &str) -> SanitizeVerdict {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return SanitizeVerdict::Empty;
    }

    // 只看开头 —— 元输出必然在起始位置
    let head: String = trimmed.chars().take(500).collect();

    for (pat, reason) in META_PATTERNS {
        if head.contains(pat) {
            return SanitizeVerdict::MetaOutput {
                matched: (*pat).to_string(),
                reason: (*reason).to_string(),
            };
        }
    }

    SanitizeVerdict::Clean
}

/// 生成给用户的替换文本。
///
/// **不静默吞掉** —— 如实说明发生了什么，并给出下一步建议。
pub fn replacement(verdict: &SanitizeVerdict) -> Option<String> {
    match verdict {
        SanitizeVerdict::Clean => None,
        SanitizeVerdict::Empty => {
            Some("（模型这一轮没有产出有效内容，请再说一次或换个说法）".to_string())
        }
        SanitizeVerdict::MetaOutput { reason, .. } => Some(format!(
            "（本堂主刚才说岔了——模型输出了无关的{}，已拦下。请再问一次，或者把问题说得更具体些。）",
            reason
        )),
    }
}

/// 一步到位：检测 + 替换。返回应当发送的文本。
///
/// `Clean` 时原样返回（零拷贝语义上的零开销）。
pub fn sanitize_or_replace(text: &str) -> String {
    let verdict = inspect(text);
    match replacement(&verdict) {
        Some(fixed) => {
            tracing::warn!(
                "拦截模型元输出（{}）：{}",
                match &verdict {
                    SanitizeVerdict::MetaOutput { matched, .. } => matched.as_str(),
                    _ => "空回复",
                },
                text.chars().take(120).collect::<String>()
            );
            fixed
        }
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes_through() {
        assert_eq!(
            inspect("服务器已经修好了，玩家可以进。"),
            SanitizeVerdict::Clean
        );
        assert!(!inspect("正常回复").should_replace());
    }

    #[test]
    fn detects_context_compaction_marker() {
        // 实测抓到的原样输出
        let bad = "[CONTEXT_COMPACTION — REFERENCE ONLY] Earlier turns were compacted \
                   into the summary below.\nGoal\nUser is building out...";
        let v = inspect(bad);
        assert!(v.should_replace(), "应命中元输出: {:?}", v);
        assert!(matches!(v, SanitizeVerdict::MetaOutput { .. }));
    }

    #[test]
    fn detects_system_reminder() {
        assert!(inspect("<system-reminder>内部提示</system-reminder>").should_replace());
    }

    #[test]
    fn detects_chat_template_markers() {
        assert!(inspect("### HUMAN: 你好\n### ASSISTANT: 呀吼").should_replace());
        assert!(inspect("<<SYS>>你是...").should_replace());
    }

    #[test]
    fn empty_is_flagged() {
        assert_eq!(inspect(""), SanitizeVerdict::Empty);
        assert_eq!(inspect("   \n  "), SanitizeVerdict::Empty);
    }

    #[test]
    fn only_checks_the_head() {
        // 正文中间提到这些词（例如在讨论框架）不该误杀
        let long_text = format!(
            "{}这里是在讨论 agent 框架的实现细节，提到 CONTEXT_COMPACTION 这个词。",
            "前言".repeat(300)
        );
        assert_eq!(
            inspect(&long_text),
            SanitizeVerdict::Clean,
            "尾部的偶然提及不该被拦"
        );
    }

    #[test]
    fn replacement_is_kind_and_informative() {
        let v = inspect("[CONTEXT_COMPACTION — REFERENCE ONLY] ...");
        let msg = replacement(&v).expect("应生成替换文本");
        assert!(msg.contains("说岔了"), "应口语化说明: {}", msg);
        assert!(!msg.contains('|'), "不应含 Markdown 表格符");
    }

    #[test]
    fn empty_gets_its_own_message() {
        let msg = replacement(&SanitizeVerdict::Empty).unwrap();
        assert!(msg.contains("没有产出有效内容"));
    }

    #[test]
    fn sanitize_or_replace_passes_clean() {
        let ok = "PVP 服已经重启好了，欢迎进来玩。";
        assert_eq!(sanitize_or_replace(ok), ok);
    }

    #[test]
    fn sanitize_or_replace_fixes_meta() {
        let bad = "[CONTEXT_COMPACTION — REFERENCE ONLY] blah blah";
        let fixed = sanitize_or_replace(bad);
        assert_ne!(fixed, bad, "应被替换");
        assert!(!fixed.contains("CONTEXT_COMPACTION"), "不该把原文透传出去");
    }
}
