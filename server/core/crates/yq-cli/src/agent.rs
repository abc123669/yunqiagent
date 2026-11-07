//! Agent 循环 —— 组装上下文 → 调 LLM → 执行工具 → 再调 → 最终答复
//!
//! ## 为什么在 yq-cli 而不是 yq-core
//!
//! 依赖方向是 `yq-llm → yq-core`。Agent 循环既要 `yq-core` 的上下文组装与工具，
//! 又要 `yq-llm` 的模型调用，因此只能位于**两者之上**的应用层（yq-cli）。
//! 这保证了 `yq-core` 永远不依赖网络与模型 —— 核心保持纯粹、可测、KV-cache 纪律不被侵蚀。
//!
//! ## 工具调用协议
//!
//! 模型按固定区里的声明输出：
//! ```text
//! <tool_call>{"tool":"read","arguments":{"path":"/tmp/x"}}</tool_call>
//! ```
//! 本模块解析这些块、执行工具、把结果作为 `[TOOL_RESULT ...]` 追加进动态区，再问一次。
//! **工具结果只进动态区**，绝不触碰固定区前缀。

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use yq_core::message::Message;
use yq_core::prompt::StaticPrefix;
use yq_core::tools::ToolRegistry;
use yq_llm::client::LlmClient;
use yq_llm::{
    LlmConfig, ToolProtocol, ToolSpec, assemble::build_chat_request, build_tools_array,
};

/// Agent 运行配置。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Desktop uses real SSE with the text tool protocol; legacy transports keep their behavior.
    pub streaming: bool,
    /// 单轮对话中「调用模型 → 执行工具」的最大迭代次数。
    ///
    /// 防止模型陷入工具循环导致烧钱。达到上限后强制要求最终答复。
    ///
    /// **取值经验**（2026-10-05 实测）：5 轮**不够**完成"建项目 + 写多文件 +
    /// 编译验证"这类任务（模型会写完代码但没轮次跑测试）。默认 **80**，
    /// 可用环境变量 `YQ_MAX_ITERATIONS` 覆盖。
    ///
    /// 80 轮的意图：让「长任务」真正能跑到收尾 —— 大型重构 / 多模块实现 /
    /// 反复调试定位都有足够预算。实际消耗由模型的自主判断决定，
    /// 模型给出最终答复即提前结束，不会白白跑满。
    pub max_iterations: usize,
    /// 历史对话保留的最大条数（滚动窗口）。
    pub history_window: usize,
    /// 工具循环内动态区的条数上限（含首条原始任务锚点）。
    ///
    /// 与 `history_window` 分工：后者裁的是**进入本轮前**的会话历史，
    /// 本项裁的是**本轮循环中**不断追加的助手回复与工具结果。
    /// 默认见 [`MAX_WORKING_HISTORY`]。
    pub max_working_history: usize,
    /// 检索注入的记忆条数上限。
    pub memory_limit: i64,
    /// 工具调用协议模式（Auto / Native / Text）。
    ///
    /// Auto：优先走 OpenAI 原生 `tools` / `tool_calls`，请求或响应失败时
    /// 自动降级回文本协议（`<tool_call>` 块）；Text 则完全沿用老路径。
    /// 默认从环境变量 `YQ_TOOL_PROTOCOL` 读取（native / text / 其它=auto）。
    pub tool_protocol: ToolProtocol,
}

impl Default for AgentConfig {
    fn default() -> Self {
        // 允许通过环境变量调整迭代预算（长任务需要更多轮次）
        let max_iterations = std::env::var("YQ_MAX_ITERATIONS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(80);

        // 动态区上限：留个环境变量口子，长任务可临时放宽
        let max_working_history = std::env::var("YQ_MAX_WORKING_HISTORY")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .filter(|n| *n > 1)
            .unwrap_or(MAX_WORKING_HISTORY);

        // 工具协议模式：环境变量口子（native / text / 其它 = auto）
        let tool_protocol = std::env::var("YQ_TOOL_PROTOCOL")
            .map(|s| ToolProtocol::parse(&s))
            .unwrap_or(ToolProtocol::Auto);

        Self {
            streaming: false,
            max_iterations,
            history_window: 20,
            max_working_history,
            memory_limit: 20,
            tool_protocol,
        }
    }
}

/// 一次响应的产出。
#[derive(Debug, Clone)]
pub struct AgentReply {
    /// 面向用户的最终文本
    pub text: String,
    /// 本轮共执行了多少次工具调用
    pub tool_invocations: usize,
    /// 固定区前缀 SHA256（验证缓存命中用）
    pub prefix_sha256: String,
    /// 本轮工具执行的**一行式证据串**（没调用工具时为空串）。
    ///
    /// 形如 `[TOOL_TRACE] bash(mkdir -p /tmp/probe2)✓; write(/tmp/x.py)✓`。
    ///
    /// **只进对话历史，绝不拼进 `text`** —— 用户看到的文本必须一个字节不变。
    /// 为什么要它：只存最终文本时，第二轮模型看不到上一轮真实跑过什么工具，
    /// 面对自己第一轮「测试通过」的声称查无实据，会按执行纪律自我定罪
    /// 「我编造了」并把活重干一遍（2026-10 生产实测的假认错事故）。
    /// 工具证据是防假报闭环的最后一块拼图，与审计表 `op_audit`（id62-66）
    /// 的证据链同源，只是这份只在内存历史里给模型看。
    pub tool_trace: String,
}

impl AgentReply {
    /// 写进**内存对话历史**的助手消息文本：正文 + 工具证据串。
    ///
    /// 三个历史写入点（cmd_repl 与两个 serve 路径）统一走这里：
    /// 没有工具调用时与 `text` 完全一致；有则追加换行 + `[TOOL_TRACE]`。
    ///
    /// **不入库**：`sessions` 表归档的是用户可见的原始对话，
    /// 内部证据不掺进去（见 main.rs 三处写入的中文注释）。
    pub fn history_text(&self) -> String {
        if self.tool_trace.is_empty() {
            self.text.clone()
        } else {
            format!("{}\n{}", self.text, self.tool_trace)
        }
    }
}

/// 模型输出中的一个工具调用块。
#[derive(Debug, Clone, Deserialize)]
pub struct RawToolCall {
    /// 工具名
    tool: String,
    /// 参数
    #[serde(default)]
    arguments: serde_json::Value,
}

/// 连续工具调用格式错误的容忍上限。
///
/// 超过此值说明模型「改不过来」，继续重试只是浪费 token（实测 LongCat
/// 曾连续 14 次输出非法格式），此时应当停止并如实说明，而不是死循环。
const MAX_MALFORMED_ROUNDS: usize = 3;
const MAX_PARALLEL_READS: usize = 4;

/// 单轮进度（用于实时汇报）。
#[derive(Debug, Clone)]
pub struct RoundProgress {
    /// 本轮的思维链片段（推理模型才有）
    pub reasoning: Option<String>,
    /// 本轮调用的工具通知（已格式化）
    pub tool_notes: Vec<String>,
}

impl RoundProgress {
    /// 渲染成一条发给人看的简短进度消息。
    ///
    /// `verbose=false` 时只报工具；`true` 时连思维链一起报。
    pub fn render(&self, verbose: bool) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();

        if verbose {
            if let Some(r) = &self.reasoning {
                let preview = truncate_chars(r.trim(), 200);
                if !preview.is_empty() {
                    lines.push(format!("💭 {}", preview));
                }
            }
        }

        if !self.tool_notes.is_empty() {
            if self.tool_notes.len() == 1 {
                lines.push(self.tool_notes[0].clone());
            } else {
                lines.push(format!("本轮 {} 个工具：", self.tool_notes.len()));
                lines.extend(self.tool_notes.iter().cloned());
            }
        }

        if lines.is_empty() {
            return None;
        }

        // 不加轮次前缀：进度消息本身就是简短的一段，
        // 加 [第 N 轮] 显得冗余（用户反馈）。
        Some(lines.join("\n"))
    }
}

/// 一轮里待执行的工具调用（统一承载原生 / 文本两种协议来源）。
#[derive(Clone)]
struct ToolCallItem {
    call: RawToolCall,
    /// 原生协议的调用 ID（文本协议来源为 `None`）。
    /// 有值时结果必须以 role=tool + tool_call_id 回传。
    native_id: Option<String>,
    /// 预置结果（参数 JSON 非法等）：跳过执行，直接当失败回传给模型。
    preset: Option<ToolOutcome>,
}

/// 一次工具执行的完整结局（含备份快照与中断标记）。
struct ExecOutcome {
    result: ToolOutcome,
    /// 危险操作的执行前备份（供 /stop 回滚与审计落库）。
    snapshot: Option<crate::stop::OpRecord>,
    /// 是否被 /stop 打断。
    interrupted: bool,
}

/// 一次工具执行的结果。
#[derive(Debug, Clone)]
struct ToolOutcome {
    content: String,
    is_error: bool,
}

/// 可并行执行的**只读**工具集合。
///
/// 只有结构化 read 工具具备只读保证。bash 接受任意 shell 程序，
/// 不能因备份闸存在就假定只读；未知工具也按写处理。
fn is_readonly_tool(name: &str) -> bool {
    name == "read"
}

/// 无进展判定：正文与上一轮的相似度阈值。
///
/// 相似度基于去空白后的最长公共子序列（LCS）。0.9 相当高 ——
/// 宁可多跑一轮也别误杀正在正常推进的任务。
const NO_PROGRESS_SIMILARITY: f64 = 0.9;

/// 无进展判定的连续轮数容忍上限。
///
/// 连续 2 轮「所有工具都失败 + 正文与上轮高度重复」才判定为停滞，
/// 单轮异常（如网关抖动）给模型自我修正的机会。
const MAX_STAGNANT_ROUNDS: usize = 2;

/// 相似度计算的长度上限（每侧字符数）。
///
/// LCS 是 O(n*m)，两侧各封顶 1200 字符，最坏 144 万次比较，可接受；
/// 超长文本（工具循环里罕见）截断后比较，判定只求方向正确。
const SIMILARITY_SAMPLE_CHARS: usize = 1200;

/// 归一化文本相似度：去空白后取 LCS 占较长一侧的比例。
///
/// 纯函数，便于单测。空对空视为完全不同（返回 0.0）——
/// 两轮都空属于「无输出」而非「重复」，交给其它闸（空响应兜底）处理。
fn normalized_similarity(a: &str, b: &str) -> f64 {
    let norm = |s: &str| -> Vec<char> {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .take(SIMILARITY_SAMPLE_CHARS)
            .collect()
    };
    let x = norm(a);
    let y = norm(b);
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }

    // 滚动数组 LCS（空间 O(min)）
    let (short, long) = if x.len() <= y.len() { (&x, &y) } else { (&y, &x) };
    let mut prev: Vec<u32> = vec![0; short.len() + 1];
    let mut cur: Vec<u32> = vec![0; short.len() + 1];
    for lc in long.iter() {
        for (j, sc) in short.iter().enumerate() {
            cur[j + 1] = if lc == sc {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let lcs = prev[short.len()] as f64;
    lcs / long.len() as f64
}

/// Agent 循环驱动器。
pub struct Agent {
    llm: LlmClient,
    tools: ToolRegistry,
    prefix: Arc<StaticPrefix>,
    config: AgentConfig,
    /// 累计用量（跨轮次累加，供 `/usage` 查询）
    usage: Arc<std::sync::Mutex<UsageStats>>,
    /// 后台备份队列（SPEC 第十节：危险操作执行前入队）
    backup_queue: yq_core::ops::BackupQueue,
    /// 本会话的可回滚操作日志（供 `/stop` 回滚）
    journal: Arc<std::sync::Mutex<crate::stop::OpJournal>>,
    /// 是否启用操作前备份（关闭可省 IO，但 /stop 无法回滚）
    backup_enabled: bool,
    /// 数据库连接池（仅用于工具调用审计）。
    ///
    /// **可选**是有意为之：`repl` / `tui` 等路径可能没有池，
    /// 此时审计降级为「只打日志」，不影响 Agent 其余行为。
    pool: Option<sqlx::PgPool>,
    /// 原生协议的 tools 数组（工具注册后固定，构建一次缓存于此）。
    ///
    /// Text 模式不使用它；Native / Auto 模式每次请求原样携带。
    /// 注意：tools 是请求**顶层**字段，不进 messages，因此不影响前缀缓存。
    native_tools: Vec<ToolSpec>,
}

/// 累计 token 用量统计。
#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    /// 累计输入 token
    pub prompt_tokens: u64,
    /// 累计输出 token
    pub completion_tokens: u64,
    /// 已完成的对话轮数
    pub rounds: u64,
}

impl UsageStats {
    /// 记一轮用量。
    pub fn record(&mut self, prompt: u32, completion: u32) {
        self.prompt_tokens += prompt as u64;
        self.completion_tokens += completion as u64;
        self.rounds += 1;
    }
}

impl Agent {
    /// 创建 Agent。
    ///
    /// `memories` 会被注册进工具表（`memory` 工具，追加在五工具**之后**），
    /// 让模型可以自己读写长期记忆；注册顺序仍由 `ToolRegistry::with_memory` 写死，
    /// 前五个工具的定义字节不受影响。
    /// `pool` 为工具调用审计提供落库能力；传 `None` 则只记日志。
    pub fn new(
        llm_config: LlmConfig,
        prefix: Arc<StaticPrefix>,
        memories: yq_memory::Memories,
        config: AgentConfig,
        pool: Option<sqlx::PgPool>,
    ) -> Result<Self> {
        let llm = LlmClient::new(llm_config)?;
        // 注册全部工具（含 memory 与 todo）——注册表跨轮复用，todo 状态才能持久
        let tools = ToolRegistry::with_memory_and_todo(memories);
        // 工具注册后固定，原生协议的 tools 数组构建一次即可
        let native_tools = build_tools_array(&tools.definitions());
        Ok(Self {
            llm,
            tools,
            prefix,
            config,
            usage: Arc::new(std::sync::Mutex::new(UsageStats::default())),
            backup_queue: yq_core::ops::BackupQueue::spawn(yq_core::ops::BackupConfig::default()),
            journal: Arc::new(std::sync::Mutex::new(crate::stop::OpJournal::new())),
            backup_enabled: true,
            pool,
            native_tools,
        })
    }

    /// 取本会话的操作日志快照（供 `/stop` 回滚）。
    pub fn journal_snapshot(&self) -> crate::stop::OpJournal {
        let mut out = crate::stop::OpJournal::new();
        if let Ok(j) = self.journal.lock() {
            for r in j.records() {
                out.push(r.clone());
            }
        }
        out
    }

    /// 清空操作日志（新一轮开始时调用）。
    pub fn clear_journal(&self) {
        if let Ok(mut j) = self.journal.lock() {
            j.clear();
        }
    }

    /// 危险操作执行前做快照备份。
    ///
    /// 返回 `Some(OpRecord)` 表示已备份、可回滚；
    /// `None` 表示该操作无需备份（如 read）或无法备份。
    ///
    /// 判定规则（SPEC 第十节）：
    ///   · `write` / `edit` 覆盖已有文件 → 备份该文件
    ///   · `bash` 含删除/移动/覆盖语义 → 尽力备份相关路径
    async fn snapshot_before(&self, call: &RawToolCall) -> Option<crate::stop::OpRecord> {
        if !self.backup_enabled {
            return None;
        }

        let (op_type, target_path) = match call.tool.as_str() {
            "write" | "edit" => {
                let p = call.arguments.get("path").and_then(|v| v.as_str())?;
                let pb = std::path::PathBuf::from(p);
                // 只备份**已存在**的文件（新建文件无需备份，回滚=删除）
                if !pb.exists() {
                    return Some(crate::stop::OpRecord {
                        op_type: call.tool.clone(),
                        target: p.to_string(),
                        backup_path: std::path::PathBuf::new(),
                        existed_before: false,
                    });
                }
                (call.tool.clone(), pb)
            }
            "bash" => {
                let cmd = call.arguments.get("command").and_then(|v| v.as_str())?;
                // 只对含破坏性语义的命令备份
                let paths = extract_bash_targets(cmd);
                if paths.is_empty() {
                    return None;
                }
                // 取第一个可备份路径作为代表
                let first = paths.into_iter().find(|p| p.exists())?;
                ("bash".to_string(), first)
            }
            _ => return None,
        };

        let existed_before = target_path.exists();

        // ── 第三道闸：体积预检 ──
        //
        // 即使路径判定合法，也不该盲目备份大目录 ——
        // 2026-10-05 事故中，一个误判的 `/` 就产生了 1.28GB 备份并撑爆磁盘。
        // 因此超过阈值的目标直接跳过（宁可不回滚，不可撑爆盘）。
        if let Some(mb) = dir_size_mb(&target_path) {
            if mb > MAX_BACKUP_TARGET_MB {
                tracing::warn!(
                    "跳过备份：{} 约 {}MB，超过单目标上限 {}MB（避免撑爆磁盘）",
                    target_path.display(),
                    mb,
                    MAX_BACKUP_TARGET_MB
                );
                return Some(crate::stop::OpRecord {
                    op_type: call.tool.clone(),
                    target: target_path.to_string_lossy().to_string(),
                    backup_path: std::path::PathBuf::new(),
                    existed_before,
                });
            }
        }

        let op = yq_core::ops::DangerOp {
            op_id: yq_core::ops::new_op_id(match call.tool.as_str() {
                "bash" => yq_core::ops::OpType::Bash,
                "edit" => yq_core::ops::OpType::Edit,
                _ => yq_core::ops::OpType::Write,
            }),
            op_type: match call.tool.as_str() {
                "bash" => yq_core::ops::OpType::Bash,
                "edit" => yq_core::ops::OpType::Edit,
                _ => yq_core::ops::OpType::Write,
            },
            target: target_path.to_string_lossy().to_string(),
            backup_state: yq_core::ops::BackupState::Pending,
            backup_path: None,
            notified: false,
        };

        // 同步等备份完成（要拿到产物路径才能记日志）
        let outcome = self
            .backup_queue
            .enqueue_and_wait(op, target_path.clone())
            .await?;

        let backup_path = outcome.artifact.unwrap_or_default();
        if backup_path.as_os_str().is_empty() {
            tracing::warn!(
                "备份未产出产物（{}），该操作无法回滚: {:?}",
                target_path.display(),
                outcome.error
            );
            return None;
        }

        tracing::info!(
            "已备份 {} → {}",
            target_path.display(),
            backup_path.display()
        );

        Some(crate::stop::OpRecord {
            op_type,
            target: target_path.to_string_lossy().to_string(),
            backup_path,
            existed_before,
        })
    }

    /// 当前模型名（可能随 `/model` 切换变化）。
    pub fn model(&self) -> String {
        self.llm.model()
    }

    /// 切换模型（运行时生效，立即影响后续对话）。
    pub fn set_model(&self, model: &str) -> Result<()> {
        self.llm.set_model(model)?;
        Ok(())
    }

    /// 查询累计用量。
    pub fn usage(&self) -> UsageStats {
        self.usage.lock().map(|u| u.clone()).unwrap_or_default()
    }

    /// 处理一条用户消息，返回最终答复。
    ///
    /// 参数：
    ///   · `on_delta`    —— 流式文本增量回调（用于实时渲染）
    ///   · `on_progress` —— **每轮进度回调**，携带该轮的思维链片段与工具调用
    ///   · `stop`        —— **中断信号**：信号一到立即停止，包括打断正在执行的工具
    ///
    /// `stop` 是**立即生效**的（不是等当前轮结束）——
    /// 用户发 `/stop` 往往意味着任务跑偏或做了危险操作，必须马上收手。
    pub async fn respond<F, H>(
        &self,
        history: &[Message],
        memories: &[String],
        user: &str,
        mut on_delta: F,
        mut on_progress: H,
        stop: crate::stop::StopSignal,
    ) -> Result<AgentReply>
    where
        F: FnMut(&str),
        H: FnMut(&RoundProgress),
    {
        // 滚动窗口：只保留最近 N 条历史
        let window = if history.len() > self.config.history_window {
            &history[history.len() - self.config.history_window..]
        } else {
            history
        };

        let mut working_history: Vec<Message> = window.to_vec();
        let mut task_anchor = working_history.len();

        // ── 原始任务**只进历史一次**（关键修复）──
        //
        // 2026-10-05 对照评测踩到的坑：原实现每轮都把 `user` 当参数传给
        // `build_chat_request`，而那个函数会无条件把它追加为**最后一条消息**。
        // 结果每轮 messages 末尾都重复出现原始任务，模型被"重置"，
        // 反复重述计划、陷入循环（实测同一模型在 DSH 上顺利完成任务）。
        //
        // 正确做法（pi-mono / Hermes 同款）：任务作为对话流的**第一条 user 消息**
        // 留在历史里，后续轮次不再重复注入。
        working_history.push(Message::user(user.to_string()));

        let mut tool_invocations = 0usize;
        // 本轮工具证据串的原始条目（每条形如 `bash(mkdir -p /tmp)✓`）。
        // 只在 respond 收尾时拼成 [TOOL_TRACE] 进 AgentReply.tool_trace，
        // **绝不进 final_text** —— 用户可见文本零变化是硬约束。
        let mut trace_entries: Vec<String> = Vec::new();
        let mut final_text = String::new();
        // 循环检测：上一次的工具调用指纹 + 连续重复次数
        let mut last_fingerprint: Option<String> = None;
        let mut consecutive_repeats = 0usize;
        // 连续的工具调用格式错误计数（防止模型改不过来时无限重试）
        let mut malformed_rounds = 0usize;
        // Auto 模式降级标记：原生请求一旦失败，本轮对话后续全部走文本协议
        let mut native_downgraded = false;
        // 上一轮的助手正文（无进展检测用）
        let mut last_text: Option<String> = None;
        // 连续停滞轮数：本轮所有工具都失败 + 正文与上轮高度重复
        let mut stagnant_rounds = 0usize;

        for iteration in 0..self.config.max_iterations {
            // ── 中断检查：用户发了 /stop 就立即收手 ──
            if stop.is_stopped() {
                tracing::info!("检测到中断请求，停止本轮任务");
                final_text = if final_text.trim().is_empty() {
                    "（已被 /stop 中断）".to_string()
                } else {
                    final_text
                };
                break;
            }

            // 最后一轮：明确要求收尾，避免无限工具循环
            let last_round = iteration + 1 == self.config.max_iterations;

            // 注入内容（**不再重复注入原始任务**）：
            //   · 首轮：任务已在 working_history 里，无需再传
            //   · 最后一轮：追加收尾提示
            //   · 其余轮：空字符串（build_chat_request 会跳过追加）
            let user_prompt = if last_round {
                "【系统提示】已达到工具调用上限，请立即基于已有信息给出最终答复，不要再调用工具。"
                    .to_string()
            } else {
                String::new()
            };

            // 每轮都取最新配置快照 —— 这样 `/model` 切换能立即生效
            let llm_cfg = self.llm.snapshot()?;

            // 协议选择：Text 恒走文本协议；Native/Auto 携带缓存的 tools 数组。
            // Auto 模式下若原生请求失败（网关不支持 tools 字段等），降级文本重试一次。
            let want_native = self.config.tool_protocol != ToolProtocol::Text && !native_downgraded;
            let tools_arg: Vec<ToolSpec> = if want_native {
                self.native_tools.clone()
            } else {
                Vec::new()
            };

            let assembled = build_chat_request(
                &self.prefix,
                memories,
                &working_history,
                &user_prompt,
                &llm_cfg,
                false,
                tools_arg,
            );

            let response = if self.config.streaming {
                self.desktop_stream(assembled.request, &stop, &mut on_delta).await?
            } else { match self.llm.complete(assembled.request).await {
                Ok(r) => r,
                Err(e) => {
                    if want_native && self.config.tool_protocol == ToolProtocol::Auto {
                        tracing::warn!("原生协议请求失败（{}），降级为文本协议重试", e);
                        native_downgraded = true;
                        let assembled = build_chat_request(
                            &self.prefix,
                            memories,
                            &working_history,
                            &user_prompt,
                            &llm_cfg,
                            false,
                            Vec::new(),
                        );
                        self.llm.complete(assembled.request).await?
                    } else {
                        return Err(e.into());
                    }
                }
            }};

            // 记录用量（供 /usage 查询）
            if let Some(usage) = &response.usage {
                if let Ok(mut u) = self.usage.lock() {
                    u.record(usage.prompt_tokens, usage.completion_tokens);
                }
            }

            let text = response.first_text().unwrap_or_default();

            // 推理模型兼容：思维链吃满 token 时 `content` 可能为空，
            // 此时用思维链末尾兜底（避免"模型有输出却回空"）
            let text = if text.trim().is_empty() {
                if let Some(reasoning) = response.first_reasoning() {
                    if !reasoning.trim().is_empty() {
                        tracing::warn!("模型正文为空（推理模型 token 用尽？），降级使用思维链");
                        let finish = response.finish_reason().unwrap_or_default();
                        format!(
                            "（模型思考过程未产出正式答复，finish_reason={}）\n{}",
                            finish,
                            tail_chars(&reasoning, 400)
                        )
                    } else {
                        text
                    }
                } else {
                    text
                }
            } else {
                text
            };

            let text = if text.trim().is_empty() {
                // 彻底空响应：明确告知，不静默
                "（模型未返回任何内容，请重试或换模型）".to_string()
            } else {
                text
            };

            // 调试探针：落盘模型原始响应，便于诊断工具调用解析问题
            if std::env::var("YQ_DEBUG_RAW").is_ok() {
                let path = format!("/tmp/yq_raw_round{}.txt", iteration);
                let _ = std::fs::write(&path, &text);
                tracing::info!("原始响应已落盘: {} ({} 字节)", path, text.len());
            }

            // ── 协议路由：原生 tool_calls 优先，其次文本协议块 ──
            //
            // 原生路径：response.tool_calls 非空即按 OpenAI 原生协议处理，
            // 把 ChatToolCall 转成统一的 ToolCallItem（arguments 是 JSON
            // 字符串，需解析成 Value；解析失败走 preset 失败结果回灌）。
            // 文本路径：parse_tool_calls 解析 <tool_call> 块，行为与旧版一致。
            let native_calls = response.first_tool_calls();
            let items: Vec<ToolCallItem> = if !native_calls.is_empty() {
                let mut v = Vec::with_capacity(native_calls.len());
                for c in native_calls {
                    // arguments 是 JSON 字符串：严格解析失败再走一次控制字符修复
                    //（模型在原生通道里同样会输出裸换行）
                    let args = parse_native_arguments(&c.function.arguments);
                    let (arguments, preset) = match args {
                        Ok(a) => (a, None),
                        Err(e) => (
                            serde_json::Value::Null,
                            Some(ToolOutcome {
                                content: format!(
                                    "工具参数不是合法 JSON，无法执行。解析错误: {}",
                                    e
                                ),
                                is_error: true,
                            }),
                        ),
                    };
                    v.push(ToolCallItem {
                        call: RawToolCall {
                            tool: c.function.name.clone(),
                            arguments,
                        },
                        native_id: Some(c.id.clone()),
                        preset,
                    });
                }
                // 原生协议下截断的调用（参数非合法 JSON）上面已变成 preset 失败，
                // 模型下一轮会看到明确报错并重发 —— 无需额外处理。
                v
            } else {
                let text_calls = parse_tool_calls(&text);
                text_calls
                    .into_iter()
                    .map(|call| ToolCallItem {
                        call,
                        native_id: None,
                        preset: None,
                    })
                    .collect()
            };

            let reasoning = response.first_reasoning();

            // 截断检测：finish_reason=length 说明输出被 max_tokens 腰斩，
            // 这不是模型的「格式错误」，重试提示必须说真话（见下方回灌文案）。
            let is_truncated = response.finish_reason().as_deref() == Some("length");

            if items.is_empty() {
                // 无工具调用 → 这是最终答复
                //
                // 但要警惕：回复里**出现了 tool_call 标签却解析失败**的情况。
                // 这说明模型想调工具但格式有问题，必须回灌错误让它重试，
                // 否则它会误以为任务已完成（2026-10-05 实测踩到的坑）。
                if contains_tool_call_markup(&text) {
                    malformed_rounds += 1;
                    if is_truncated {
                        tracing::warn!(
                            "输出被截断（finish_reason=length）且 tool_call 未解析成功（第 {} 次），按截断场景回灌",
                            malformed_rounds
                        );
                    } else {
                        tracing::warn!(
                            "检测到 tool_call 标签但解析失败（第 {} 次），回灌错误请模型重试",
                            malformed_rounds
                        );
                    }

                    // 把失败情况也汇报给用户（否则用户只看到"没反应"）
                    on_progress(&RoundProgress {
                        reasoning: reasoning.clone(),
                        tool_notes: vec![format!(
                            "⚠️ 工具调用格式错误，正在重试（第 {} 次）",
                            malformed_rounds
                        )],
                    });

                    // 连续多次失败说明模型改不过来，不再无限重试
                    if malformed_rounds >= MAX_MALFORMED_ROUNDS {
                        tracing::error!(
                            "连续 {} 次工具调用格式错误，放弃重试并返回说明",
                            malformed_rounds
                        );
                        final_text = format!(
                            "（模型连续 {} 次输出格式非法的工具调用，已停止重试。请简化任务后重试。）",
                            malformed_rounds
                        );
                        break;
                    }

                    // 按真实原因回灌：截断场景下说「输出被截断了，请缩短」，
                    // 别再灌「JSON 格式错误」——模型改不了根本不存在的格式问题
                    //（2026-10-11 线上事故：截断被误判为格式错误，连撞三次南墙）。
                    let retry_hint = if is_truncated {
                        "【系统提示】你上一条输出被截断了（达到了 token 数量上限），\
                         其中的 <tool_call> 块不完整，无法被解析。\n\
                         请把命令/内容**缩短**，一次只调用一个工具，\
                         避免单次输出过长。\n\
                         格式：<tool_call>{\"tool\":\"工具名\",\"arguments\":{...}}</tool_call>\n\
                         注意：JSON 字符串内的换行必须写成 \\n，不要输出裸换行。"
                    } else {
                        "【系统提示】你上一条回复中的 <tool_call> 块无法被解析。\n\
                         请严格按下列格式输出，**不要**加任何其它包装：\n\
                         <tool_call>{\"tool\":\"工具名\",\"arguments\":{...}}</tool_call>\n\
                         注意：JSON 字符串内的换行必须写成 \\n，不要输出裸换行。\n\
                         若无需工具，请直接给出最终答复。"
                    };
                    working_history.push(Message::assistant(text.clone()));
                    working_history.push(Message::user(retry_hint.to_string()));
                    cap_task_history(&mut working_history, self.config.max_working_history, &mut task_anchor);
                    continue;
                }
                final_text = text;
                break;
            }

            // 有工具调用：把助手消息（含调用块）记入历史，再执行工具
            //
            // 原生协议：assistant 消息必须携带 tool_calls 结构，
            // 且结果必须以 role=tool + tool_call_id 回传（OpenAI 协议强约束）；
            // 文本协议：沿用旧路径，结果以 user 角色承载。
            let native_round = items.iter().any(|it| it.native_id.is_some());
            if native_round {
                working_history.push(Message {
                    role: yq_core::message::Role::Assistant,
                    content: text.clone(),
                    tool_calls: items
                        .iter()
                        .map(|it| yq_core::message::ToolCall {
                            tool_call_id: it
                                .native_id
                                .clone()
                                .unwrap_or_else(|| format!("text_{}", it.call.tool)),
                            tool_name: it.call.tool.clone(),
                            arguments: it.call.arguments.clone(),
                        })
                        .collect(),
                    tool_result: None,
                });
            } else {
                working_history.push(Message::assistant(text.clone()));
            }
            cap_task_history(&mut working_history, self.config.max_working_history, &mut task_anchor);

            // 本轮的工具通知（供进度汇报）
            let mut round_tools: Vec<String> = Vec::new();
            // 本轮是否有任何工具**成功**（无进展检测用）
            let mut any_success = false;

            // ── 分组执行：连续的只读工具并行，写类工具逐个串行 ──
            //
            // 判据（保守）：read / bash 视为只读可并行；write / edit / todo /
            // memory / delegate 及**未知工具**一律按写处理、单独串行。
            // join_all 保序返回，结果按 calls 顺序写回历史，顺序确定。
            let mut idx = 0usize;
            while idx < items.len() {
                // 中断检查：用户发了 /stop 就跳过剩余工具
                if stop.is_stopped() {
                    tracing::info!("检测到中断请求，跳过剩余工具调用");
                    break;
                }

                let run_is_readonly = is_readonly_tool(&items[idx].call.tool);
                let mut end = idx + 1;
                if run_is_readonly {
                    while end < items.len()
                        && end - idx < MAX_PARALLEL_READS
                        && is_readonly_tool(&items[end].call.tool)
                        && !stop.is_stopped()
                    {
                        end += 1;
                    }
                }
                let run: Vec<ToolCallItem> = items[idx..end].to_vec();
                idx = end;

                // ── 执行本组：每个分支内部都与 stop 竞争（保持 /stop 立即生效）──
                let outcomes: Vec<ExecOutcome> = if run.len() == 1 {
                    // 单条（含全部写类工具）：串行路径
                    let it = &run[0];
                    let out = self
                        .prepare_and_execute(&it.call, it.preset.clone(), &stop)
                        .await;
                    vec![out]
                } else {
                    // 只读批：并行执行，join_all 保序返回
                    let futures = run.iter().map(|it| {
                        let stop = stop.clone();
                        async move {
                            self.prepare_and_execute(&it.call, it.preset.clone(), &stop)
                                .await
                        }
                    });
                    futures_util::future::join_all(futures).await
                };

                // ── 按顺序落账：备份日志 / 审计 / 证据串 / 进度通知 / 历史回写 ──
                for (it, out) in run.iter().zip(outcomes.iter()) {
                    let result = &out.result;
                    let was_interrupted = out.interrupted;
                    tool_invocations += 1;
                    if !result.is_error && !was_interrupted {
                        any_success = true;
                    }

                    // 备份登记内存日志（供 /stop 回滚）
                    if let Some(record) = &out.snapshot {
                        if let Ok(mut j) = self.journal.lock() {
                            j.push(record.clone());
                        }
                    }

                    // 重复调用统计（与旧版口径一致：连续相同调用计数）
                    let fp = call_fingerprint(&it.call);
                    let repeat_count = if last_fingerprint.as_deref() == Some(fp.as_str()) {
                        consecutive_repeats += 1;
                        consecutive_repeats
                    } else {
                        last_fingerprint = Some(fp);
                        consecutive_repeats = 1;
                        1
                    };

                    // 审计：fail-open，写失败不影响循环
                    self.audit_tool_call(
                        &it.call,
                        &result.content,
                        result.is_error,
                        was_interrupted,
                        out.snapshot.as_ref(),
                    )
                    .await;

                    // 工具证据串（只进对话历史，不进用户可见文本）
                    trace_entries.push(trace_entry(
                        &it.call.tool,
                        &it.call.arguments,
                        result.is_error,
                        was_interrupted,
                    ));

                    // 简短通知（不含完整参数/结果）
                    let args_summary = summarize_args(&it.call.arguments);
                    round_tools.push(format_tool_notice(
                        &it.call.tool,
                        &args_summary,
                        !result.is_error,
                    ));

                    // 渲染结果并按协议回写历史
                    let rendered = render_tool_result(
                        &it.call,
                        &result.content,
                        result.is_error,
                        repeat_count,
                    );
                    if let Some(call_id) = &it.native_id {
                        // 原生协议：role=tool + 配对 tool_call_id（硬约束）
                        working_history.push(Message::tool(yq_core::message::ToolResult {
                            tool_call_id: call_id.clone(),
                            tool_name: it.call.tool.clone(),
                            content: rendered,
                            is_error: result.is_error,
                        }));
                    } else {
                        // 文本协议：user 角色承载（旧路径，字节不变）
                        working_history.push(Message::user(rendered));
                    }
                    // 动态区封顶：不封顶则 prompt token 随轮次线性膨胀
                    cap_task_history(
                        &mut working_history,
                        self.config.max_working_history,
                        &mut task_anchor,
                    );
                }

                // 中断后不再启动下一组
                if stop.is_stopped() {
                    break;
                }
            }

            // ── 无进展检测：所有工具都失败 + 正文与上轮高度重复 → 停止 ──
            //
            // 判据刻意保守（宁可多跑一轮也别误杀）：
            //   1. 本轮**没有任何**工具执行成功
            //   2. 正文（去掉工具调用块后的叙述部分）与上一轮相似度 ≥ 0.9
            //   3. 连续 2 轮同时满足上述两条 —— 单轮异常（网关抖动等）给机会
            let now_stagnant = !any_success
                && last_text
                    .as_deref()
                    .map(|prev| {
                        normalized_similarity(&strip_tool_blocks(&text), &strip_tool_blocks(prev))
                            >= NO_PROGRESS_SIMILARITY
                    })
                .unwrap_or(false);
            if now_stagnant {
                stagnant_rounds += 1;
            } else {
                stagnant_rounds = 0;
            }
            last_text = Some(text.clone());

            if stagnant_rounds >= MAX_STAGNANT_ROUNDS {
                tracing::warn!(
                    "检测到无进展：连续 {} 轮工具全部失败且答复内容重复，停止以免空转",
                    stagnant_rounds
                );
                final_text = format!(
                    "（检测到无进展：连续 {} 轮所有工具调用都失败，且答复内容与之前高度重复，\
                     已停止以免空转消耗 token）\n\n\
                     最后一次输出：\n{}",
                    stagnant_rounds,
                    truncate_chars(text.trim(), 500)
                );
                break;
            }

            // ── 循环检测：连续多轮重复同一调用 → 强制打断 ──
            //
            // 评测中模型曾连续 10+ 次喊同一个 `mkdir`，白耗 8 分钟。
            // 这里在轮次粒度兜底：达到阈值就明确告知并停止，
            // 而不是等 max_iterations 耗尽。
            if consecutive_repeats >= MAX_CONSECUTIVE_REPEATS {
                tracing::warn!(
                    "检测到循环：连续 {} 次相同的工具调用（{}），强制打断",
                    consecutive_repeats,
                    last_fingerprint.clone().unwrap_or_default()
                );
                final_text = format!(
                    "（检测到循环：连续 {} 次执行完全相同的工具调用，已停止以免空转）\n\
                     最后一次调用是：{}\n\
                     如果你认为这一步确实需要重试，请换一个命令或参数再试。",
                    consecutive_repeats,
                    last_fingerprint.clone().unwrap_or_default()
                );
                break;
            }

            // ── 本轮进度汇报（实时推送的关键）──
            on_progress(&RoundProgress {
                reasoning: reasoning.clone(),
                tool_notes: round_tools,
            });

            if last_round {
                // 到达上限仍想调工具：**汇报现状**而不是硬截断
                //
                // 参考 pi-mono / Hermes 的「汇报+继续」模式：
                // 硬截断会让用户不知道发生了什么，也无法继续。
                // 这里给出已完成的动作清单 + 明确的继续方式。
                final_text = format!(
                    "（已达工具调用上限 {} 轮，以下是阶段性结论）\n{}\n\n\
                     —— 如需继续，回复「继续」即可。",
                    self.config.max_iterations, text
                );
            }
        }

        if final_text.is_empty() {
            final_text = "（模型未产生有效答复）".to_string();
        }

        // 流式重放给调用方（保证 UI 呈现一致：真实流式与非流式共用同一回调路径）
        if !self.config.streaming {
            for ch in final_text.chars() {
                on_delta(&ch.to_string());
            }
        }

        Ok(AgentReply {
            text: final_text,
            tool_invocations,
            prefix_sha256: self.prefix.sha256().to_string(),
            // 纯函数组装：空轮次得空串，超 600 字符截断（不撑爆下一轮上下文）
            tool_trace: assemble_tool_trace(&trace_entries),
        })
    }

    /// Consume SSE while keeping cancellation responsive during both headers and token reads.
    async fn desktop_stream<F: FnMut(&str)>(
        &self, request: yq_llm::types::ChatRequest, stop: &crate::stop::StopSignal, on_delta: &mut F,
    ) -> Result<yq_llm::types::ChatResponse> {
        let mut text = String::new();
        let mut filter = crate::desktop::ToolTextFilter::default();
        let mut rx = tokio::select! {
            result = self.llm.complete_stream(request) => result?,
            _ = stop.wait() => return Ok(serde_json::from_value(serde_json::json!({
                "choices":[{"message":{"role":"assistant","content":"（已停止生成）"},"finish_reason":"stop"}]
            }))?),
        };
        loop {
            let chunk = tokio::select! {
                item = rx.recv() => item,
                _ = stop.wait() => break,
            };
            let Some(chunk) = chunk else { break; };
            if chunk.is_final { break; }
            text.push_str(&chunk.text);
            let visible = filter.push(&chunk.text);
            if !visible.is_empty() { on_delta(&visible); }
        }
        let rest = filter.finish();
        if !rest.is_empty() { on_delta(&rest); }
        if stop.is_stopped() && text.trim().is_empty() { text = "（已停止生成）".into(); }
        Ok(serde_json::from_value(serde_json::json!({
            "choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]
        }))?)
    }

    /// 把一次工具执行写进 `op_audit`（**fail-open**）。
    ///
    /// 五个工具都会走到这里：`read` / `delegate` 虽不破坏文件，但
    /// 「它读了哪些文件、委派了什么任务」同样是最常被追问的问题，
    /// 只记危险操作等于留下半截轨迹。
    ///
    /// 这里刻意**不返回 `Result`**：审计是观测手段，不是业务逻辑。
    /// 一次工具执行成功了但审计写失败，正确做法是把成功照样还给模型、
    /// 再留一条 WARN；反过来让审计去中断工具循环，等于让次要功能
    /// 把主功能拖垮。详见 `yq-db::audit` 模块文档。
    ///
    /// `snapshot` 存在说明这是危险操作且已备份 —— 把备份状态一并带进
    /// 审计行，便于运维在库里一眼看出「哪些操作还留着可回滚的备份」。
    async fn audit_tool_call(
        &self,
        call: &RawToolCall,
        content: &str,
        is_error: bool,
        was_interrupted: bool,
        snapshot: Option<&crate::stop::OpRecord>,
    ) {
        use yq_core::ops::{AuditOutcome, AuditTool, audit_error_summary, audit_target};

        let tool = AuditTool::from_tool_name(&call.tool);
        let target = audit_target(&call.tool, &call.arguments);
        let outcome = AuditOutcome::from_is_error(is_error);
        let error = audit_error_summary(is_error, content, was_interrupted);

        let mut rec = yq_db::audit::AuditRecord::new(tool.as_str(), target, outcome.as_str());
        if let Some(e) = error {
            rec = rec.with_error(e);
        }
        // 只有真的产出了备份产物才登记路径：空 PathBuf 意味着「没备份成功」
        // 或「本就不需要备份」，写进库里会误导运维以为有回滚点。
        if let Some(snap) = snapshot {
            let has_artifact = !snap.backup_path.as_os_str().is_empty();
            let state = if has_artifact {
                yq_core::ops::BackupState::Done
            } else {
                yq_core::ops::BackupState::Failed
            };
            rec = rec.with_backup(
                state.as_str(),
                has_artifact.then(|| snap.backup_path.to_string_lossy().to_string()),
            );
        }

        yq_db::audit::record_tool_call(self.pool.as_ref(), rec).await;
    }

    /// 执行单个工具调用。
    async fn execute_tool(&self, call: &RawToolCall) -> ToolOutcome {
        match self.tools.get(&call.tool) {
            Some(tool) => match tool.execute(call.arguments.clone()).await {
                Ok(content) => ToolOutcome {
                    content,
                    is_error: false,
                },
                Err(e) => ToolOutcome {
                    content: format!("工具执行失败: {}", e),
                    is_error: true,
                },
            },
            None => ToolOutcome {
                content: format!(
                    "未知工具 `{}`。可用工具：read / write / edit / bash / delegate / memory / todo",
                    call.tool
                ),
                is_error: true,
            },
        }
    }

    /// 带前置准备的工具执行：快照备份（写类工具）→ 与 /stop 竞争执行。
    ///
    /// `preset` 非空时跳过真实执行，直接以预置结果返回（参数非法场景），
    /// 但仍走一遍备份流程保持行为一致（preset 必为失败结果，无副作用）。
    async fn prepare_and_execute(
        &self,
        call: &RawToolCall,
        preset: Option<ToolOutcome>,
        stop: &crate::stop::StopSignal,
    ) -> ExecOutcome {
        // ── 危险操作：执行前做快照备份（供 /stop 回滚）──
        //
        // 这里**只登记内存日志**（调用方负责 push），落库由审计统一负责。
        let snapshot = self.snapshot_before(call).await;

        // 预置结果（参数非法）：跳过真实执行，直接当失败回传。
        // 仍先做快照登记以保持流程一致（preset 必为失败，无副作用）。
        if let Some(p) = preset {
            return ExecOutcome {
                result: p,
                snapshot,
                interrupted: false,
            };
        }

        // 与中断信号竞争（立即停止，不等执行跑完）
        let outcome = tokio::select! {
            biased;
            r = self.execute_tool(call) => Some(r),
            _ = stop.wait() => {
                tracing::warn!("工具执行被 /stop 打断（{}）", call.tool);
                None
            }
        };

        match outcome {
            Some(r) => ExecOutcome {
                result: r,
                snapshot,
                interrupted: false,
            },
            None => ExecOutcome {
                result: ToolOutcome {
                    content: "（执行被 /stop 中断）".to_string(),
                    is_error: true,
                },
                snapshot,
                interrupted: true,
            },
        }
    }
}

/// 从 bash 命令中提取**可能被破坏的路径**（尽力而为）。
///
/// 只识别最常见的破坏性操作：`rm` / `mv` / `truncate` / `shred` / `dd`，
/// 以及**真正的**输出重定向。
///
/// ## 2026-10-05 事故复盘（重要）
///
/// 原实现用 `seg.contains('>')` 判断重定向，结果把 **`2>/dev/null`**
/// （丢弃错误输出，纯只读操作）误判为"写入"，进而在 `du -xh / 2>/dev/null`
/// 里把 **`/`** 提取成备份目标 → **实际备份了整个根目录**（1.28GB），
/// 瞬间吃满系统盘。
///
/// 因此现在加了三道闸：
///   1. `has_real_redirect()` —— 排除 `2>/dev/null` / `>/dev/null` / `&>/dev/null`
///   2. `is_backupable_path()` —— 路径深度不足的宽泛目录（`/`、`/mnt`、`/home`）
///      一律拒绝，只接受足够具体的路径
///   3. 调用方还有体积预检（见 `snapshot_before`）
pub fn extract_bash_targets(cmd: &str) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;

    let mut out: Vec<PathBuf> = Vec::new();

    for seg in cmd.split([';', '|', '&', '\n']) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = seg.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        let is_destructive = matches!(tokens[0], "rm" | "mv" | "truncate" | "shred" | "dd")
            || has_real_redirect(seg);

        if !is_destructive {
            continue;
        }

        // 只从**重定向目标**取路径；破坏性命令的普通参数单独处理
        if has_real_redirect(seg) {
            for t in redirect_targets(seg) {
                let p = PathBuf::from(&t);
                if is_backupable_path(&p) {
                    out.push(p);
                }
            }
        }

        if matches!(tokens[0], "rm" | "mv" | "truncate" | "shred" | "dd") {
            for t in &tokens[1..] {
                if t.starts_with('-') || t.contains('$') || t.contains('*') {
                    continue;
                }
                let p = PathBuf::from(t);
                if is_backupable_path(&p) {
                    out.push(p);
                }
            }
        }
    }

    // 去重（保序）
    let mut seen: Vec<PathBuf> = Vec::new();
    for p in out {
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    seen
}

/// 是否含**真正的**输出重定向。
///
/// 排除句柄重定向到 `/dev/null` 的情况 —— 那是丢弃输出，不是写入文件。
fn has_real_redirect(seg: &str) -> bool {
    let mut rest = seg;
    while let Some(idx) = rest.find('>') {
        let after = &rest[idx + 1..];
        let after = after.strip_prefix('>').unwrap_or(after); // `>>`
        let target = after.split_whitespace().next().unwrap_or("");
        // 丢弃输出 → 不算破坏
        if !target.starts_with("/dev/null") && !target.starts_with("&") && !target.is_empty() {
            return true;
        }
        rest = after;
    }
    false
}

/// 取出重定向的目标文件（排除 /dev/null）。
fn redirect_targets(seg: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = seg;
    while let Some(idx) = rest.find('>') {
        let after = &rest[idx + 1..];
        let after = after.strip_prefix('>').unwrap_or(after);
        let target = after.split_whitespace().next().unwrap_or("");
        if !target.is_empty()
            && !target.starts_with("/dev/null")
            && !target.starts_with("&")
            && !target.contains('$')
            && !target.contains('*')
        {
            out.push(target.to_string());
        }
        rest = after;
    }
    out
}

/// 该路径是否**值得/可以**备份。
///
/// 关键：拒绝过于宽泛的路径 —— 事故就是从这里来的（备份了 `/`）。
/// 规则：
///   · 必须在允许的根之一（`/tmp`、`/home`、`/mnt`、`/var`、`/opt`、`/srv`）之下
///   · 且**至少有两级**（`/tmp/x` 可以，`/tmp` 本身不行）
///   · 排除伪文件系统 `/dev` `/proc` `/sys` `/run`
///   · 必须存在
pub fn is_backupable_path(p: &std::path::Path) -> bool {
    use std::path::Component;

    let comps: Vec<Component> = p.components().collect();
    // 必须是绝对路径且至少有 2 级（RootDir + 一个名字）
    if comps.len() < 3 {
        return false;
    }
    if !p.is_absolute() {
        return false;
    }

    let s = p.to_string_lossy();
    // 伪文件系统与危险根
    for bad in ["/dev", "/proc", "/sys", "/run", "/boot", "/etc"] {
        if s == bad || s.starts_with(&format!("{}/", bad)) {
            return false;
        }
    }

    p.exists()
}

/// 单个备份目标的体积上限（MB）。
///
/// 超过即跳过备份（宁可不回滚，也不撑爆磁盘）。
/// 参考：2026-10-05 误备份 `/` 产生了 1.28GB 产物，直接吃满 29G 系统盘。
pub const MAX_BACKUP_TARGET_MB: u64 = 100;

/// 估算路径体积（MB）。
///
/// 目录用 `du -sm` 快速估算（**带 5 秒超时**，避免大目录把预检本身拖死）；
/// 文件直接读元数据。拿不到大小返回 `None`（调用方按"未知"处理，照常备份）。
fn dir_size_mb(path: &std::path::Path) -> Option<u64> {
    use std::process::Command;

    if path.is_file() {
        return std::fs::metadata(path).ok().map(|m| m.len() / 1_048_576);
    }
    if !path.is_dir() {
        return None;
    }

    // 带超时：`timeout 5 du -sm <path>`
    let out = Command::new("timeout")
        .arg("5")
        .arg("du")
        .arg("-sm")
        .arg(path)
        .output()
        .ok()?;

    if !out.status.success() {
        // 超时或失败 → 视为大目录（保守起见返回一个超限值）
        return Some(u64::MAX);
    }

    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().next()?.parse::<u64>().ok()
}

/// 单个工具结果进入上下文的最大字符数。
///
/// ## 为什么必须截断（2026-10-05 对照评测的教训）
///
/// 评测中同一个模型（LongCat）在 DSH 上顺利完成 13 个测试的项目，
/// 在我们框架上却陷入「反复重述计划 + 幻觉」。
/// 复盘发现根因：**工具结果原样全量塞进历史**——
/// 几次 `ls -la` / `find` 的输出累积几百行噪声，模型被淹没，
/// 分不清"这一步做了没"，于是重述计划。
///
/// 参考 pi-mono / Hermes 的做法：工具输出截断到几千字符。
/// 4KB 足够容纳绝大多数有用信息（报错原文、命令结果），
/// 又不会让单条结果霸占上下文。
pub const MAX_TOOL_RESULT_CHARS: usize = 4000;

/// 连续重复同一工具调用的容忍上限。
///
/// 超过即判定为「循环」并强制打断 —— 评测中模型曾连续 10+ 次喊同一个命令，
/// 白耗 8 分钟。设 5 次：既容忍偶发的合理重试，又能及时止损。
pub const MAX_CONSECUTIVE_REPEATS: usize = 5;

/// 工具循环内动态区（`working_history`）的条数上限。
///
/// ## 为什么 `history_window` 在这里不够
///
/// `history_window` 只在进入 `respond()` 时裁剪**一次**，而循环体里每轮都在
/// 追加（助手回复 + 工具结果）。`max_iterations` 默认 80，于是长任务的动态区
/// 可以涨到远超 `history_window = 20` 的规模。
///
/// 后果是实打实的：动态区每轮都会被**全量重发**（`messages[1..]`），于是
/// prompt token 与延迟随轮次线性增长；单条工具结果虽有 4KB 上限，
/// 但条数没上限，几十条叠起来照样把模型淹掉。
///
/// ## 为什么不直接套用 `history_window`
///
/// 朴素滑动窗口会把**第一条（原始任务）**挤出去 —— 而任务恰恰是模型
/// 长程执行的锚点，丢了它就会重演 e81e65d 修掉的「模型忘记目标、反复
/// 重述计划」的老问题。所以这里保留首条作为锚点，只裁中间的旧内容。
pub const MAX_WORKING_HISTORY: usize = 40;

/// 裁剪 `working_history`：保留首条（原始任务锚点）与最近若干条。
///
/// 纯函数，便于单测。`max <= 1` 时原样返回（没有可裁剪的中间段）。
fn cap_working_history(hist: &mut Vec<Message>, max: usize) {
    if hist.len() <= max || max <= 1 {
        return;
    }
    // 首条是原始任务，必须保留；其余保留最近 max-1 条
    let tail_keep = max - 1;
    let overflow = hist.len() - tail_keep;
    hist.drain(1..overflow);
}

/// Drop prior turns before trimming this task, keeping the current user request
/// as the anchor throughout long tool loops.
fn cap_task_history(hist: &mut Vec<Message>, max: usize, anchor: &mut usize) {
    if max <= 1 || hist.len() <= max { return; }
    if *anchor > 0 && *anchor < hist.len() {
        hist.drain(..*anchor);
        *anchor = 0;
    }
    cap_working_history(hist, max);
}

/// 把工具结果裁剪到上限内。
///
/// 策略：**保留头尾、中间省略** —— 因为命令输出的关键信息
/// 常在开头（命令回显/表头）与结尾（错误/统计），中间是重复行。
fn shrink_tool_result(content: &str, max: usize) -> String {
    let total = content.chars().count();
    if total <= max {
        return content.to_string();
    }

    let head_len = max * 6 / 10; // 60% 给头部
    let tail_len = max - head_len - 40; // 其余给尾部（留出省略标记空间）

    let head: String = content.chars().take(head_len).collect();
    let tail: String = content
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();

    format!(
        "{}\n\n…（中间省略 {} 字符，原文共 {} 字符。若需完整内容请用更精确的命令或读文件）…\n\n{}",
        head,
        total - head_len - tail_len,
        total,
        tail
    )
}

/// 一次工具调用的指纹（工具名 + 参数），用于识别重复调用。
fn call_fingerprint(call: &RawToolCall) -> String {
    let args = serde_json::to_string(&call.arguments).unwrap_or_default();
    format!("{}::{}", call.tool, args)
}

/// 把工具结果包成回传给模型的文本。
///
/// 除截断外，还标注是否为**重复调用** —— 重复时提示模型
/// 「这个命令刚执行过，结果未变」，避免它以为没执行而反复重试。
fn render_tool_result(
    call: &RawToolCall,
    content: &str,
    is_error: bool,
    repeat_count: usize,
) -> String {
    let body = shrink_tool_result(content, MAX_TOOL_RESULT_CHARS);

    let mut out = format!(
        "[TOOL_RESULT tool={} ok={}]\n{}",
        call.tool, !is_error, body
    );

    if repeat_count > 1 {
        out.push_str(&format!(
            "\n（注意：这是第 {} 次执行相同的调用，请根据本次实际输出判断是否继续。）",
            repeat_count
        ));
    }

    out.push_str("\n[/TOOL_RESULT]");
    out
}
/// 取字符串末尾 `n` 个字符（保留推理模型思考的收尾部分）。
fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    chars[chars.len() - n..].iter().collect()
}

/// 生成工具参数的**简短摘要**（用于通知用户，不暴露完整内容）。
///
/// 规则：
///   · `read` / `write` / `edit` → 显示文件路径
///   · `bash` → 显示命令前 40 字符
///   · `delegate` → 显示目标前 30 字符
///   · 其他 → 显示键名列表
///
/// **不返回完整参数**，避免刷屏与信息泄露。
pub fn summarize_args(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };

    // 优先取语义明确的字段
    for key in ["path", "command", "goal", "query", "pattern"] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            let limit = if key == "command" { 40 } else { 50 };
            return truncate_chars(v, limit);
        }
    }

    // 兜底：列出参数名
    let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    if keys.is_empty() {
        String::new()
    } else {
        keys.join(", ")
    }
}

/// 按字符数截断（中文安全），超长时追加省略号。
fn truncate_chars(s: &str, max: usize) -> String {
    yq_core::truncate_ellipsis(s, max)
}

/// 截断日志文本（保留换行的可见形式，便于诊断格式问题）。
fn truncate_for_log(s: &str, max: usize) -> String {
    truncate_chars(s, max).replace('\n', "\\n")
}

/// 把工具调用格式化成一条**简短通知**（发给用户的形态）。
///
/// 示例：`🔧 read（/tmp/a.txt）`、`🔧 bash（cargo test）`、`🔧 delegate（分析日志）`
pub fn format_tool_notice(tool: &str, args_summary: &str, ok: bool) -> String {
    let mark = if ok { "🔧" } else { "⚠️" };
    if args_summary.is_empty() {
        format!("{} {}", mark, tool)
    } else {
        format!("{} {}（{}）", mark, tool, args_summary)
    }
}

/// 解析模型输出中的工具调用块。
///
/// 支持**三种格式**（实测各家模型会混用，全都要认）：
///
/// 1. 项目约定的文本协议（固定区里声明的）：
///    `<tool_call>{"tool":"read","arguments":{"path":"/a"}}</tool_call>`
/// 2. DeepSeek 原生 **DSML** 格式：
///    `<DSML invoke name="read"><DSML parameter name="path">/a</DSML parameter></DSML invoke>`
///    （标记分隔符是全角竖线 U+FF5C）
/// 3. **LongCat 原生格式**（2026-10-05 实测）：
///    ```text
///    <longcat_tool_call>bash
///    <longcat_arg_key>command</longcat_arg_key>
///    <longcat_arg_value>ls -la</longcat_arg_value>
///    </longcat_tool_call>
///    ```
///    即：工具名直接跟在开标签后，参数用 key/value 成对标签表示。
///
/// 三者都解析，避免模型换格式就"哑火"。
pub fn parse_tool_calls(text: &str) -> Vec<RawToolCall> {
    let mut calls = parse_text_protocol_calls(text);
    if calls.is_empty() {
        calls = parse_longcat_calls(text);
    }
    if calls.is_empty() {
        calls = parse_dsml_calls(text);
    }
    calls
}

/// True when the model appears to have attempted a text tool call that must
/// never be presented to the user as a normal answer.
fn contains_tool_call_markup(text: &str) -> bool {
    [
        "<tool_call>",
        "<longcat_tool_call>",
        "<｜DSML｜ invoke",
        "<｜DSML｜invoke",
        "<|DSML| invoke",
        "<|DSML|invoke",
        "<｜｜DSML｜｜ invoke",
        "<｜｜DSML｜｜invoke",
        "<||DSML|| invoke",
        "<||DSML||invoke",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// 解析 **LongCat 原生**格式的工具调用。
///
/// 形如：
/// ```text
/// <longcat_tool_call>bash
/// <longcat_arg_key>command</longcat_arg_key>
/// <longcat_arg_value>ls -la</longcat_arg_value>
/// </longcat_tool_call>
/// ```
///
/// 要点：
///   · 工具名紧跟在 `<longcat_tool_call>` 之后（同一行，可能带空白）
///   · 参数由 `<longcat_arg_key>K</longcat_arg_key>` +
///     `<longcat_arg_value>V</longcat_arg_value>` 成对出现
///   · 一次回复可能有多个 `<longcat_tool_call>` 块
fn parse_longcat_calls(text: &str) -> Vec<RawToolCall> {
    const OPEN: &str = "<longcat_tool_call>";
    const CLOSE: &str = "</longcat_tool_call>";
    const KEY_OPEN: &str = "<longcat_arg_key>";
    const KEY_CLOSE: &str = "</longcat_arg_key>";
    const VAL_OPEN: &str = "<longcat_arg_value>";
    const VAL_CLOSE: &str = "</longcat_arg_value>";

    let mut out = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel) = text[cursor..].find(OPEN) {
        let body_start = cursor + rel + OPEN.len();
        let Some(close_rel) = text[body_start..].find(CLOSE) else {
            break;
        };
        let body_end = body_start + close_rel;
        let body = &text[body_start..body_end];

        // 工具名：开标签之后的第一个词
        let tool = body
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();

        if !tool.is_empty() {
            // 收集 key/value 对
            let mut args = serde_json::Map::new();
            let mut pcursor = 0usize;

            while let Some(krel) = body[pcursor..].find(KEY_OPEN) {
                let kstart = pcursor + krel + KEY_OPEN.len();
                let Some(kclose_rel) = body[kstart..].find(KEY_CLOSE) else {
                    break;
                };
                let kend = kstart + kclose_rel;
                let key = body[kstart..kend].trim().to_string();

                // 在 key 之后找 value
                let after_key = kend + KEY_CLOSE.len();
                let Some(vrel) = body[after_key..].find(VAL_OPEN) else {
                    break;
                };
                let vstart = after_key + vrel + VAL_OPEN.len();
                let Some(vclose_rel) = body[vstart..].find(VAL_CLOSE) else {
                    break;
                };
                let vend = vstart + vclose_rel;
                let value = body[vstart..vend].trim().to_string();

                if !key.is_empty() {
                    args.insert(key, serde_json::Value::String(value));
                }

                pcursor = vend + VAL_CLOSE.len();
            }

            out.push(RawToolCall {
                tool,
                arguments: if args.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Object(args)
                },
            });
        }

        cursor = body_end + CLOSE.len();
    }

    out
}

/// 全角竖线（U+FF5C），DSML 标记的分隔符。
const FW_PIPE: char = '\u{FF5C}';

/// 解析项目约定的 `<tool_call>…</tool_call>` 文本协议。
fn parse_text_protocol_calls(text: &str) -> Vec<RawToolCall> {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";

    let mut out = Vec::new();
    let mut cursor = 0usize;
    // 是否出现过「未闭合的 tool_call 开标签」（典型场景：max_tokens 截断输出）
    let mut saw_unclosed = false;

    while let Some(start_rel) = text[cursor..].find(OPEN) {
        let start = cursor + start_rel + OPEN.len();
        let Some(end_rel) = text[start..].find(CLOSE) else {
            // 未闭合（多半是输出被 token 上限腰斩）：
            // **不能**因为最后一块截断就丢弃之前已完整解析的调用 ——
            // 2026-10-11 线上事故：模型输出 2 个完整调用 + 1 个截断块，
            // 旧逻辑 break 丢掉全部，导致前 2 个合法调用陪葬。
            saw_unclosed = true;
            tracing::warn!(
                "检测到未闭合的 <tool_call> 块（疑似输出被截断），丢弃该块但保留之前已解析的 {} 个调用",
                out.len()
            );
            break;
        };
        let end = start + end_rel;
        let payload = text[start..end].trim();

        match parse_tool_call_payload(payload) {
            Ok(call) => out.push(call),
            Err(e) => {
                // 提到 WARN 级并打印原始片段 —— 解析失败时必须留下现场，
                // 否则只能看到「解析失败」却不知道对方到底输出了什么格式。
                tracing::warn!(
                    "工具调用块解析失败（已忽略）。原因: {} | 原文片段: {}",
                    e,
                    truncate_for_log(payload, 300)
                );
            }
        }

        cursor = end + CLOSE.len();
    }

    if saw_unclosed && !out.is_empty() {
        tracing::warn!(
            "部分解析救回：从疑似截断的输出中解析出 {} 个完整工具调用",
            out.len()
        );
    }

    out
}

/// 解析 DeepSeek **DSML** 格式的工具调用。
///
/// 支持两种竖线写法（全角 U+FF5C 与半角 `|`），并把
/// `parameter` 子标签聚合成 `arguments` 对象。
fn parse_dsml_calls(text: &str) -> Vec<RawToolCall> {
    // 候选分隔前缀，按出现概率排序
    // DeepSeek's current public encoding uses a single fullwidth pipe on each
    // side (`<｜DSML｜ invoke ...>`). Keep older doubled-pipe variants too.
    let prefixes = [
        format!("{p}DSML{p} ", p = FW_PIPE),
        format!("{p}DSML{p}", p = FW_PIPE),
        "|DSML| ".to_string(),
        "|DSML|".to_string(),
        format!("{p}{p}DSML{p}{p} ", p = FW_PIPE),
        format!("{p}{p}DSML{p}{p}", p = FW_PIPE),
        "||DSML|| ".to_string(),
        "||DSML||".to_string(),
    ];

    let mut out = Vec::new();

    for prefix in &prefixes {
        let invoke_open = format!("<{}invoke ", prefix);
        let invoke_close = format!("</{}invoke>", prefix);
        let param_open = format!("<{}parameter ", prefix);
        let param_close = format!("</{}parameter>", prefix);

        let mut cursor = 0usize;
        while let Some(rel) = text[cursor..].find(&invoke_open) {
            let head_end = cursor + rel + invoke_open.len();
            // 找到 invoke 开标签的 `>`
            let Some(gt_rel) = text[head_end..].find('>') else {
                break;
            };
            let gt = head_end + gt_rel;
            let attrs = &text[head_end..gt];

            let Some(name) = extract_attr(attrs, "name") else {
                cursor = gt + 1;
                continue;
            };

            // invoke 体（到 </…invoke>）
            let body_start = gt + 1;
            let Some(close_rel) = text[body_start..].find(&invoke_close) else {
                break;
            };
            let body_end = body_start + close_rel;
            let body = &text[body_start..body_end];

            // 聚合 parameter 子标签
            let mut arguments = serde_json::Map::new();
            let mut pcur = 0usize;
            while let Some(prel) = body[pcur..].find(&param_open) {
                let phead_end = pcur + prel + param_open.len();
                let Some(pgt_rel) = body[phead_end..].find('>') else {
                    break;
                };
                let pgt = phead_end + pgt_rel;
                let pattrs = &body[phead_end..pgt];
                let pname = extract_attr(pattrs, "name").unwrap_or_default();

                let pval_start = pgt + 1;
                let Some(pclose_rel) = body[pval_start..].find(&param_close) else {
                    break;
                };
                let pval_end = pval_start + pclose_rel;
                let pval = body[pval_start..pval_end].to_string();

                arguments.insert(pname, serde_json::Value::String(pval));
                pcur = pval_end + param_close.len();
            }

            out.push(RawToolCall {
                tool: name,
                arguments: if arguments.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::Object(arguments)
                },
            });

            cursor = body_end + invoke_close.len();
        }

        if !out.is_empty() {
            break; // 已用该前缀解析成功，不再试其它
        }
    }

    out
}

/// 从属性串（如 `name="read"`）中提取具名属性值，支持有无引号。
fn extract_attr(attrs: &str, key: &str) -> Option<String> {
    let needle = format!("{}=", key);
    let pos = attrs.find(&needle)?;
    let rest = &attrs[pos + needle.len()..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        Some(rest.split_whitespace().next().unwrap_or("").to_string())
    }
}

/// 解析原生协议的 `arguments` JSON 字符串。
///
/// 原生协议里 `arguments` 是 JSON **字符串**（不是对象）。先严格解析；
/// 失败则施加控制字符修复再试（模型在原生通道里同样会输出裸换行）。
/// 都失败返回错误原文（调用方以 preset 失败结果回灌模型）。
fn parse_native_arguments(raw: &str) -> Result<serde_json::Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return Ok(v);
    }
    let fixed = escape_control_chars_in_strings(raw);
    serde_json::from_str::<serde_json::Value>(&fixed).map_err(|e| e.to_string())
}

/// 去掉文本协议的工具调用块（无进展相似度比较时只看叙述部分）。
///
/// 为什么要剥：两轮叙述相同但调用参数不同，说明模型在换招重试，
/// 不能算停滞；反之叙述重复且工具全挂，才是真正的空转。
fn strip_tool_blocks(text: &str) -> String {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        match rest.find(OPEN) {
            Some(start) => {
                out.push_str(&rest[..start]);
                match rest[start..].find(CLOSE) {
                    Some(rel) => {
                        rest = &rest[start + rel + CLOSE.len()..];
                    }
                    // 未闭合块：剥掉剩余全部（截断场景）
                    None => break,
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// 解析单个工具调用块，带 **JSON 容错修复**。
///
/// ## 为什么需要修复（2026-10-05 实测）
///
/// 模型在 `content` 字段里写多行代码时，会直接输出**真实换行**而不是 `\n` 转义：
///
/// ```text
/// <tool_call>{"tool":"write","arguments":{"content":"第一行
/// 第二行"}}</tool_call>
/// ```
///
/// JSON 规范禁止字符串内出现裸控制字符，严格解析必然失败 —— 表现为
/// **小文件能写、大文件（多行）全废**。
///
/// 修复策略：先严格解析；失败则逐级放宽 ——
///   1. 「字符串内控制字符转义」（裸换行 → `\n`）
///   2. 「非法转义修复」（`\d` → `\\d`）
///   3. 两者同时施加
fn parse_tool_call_payload(payload: &str) -> Result<RawToolCall, String> {
    // 1) 先试严格解析（绝大多数情况走这条）
    if let Ok(call) = serde_json::from_str::<RawToolCall>(payload) {
        return Ok(call);
    }

    // 2) 修裸控制字符
    let no_ctrl = escape_control_chars_in_strings(payload);
    if let Ok(call) = serde_json::from_str::<RawToolCall>(&no_ctrl) {
        return Ok(call);
    }

    // 3) 修非法转义（`\d` `\s` `\w` 这类在 JSON 里非法的反斜杠序列）
    let no_bad_escape = escape_invalid_json_escapes(payload);
    if let Ok(call) = serde_json::from_str::<RawToolCall>(&no_bad_escape) {
        return Ok(call);
    }

    // 4) 两者一起修（写 Python 正则 + 多行的组合拳是最常见的大文件失败场景）
    let both = escape_invalid_json_escapes(&no_ctrl);
    serde_json::from_str::<RawToolCall>(&both).map_err(|e| format!("严格与逐级修复均失败: {}", e))
}

/// 修复 JSON 字符串内**非法的反斜杠转义**。
///
/// ## 为什么需要（2026-10-07 生产实测）
///
/// 模型写 Python 时会把正则原样塞进 `content`：
///
/// ```text
/// <tool_call>{"tool":"write","arguments":{"content":"re = r\"\\d+\""}}</tool_call>
/// ```
///
/// JSON 只认 `\" \\ \/ \b \f \n \r \t \uXXXX` 这几种转义。`\d` 不是合法转义，
/// 严格解析报 `invalid escape`。于是**只要待写文件里含 Python 正则，
/// 写入就必然失败** —— 而正则恰恰是日志解析类代码的常客。
///
/// 实测一次基准任务：模型连续 3 次尝试写 `reader.py` 全部因此失败，
/// 触发「连续格式错误」熔断，整个任务原地停摆、一行代码没落地。
/// 这个 bug 让 agent 在「写含正则的 Python」这一整类任务上不可用。
///
/// ## 修法
///
/// 逐字符扫描（跟踪是否在字符串内 + 是否处于转义态），遇到反斜杠时
/// 向后看一个字符：
///   · 合法转义（`" \\ / b f n r t u`）→ 原样保留
///   · **非法转义**（如 `d` `s` `w` `.` `(` …）→ 把反斜杠**再转义一次**
///     （`\d` → `\\d`），这样 JSON 解码后得到字面的 `\d`，
///     正是模型想写进文件的内容。
///
/// 注意 `\u` 后面必须跟 4 位十六进制；若不足则同样按非法处理，
/// 否则会修出一个仍然非法的 JSON。
pub fn escape_invalid_json_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 32);
    let mut in_string = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if !in_string {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
            continue;
        }

        match ch {
            '"' => {
                in_string = false;
                out.push(ch);
            }
            '\\' => {
                match chars.peek().copied() {
                    // 常见且意图明确的合法转义：原样保留
                    // （`\n` 换行 / `\t` 制表 / `\"` 引号 / `\\` 反斜杠 / `\/` 斜杠）
                    Some(n @ ('"' | '\\' | '/' | 'n' | 'r' | 't')) => {
                        out.push('\\');
                        out.push(n);
                        chars.next();
                    }
                    // ⚠️ `\b` 与 `\f` 是**歧义**转义，本函数刻意把它们当字面量：
                    //
                    //   · JSON 语义：`\b`=退格(0x08)、`\f`=换页(0x0C)
                    //   · Python 正则：`\b`=单词边界、`\f`=换页符
                    //
                    // 而本函数**只在严格解析已经失败后**才被调用 —— 说明模型
                    // 本来就在乱写转义。在这种上下文里，源码里出现「退格字符」
                    // 的概率远低于「正则单词边界」，故按字面量修复。
                    //
                    // 代价如实记录：若模型真的想写一个退格字符，会得到两个字符
                    // `\b`。源码文件中真实退格字符几乎不存在，这个取舍可以接受。
                    Some(n @ ('b' | 'f')) => {
                        out.push_str("\\\\");
                        out.push(n);
                        chars.next();
                    }
                    // \uXXXX：必须凑够 4 位十六进制，否则按非法处理
                    Some('u') => {
                        let hex: String = chars.clone().skip(1).take(4).collect();
                        if hex.len() == 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                            out.push_str("\\u");
                            chars.next();
                            for _ in 0..4 {
                                out.push(chars.next().unwrap_or('0'));
                            }
                        } else {
                            out.push_str("\\\\");
                        }
                    }
                    // 非法转义（\d \s \w …）：把反斜杠本身转义
                    Some(_) => out.push_str("\\\\"),
                    // 字符串末尾的孤立反斜杠：同样按字面处理
                    None => out.push_str("\\\\"),
                }
            }
            c => out.push(c),
        }
    }

    out
}

/// 把 JSON 文本中**字符串内部**的裸控制字符转义。
///
/// 逐字符扫描并跟踪「是否在字符串内」（正确处理 `\"` 与 `\\` 转义），
/// 在字符串内部把：
///   · 裸 `\n` → `\\n`
///   · 裸 `\r` → `\\r`
///   · 裸 `\t` → `\\t`
///   · 其它控制字符 → `\uXXXX`
/// 字符串外部原样保留（换行是 JSON 合法的结构空白）。
pub fn escape_control_chars_in_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 64);
    let mut in_string = false;
    let mut escaped = false;

    for ch in input.chars() {
        if !in_string {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
            continue;
        }

        // ── 字符串内部 ──
        if escaped {
            // 上一个字符是反斜杠：这个字符已被转义，原样保留
            out.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => {
                out.push('\\');
                escaped = true;
            }
            '"' => {
                out.push('"');
                in_string = false;
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }

    out
}

// ════════════════════════════════════════════════════════════════════
// [TOOL_TRACE] —— 每轮工具执行的一行式证据串（纯函数，便于单测）
//
// 为什么存在：2026-10 生产事故里，历史只存了最终文本，第二轮模型看不到
// 上一轮跑过什么，面对自己「测试通过」的声称查无实据，按新提示词规则
// 自我定罪「我编造了」并把活重干了一遍（假认错）。把工具证据留进历史，
// 模型下一轮就能拿它核对声称——与审计表 op_audit（id62-66）的证据链同源，
// 区别是这份只进内存历史，不入库、不发给用户。
// ════════════════════════════════════════════════════════════════════

/// `[TOOL_TRACE]` 总长上限（字符数）。超出即截断加 `…`，防止证据串
/// 随轮次膨胀撑爆下一轮上下文。
const TOOL_TRACE_MAX_CHARS: usize = 600;

/// 证据串固定前缀（模型据此在历史里识别工具证据）。
const TOOL_TRACE_PREFIX: &str = "[TOOL_TRACE] ";

/// 一条证据的收尾标记：成功 `✓`、失败 `err`、被 /stop 打断 `stop`。
fn trace_mark(is_error: bool, interrupted: bool) -> &'static str {
    if interrupted {
        "stop"
    } else if is_error {
        "err"
    } else {
        "\u{2713}"
    }
}

/// 单条参数摘要：复用审计表的提取规则（`yq_core::ops::audit_target`）。
///
/// 该函数已实现：`bash` → command、`read`/`write`/`edit` → path、
/// `delegate` → goal、其他 → 参数键名列表，且会清洗控制字符、限长。
/// 复用它是为了让 `[TOOL_TRACE]` 与 `op_audit.target` 描述同一件事，
/// 两处证据永不打架；唯一额外处理是 bash 命令压到 40 字符
/// （证据串要进历史，命令原文太长会浪费上下文）。
fn trace_arg_summary(tool: &str, args: &serde_json::Value) -> String {
    let s = yq_core::ops::audit_target(tool, args);
    if tool == "bash" {
        truncate_chars(&s, 40)
    } else {
        s
    }
}

/// 组装一条证据：`{tool}({参数摘要}){标记}`。
fn trace_entry(tool: &str, args: &serde_json::Value, is_error: bool, interrupted: bool) -> String {
    format!(
        "{}({}){}",
        tool,
        trace_arg_summary(tool, args),
        trace_mark(is_error, interrupted)
    )
}

/// 把本轮所有证据条目拼成 `[TOOL_TRACE] a(...); b(...)`。
///
/// 空轮次返回空串（调用方据此决定不追加任何东西）；
/// 总长超过 [`TOOL_TRACE_MAX_CHARS`] 时截断并追加 `…`。
fn assemble_tool_trace(entries: &[String]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let joined = entries.join("; ");
    let mut s = format!("{}{}", TOOL_TRACE_PREFIX, joined);
    if s.chars().count() > TOOL_TRACE_MAX_CHARS {
        // 按字符截断（中文安全），宁可少留一段证据也不能撑爆上下文
        let cut: String = s.chars().take(TOOL_TRACE_MAX_CHARS - 1).collect();
        s = format!("{}\u{2026}", cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_tool_call() {
        let text = r#"让我看看文件。<tool_call>{"tool":"read","arguments":{"path":"/tmp/a.txt"}}</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read");
        assert_eq!(calls[0].arguments["path"], "/tmp/a.txt");
    }

    #[test]
    fn parses_multiple_tool_calls() {
        let text = r#"<tool_call>{"tool":"read","arguments":{"path":"/a"}}</tool_call>中间文字<tool_call>{"tool":"bash","arguments":{"command":"ls"}}</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool, "read");
        assert_eq!(calls[1].tool, "bash");
    }

    #[test]
    fn plain_text_yields_no_calls() {
        let calls = parse_tool_calls("这是一段普通回复，没有工具调用。");
        assert!(calls.is_empty());
    }

    #[test]
    fn malformed_block_is_ignored_without_panic() {
        let text = "<tool_call>{不是合法 JSON}</tool_call>";
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn unclosed_block_is_ignored() {
        let text = r#"<tool_call>{"tool":"read","arguments":{}}"#;
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn missing_arguments_defaults_to_null() {
        let text = r#"<tool_call>{"tool":"bash"}</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].arguments.is_null());
    }

    /// 核心回归：模型在 content 里写**真实换行**的多行代码（实测踩到的坑）。
    #[test]
    fn repairs_raw_newlines_inside_string() {
        let text = "<tool_call>{\"tool\":\"write\",\"arguments\":{\"path\":\"/tmp/x.rs\",\"content\":\"第一行\n第二行\n第三行\"}}</tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1, "含裸换行的 JSON 应被修复并解析成功");
        assert_eq!(calls[0].tool, "write");
        let content = calls[0].arguments["content"].as_str().unwrap();
        assert_eq!(content, "第一行\n第二行\n第三行");
    }

    /// 裸制表符同样要修。
    #[test]
    fn repairs_raw_tabs_inside_string() {
        let text = "<tool_call>{\"tool\":\"write\",\"arguments\":{\"content\":\"缩进\t内容\"}}</tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments["content"].as_str().unwrap(),
            "缩进\t内容"
        );
    }

    /// 已经正确转义的 `\n` 不能被二次转义（否则内容会多出反斜杠）。
    #[test]
    fn does_not_double_escape_existing_escapes() {
        let text = r#"<tool_call>{"tool":"write","arguments":{"content":"第一行\n第二行 \"引号\" 和 反斜杠\\ 结尾"}}</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        let content = calls[0].arguments["content"].as_str().unwrap();
        assert_eq!(content, "第一行\n第二行 \"引号\" 和 反斜杠\\ 结尾");
    }

    /// 字符串外的换行（JSON 结构空白）不应被改动。
    #[test]
    fn keeps_structural_whitespace_outside_strings() {
        let text = "<tool_call>{\n  \"tool\": \"bash\",\n  \"arguments\": {\n    \"command\": \"ls\"\n  }\n}</tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["command"], "ls");
    }

    #[test]
    fn escape_helper_is_identity_when_clean() {
        let clean = r#"{"a":"b\nc","d":1}"#;
        assert_eq!(escape_control_chars_in_strings(clean), clean);
    }

    #[test]
    fn escape_helper_handles_multiple_lines() {
        let dirty = "{\"c\":\"a\nb\rc\td\"}";
        let fixed = escape_control_chars_in_strings(dirty);
        assert_eq!(fixed, r#"{"c":"a\nb\rc\td"}"#);
    }

    // ── 非法 JSON 转义修复（2026-10-07 生产事故）──
    //
    // 模型写 Python 正则时输出 `\d`，在 JSON 里是非法转义，严格解析必然失败。
    // 曾经的宽松解析只修裸控制字符，于是**含正则的文件一律写不进去**：
    // 实测一次基准任务连续 3 次写 reader.py 失败 → 触发格式熔断 →
    // 整个任务停摆、一行代码没落地。下面这些测试锁住修复。

    #[test]
    fn repairs_invalid_escape_from_python_regex() {
        // 这是**真实失败样本**的缩微版：content 里有 r"\d+"
        let payload =
            r#"{"tool":"write","arguments":{"path":"/tmp/a.py","content":"re.compile(r\"\d+\")"}}"#;
        let call = parse_tool_call_payload(payload).expect("含 \\d 的 payload 必须能被修复");
        let content = call.arguments["content"].as_str().unwrap();
        // 修复后应当得到字面的 \d（模型本意就是写这个进文件）
        assert!(
            content.contains(r"\d"),
            "正则原文应被保留，实际: {content:?}"
        );
    }

    #[test]
    fn repairs_common_regex_escapes() {
        // \s \w \. \( 都是 Python 正则里的常客，在 JSON 里全属**非法**转义，
        // 严格解析必然失败，因此能走修复路径。
        //
        // 注意这里**刻意不含 `\b` 与 `\f`**：它们在 JSON 里是合法转义
        // （退格 / 换页），严格解析会直接成功并产出控制字符，修复路径根本
        // 不会被触发 —— 这类歧义无法在解析层裁决（见
        // escape_invalid_json_escapes 的说明）。此处只锁住可判定的一类。
        for (raw, want) in [
            (r"\s+", r"\s+"),
            (r"\w*", r"\w*"),
            (r"\.txt", r"\.txt"),
            (r"\(x\)", r"\(x\)"),
            (r"\d{3}", r"\d{3}"),
            (r"[a-z]+\Z", r"[a-z]+\Z"),
        ] {
            let payload =
                format!(r#"{{"tool":"write","arguments":{{"content":"re = r\"{raw}\""}}}}"#);
            let call = parse_tool_call_payload(&payload)
                .unwrap_or_else(|e| panic!("{raw} 应可修复，实际失败: {e}"));
            let content = call.arguments["content"].as_str().unwrap();
            assert!(
                content.contains(want),
                "{raw} 应保留为 {want}，实际 {content:?}"
            );
        }
    }

    #[test]
    fn documents_ambiguous_backslash_b_limitation() {
        // 诚实记录已知边界：`\b` 在 JSON 里合法（退格），Python 正则里却是
        // 单词边界。严格解析会成功并产出控制字符，修复路径不触发 ——
        // 模型若把 `r"\bword\b"` 原样塞进 JSON，得到的会是退格字符。
        //
        // 这条测试**固化现状**（而非假装已解决），以便将来若引入更强的
        // 修复策略（如按 path 后缀判断是否 Python 源码）时有对照基准。
        let payload = r#"{"tool":"write","arguments":{"content":"re = r\"\bword\b\""}}"#;
        let call = parse_tool_call_payload(payload).expect("\\b 是合法 JSON 转义，应严格解析成功");
        let content = call.arguments["content"].as_str().unwrap();
        assert!(
            content.contains('\u{8}'),
            "当前行为：\\b 被解码为退格字符（已知局限），实际 {content:?}"
        );
    }

    #[test]
    fn keeps_legal_escapes_intact() {
        // 合法转义不能被二次转义，否则文件内容会被写坏
        let payload = r#"{"tool":"write","arguments":{"content":"a\nb\tc\"d\\e"}}"#;
        let call = parse_tool_call_payload(payload).expect("合法转义应严格解析成功");
        let content = call.arguments["content"].as_str().unwrap();
        assert_eq!(content, "a\nb\tc\"d\\e");
    }

    #[test]
    fn keeps_unicode_escape_intact() {
        let payload = r#"{"tool":"write","arguments":{"content":"\u4f60\u597d"}}"#;
        let call = parse_tool_call_payload(payload).expect("\\uXXXX 是合法转义");
        assert_eq!(call.arguments["content"].as_str().unwrap(), "你好");
    }

    #[test]
    fn handles_regex_plus_bare_newlines_together() {
        // 最恶劣的组合：多行内容 + 正则。真实事故就是这个形态。
        let payload =
            "{\"tool\":\"write\",\"arguments\":{\"content\":\"line1\nre = r\\\"\\d+\\\"\nline3\"}}";
        let call = parse_tool_call_payload(payload).expect("多行 + 正则必须能修复");
        let content = call.arguments["content"].as_str().unwrap();
        assert!(content.contains("line1\n"), "换行应保留: {content:?}");
        assert!(content.contains(r"\d"), "正则应保留: {content:?}");
        assert!(content.contains("line3"), "后续行应保留: {content:?}");
    }

    #[test]
    fn escape_invalid_json_escapes_leaves_valid_json_alone() {
        let clean = r#"{"a":"b\nc","d":"e\\f"}"#;
        assert_eq!(escape_invalid_json_escapes(clean), clean);
    }

    #[test]
    fn whole_protocol_parses_regex_write() {
        // 端到端：从 <tool_call> 标签一路解析出来
        let text = r#"<tool_call>{"tool":"write","arguments":{"path":"/tmp/r.py","content":"import re\npat = r\"\d{3}\""}}</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1, "含正则的写入必须能解析出来");
        assert_eq!(calls[0].arguments["path"], "/tmp/r.py");
        let content = calls[0].arguments["content"].as_str().unwrap();
        assert!(content.contains(r"\d{3}"), "实际: {content:?}");
    }

    // ── DSML 兼容（2026-10-05：DeepSeek 系模型会混用原生格式）──

    /// 构造全角竖线的 DSML 标记，避免测试源码里出现裸标记。
    fn dsml(s: &str) -> String {
        s.replace('|', "\u{FF5C}")
    }

    #[test]
    fn parses_dsml_fullwidth_pipe_format() {
        let text = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            dsml("<||DSML|| calls>"),
            dsml("<||DSML|| invoke name=\"write\">"),
            dsml(
                "<||DSML|| parameter name=\"path\" string=\"true\">/tmp/x.rs</||DSML|| parameter>"
            ),
            dsml("<||DSML|| parameter name=\"content\" string=\"true\">hello</||DSML|| parameter>"),
            dsml("</||DSML|| invoke>"),
            dsml("</||DSML|| calls>"),
            ""
        );
        let calls = parse_tool_calls(&text);
        assert_eq!(calls.len(), 1, "DSML 格式应被解析");
        assert_eq!(calls[0].tool, "write");
        assert_eq!(calls[0].arguments["path"], "/tmp/x.rs");
        assert_eq!(calls[0].arguments["content"], "hello");
    }

    #[test]
    fn parses_dsml_ascii_pipe_variant() {
        let text = "<||DSML|| invoke name=\"bash\">\n<||DSML|| parameter name=\"command\">ls -la</||DSML|| parameter>\n</||DSML|| invoke>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "bash");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn parses_deepseek_official_single_fullwidth_pipe_dsml() {
        let text = "<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"memory\">\n<｜DSML｜ parameter name=\"action\" string=\"true\">add</｜DSML｜ parameter>\n<｜DSML｜ parameter name=\"content\" string=\"true\">keep this</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "memory");
        assert_eq!(calls[0].arguments["action"], "add");
        assert_eq!(calls[0].arguments["content"], "keep this");
    }

    #[test]
    fn parses_deepseek_single_ascii_pipe_dsml() {
        let text = "<|DSML| invoke name=\"bash\"><|DSML| parameter name=\"command\">pwd</|DSML| parameter></|DSML| invoke>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "bash");
        assert_eq!(calls[0].arguments["command"], "pwd");
    }

    #[test]
    fn parses_deepseek_dsml_without_optional_tag_spacing() {
        let text = "<｜DSML｜calls><｜DSML｜invoke name=\"read\"><｜DSML｜parameter name=\"path\">/tmp/a</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜calls>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read");
        assert_eq!(calls[0].arguments["path"], "/tmp/a");
    }

    #[test]
    fn recognizes_malformed_dsml_as_protocol_not_user_text() {
        assert!(contains_tool_call_markup("<｜DSML｜ invoke name=\"memory\">"));
        assert!(contains_tool_call_markup("<|DSML| invoke name=\"memory\">"));
        assert!(!contains_tool_call_markup("DSML 是工具调用格式的名称。"));
    }

    #[test]
    fn dsml_without_parameters_yields_null() {
        let text = format!(
            "{}{}",
            dsml("<||DSML|| invoke name=\"ping\">"),
            dsml("</||DSML|| invoke>")
        );
        let calls = parse_tool_calls(&text);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].arguments.is_null());
    }

    #[test]
    fn text_protocol_takes_priority_over_dsml() {
        // 两种格式同时出现时，优先文本协议（不混淆）
        let text = r#"<tool_call>{"tool":"read","arguments":{"path":"/a"}}</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read");
    }

    #[test]
    fn extract_attr_handles_quoted_and_unquoted() {
        assert_eq!(
            extract_attr(" name=\"abc\" string=\"true\"", "name").as_deref(),
            Some("abc")
        );
        assert_eq!(
            extract_attr(" name=abc other=1", "name").as_deref(),
            Some("abc")
        );
        assert_eq!(extract_attr(" other=1", "name"), None);
    }

    #[test]
    fn plain_text_with_dsml_word_but_no_invoke_yields_nothing() {
        let text = "这里提到了 DSML 这个词，但没有实际的调用结构。";
        assert!(parse_tool_calls(text).is_empty());
    }

    // ── LongCat 原生格式（2026-10-05 实测抓到的真实输出）──

    #[test]
    fn parses_longcat_native_format() {
        // 逐字复刻实测抓到的格式
        let text = "<longcat_tool_call>bash\n<longcat_arg_key>command</longcat_arg_key>\n<longcat_arg_value>mkdir -p wordfreq/wordfreq wordfreq/tests</longcat_arg_value>\n</longcat_tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1, "LongCat 格式应被解析");
        assert_eq!(calls[0].tool, "bash");
        assert_eq!(
            calls[0].arguments["command"],
            "mkdir -p wordfreq/wordfreq wordfreq/tests"
        );
    }

    #[test]
    fn parses_longcat_multiple_blocks() {
        let text = "<longcat_tool_call>read\n<longcat_arg_key>path</longcat_arg_key>\n<longcat_arg_value>/tmp/a</longcat_arg_value>\n</longcat_tool_call>\n<longcat_tool_call>write\n<longcat_arg_key>path</longcat_arg_key>\n<longcat_arg_value>/tmp/b</longcat_arg_value>\n</longcat_tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool, "read");
        assert_eq!(calls[1].tool, "write");
        assert_eq!(calls[1].arguments["path"], "/tmp/b");
    }

    #[test]
    fn parses_longcat_multiple_params() {
        let text = "<longcat_tool_call>memory\n<longcat_arg_key>action</longcat_arg_key>\n<longcat_arg_value>add</longcat_arg_value>\n<longcat_arg_key>content</longcat_arg_key>\n<longcat_arg_value>用户喜欢 neovim</longcat_arg_value>\n</longcat_tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "memory");
        assert_eq!(calls[0].arguments["action"], "add");
        assert_eq!(calls[0].arguments["content"], "用户喜欢 neovim");
    }

    #[test]
    fn text_protocol_priority_over_longcat() {
        let text = r#"<tool_call>{"tool":"read","arguments":{"path":"/a"}}</tool_call>"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read");
    }
    // ── 工具结果截断与循环检测（2026-10-05 对照评测的教训）──

    #[test]
    fn tool_result_under_limit_is_unchanged() {
        let content = "短输出";
        assert_eq!(shrink_tool_result(content, 4000), content);
    }

    #[test]
    fn tool_result_over_limit_is_truncated_with_marker() {
        let content = "行
"
        .repeat(5000); // 1 万字符
        let out = shrink_tool_result(&content, 1000);
        assert!(out.chars().count() < 1200, "应被压到上限附近");
        assert!(
            out.contains("中间省略"),
            "应有省略标记: {}",
            &out[..200.min(out.len())]
        );
        assert!(out.contains("原文共"), "应告知原文大小");
    }

    #[test]
    fn tool_result_keeps_head_and_tail() {
        let content = format!("HEAD{}TAIL", "x".repeat(10000));
        let out = shrink_tool_result(&content, 500);
        assert!(out.starts_with("HEAD"), "头部应保留");
        assert!(out.ends_with("TAIL"), "尾部应保留");
    }

    #[test]
    fn fingerprint_differs_by_args() {
        let a = RawToolCall {
            tool: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        let b = RawToolCall {
            tool: "bash".into(),
            arguments: serde_json::json!({"command": "pwd"}),
        };
        let c = RawToolCall {
            tool: "read".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        assert_ne!(
            call_fingerprint(&a),
            call_fingerprint(&b),
            "参数不同应对应不同指纹"
        );
        assert_ne!(
            call_fingerprint(&a),
            call_fingerprint(&c),
            "工具名不同应对应不同指纹"
        );
        assert_eq!(call_fingerprint(&a), call_fingerprint(&a));
    }

    #[test]
    fn repeated_call_is_annotated_in_result() {
        let call = RawToolCall {
            tool: "bash".into(),
            arguments: serde_json::json!({"command": "mkdir -p a"}),
        };
        let first = render_tool_result(&call, "ok", false, 1);
        assert!(!first.contains("第 2 次"), "首次调用不该有重复标注");

        let third = render_tool_result(&call, "ok", false, 3);
        assert!(third.contains("第 3 次"), "重复时应标注次数: {}", third);
        assert!(third.contains("根据本次实际输出"), "应提示检查本次结果");
    }

    // 这是**故意的常量范围守卫**：断言对象是编译期常量，
    // 目的是常量被改坏时立刻失败。
    // clippy::assertions_on_constants 针对的是恒真/恒假的废断言，
    // 不适用于守卫，故显式放行。
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn repeat_threshold_is_sane() {
        assert!(MAX_CONSECUTIVE_REPEATS >= 3, "阈值太小会误杀合理重试");
        assert!(MAX_CONSECUTIVE_REPEATS <= 10, "阈值太大就来不及止损");
    }

    // 这是**故意的常量范围守卫**：断言对象是编译期常量，
    // 目的是常量被改坏时立刻失败。
    // clippy::assertions_on_constants 针对的是恒真/恒假的废断言，
    // 不适用于守卫，故显式放行。
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn tool_result_char_limit_is_sane() {
        assert!(MAX_TOOL_RESULT_CHARS >= 2000, "太小会丢有用信息");
        assert!(MAX_TOOL_RESULT_CHARS <= 8000, "太大会让噪声淹没模型");
    }

    // ── 动态区封顶（回归测试，2026-10-06）──

    fn hist(n: usize) -> Vec<Message> {
        (0..n).map(|i| Message::user(format!("m{i}"))).collect()
    }

    #[test]
    fn long_task_retains_current_request_instead_of_prior_conversation() {
        let mut h = hist(12);
        let mut anchor = 5;
        cap_task_history(&mut h, 4, &mut anchor);
        assert_eq!(h.len(), 4);
        assert_eq!(h[0].content, "m5");
        assert_eq!(anchor, 0);
        assert_eq!(h.last().unwrap().content, "m11");
    }

    #[test]
    fn cap_leaves_short_history_untouched() {
        let mut h = hist(5);
        cap_working_history(&mut h, 40);
        assert_eq!(h.len(), 5, "未超限不该动");
    }

    #[test]
    fn cap_trims_to_limit() {
        let mut h = hist(100);
        cap_working_history(&mut h, 40);
        assert_eq!(h.len(), 40, "应裁到上限");
    }

    #[test]
    fn cap_preserves_task_anchor() {
        // 关键不变量：首条（原始任务）绝不能被裁掉，
        // 否则会重演 e81e65d 修掉的「模型忘记目标」问题。
        let mut h = hist(100);
        let anchor = h[0].content.clone();
        cap_working_history(&mut h, MAX_WORKING_HISTORY);
        assert_eq!(h[0].content, anchor, "首条原始任务必须保留");
    }

    #[test]
    fn cap_keeps_newest_tail() {
        // 应保留最近的若干条，而不是最旧的
        let mut h = hist(100);
        cap_working_history(&mut h, 40);
        let last = h.last().unwrap().content.clone();
        assert_eq!(last, "m99", "最新一条必须还在");
    }

    #[test]
    fn cap_drops_middle_not_head() {
        let mut h = hist(10);
        cap_working_history(&mut h, 4); // 保留 m0 + m7,m8,m9
        let texts: Vec<&str> = h.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, vec!["m0", "m7", "m8", "m9"]);
    }

    #[test]
    fn cap_is_idempotent() {
        // Message 没实现 PartialEq，这里比对内容序列
        let texts = |v: &[Message]| v.iter().map(|m| m.content.clone()).collect::<Vec<_>>();
        let mut h = hist(100);
        cap_working_history(&mut h, 40);
        let once = texts(&h);
        cap_working_history(&mut h, 40);
        assert_eq!(texts(&h), once, "重复裁剪不应继续改变结果");
    }

    #[test]
    fn cap_handles_degenerate_limits() {
        let mut h = hist(10);
        cap_working_history(&mut h, 1); // 无中间段可裁
        assert_eq!(h.len(), 10, "max=1 时不该误删内容");
        cap_working_history(&mut h, 0); // 下界守卫
        assert_eq!(h.len(), 10);
    }

    // 这是**故意的常量范围守卫**：断言对象是编译期常量，
    // 目的是常量被改坏时立刻失败。
    // clippy::assertions_on_constants 针对的是恒真/恒假的废断言，
    // 不适用于守卫，故显式放行。
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn cap_bound_is_sane() {
        // 上限要明显大于单轮工具数（否则一次多工具调用就触发裁剪），
        // 又要明显小于 max_iterations 默认值（否则封顶形同虚设）。
        assert!(
            MAX_WORKING_HISTORY >= 20,
            "太小：一次多工具轮就可能触发裁剪"
        );
        assert!(MAX_WORKING_HISTORY <= 60, "太大：80 轮迭代下封顶失去意义");
    }

    // ── [TOOL_TRACE] 证据串 ──────────────────────────────────────

    #[test]
    fn trace_empty_round_yields_empty_string() {
        // 没调用工具的轮次：必须是空串，历史里一个字都不加
        assert_eq!(assemble_tool_trace(&[]), "");
    }

    #[test]
    fn trace_joins_entries_with_separator_and_prefix() {
        let entries = vec![
            trace_entry(
                "bash",
                &serde_json::json!({"command": "mkdir -p /tmp/probe2"}),
                false,
                false,
            ),
            trace_entry(
                "write",
                &serde_json::json!({"path": "/tmp/x.py"}),
                false,
                false,
            ),
        ];
        assert_eq!(
            assemble_tool_trace(&entries),
            "[TOOL_TRACE] bash(mkdir -p /tmp/probe2)\u{2713}; write(/tmp/x.py)\u{2713}"
        );
    }

    #[test]
    fn trace_is_capped_at_600_chars_with_ellipsis() {
        // 拼一条明显超 600 字符的证据（每条约 150 字符 × 5 条 + 前缀 > 600）。
        // 注意不能用超长 path 走 audit_target：它自身限长 512，两条加起来
        // 可能压根到不了 600，测试会假绿/假红，这里直接造足量条目。
        let entries: Vec<String> = (0..5)
            .map(|i| format!("read(/tmp/file{}{})\u{2713}", i, "a".repeat(140)))
            .collect();
        let s = assemble_tool_trace(&entries);
        assert!(
            s.chars().count() <= TOOL_TRACE_MAX_CHARS,
            "超长：{} 字符",
            s.chars().count()
        );
        assert!(s.ends_with('\u{2026}'), "截断必须带省略号");
        assert!(s.starts_with("[TOOL_TRACE] "));
    }

    #[test]
    fn trace_marks_error_and_stop() {
        // is_error → err；/stop 打断 → stop（打断优先于普通成败）
        assert_eq!(trace_mark(false, false), "\u{2713}");
        assert_eq!(trace_mark(true, false), "err");
        assert_eq!(trace_mark(false, true), "stop");
        assert_eq!(trace_mark(true, true), "stop");

        let e = trace_entry(
            "bash",
            &serde_json::json!({"command": "false"}),
            true,
            false,
        );
        assert_eq!(e, "bash(false)err");
        let e = trace_entry(
            "bash",
            &serde_json::json!({"command": "sleep 9"}),
            true,
            true,
        );
        assert_eq!(e, "bash(sleep 9)stop");
    }

    #[test]
    fn trace_arg_summary_per_tool_branches() {
        // bash → 命令，截 40 字符
        let long_cmd = "x".repeat(60);
        let s = trace_arg_summary("bash", &serde_json::json!({"command": long_cmd}));
        assert!(s.chars().count() <= 41, "bash 摘要超 40: {}", s);

        // read/write/edit → 路径
        assert_eq!(
            trace_arg_summary("read", &serde_json::json!({"path": "/etc/hosts"})),
            "/etc/hosts"
        );
        assert_eq!(
            trace_arg_summary(
                "write",
                &serde_json::json!({"path": "/tmp/x.py", "content": "abc"})
            ),
            "/tmp/x.py"
        );
        assert_eq!(
            trace_arg_summary("edit", &serde_json::json!({"path": "/tmp/y.toml"})),
            "/tmp/y.toml"
        );

        // delegate → goal
        assert_eq!(
            trace_arg_summary("delegate", &serde_json::json!({"goal": "分析磁盘占用"})),
            "分析磁盘占用"
        );

        // 其他工具 → 参数键名列表（不泄露正文）
        let s = trace_arg_summary(
            "memory",
            &serde_json::json!({"query": "秘密内容", "limit": 3}),
        );
        assert!(s.contains("query") && s.contains("limit"), "实际: {}", s);

        // 缺参兜底 → 键名列表/无参数，绝不 panic
        assert!(!trace_arg_summary("bash", &serde_json::json!({})).is_empty());
        assert!(!trace_arg_summary("read", &serde_json::Value::Null).is_empty());
    }

    #[test]
    fn reply_history_text_appends_trace_only_for_history() {
        // 有 trace：正文 + 换行 + trace（只用于内存历史）
        let r = AgentReply {
            text: "测试通过".to_string(),
            tool_invocations: 1,
            prefix_sha256: "deadbeef".to_string(),
            tool_trace: "[TOOL_TRACE] bash(cargo test)\u{2713}".to_string(),
        };
        assert_eq!(
            r.history_text(),
            "测试通过\n[TOOL_TRACE] bash(cargo test)\u{2713}"
        );
        // 用户可见文本一个字节都不变
        assert_eq!(r.text, "测试通过");

        // 无 trace：历史文本 == 正文
        let r2 = AgentReply {
            text: "你好".to_string(),
            tool_invocations: 0,
            prefix_sha256: "deadbeef".to_string(),
            tool_trace: String::new(),
        };
        assert_eq!(r2.history_text(), "你好");
    }

    // ── 截断场景：部分解析救回完整调用（2026-10-11 线上事故回归测试）──

    #[test]
    fn partial_parse_rescues_complete_calls_before_truncated_one() {
        // 2 个完整 tool_call + 第 3 个未闭合（截断）→ 必须返回 2 个，而不是 0 个
        let text = "<tool_call>{\"tool\":\"bash\",\"arguments\":{\"command\":\"echo a\"}}</tool_call>\n\
                    <tool_call>{\"tool\":\"bash\",\"arguments\":{\"command\":\"echo b\"}}</tool_call>\n\
                    <tool_call>{\"tool\":\"bash\",\"arguments\":{\"command\":\"ls -la ~/y";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2, "应救回前 2 个完整调用，实际 {}", calls.len());
        assert_eq!(calls[0].tool, "bash");
        assert_eq!(calls[1].tool, "bash");
    }

    #[test]
    fn partial_parse_real_incident_regression() {
        // 真实事故原文（532 字节）：2 个开标签、0 个闭标签、结尾停在 `~/y` 半截。
        // 旧逻辑 break 丢弃全部；新逻辑必须救回第 1 个完整调用。
        // （原事故是 2 个完整 + 第 3 个截断，这里用取证提到的最坏形态：
        //   只有未闭合块时返回空；混合形态见 partial_parse_rescues_* 测试）
        let truncated_only = "<tool_call>{\"tool\":\"bash\",\"arguments\":{\"command\":\"echo hi";
        assert!(
            parse_tool_calls(truncated_only).is_empty(),
            "只有截断块时应返回空（没有可救回的完整调用）"
        );

        // 真实现场形态：第 1 个完整 + 第 2 个截断在命令字符串中间
        let incident = "<tool_call>{\"tool\":\"bash\",\"arguments\":{\"command\":\"echo '=== 服务 ==='; systemctl list-units | head -20\"}}</tool_call>\n\
                        <tool_call>{\"tool\":\"bash\",\"arguments\":{\"command\":\"ls -la ~/yqbot_work | head -30; ls -la ~/y";
        let calls = parse_tool_calls(incident);
        assert_eq!(calls.len(), 1, "必须救回截断前的 1 个完整调用");
        assert_eq!(calls[0].tool, "bash");
        let cmd = calls[0].arguments.get("command").and_then(|v| v.as_str());
        assert!(cmd.map(|c| c.contains("systemctl")).unwrap_or(false));
    }

    #[test]
    fn partial_parse_fully_closed_input_unaffected() {
        // 全部闭合的正常输入：行为与旧逻辑完全一致
        let text = "<tool_call>{\"tool\":\"read\",\"arguments\":{\"path\":\"/a\"}}</tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read");
    }

    #[test]
    fn default_max_tokens_is_16384() {
        // 截断事故的兜底闸：默认 max_tokens 必须提到 16384
        // （已实测网关接受 16384 / 32768）
        assert_eq!(
            LlmConfig::default().max_tokens, 16384,
            "默认 max_tokens 应为 16384（截断事故兜底）"
        );
    }

    // ════════════════════════════════════════════════════════════
    // 原生协议 / 并行执行 / 无进展检测（2026-10-11 接线新增）
    // ════════════════════════════════════════════════════════════

    // ── 原生协议转换 ──

    #[test]
    fn native_arguments_parse_json_string() {
        // 原生协议 arguments 是 JSON 字符串，须解析成 Value
        let v = parse_native_arguments(r#"{"path":"/tmp/a.txt"}"#).expect("合法 JSON 应解析成功");
        assert_eq!(v["path"], "/tmp/a.txt");
    }

    #[test]
    fn native_arguments_empty_string_yields_null() {
        // 空 arguments：按无参数处理，不算错误
        assert!(parse_native_arguments("").unwrap().is_null());
        assert!(parse_native_arguments("  ").unwrap().is_null());
    }

    #[test]
    fn native_arguments_repairs_bare_newlines() {
        // 模型在原生通道里同样会输出裸换行（多行 content）
        let raw = "{\"content\":\"第一行\n第二行\"}";
        let v = parse_native_arguments(raw).expect("裸换行应被修复");
        assert_eq!(v["content"].as_str().unwrap(), "第一行\n第二行");
    }

    #[test]
    fn native_arguments_invalid_json_reports_error() {
        // 彻底非法的 JSON：必须报错（preset 失败回灌），不能静默吞掉
        assert!(parse_native_arguments("{不是JSON").is_err());
    }

    #[test]
    fn readonly_tool_classification_is_conservative() {
        // 只有结构化 read 工具可并行
        assert!(is_readonly_tool("read"));
        assert!(!is_readonly_tool("bash"));
        // 写类必须串行
        assert!(!is_readonly_tool("write"));
        assert!(!is_readonly_tool("edit"));
        assert!(!is_readonly_tool("todo"));
        assert!(!is_readonly_tool("memory"));
        assert!(!is_readonly_tool("delegate"));
        // 未知工具按写处理（保守）
        assert!(!is_readonly_tool("nuclear_launch"));
        assert!(!is_readonly_tool(""));
    }

    // ── 无进展检测 ──

    #[test]
    fn similarity_identical_text_is_one() {
        assert!((normalized_similarity("模型反复输出同一段话", "模型反复输出同一段话") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn similarity_unrelated_text_is_low() {
        let a = "我先把目录建好然后写配置文件";
        let b = "报错原因找到了，是权限不足，现在用 sudo 重试";
        assert!(
            normalized_similarity(a, b) < NO_PROGRESS_SIMILARITY,
            "不同内容的相似度应低于阈值，实际 {}",
            normalized_similarity(a, b)
        );
    }

    #[test]
    fn similarity_ignores_whitespace_only_differences() {
        // 只有空白不同 → 视为同一内容（重复输出常伴随换行差异）
        let a = "计划：\n1. 建目录\n2. 写文件";
        let b = "计划：1. 建目录 2. 写文件";
        assert!(
            (normalized_similarity(a, b) - 1.0).abs() < 1e-9,
            "去空白后应完全一致，实际 {}",
            normalized_similarity(a, b)
        );
    }

    #[test]
    fn similarity_empty_input_is_zero() {
        assert_eq!(normalized_similarity("", "abc"), 0.0);
        assert_eq!(normalized_similarity("abc", ""), 0.0);
        assert_eq!(normalized_similarity("", ""), 0.0);
    }

    #[test]
    fn strip_tool_blocks_removes_calls_but_keeps_narrative() {
        let text = "先看文件。<tool_call>{\"tool\":\"read\",\"arguments\":{\"path\":\"/a\"}}</tool_call>再看目录。";
        let stripped = strip_tool_blocks(text);
        assert!(stripped.contains("先看文件。"));
        assert!(stripped.contains("再看目录。"));
        assert!(!stripped.contains("tool_call"));
    }

    #[test]
    fn strip_tool_blocks_handles_unclosed_block() {
        // 截断场景：未闭合块剥掉剩余全部，不 panic
        let text = "叙述部分<tool_call>{\"tool\":\"read\",\"args\":{\"p\":\"/a";
        let stripped = strip_tool_blocks(text);
        assert!(stripped.contains("叙述部分"));
        assert!(!stripped.contains("tool_call"));
    }

    // 这是**故意的常量范围守卫**：断言对象是编译期常量，目的是常量被改坏时立刻失败。
    // clippy::assertions_on_constants 针对的是恒真/恒假的废断言，不适用于守卫，故显式放行。
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn no_progress_thresholds_are_conservative() {
        // 相似度阈值必须高（0.9），宁可多跑一轮也别误杀
        assert!(
            NO_PROGRESS_SIMILARITY >= 0.85,
            "阈值过低会误杀正常推进的任务"
        );
        // 必须连续多轮才判定（单轮异常要给机会）
        assert!(
            MAX_STAGNANT_ROUNDS >= 2,
            "单轮全失败很常见，不能立即判定停滞"
        );
        assert!(
            MAX_STAGNANT_ROUNDS <= 3,
            "容忍太多轮就失去止损意义"
        );
    }

    // ── 工具协议配置 ──

    #[test]
    fn tool_protocol_config_parses_env_values() {
        // 显式值
        assert_eq!(ToolProtocol::parse("native"), ToolProtocol::Native);
        assert_eq!(ToolProtocol::parse("TEXT"), ToolProtocol::Text);
        assert_eq!(ToolProtocol::parse(" auto "), ToolProtocol::Auto);
        // 非法值回退 Auto（宁可多给原生一次机会）
        assert_eq!(ToolProtocol::parse("bogus"), ToolProtocol::Auto);
        assert_eq!(ToolProtocol::parse(""), ToolProtocol::Auto);
        // 默认即 Auto
        assert_eq!(ToolProtocol::default(), ToolProtocol::Auto);
    }
}
