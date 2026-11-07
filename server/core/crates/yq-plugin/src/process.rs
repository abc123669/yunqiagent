//! 外部进程轨 —— 通过 stdio + JSON-RPC 与插件通信
//!
//! 协议（SPEC 第十一节）：
//!   · 宿主 → 插件：一行 JSON（`RpcRequest`）
//!   · 插件 → 宿主：一行 JSON（`RpcResponse`）
//!   · 换行分隔（JSON Lines），UTF-8
//!
//! **安全警告**（SPEC 要求"文档中明确警告"）：
//!   外部进程轨的权限是**进程级**的 —— 插件拿到的是宿主赋予该子进程的完整权限。
//!   能力校验（`capability.rs`）只能约束**宿主代为执行**的操作；
//!   插件自身直接发起 syscall 的行为**无法**被 Rust 侧拦截。
//!   因此：**只运行你信任的插件**。需要强隔离时用 WASM 轨（需 `--features wasm`）。

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// JSON-RPC 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    /// 恒为 "2.0"
    pub jsonrpc: String,
    /// 请求 ID（配对响应用）
    pub id: u64,
    /// 方法名：`describe` | `call_tool`
    pub method: String,
    /// 参数
    #[serde(default)]
    pub params: serde_json::Value,
}

impl RpcRequest {
    /// 构造 `describe` 请求（询问插件能力）。
    pub fn describe(id: u64) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: "describe".to_string(),
            params: serde_json::Value::Null,
        }
    }

    /// 构造 `call_tool` 请求。
    pub fn call_tool(id: u64, tool: &str, arguments: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: "call_tool".to_string(),
            params: serde_json::json!({ "tool": tool, "arguments": arguments }),
        }
    }
}

/// JSON-RPC 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    /// 恒为 "2.0"
    #[serde(default)]
    pub jsonrpc: String,
    /// 请求 ID
    #[serde(default)]
    pub id: u64,
    /// 成功结果
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// 错误信息
    #[serde(default)]
    pub error: Option<String>,
}

impl RpcResponse {
    /// 是否成功。
    pub fn is_ok(&self) -> bool {
        self.error.is_none() && self.result.is_some()
    }

    /// 取文本结果（若结果本身是字符串，直接返回；否则 JSON 序列化）。
    pub fn as_text(&self) -> String {
        match &self.result {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => self.error.clone().unwrap_or_else(|| "(空响应)".to_string()),
        }
    }
}

/// 外部进程插件句柄。
pub struct ProcessPlugin {
    /// 插件名
    name: String,
    /// 子进程
    child: Child,
    /// 标准输入
    stdin: ChildStdin,
    /// 标准输出（行缓冲）
    stdout: BufReader<ChildStdout>,
    /// 下一个请求 ID
    next_id: u64,
}

/// 进程轨错误。
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// 启动失败
    #[error("启动插件进程失败: {0}")]
    Spawn(String),
    /// 管道不可用
    #[error("插件标准输入/输出不可用")]
    NoPipe,
    /// IO 错误
    #[error("插件 IO 错误: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 编解码错误
    #[error("插件 JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    /// 进程提前退出
    #[error("插件进程已退出（未返回响应）")]
    Closed,
}

impl ProcessPlugin {
    /// 启动一个外部进程插件。
    ///
    /// `command` 形如 `["python3", "plugin.py"]`；首元素为可执行文件。
    pub async fn spawn(name: &str, command: &[String]) -> Result<Self, ProcessError> {
        if command.is_empty() {
            return Err(ProcessError::Spawn("command 为空".to_string()));
        }

        let mut cmd = Command::new(&command[0]);
        if command.len() > 1 {
            cmd.args(&command[1..]);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ProcessError::Spawn(format!("{}: {}", command[0], e)))?;

        let stdin = child.stdin.take().ok_or(ProcessError::NoPipe)?;
        let stdout = child.stdout.take().ok_or(ProcessError::NoPipe)?;

        Ok(Self {
            name: name.to_string(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    /// 插件名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 发送一次 JSON-RPC 请求并等待响应。
    pub async fn call(&mut self, req: RpcRequest) -> Result<RpcResponse, ProcessError> {
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        let mut buf = String::new();
        let n = self.stdout.read_line(&mut buf).await?;
        if n == 0 {
            return Err(ProcessError::Closed);
        }

        let resp: RpcResponse = serde_json::from_str(buf.trim())?;
        Ok(resp)
    }

    /// 询问插件能力（`describe`）。
    pub async fn describe(&mut self) -> Result<RpcResponse, ProcessError> {
        let id = self.take_id();
        self.call(RpcRequest::describe(id)).await
    }

    /// 调用插件暴露的工具。
    pub async fn call_tool(
        &mut self,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<RpcResponse, ProcessError> {
        let id = self.take_id();
        self.call(RpcRequest::call_tool(id, tool, arguments)).await
    }

    /// 取下一个请求 ID。
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// 关闭插件：先尝试关闭 stdin（通知退出），再等待进程结束。
    ///
    /// 对应 SPEC 的「卸载时完整撤销副作用」：进程被 kill_on_drop 兜底，
    /// 这里显式等待并回收，避免僵尸进程。
    pub async fn shutdown(mut self) -> Result<(), ProcessError> {
        // 关闭 stdin，多数插件据此退出
        drop(self.stdin);
        // 给 3 秒优雅退出窗口
        match tokio::time::timeout(std::time::Duration::from_secs(3), self.child.wait()).await {
            Ok(Ok(_status)) => Ok(()),
            Ok(Err(e)) => Err(ProcessError::Io(e)),
            Err(_) => {
                // 超时：强制 kill
                let _ = self.child.kill().await;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_request_serializes_correctly() {
        let req = RpcRequest::call_tool(7, "web_search", serde_json::json!({"query": "rust"}));
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.contains("\"jsonrpc\":\"2.0\""));
        assert!(text.contains("\"method\":\"call_tool\""));
        assert!(text.contains("\"web_search\""));
    }

    #[test]
    fn rpc_response_parses_and_reports_ok() {
        let resp: RpcResponse =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"result":"done"}"#).unwrap();
        assert!(resp.is_ok());
        assert_eq!(resp.as_text(), "done");
    }

    #[test]
    fn rpc_response_with_error_is_not_ok() {
        let resp: RpcResponse =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"error":"boom"}"#).unwrap();
        assert!(!resp.is_ok());
        assert_eq!(resp.as_text(), "boom");
    }

    #[test]
    fn describe_request_has_describe_method() {
        let req = RpcRequest::describe(3);
        assert_eq!(req.method, "describe");
        assert_eq!(req.id, 3);
    }

    /// 端到端：用一个真实的 shell 进程验证 stdio JSON-RPC 往返。
    #[tokio::test]
    async fn end_to_end_stdio_rpc_with_real_process() {
        // 一个最简单的"插件"：读一行，原样回一个合法 JSON-RPC 响应
        let script = r#"while IFS= read -r line; do id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'); echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":\"pong\"}"; done"#;
        let mut plugin = ProcessPlugin::spawn(
            "echo_plugin",
            &["bash".to_string(), "-c".to_string(), script.to_string()],
        )
        .await
        .expect("应能启动");

        let resp = plugin
            .call_tool("ping", serde_json::json!({}))
            .await
            .expect("应能往返");
        assert!(resp.is_ok());
        assert_eq!(resp.as_text(), "pong");

        plugin.shutdown().await.expect("应能优雅关闭");
    }
}
