//! yq-heartbeat —— 非阻塞心跳调度
//!
//! 硬性约束（SPEC 第九节）：
//!   1. 心跳在**独立 tokio task** 中运行
//!   2. 结果通过 **mpsc channel** 投递到主事件循环
//!   3. **绝不阻塞消息处理**（OpenClaw 最大教训）
//!   4. 心跳触发时若有用户消息在处理 → **排队，不插队**
//!
//! 设计要点：
//!   · 调度循环完全在 `tokio::spawn` 中，主循环只做 `try_recv`（非阻塞）
//!   · 心跳消息加 `[心跳]` 前缀，单条 ≤ 200 字
//!   · QQ 发送失败 → 落盘 `~/.yunqiagent/heartbeat/YYYY-MM-DD.md`

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Local, Timelike};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// 心跳投递目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum Target {
    /// 发往 QQ
    #[default]
    Qq,
    /// 仅本地落盘
    Local,
    /// 两者都做
    Both,
}

/// 心跳配置。
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// 间隔；`None` 表示禁用
    pub every: Option<Duration>,
    /// 活跃时段 [起始, 结束]，如 ("08:00", "23:00")
    pub active_hours: (String, String),
    /// 投递目标
    pub target: Target,
    /// 本地落盘目录
    pub local_dir: PathBuf,
    /// 是否在提醒中包含推理过程
    pub include_reasoning: bool,
    /// HEARTBEAT.md 清单路径
    pub checklist_path: PathBuf,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let base = PathBuf::from(home).join(".yunqiagent");
        Self {
            every: Some(Duration::from_secs(30 * 60)),
            active_hours: ("08:00".to_string(), "23:00".to_string()),
            target: Target::Qq,
            local_dir: base.join("heartbeat"),
            include_reasoning: false,
            checklist_path: base.join("HEARTBEAT.md"),
        }
    }
}

impl HeartbeatConfig {
    /// 关闭心跳的配置（`every = None` 即不触发）。
    ///
    /// 用 struct update 语法而非「default() 后逐字段赋值」：
    /// 前者让编译器帮忙锁定「只改这一个字段」的意图，
    /// 后者会让 `field_reassign_with_default` 类误用（改漏/改多）无感通过。
    pub fn disabled() -> Self {
        Self {
            every: None,
            ..Self::default()
        }
    }
}

/// 一次心跳的产出（投递给主循环）。
#[derive(Debug, Clone)]
pub struct HeartbeatTick {
    /// 触发时间
    pub fired_at: DateTime<Local>,
    /// 状态：ok / alert / error
    pub status: HeartbeatStatus,
    /// 提醒文本（≤3 句；无事项时为 None）
    pub message: Option<String>,
    /// 是否需要投递到 QQ
    pub should_deliver: bool,
}

/// 心跳状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatStatus {
    /// 正常（无事项）
    Ok,
    /// 有提醒事项
    Alert,
    /// 执行出错
    Error,
}

impl HeartbeatStatus {
    /// 数据库存储字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            HeartbeatStatus::Ok => "ok",
            HeartbeatStatus::Alert => "alert",
            HeartbeatStatus::Error => "error",
        }
    }
}

/// 单条消息最大字符数（QQ 风控）。
pub const MAX_MESSAGE_CHARS: usize = 200;

/// 心跳消息前缀。
pub const HEARTBEAT_PREFIX: &str = "[心跳] ";

/// 心跳句柄。
pub struct HeartbeatHandle {
    rx: mpsc::Receiver<HeartbeatTick>,
    _task: tokio::task::JoinHandle<()>,
}

impl HeartbeatHandle {
    /// **非阻塞**尝试取一次心跳产出。
    ///
    /// 绝不在消息处理路径上 await —— 保证主循环不被心跳拖慢。
    pub fn try_recv(&mut self) -> Option<HeartbeatTick> {
        self.rx.try_recv().ok()
    }
}

/// 错误类型。
#[derive(Debug, thiserror::Error)]
pub enum HeartbeatError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}

/// 启动心跳调度器。
///
/// 返回句柄；调度循环运行在**独立 tokio task**，不影响调用方。
pub fn spawn(config: HeartbeatConfig) -> HeartbeatHandle {
    let (tx, rx) = mpsc::channel(8);

    let task = tokio::spawn(async move {
        let Some(interval) = config.every else {
            tracing::info!("心跳已禁用（every = None）");
            return;
        };

        tracing::info!("心跳调度器已启动，间隔 {:?}", interval);

        loop {
            // 睡眠在独立 task 中进行，绝不阻塞主循环
            tokio::time::sleep(interval).await;

            let now = Local::now();
            if !in_active_hours(&now, &config.active_hours) {
                tracing::debug!("心跳跳过：当前不在活跃时段");
                continue;
            }

            let tick = run_once(&config, now).await;
            if tx.send(tick).await.is_err() {
                // 主循环已关闭
                tracing::info!("心跳调度器退出：接收端已关闭");
                return;
            }
        }
    });

    HeartbeatHandle { rx, _task: task }
}

/// 判断当前是否在活跃时段内。
fn in_active_hours(now: &DateTime<Local>, hours: &(String, String)) -> bool {
    let parse = |s: &str| -> Option<u32> {
        let mut parts = s.split(':');
        let h: u32 = parts.next()?.trim().parse().ok()?;
        let m: u32 = parts.next()?.trim().parse().ok()?;
        Some(h * 60 + m)
    };
    match (parse(&hours.0), parse(&hours.1)) {
        (Some(start), Some(end)) => {
            let cur = now.hour() * 60 + now.minute();
            if start <= end {
                cur >= start && cur <= end
            } else {
                // 跨午夜
                cur >= start || cur <= end
            }
        }
        _ => true,
    }
}

/// 执行一次心跳检查。
async fn run_once(config: &HeartbeatConfig, now: DateTime<Local>) -> HeartbeatTick {
    match read_checklist(&config.checklist_path).await {
        Ok(items) if items.is_empty() => HeartbeatTick {
            fired_at: now,
            status: HeartbeatStatus::Ok,
            message: None,
            should_deliver: false,
        },
        Ok(items) => {
            let message = compose_reminder(&items);
            HeartbeatTick {
                fired_at: now,
                status: HeartbeatStatus::Alert,
                message: Some(message),
                should_deliver: matches!(config.target, Target::Qq | Target::Both),
            }
        }
        Err(e) => {
            tracing::warn!("读取 HEARTBEAT.md 失败: {}", e);
            HeartbeatTick {
                fired_at: now,
                status: HeartbeatStatus::Error,
                message: Some(format!("[心跳] 读取清单失败：{}", e)),
                should_deliver: false,
            }
        }
    }
}

/// 读取 HEARTBEAT.md 清单，返回非空条目。
///
/// 若文件不存在则创建空模板。
pub async fn read_checklist(path: &PathBuf) -> Result<Vec<String>, HeartbeatError> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            path,
            "# HEARTBEAT 检查清单\n\n<!-- 每行一条待办；空行与 # 开头忽略 -->\n",
        )
        .await?;
        return Ok(Vec::new());
    }

    let text = tokio::fs::read_to_string(path).await?;
    let items: Vec<String> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect();
    Ok(items)
}

/// 由清单条目生成不超过 3 句的提醒，加 `[心跳]` 前缀，单条 ≤ 200 字。
pub fn compose_reminder(items: &[String]) -> String {
    let mut body = String::new();
    for (idx, item) in items.iter().take(3).enumerate() {
        if idx > 0 {
            body.push('；');
        }
        body.push_str(item);
    }
    if items.len() > 3 {
        body.push_str(&format!("；等共 {} 项", items.len()));
    }

    let mut msg = format!("{}{}", HEARTBEAT_PREFIX, body);
    if msg.chars().count() > MAX_MESSAGE_CHARS {
        msg = msg.chars().take(MAX_MESSAGE_CHARS - 1).collect::<String>();
        msg.push('…');
    }
    msg
}

/// 把心跳消息落盘到 `local_dir/YYYY-MM-DD.md`（QQ 发送失败或 target=local/both 时）。
pub async fn archive_to_local(
    local_dir: &PathBuf,
    now: &DateTime<Local>,
    message: &str,
) -> Result<PathBuf, HeartbeatError> {
    tokio::fs::create_dir_all(local_dir).await?;
    let file = local_dir.join(format!("{}.md", now.format("%Y-%m-%d")));
    let line = format!("- {} {}\n", now.format("%H:%M:%S"), message);

    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .await?;
    f.write_all(line.as_bytes()).await?;
    Ok(file)
}

/// 把长消息按 ≤200 字分片（QQ 风控）。
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return vec![text.to_string()];
    }
    chars
        .chunks(max_chars)
        .map(|c| c.iter().collect::<String>())
        .collect()
}
