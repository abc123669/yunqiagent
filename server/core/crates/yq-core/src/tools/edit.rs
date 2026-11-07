//! edit 工具 —— 在文件中精确替换文本片段

use async_trait::async_trait;
use serde_json::Value;

use super::{Tool, ToolDefinition};
use crate::error::ToolError;

/// edit 工具。
pub struct EditTool;

/// 参数 JSON Schema —— 编译期字面量，直接进固定区。
const EDIT_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"目标文件路径"},"old_string":{"type":"string","description":"被替换的原文"},"new_string":{"type":"string","description":"替换后的新文本"},"replace_all":{"type":"boolean","description":"是否替换所有匹配（默认 false）"}},"required":["path","old_string","new_string"]}"#;

#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit",
            description: "在文件中精确替换指定文本片段",
            input_schema: EDIT_INPUT_SCHEMA,
        }
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw_path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArgument("path"))?;

        // 展开开头的 `~`（模型很自然会写 ~/xxx，PathBuf 不认）
        let path_buf = super::expand_tilde(raw_path);
        let path = path_buf
            .to_str()
            .ok_or_else(|| ToolError::InvalidArguments("路径包含非 UTF-8 字符".to_string()))?;
        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArgument("old_string"))?;
        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArgument("new_string"))?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string.is_empty() {
            return Err(ToolError::InvalidArguments(
                "old_string 不能为空".to_string(),
            ));
        }

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(ToolError::Io)?;

        let occurrences = content.matches(old_string).count();
        if occurrences == 0 {
            return Err(ToolError::InvalidArguments(format!(
                "在 {} 中未找到待替换文本",
                path
            )));
        }
        if occurrences > 1 && !replace_all {
            return Err(ToolError::InvalidArguments(format!(
                "待替换文本在 {} 中出现 {} 次，请使用 replace_all=true 或提供更精确的 old_string",
                path, occurrences
            )));
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        tokio::fs::write(path, &new_content)
            .await
            .map_err(ToolError::Io)?;

        let replaced = if replace_all { occurrences } else { 1 };
        Ok(format!("已在 {} 中替换 {} 处", path, replaced))
    }
}
