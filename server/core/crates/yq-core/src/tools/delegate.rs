//! delegate 工具 —— 子代理委派
//!
//! 子代理**只返回结构化摘要**，不保留中间过程。
//! 中间过程（工具调用流水、冗长输出）在本模块内被丢弃，
//! 只有 `SubagentSummary` 会以 JSON 形式返回给主 Agent（进动态区）。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Tool, ToolDefinition};
use crate::error::ToolError;

/// delegate 工具。
pub struct DelegateTool;

/// 参数 JSON Schema —— 编译期字面量，直接进固定区。
const DELEGATE_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"goal":{"type":"string","description":"委派给子代理的目标"},"context":{"type":"string","description":"必要的背景信息，可选"}},"required":["goal"]}"#;

/// 单条结论的最大字符数（超出截断，防止摘要膨胀）。
const MAX_FINDING_CHARS: usize = 500;

/// 子代理执行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryStatus {
    /// 成功
    Success,
    /// 部分完成
    Partial,
    /// 失败
    Failed,
}

/// 子代理结构化摘要 —— **只保留结论，丢弃中间过程**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSummary {
    /// 委派目标
    pub goal: String,
    /// 执行状态
    pub status: SummaryStatus,
    /// 关键结论（结构化条目）
    pub findings: Vec<String>,
    /// 产出物（文件路径 / 标识）
    pub artifacts: Vec<String>,
    /// 阻塞项
    pub blockers: Vec<String>,
}

impl SubagentSummary {
    /// 新建空摘要。
    pub fn new(goal: impl Into<String>, status: SummaryStatus) -> Self {
        Self {
            goal: goal.into(),
            status,
            findings: Vec::new(),
            artifacts: Vec::new(),
            blockers: Vec::new(),
        }
    }

    /// 追加一条结论（超长自动截断）。
    pub fn add_finding(&mut self, finding: impl Into<String>) -> &mut Self {
        let mut s = finding.into();
        if s.chars().count() > MAX_FINDING_CHARS {
            s = s.chars().take(MAX_FINDING_CHARS).collect::<String>();
            s.push_str("…(已截断)");
        }
        self.findings.push(s);
        self
    }

    /// 序列化为紧凑 JSON（返回给主 Agent 的内容）。
    pub fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| format!("{{\"goal\":\"{}\",\"status\":\"failed\"}}", self.goal))
    }
}

/// 从子代理的原始输出中提取结构化摘要。
///
/// 阶段二为**确定性 mock 实现**：
///   · 不调用任何 LLM
///   · 不引入随机数 / 时间戳（避免污染）
///   · 把原始输出按行切成 findings，过滤空行
///
/// 阶段四将替换为真实子代理驱动，但**接口与返回结构不变**。
pub fn summarize_raw_output(goal: &str, raw_output: &str) -> SubagentSummary {
    let mut summary = SubagentSummary::new(goal, SummaryStatus::Success);
    for line in raw_output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        summary.add_finding(trimmed);
    }
    if summary.findings.is_empty() {
        summary.status = SummaryStatus::Partial;
        summary.add_finding("(子代理未产生可提取的结论)");
    }
    summary
}

#[async_trait]
impl Tool for DelegateTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "delegate",
            description: "把子任务委派给子代理，只返回结构化摘要（不保留中间过程）",
            input_schema: DELEGATE_INPUT_SCHEMA,
        }
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let goal = args
            .get("goal")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::MissingArgument("goal"))?;
        let context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");

        // 阶段二：无真实子代理驱动。构造一个确定性的摘要，
        // 明确反映"已接收委派"，供上层链路测试。不产生随机内容。
        let mut summary = SubagentSummary::new(goal, SummaryStatus::Success);
        summary.add_finding(format!("已接收委派目标：{}", goal));
        if !context.is_empty() {
            summary.add_finding(format!("背景信息长度：{} 字符", context.chars().count()));
        }
        summary.add_finding("阶段二 mock：真实子代理驱动将于阶段四接入");

        Ok(summary.to_json())
    }
}
