//! 固定区前缀 —— 字节级固定 + SHA256 指纹
//!
//! `StaticPrefix` 一旦构建完成即**不可变**：
//!   · `text` 是固定区全文
//!   · `sha256` 是它的 SHA256（十六进制小写）
//!
//! `extract_prefix_hash` 语义由上层 `BuiltContext` 直接返回本结构体的 `sha256`，
//! **绝不从拼接后的完整字符串反推截取**。

use sha2::{Digest, Sha256};

use crate::persona::Persona;

/// 固定区前缀。
#[derive(Debug, Clone)]
pub struct StaticPrefix {
    /// 固定区全文（字节级固定）
    text: String,
    /// 固定区 SHA256（十六进制小写）
    sha256: String,
}

impl StaticPrefix {
    /// 组装固定区。
    ///
    /// 组装顺序严格固定：
    ///   1. 系统提示
    ///   2. 工具定义（由 `ToolRegistry::render_for_prefix()` 预渲染，顺序已固定）
    ///   3. persona
    ///   4. 静态指令
    ///
    /// `tools_prefix` 必须是**已渲染好的工具定义字符串**，
    /// 本函数只做拼接，不解析、不序列化。
    ///
    /// 换行对称性：每个区块后**保证恰好一个 `\n`** ——
    /// 不依赖下游 `render_for_prefix()` / `Persona::render()` 是否自带末尾换行。
    /// 这是防脆弱设计：将来改 render 逻辑不会意外改变前缀字节。
    pub fn build(
        system_prompt: &str,
        tools_prefix: &str,
        persona: &Persona,
        static_rules: &str,
    ) -> Self {
        let mut text = String::new();

        // 1. 系统提示
        text.push_str("[SYSTEM]\n");
        text.push_str(system_prompt);
        if !text.ends_with('\n') {
            text.push('\n');
        }

        // 2. 工具定义（顺序由调用方保证固定）
        text.push_str("[TOOLS]\n");
        text.push_str(tools_prefix);
        if !text.ends_with('\n') {
            text.push('\n');
        }

        // 3. persona
        text.push_str("[PERSONA]\n");
        let persona_text = persona.render();
        text.push_str(&persona_text);
        if !text.ends_with('\n') {
            text.push('\n');
        }

        // 4. 静态指令
        text.push_str("[STATIC_RULES]\n");
        text.push_str(static_rules);
        if !text.ends_with('\n') {
            text.push('\n');
        }

        let sha256 = Self::hash(&text);
        Self { text, sha256 }
    }

    /// 计算字符串的 SHA256（十六进制小写）。
    fn hash(input: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// 固定区全文。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 固定区 SHA256。
    ///
    /// `BuiltContext::prefix_sha256` 直接返回此值，杜绝反推。
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// 固定区字节长度。
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}
