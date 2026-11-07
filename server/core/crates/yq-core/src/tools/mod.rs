//! 工具系统 —— 五工具抽象与固定顺序注册表
//!
//! 硬性约束：
//!   · 五工具定义**顺序固定**（read → write → edit → bash → delegate），
//!     不得因运行时状态、配置或调用时机重排
//!   · `input_schema` 是 `&'static str`（编译期字面量），
//!     前缀组装时**直接拼接字符串**，全程不经过 `serde_json::Value`
//!   · 工具**结果**只能进入动态区，绝不出现在固定区
//!   · 所有标识符 snake_case

pub mod bash;
pub mod delegate;
pub mod edit;
pub mod memory;
pub mod read;
pub mod todo;
pub mod write;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ToolError;

/// 展开路径开头的 `~` 为用户家目录。
///
/// ## 为什么需要（2026-10-05 实测）
///
/// 模型很自然地会写 `~/wordfreq/main.py` 这样的路径，但 `std::path::PathBuf`
/// **不会**展开 `~` —— 结果是创建一个**名为 `~` 的字面量目录**，
/// 文件写到 `./~/wordfreq/main.py`，模型随后发现"文件找不到"，
/// 白费一轮去排查。
///
/// 实测对照评测中，我们这边的 agent 自己发现了这个问题并改用绝对路径绕开，
/// 但工具层本就该处理它 —— 让模型不必操心这种 shell 惯例。
///
/// 规则：
///   · `~`      → `$HOME`
///   · `~/x/y`  → `$HOME/x/y`
///   · 其它      → 原样返回
///   · `$HOME` 取不到时原样返回（不猜）
pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    use std::path::PathBuf;

    let trimmed = path.trim();
    if trimmed == "~" || trimmed.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = trimmed.trim_start_matches('~').trim_start_matches('/');
            let mut p = PathBuf::from(home);
            if !rest.is_empty() {
                for part in rest.split('/').filter(|s| !s.is_empty()) {
                    p.push(part);
                }
            }
            return p;
        }
    }
    PathBuf::from(trimmed)
}

/// 工具定义的元数据。
///
/// 所有字段均为编译期常量，天然保证固定区字节级稳定。
#[derive(Debug, Clone, Copy)]
pub struct ToolDefinition {
    /// 工具名（snake_case）
    pub name: &'static str,
    /// 用途描述（进固定区）
    pub description: &'static str,
    /// 参数 JSON Schema 的**字面量字符串**（原始 raw string）。
    ///
    /// 直接拼接进前缀，不经 `serde_json` 序列化 —— 杜绝 Map 顺序漂移。
    pub input_schema: &'static str,
}

/// 工具统一接口。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 返回工具定义（name / description / input_schema）。
    fn definition(&self) -> ToolDefinition;

    /// 执行工具，返回结果文本。
    ///
    /// 返回值只会被写入**动态区**。
    async fn execute(&self, args: Value) -> Result<String, ToolError>;

    /// 便利方法：返回工具名。
    fn name(&self) -> &'static str {
        self.definition().name
    }
}

/// 五工具注册表。
///
/// 内部 `Vec` 的顺序即固定区中的定义顺序 —— **注册顺序写死，永不重排**。
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// 按 SPEC 规定的固定顺序注册全部五工具：
    ///
    /// 1. read
    /// 2. write
    /// 3. edit
    /// 4. bash
    /// 5. delegate
    ///
    /// 顺序写死在函数体内，不接受任何参数调整。
    pub fn new_default() -> Self {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(read::ReadTool),
            Box::new(write::WriteTool),
            Box::new(edit::EditTool),
            Box::new(bash::BashTool),
            Box::new(delegate::DelegateTool),
        ];
        Self { tools }
    }

    /// 注册全部工具（含 `memory` 与 `todo`）—— **顺序写死，永不重排**。
    ///
    /// `memory` 追加在最后：加它会使固定区前缀变化一次（缓存失效一次），
    /// 但此后永久稳定；放在末尾可让前五个工具的定义字节**完全不变**。
    pub fn with_memory(memories: yq_memory::Memories) -> Self {
        let mut reg = Self::new_default();
        reg.tools.push(Box::new(memory::MemoryTool::new(memories)));
        reg
    }

    /// 注册全部工具（含 `memory` 与 `todo`）—— **顺序写死，永不重排**。
    ///
    /// ## 顺序（写死）
    ///
    /// 索引 0..4 为五工具，索引 5 为 `memory`，索引 6 为 `todo` ——
    /// 只在尾部追加，前六个工具的定义字节完全不变。
    ///
    /// ## 为什么 todo 必须单例
    ///
    /// 清单状态存在 `TodoTool` 实例内的 `Mutex` 里。`Agent` 持有唯一的
    /// `ToolRegistry` 并在每一轮复用，因此状态跨轮持久 ——
    /// **绝不能每轮重建注册表**，否则清单每轮被清空。
    pub fn with_memory_and_todo(memories: yq_memory::Memories) -> Self {
        let mut reg = Self::with_memory(memories);
        reg.tools.push(Box::new(todo::TodoTool::new()));
        reg
    }

    /// 工具定义列表，顺序即注册顺序。
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    /// 按名查找工具。
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.definition().name == name)
            .map(|b| b.as_ref())
    }

    /// 工具数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 渲染"工具定义"前缀片段（进固定区）。
    ///
    /// 实现要点：
    ///   · 顺序严格按 `Vec` 顺序
    ///   · `input_schema` **直接 push_str 字面量**，不解析、不序列化
    ///   · 输出格式写死，保证字节级稳定
    pub fn render_for_prefix(&self) -> String {
        let mut out = String::new();
        for (idx, def) in self.definitions().iter().enumerate() {
            out.push_str("[tool ");
            out.push_str(&idx.to_string());
            out.push_str("] name=");
            out.push_str(def.name);
            out.push('\n');
            out.push_str("description=");
            out.push_str(def.description);
            out.push('\n');
            out.push_str("input_schema=");
            // 直接拼接编译期字面量，绝不经过 serde_json::Value
            out.push_str(def.input_schema);
            out.push('\n');
        }
        out
    }

    /// 渲染**含 `memory`** 的完整工具定义串，供固定区使用。
    ///
    /// ## 为什么单独一个方法
    ///
    /// `memory` 工具需要数据库连接池才能**实例化**，但固定区只需要它的
    /// **定义**（name / description / input_schema 全是编译期常量）。
    /// 因此这里直接拼 `memory::MEMORY_SCHEMA`，**不需要建池** ——
    /// 否则 `build_static_prefix()` 就得先连数据库，启动顺序会变得脆弱。
    ///
    /// ## 顺序（写死，永不重排）
    ///
    /// 前五个工具保持原索引 0..4，`memory` 固定为索引 5 ——
    /// 这样加它只让前缀**变长一次**，前五个工具的字节完全不变。
    pub fn render_for_prefix_with_memory() -> String {
        let base = Self::new_default();
        let mut out = base.render_for_prefix();

        // 索引 5 = memory（与 with_memory() 的追加顺序一致）
        out.push_str("[tool 5] name=");
        out.push_str("memory");
        out.push('\n');
        out.push_str("description=");
        out.push_str("读写长期记忆。你可以主动记住用户偏好、环境事实、踩过的坑与可复用流程——它们会在后续对话中自动注入，让你跨会话不忘事。用法：action=add 新增，replace/remove 用一段唯一短文本定位既有记忆。");
        out.push('\n');
        out.push_str("input_schema=");
        out.push_str(memory::MEMORY_SCHEMA);
        out.push('\n');

        // 索引 6 = todo（与 with_memory_and_todo() 的追加顺序一致）
        out.push_str("[tool 6] name=");
        out.push_str("todo");
        out.push('\n');
        out.push_str("description=");
        out.push_str("维护当前任务清单，让你在长任务里不丢目标、不重复规划。接到多步任务时先 write 一份完整计划；每完成一步就 update 对应条目为 done；开始某条前标 in_progress。清单每轮都会让你看到，随时用 read 回顾。");
        out.push('\n');
        out.push_str("input_schema=");
        out.push_str(todo::TODO_SCHEMA);
        out.push('\n');

        out
    }
}
#[cfg(test)]
mod tilde_tests {
    use super::*;

    /// 取当前 HOME（测试环境必有）。
    fn home() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("HOME").expect("测试环境应有 HOME"))
    }

    #[test]
    fn expands_bare_tilde() {
        assert_eq!(expand_tilde("~"), home());
    }

    #[test]
    fn expands_tilde_with_path() {
        assert_eq!(
            expand_tilde("~/wordfreq/main.py"),
            home().join("wordfreq/main.py")
        );
    }

    #[test]
    fn handles_nested_and_redundant_slashes() {
        assert_eq!(expand_tilde("~/a/b/c.txt"), home().join("a/b/c.txt"));
        // 多余斜杠应被规整
        assert_eq!(expand_tilde("~//a//b"), home().join("a/b"));
    }

    #[test]
    fn leaves_absolute_paths_alone() {
        assert_eq!(
            expand_tilde("/tmp/x.txt"),
            std::path::PathBuf::from("/tmp/x.txt")
        );
        assert_eq!(
            expand_tilde("/home/yq/x"),
            std::path::PathBuf::from("/home/yq/x")
        );
    }

    #[test]
    fn leaves_relative_paths_alone() {
        assert_eq!(
            expand_tilde("wordfreq/main.py"),
            std::path::PathBuf::from("wordfreq/main.py")
        );
        assert_eq!(expand_tilde("./x"), std::path::PathBuf::from("./x"));
    }

    #[test]
    fn does_not_expand_tilde_in_the_middle() {
        // 只展开**开头**的 ~
        assert_eq!(
            expand_tilde("/tmp/~weird"),
            std::path::PathBuf::from("/tmp/~weird")
        );
        assert_eq!(expand_tilde("a~b"), std::path::PathBuf::from("a~b"));
    }

    #[test]
    fn other_users_tilde_is_left_alone() {
        // ~someone 是别的用户的家目录，不猜
        assert_eq!(
            expand_tilde("~other/x"),
            std::path::PathBuf::from("~other/x")
        );
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(expand_tilde("  ~/x.txt  "), home().join("x.txt"));
    }
}
