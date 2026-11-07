//! read 工具 —— 读取文件内容（带行号）

use async_trait::async_trait;
use serde_json::Value;

use super::{Tool, ToolDefinition};
use crate::error::ToolError;

/// read 工具。
pub struct ReadTool;

const MAX_READ_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_READ_OUTPUT_BYTES: usize = 128 * 1024;

/// 参数 JSON Schema —— 编译期字面量，直接进固定区。
const READ_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"要读取的文件路径"},"offset":{"type":"integer","description":"起始行号（1-indexed），可选"},"limit":{"type":"integer","description":"最多读取的行数，可选"}},"required":["path"]}"#;

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read",
            description: "读取指定文件的内容，返回带行号的文本",
            input_schema: READ_INPUT_SCHEMA,
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

        let offset = args
            .get("offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(2000)
            .max(1) as usize;

        let metadata = tokio::fs::metadata(path).await.map_err(ToolError::Io)?;
        if metadata.len() > MAX_READ_FILE_BYTES {
            return Err(ToolError::Other(format!(
                "文件大于 {} MiB；请用 offset/limit 或 bash 分段读取",
                MAX_READ_FILE_BYTES / (1024 * 1024)
            )));
        }
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(ToolError::Io)?;

        let mut out = String::new();
        let mut emitted = 0usize;
        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            if line_no < offset {
                continue;
            }
            if emitted >= limit {
                break;
            }
            let formatted = format!("{}|{}\n", line_no, line);
            if out.len() + formatted.len() > MAX_READ_OUTPUT_BYTES {
                let remaining = MAX_READ_OUTPUT_BYTES.saturating_sub(out.len());
                let marker = "\n[read output truncated; narrow offset/limit]\n";
                let text_budget = remaining.saturating_sub(marker.len());
                let mut end = text_budget.min(formatted.len());
                while !formatted.is_char_boundary(end) { end -= 1; }
                out.push_str(&formatted[..end]);
                out.push_str(marker);
                break;
            }
            out.push_str(&formatted);
            emitted += 1;
        }

        if out.is_empty() {
            out.push_str("(文件为空或指定范围无内容)\n");
        }
        Ok(out)
    }
}
