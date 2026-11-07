//! bash 工具 —— 执行 shell 命令

use std::time::Duration;
use std::{io, process::Stdio};

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use super::{Tool, ToolDefinition};
use crate::error::ToolError;

/// bash 工具。
pub struct BashTool;

/// 默认超时秒数。
///
/// 为什么是 120 而不是 60：树莓派 4B 上 `pip install -e .` 这类安装命令
/// 实测被 60 秒截断过（审计表里留有 error 记录），慢盘 + 编译本来就吃时间，
/// 放宽到 120 秒才够跑完常见任务。
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// 超时上限秒数。
///
/// 为什么必须有上限：`timeout_secs` 由模型提供，此前无上限（能传 999999），
/// 一条挂死的命令会在 `tokio::time::timeout` 里长期霸占 bash 子进程；
/// 本机内存只有 4GB，挂死命令堆积足以把系统拖垮，所以钳到 600 秒。
const MAX_TIMEOUT_SECS: u64 = 600;
const MAX_CAPTURE_BYTES: usize = 128 * 1024;

async fn drain_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> io::Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let mut captured = Vec::with_capacity(MAX_CAPTURE_BYTES.min(8192));
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 { break; }
        let keep = (MAX_CAPTURE_BYTES - captured.len()).min(n);
        captured.extend_from_slice(&buffer[..keep]);
        truncated |= keep < n;
    }
    Ok((captured, truncated))
}

/// 参数 JSON Schema —— 编译期字面量，直接进固定区。
const BASH_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"command":{"type":"string","description":"要执行的 shell 命令"},"timeout_secs":{"type":"integer","description":"超时秒数（默认 120，上限 600）"}},"required":["command"]}"#;

/// 计算实际生效的超时秒数：缺省用默认值，模型传入值钳到 `[1, 上限]`。
///
/// 抽成纯函数是为了能离线单测钳制边界（跑真命令的超时测试太慢也不稳）。
fn resolve_timeout_secs(args: &Value) -> u64 {
    args.get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        // 下限 1：0 秒没有意义（等于必然超时）；上限见 MAX_TIMEOUT_SECS 注释
        .clamp(1, MAX_TIMEOUT_SECS)
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash",
            description: "在 shell 中执行命令，返回 stdout / stderr / 退出码",
            input_schema: BASH_INPUT_SCHEMA,
        }
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArgument("command"))?;

        let timeout_secs = resolve_timeout_secs(&args);

        let mut child = Command::new("bash")
            .arg("-c").arg(command)
            .stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn().map_err(ToolError::Io)?;
        let stdout = child.stdout.take().ok_or_else(|| ToolError::Other("stdout pipe unavailable".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| ToolError::Other("stderr pipe unavailable".into()))?;
        let fut = async {
            tokio::join!(child.wait(), drain_capped(stdout), drain_capped(stderr))
        };
        let (status, stdout, stderr) = tokio::time::timeout(Duration::from_secs(timeout_secs), fut)
            .await
            .map_err(|_| ToolError::Other(format!("命令执行超时（{} 秒）", timeout_secs)))?;
        let status = status.map_err(ToolError::Io)?;
        let (stdout, stdout_cut) = stdout.map_err(ToolError::Io)?;
        let (stderr, stderr_cut) = stderr.map_err(ToolError::Io)?;

        let stdout = String::from_utf8_lossy(&stdout);
        let stderr = String::from_utf8_lossy(&stderr);
        let exit_code = status.code().unwrap_or(-1);

        let mut out = String::new();
        out.push_str(&format!("exit_code={}\n", exit_code));
        out.push_str("stdout:\n");
        out.push_str(&stdout);
        if stdout_cut { out.push_str("\n[stdout truncated at 128 KiB]\n"); }
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("stderr:\n");
        out.push_str(&stderr);
        if stderr_cut { out.push_str("\n[stderr truncated at 128 KiB]\n"); }
        if !stderr.ends_with('\n') {
            out.push('\n');
        }

        if status.success() {
            Ok(out)
        } else {
            Err(ToolError::Other(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[cfg(unix)]
    async fn unsuccessful_command_is_an_error_with_diagnostics() {
        let result = BashTool.execute(json!({"command":"printf failure >&2; exit 7"})).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("exit_code=7"));
        assert!(err.contains("failure"));
    }

    /// 缺省（模型没传 timeout_secs）用默认值 120。
    #[test]
    fn timeout_defaults_to_120() {
        assert_eq!(resolve_timeout_secs(&json!({"command": "ls"})), 120);
    }

    /// 模型传巨大值（曾可传 999999）必须被钳到上限，防止挂死命令长期占资源。
    #[test]
    fn timeout_clamps_huge_value_to_max() {
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": 999999})),
            600
        );
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": 601})),
            600
        );
    }

    /// 下限钳到 1：0 秒等于必然超时，没有意义。
    #[test]
    fn timeout_clamps_zero_to_one() {
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": 0})),
            1
        );
    }

    /// 区间内的值原样生效，边界值不被误伤。
    #[test]
    fn timeout_keeps_valid_value() {
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": 300})),
            300
        );
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": 600})),
            600
        );
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": 1})),
            1
        );
    }

    /// 非正整数（负数、字符串、浮点）拿不到 u64，回落到默认值而不是报错。
    #[test]
    fn timeout_falls_back_on_invalid_value() {
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": -1})),
            120
        );
        assert_eq!(
            resolve_timeout_secs(&json!({"command": "ls", "timeout_secs": "60"})),
            120
        );
    }

    /// Schema 描述必须和实际生效值一致（默认 120 / 上限 600），
    /// 否则模型会按过期的「默认 60」来规划命令时长。
    #[test]
    fn schema_documents_default_and_cap() {
        assert!(BASH_INPUT_SCHEMA.contains("超时秒数（默认 120，上限 600）"));
        assert_eq!(DEFAULT_TIMEOUT_SECS, 120);
        assert_eq!(MAX_TIMEOUT_SECS, 600);
    }

    #[tokio::test]
    async fn captured_output_is_bounded_and_excess_bytes_are_drained() {
        let input = vec![b'x'; MAX_CAPTURE_BYTES + 97];
        let (captured, truncated) = drain_capped(std::io::Cursor::new(input)).await.unwrap();
        assert_eq!(captured.len(), MAX_CAPTURE_BYTES);
        assert!(truncated);
    }
}
