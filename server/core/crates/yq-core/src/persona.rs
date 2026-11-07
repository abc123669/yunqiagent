//! persona 系统 —— 进固定区的角色设定
//!
//! 硬性约束：
//!   · 所有字段固定；集合字段一律用 `Vec` 保序
//!   · **禁止 HashMap / HashSet / BTreeMap**
//!   · `render()` 的输出格式**写死在代码里**，字段名 / 换行 / 分隔符不可改动
//!   · 空集合渲染成"保留字段名，值为空"

use serde::{Deserialize, Serialize};

/// persona 设定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persona {
    /// 称呼方式
    #[serde(default)]
    pub address: String,
    /// 语气风格
    #[serde(default)]
    pub tone: String,
    /// 语言（默认中文简体）
    #[serde(default = "default_language")]
    pub language: String,

    /// 禁忌词（保序）
    #[serde(default)]
    pub forbidden_words: Vec<String>,
    /// 偏好（保序）
    #[serde(default)]
    pub preferences: Vec<String>,
    /// 额外静态指令（保序）
    #[serde(default)]
    pub extra_rules: Vec<String>,
}

fn default_language() -> String {
    "zh-CN".to_string()
}

impl Default for Persona {
    fn default() -> Self {
        Self {
            address: "用户".to_string(),
            tone: "平实".to_string(),
            language: default_language(),
            forbidden_words: Vec::new(),
            preferences: Vec::new(),
            extra_rules: Vec::new(),
        }
    }
}

impl Persona {
    /// 渲染为固定区片段。
    ///
    /// 格式**写死**（字段名、换行、逗号、分号全部硬编码）：
    /// ```text
    /// address=大导演
    /// tone=戏谑
    /// language=zh-CN
    /// forbidden_words=foo,bar
    /// preferences=x,y
    /// extra_rules=rule1;rule2
    /// ```
    ///
    /// 空集合：保留字段名，值为空字符串（如 `forbidden_words=`）。
    pub fn render(&self) -> String {
        let mut out = String::new();

        // address
        out.push_str("address=");
        out.push_str(&self.address);
        out.push('\n');

        // tone
        out.push_str("tone=");
        out.push_str(&self.tone);
        out.push('\n');

        // language
        out.push_str("language=");
        out.push_str(&self.language);
        out.push('\n');

        // forbidden_words —— 逗号分隔；空集合保留字段名
        out.push_str("forbidden_words=");
        out.push_str(&self.forbidden_words.join(","));
        out.push('\n');

        // preferences —— 逗号分隔；空集合保留字段名
        out.push_str("preferences=");
        out.push_str(&self.preferences.join(","));
        out.push('\n');

        // extra_rules —— 分号分隔；空集合保留字段名
        out.push_str("extra_rules=");
        out.push_str(&self.extra_rules.join(";"));
        out.push('\n');

        out
    }
}
