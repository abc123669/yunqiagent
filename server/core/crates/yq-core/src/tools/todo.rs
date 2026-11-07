//! `todo` 工具 —— 让模型显式维护任务清单（长任务能力的头号支撑）
//!
//! ## 为什么需要它
//!
//! 在此之前，agent 循环每一轮都让模型「下一步做什么」—— 模型看不到任何
//! 显式的任务状态，于是：
//!   · 长任务做着做着忘了目标，反复重新规划
//!   · 做完一步不知道还剩几步，容易提前收工或原地打转
//!
//! 本工具让模型自己维护一份任务清单：write 整体覆盖、update 改单条状态、
//! read 读取当前进度。清单内容随工具结果进入**动态区**，不影响 KV-cache
//! 固定区前缀的字节稳定性。
//!
//! ## 存储设计
//!
//! 进程内 `Arc<Mutex<Vec<TodoItem>>>`，由**工具实例持有**。agent 循环每轮
//! 复用同一个 `ToolRegistry`（即同一个工具实例），因此状态跨轮持久。
//! 注意：绝不能每轮重建工具实例，否则清单会被清空。
//!
//! ## 与 memory 工具的分工
//!
//!   · memory：跨会话的长期知识（用户偏好、环境事实、踩坑经验）
//!   · todo  ：本会话内的执行计划与进度（会话结束即失效，刻意不落盘）

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Mutex;

use crate::tools::{Tool, ToolDefinition, ToolError};

/// 单条任务的字符上限（与 memory 的单条上限同数量级，防清单膨胀）。
pub const MAX_ITEM_CHARS: usize = 200;

/// 清单总条数上限（防模型把清单当草稿纸无限堆）。
pub const MAX_ITEMS: usize = 50;

/// 任务状态：pending 待办 / in_progress 进行中 / done 已完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    /// 解析模型给的字符串状态；非法值返回 Err。
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "done" => Ok(Self::Done),
            other => Err(format!(
                "非法状态 `{}`（应为 pending / in_progress / done）",
                other
            )),
        }
    }

    /// 人类可读的中文标记（工具结果直接进模型上下文，必须一目了然）。
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "待办",
            Self::InProgress => "进行中",
            Self::Done => "已完成",
        }
    }
}

/// 单条任务。
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

/// todo 工具的静态定义。
///
/// 注意：`input_schema` 是 `&'static str` 字面量 —— 进固定区的内容
/// **绝不**经 `serde_json::Value` 序列化（保字节稳定）。
pub const TODO_SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["write","update","read"],"description":"write=用 items 整体覆盖清单（首次规划或多步调整时用）；update=只改某条任务的状态（index 从 1 起）；read=读取当前清单与进度"},"items":{"type":"array","description":"任务数组（action=write 时必填），按执行顺序排列","items":{"type":"object","properties":{"text":{"type":"string","description":"任务描述，一句话说清做什么"},"status":{"type":"string","enum":["pending","in_progress","done"],"description":"任务状态，默认 pending"}},"required":["text"]}},"index":{"type":"integer","description":"action=update 时的目标条目序号（从 1 起，与 read 输出里的编号一致）"},"status":{"type":"string","enum":["pending","in_progress","done"],"description":"action=update 时的新状态"}},"required":["action"]}"#;

/// todo 工具。
///
/// 清单状态存在实例内的 `Mutex` 里 —— 实例由 `ToolRegistry` 持有，
/// agent 循环每轮复用同一个注册表，因此状态跨轮持久。
pub struct TodoTool {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoTool {
    /// 构造（清单初始为空）。
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }

    /// 整体覆盖写清单。
    fn write(&self, args: &Value) -> Result<String, ToolError> {
        let raw_items = args
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidArguments("action=write 需要提供 items 数组".into()))?;

        if raw_items.is_empty() {
            return Err(ToolError::InvalidArguments(
                "items 不能为空：要么 write 至少一条任务，要么用 update / read".into(),
            ));
        }
        if raw_items.len() > MAX_ITEMS {
            return Err(ToolError::InvalidArguments(format!(
                "任务条数 {} 超过上限 {}。清单应是精炼的执行计划，不是流水账",
                raw_items.len(),
                MAX_ITEMS
            )));
        }

        let mut items = Vec::with_capacity(raw_items.len());
        for (i, raw) in raw_items.iter().enumerate() {
            let text = raw
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidArguments(format!("items[{}] 缺少 text 字段", i + 1))
                })?
                .trim()
                .to_string();
            if text.is_empty() {
                return Err(ToolError::InvalidArguments(format!(
                    "items[{}] 的 text 不能为空",
                    i + 1
                )));
            }
            let n = text.chars().count();
            if n > MAX_ITEM_CHARS {
                return Err(ToolError::InvalidArguments(format!(
                    "items[{}] 的 text 过长（{} 字符），单条上限 {} 字符。请拆成更小的任务",
                    i + 1,
                    n,
                    MAX_ITEM_CHARS
                )));
            }
            let status = match raw.get("status").and_then(|v| v.as_str()) {
                None => TodoStatus::Pending,
                Some(s) => TodoStatus::parse(s).map_err(ToolError::InvalidArguments)?,
            };
            items.push(TodoItem { text, status });
        }

        let done = items.iter().filter(|t| t.status == TodoStatus::Done).count();
        *self.items.lock().expect("todo 清单锁不应中毒") = items;
        Ok(self.render(Some(format!(
            "已写入 {} 条任务（其中 {} 条已完成）。开始执行后记得用 action=update 同步进度。",
            raw_items.len(),
            done
        ))))
    }

    /// 更新某条任务的状态。
    fn update(&self, args: &Value) -> Result<String, ToolError> {
        let index = args
            .get("index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ToolError::InvalidArguments("action=update 需要提供 index（从 1 起）".into()))?
            as usize;
        let status_str = args
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("action=update 需要提供 status".into()))?;
        let status = TodoStatus::parse(status_str).map_err(ToolError::InvalidArguments)?;

        let mut guard = self.items.lock().expect("todo 清单锁不应中毒");
        if guard.is_empty() {
            return Err(ToolError::InvalidArguments(
                "当前清单为空，请先用 action=write 规划任务".into(),
            ));
        }
        if index == 0 || index > guard.len() {
            return Err(ToolError::InvalidArguments(format!(
                "index={} 超出范围（当前清单共 {} 条，序号从 1 起）",
                index,
                guard.len()
            )));
        }
        guard[index - 1].status = status;
        drop(guard);

        Ok(self.render(Some(format!("第 {} 条已标记为[{}]。", index, status.label()))))
    }

    /// 渲染当前清单为人类可读的中文文本（直接进模型上下文）。
    fn render(&self, note: Option<String>) -> String {
        let guard = self.items.lock().expect("todo 清单锁不应中毒");
        if guard.is_empty() {
            return "当前任务清单为空。".to_string();
        }
        let mut out = String::from("当前任务清单：\n");
        for (i, item) in guard.iter().enumerate() {
            out.push_str(&format!("{}. [{}] {}\n", i + 1, item.status.label(), item.text));
        }
        let done = guard.iter().filter(|t| t.status == TodoStatus::Done).count();
        out.push_str(&format!(
            "进度：{}/{} 已完成。",
            done,
            guard.len()
        ));
        if let Some(note) = note {
            out.push(' ');
            out.push_str(&note);
        }
        out
    }
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "todo",
            description: "维护当前任务清单，让你在长任务里不丢目标、不重复规划。接到多步任务时先 write 一份完整计划；每完成一步就 update 对应条目为 done；开始某条前标 in_progress。清单每轮都会让你看到，随时用 read 回顾。",
            input_schema: TODO_SCHEMA,
        }
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("缺少 action".into()))?;

        match action {
            "write" => self.write(&args),
            "update" => self.update(&args),
            "read" => Ok(self.render(None)),
            other => Err(ToolError::InvalidArguments(format!(
                "未知 action `{}`（应为 write / update / read）",
                other
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造 write 参数的便捷函数。
    fn write_args(items: Value) -> Value {
        json!({"action": "write", "items": items})
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let tool = TodoTool::new();
        let out = tool
            .execute(write_args(json!([
                {"text": "抓取数据"},
                {"text": "清洗数据", "status": "in_progress"},
                {"text": "写报告", "status": "done"}
            ])))
            .await
            .expect("write 应成功");
        assert!(out.contains("已写入 3 条任务"));

        let out = tool.execute(json!({"action": "read"})).await.unwrap();
        assert!(out.contains("1. [待办] 抓取数据"));
        assert!(out.contains("2. [进行中] 清洗数据"));
        assert!(out.contains("3. [已完成] 写报告"));
        assert!(out.contains("进度：1/3 已完成"));
    }

    #[tokio::test]
    async fn update_changes_status() {
        let tool = TodoTool::new();
        tool.execute(write_args(json!([{"text": "任务甲"}, {"text": "任务乙"}])))
            .await
            .unwrap();

        let out = tool
            .execute(json!({"action": "update", "index": 1, "status": "done"}))
            .await
            .unwrap();
        assert!(out.contains("第 1 条已标记为[已完成]"));
        assert!(out.contains("1. [已完成] 任务甲"));
        assert!(out.contains("进度：1/2"));
    }

    #[tokio::test]
    async fn status_crosses_rounds_on_same_instance() {
        // 模拟 agent 循环：同一个实例，多轮调用之间状态必须持久
        let tool = TodoTool::new();
        tool.execute(write_args(json!([{"text": "第一步"}]))).await.unwrap();
        tool.execute(json!({"action": "update", "index": 1, "status": "in_progress"})).await.unwrap();
        let out = tool.execute(json!({"action": "read"})).await.unwrap();
        assert!(out.contains("1. [进行中] 第一步"));
    }

    #[tokio::test]
    async fn update_out_of_range_is_friendly_error() {
        let tool = TodoTool::new();
        tool.execute(write_args(json!([{"text": "唯一任务"}]))).await.unwrap();
        let err = tool
            .execute(json!({"action": "update", "index": 5, "status": "done"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("超出范围"));
        assert!(err.to_string().contains("序号从 1 起"));
    }

    #[tokio::test]
    async fn update_on_empty_list_is_friendly_error() {
        let tool = TodoTool::new();
        let err = tool
            .execute(json!({"action": "update", "index": 1, "status": "done"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("当前清单为空"));
    }

    #[tokio::test]
    async fn invalid_status_is_rejected() {
        let tool = TodoTool::new();
        let err = tool
            .execute(write_args(json!([{"text": "任务", "status": "finished"}])))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("非法状态"));
    }

    #[tokio::test]
    async fn missing_action_is_rejected() {
        let tool = TodoTool::new();
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("缺少 action"));
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let tool = TodoTool::new();
        let err = tool.execute(json!({"action": "delete"})).await.unwrap_err();
        assert!(err.to_string().contains("未知 action"));
        assert!(err.to_string().contains("write / update / read"));
    }

    #[tokio::test]
    async fn write_without_items_is_rejected() {
        let tool = TodoTool::new();
        let err = tool.execute(json!({"action": "write"})).await.unwrap_err();
        assert!(err.to_string().contains("items"));
    }

    #[tokio::test]
    async fn empty_items_is_rejected() {
        let tool = TodoTool::new();
        let err = tool.execute(write_args(json!([]))).await.unwrap_err();
        assert!(err.to_string().contains("items 不能为空"));
    }

    #[tokio::test]
    async fn oversize_text_is_rejected() {
        let tool = TodoTool::new();
        let long = "长".repeat(MAX_ITEM_CHARS + 1);
        let err = tool
            .execute(write_args(json!([{"text": long}])))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("过长"));
    }

    #[tokio::test]
    async fn too_many_items_is_rejected() {
        let tool = TodoTool::new();
        let items: Vec<Value> = (0..=MAX_ITEMS)
            .map(|i| json!({"text": format!("任务 {}", i)}))
            .collect();
        let err = tool.execute(write_args(json!(items))).await.unwrap_err();
        assert!(err.to_string().contains("超过上限"));
    }

    #[tokio::test]
    async fn read_on_empty_list_is_friendly() {
        let tool = TodoTool::new();
        let out = tool.execute(json!({"action": "read"})).await.unwrap();
        assert!(out.contains("当前任务清单为空"));
    }

    #[test]
    fn definition_is_static_and_named_todo() {
        let tool = TodoTool::new();
        let def = tool.definition();
        assert_eq!(def.name, "todo");
        // schema 必须是编译期字面量（&'static str），可安全进固定区
        let static_schema: &'static str = def.input_schema;
        assert!(static_schema.contains("\"write\""));
    }
}
