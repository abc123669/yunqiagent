//! 插件清单（`plugin.toml`）解析
//!
//! 清单声明（SPEC 第十一节）：
//!   · 名称、版本、作者、许可
//!   · **所需权限**：文件路径、网络域名、命令
//!   · **暴露的工具定义**（JSON Schema）
//!
//! 清单是**唯一**的权限来源：宿主端据此做能力校验，超出声明范围一律拒绝。

use serde::{Deserialize, Serialize};

/// 插件清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// 插件名（`snake_case`，唯一标识）
    pub name: String,
    /// 版本号（语义化版本）
    pub version: String,
    /// 作者
    #[serde(default)]
    pub author: String,
    /// 许可
    #[serde(default)]
    pub license: String,
    /// 一句话描述
    #[serde(default)]
    pub description: String,
    /// 运行轨道：`process` | `wasm`
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 外部进程轨：可执行文件与参数
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// WASM 轨：wasm 文件相对路径
    #[serde(default)]
    pub wasm_path: Option<String>,
    /// 所需权限声明
    #[serde(default)]
    pub permissions: PermissionDecl,
    /// 暴露的工具定义
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
}

/// 运行轨道默认值。
fn default_kind() -> String {
    "process".to_string()
}

/// 权限声明。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionDecl {
    /// 允许访问的文件路径前缀（白名单）
    #[serde(default)]
    pub file_paths: Vec<String>,
    /// 允许访问的网络域名（白名单）
    #[serde(default)]
    pub network_domains: Vec<String>,
    /// 允许执行的命令（白名单，首词匹配）
    #[serde(default)]
    pub commands: Vec<String>,
}

/// 插件暴露的一个工具。
///
/// `input_schema` 是 **JSON Schema 字面量字符串**（与 yq-core 的 ToolDefinition 同构）——
/// 注意：插件工具**不进 KV-cache 固定区**（数量与内容随插件变化），
/// 而是在动态区注入，因此这里用 `String` 而非 `&'static str`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// 工具名（`snake_case`）
    pub name: String,
    /// 用途描述
    #[serde(default)]
    pub description: String,
    /// 参数 JSON Schema（字符串形式）
    #[serde(default)]
    pub input_schema: String,
}

impl PluginManifest {
    /// 从 TOML 文本解析。
    pub fn from_toml(text: &str) -> Result<Self, ManifestError> {
        let manifest: PluginManifest = toml::from_str(text)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 从文件解析。
    pub fn from_file(path: &std::path::Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ManifestError::Io(path.display().to_string(), e.to_string()))?;
        Self::from_toml(&text)
    }

    /// 基本校验。
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.trim().is_empty() {
            return Err(ManifestError::Invalid("name 不能为空".to_string()));
        }
        if !is_snake_case(&self.name) {
            return Err(ManifestError::Invalid(format!(
                "name `{}` 必须是 snake_case",
                self.name
            )));
        }
        if self.version.trim().is_empty() {
            return Err(ManifestError::Invalid("version 不能为空".to_string()));
        }

        match self.kind.as_str() {
            "process" => {
                if self.command.as_ref().map(|c| c.is_empty()).unwrap_or(true) {
                    return Err(ManifestError::Invalid(
                        "kind=process 时必须提供非空 command".to_string(),
                    ));
                }
            }
            "wasm" => {
                if self.wasm_path.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(ManifestError::Invalid(
                        "kind=wasm 时必须提供 wasm_path".to_string(),
                    ));
                }
            }
            other => {
                return Err(ManifestError::Invalid(format!(
                    "kind `{}` 不合法（应为 process 或 wasm）",
                    other
                )));
            }
        }

        for tool in &self.tools {
            if !is_snake_case(&tool.name) {
                return Err(ManifestError::Invalid(format!(
                    "工具名 `{}` 必须是 snake_case",
                    tool.name
                )));
            }
        }

        Ok(())
    }

    /// 是否所有工具都需要 at 声明。
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

/// 清单错误。
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// 文件读写错误
    #[error("读取清单失败（{0}）: {1}")]
    Io(String, String),
    /// TOML 解析错误
    #[error("清单 TOML 解析失败: {0}")]
    Toml(#[from] toml::de::Error),
    /// 内容不合法
    #[error("清单不合法: {0}")]
    Invalid(String),
}

/// 判断是否为合法 snake_case 标识符。
pub fn is_snake_case(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
name = "web_search"
version = "0.1.0"
author = "云栖"
license = "MIT"
description = "网页搜索插件"
kind = "process"
command = ["python3", "plugin.py"]

[permissions]
network_domains = ["api.example.com"]
file_paths = ["/tmp/yq-plugin"]

[[tools]]
name = "web_search"
description = "搜索网页"
input_schema = '{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}'
"#;

    #[test]
    fn parses_valid_manifest() {
        let m = PluginManifest::from_toml(GOOD).expect("应解析成功");
        assert_eq!(m.name, "web_search");
        assert_eq!(m.kind, "process");
        assert_eq!(m.tool_count(), 1);
        assert_eq!(m.permissions.network_domains, vec!["api.example.com"]);
        assert_eq!(m.command.as_ref().unwrap()[0], "python3");
    }

    #[test]
    fn rejects_non_snake_case_name() {
        let bad = r#"
name = "WebSearch"
version = "1.0.0"
command = ["x"]
"#;
        assert!(PluginManifest::from_toml(bad).is_err());
    }

    #[test]
    fn rejects_process_without_command() {
        let bad = r#"
name = "no_cmd"
version = "1.0.0"
kind = "process"
"#;
        assert!(PluginManifest::from_toml(bad).is_err());
    }

    #[test]
    fn rejects_wasm_without_path() {
        let bad = r#"
name = "no_wasm"
version = "1.0.0"
kind = "wasm"
"#;
        assert!(PluginManifest::from_toml(bad).is_err());
    }

    #[test]
    fn rejects_unknown_kind() {
        let bad = r#"
name = "weird"
version = "1.0.0"
kind = "magic"
command = ["x"]
"#;
        assert!(PluginManifest::from_toml(bad).is_err());
    }

    #[test]
    fn snake_case_detection() {
        assert!(is_snake_case("web_search"));
        assert!(is_snake_case("a1_b2"));
        assert!(!is_snake_case("WebSearch"));
        assert!(!is_snake_case("web-search"));
        assert!(!is_snake_case(""));
        assert!(!is_snake_case("1abc"));
    }
}
