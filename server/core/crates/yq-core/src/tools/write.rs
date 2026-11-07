//! write 工具 —— 写入整个文件

use async_trait::async_trait;
use serde_json::Value;

use super::{Tool, ToolDefinition};
use crate::error::ToolError;

/// write 工具。
pub struct WriteTool;

/// 参数 JSON Schema —— 编译期字面量，直接进固定区。
const WRITE_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"要写入的文件路径"},"content":{"type":"string","description":"文件完整内容"}},"required":["path","content"]}"#;

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write",
            description: "将内容写入指定文件（覆盖已有内容）",
            input_schema: WRITE_INPUT_SCHEMA,
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
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArgument("content"))?;

        // 若目标已有文件，先读取旧内容长度用于回报（不阻断写入）
        let existed = tokio::fs::metadata(path).await.is_ok();

        // 确保父目录存在
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(ToolError::Io)?;
            }
        }

        tokio::fs::write(path, content)
            .await
            .map_err(ToolError::Io)?;

        let action = if existed { "覆盖" } else { "新建" };
        Ok(format!(
            "已{}文件 {}（{} 字节）",
            action,
            path,
            content.len()
        ))
    }
}
