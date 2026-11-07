//! 危险操作 + 后台慢速备份队列
//!
//! 硬性约束（SPEC 第十节）：
//!   · 危险操作 = write / edit 覆盖已有文件、bash 中含删除/移动/覆盖语义的命令
//!   · **不审批，直接执行**
//!   · 执行前，将被操作的目录加入备份队列
//!   · 备份**后台慢速**进行，6 小时窗口内完成即可（树莓派 CPU 弱）
//!   · 压缩（zstd），按天归档：`~/.yunqiagent/backups/YYYY-MM-DD/<op_id>.tar.zst`
//!   · 保留策略：默认 7 天
//!   · 后台 task 限制 CPU（nice）和 IO（ionice），不得影响交互响应
//!
//! 关键：`BackupQueue::enqueue()` **立即返回**，绝不阻塞调用方。

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Local;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// 危险操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpType {
    /// bash 命令
    Bash,
    /// write 工具
    Write,
    /// edit 工具
    Edit,
}

impl OpType {
    /// 数据库存储字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            OpType::Bash => "bash",
            OpType::Write => "write",
            OpType::Edit => "edit",
        }
    }
}

/// 备份状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupState {
    /// 排队等待
    Pending,
    /// 进行中
    Running,
    /// 完成
    Done,
    /// 失败
    Failed,
}

impl BackupState {
    /// 数据库存储字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            BackupState::Pending => "pending",
            BackupState::Running => "running",
            BackupState::Done => "done",
            BackupState::Failed => "failed",
        }
    }
}

// ═══════════════════════════════════════════════════════════════
//  工具调用审计 —— 「这机器人对我这台机器做了什么」的答案
// ═══════════════════════════════════════════════════════════════

/// `target` 列的长度上限（按字符数）。
///
/// `op_audit.target` 是 `text` 类型，理论能装下任意多内容，但**绝不能**把
/// 完整的工具输出灌进去：审计表是给运维用 SQL 翻的，一条几千行的
/// `cat` 输出会让整张表迅速膨胀到无法阅读。
/// 因此写入侧一律只存**参数**（命令串 / 路径 / 委派目标），且强制限长。
pub const AUDIT_TARGET_MAX_CHARS: usize = 512;

/// `error_summary` 列的长度上限（按字符数）。
///
/// 错误摘要只需要能定位问题，不需要完整错误体（完整错误在日志里）。
pub const AUDIT_ERROR_MAX_CHARS: usize = 200;

/// 被审计的工具。
///
/// 与 [`OpType`] 的区别：`OpType` 只覆盖**危险**操作（write/edit/bash），
/// 而审计要记录**全部五个工具** —— `read` 虽然无害，但「它到底读了哪些
/// 文件」同样是最常被追问的问题，缺失这一半轨迹，审计价值就打折了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditTool {
    /// 读文件
    Read,
    /// 写文件
    Write,
    /// 编辑文件
    Edit,
    /// 执行 shell（无沙箱、无白名单，最高危）
    Bash,
    /// 委派子代理
    Delegate,
    /// 未注册/未知工具名（原样保留，便于排查「模型幻觉出工具名」）
    Other(String),
}

impl AuditTool {
    /// 由工具名解析；不认识的名字一律进 [`AuditTool::Other`]。
    pub fn from_tool_name(name: &str) -> Self {
        match name {
            "read" => Self::Read,
            "write" => Self::Write,
            "edit" => Self::Edit,
            "bash" => Self::Bash,
            "delegate" => Self::Delegate,
            other => Self::Other(other.to_string()),
        }
    }

    /// 数据库存储字符串。
    pub fn as_str(&self) -> &str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Bash => "bash",
            Self::Delegate => "delegate",
            Self::Other(name) => name.as_str(),
        }
    }
}

/// 工具执行结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// 工具正常返回
    Ok,
    /// 工具报错 / 未知工具 / 被 /stop 打断
    Error,
}

impl AuditOutcome {
    /// 数据库存储字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }

    /// 由工具执行层的 `is_error` 判定。
    pub fn from_is_error(is_error: bool) -> Self {
        if is_error { Self::Error } else { Self::Ok }
    }
}

/// 把控制字符转成可见转义形式，并按字符数限长 —— 得到「单行、可读、定长」的审计文本。
///
/// 为什么必须转义：`bash` 的命令串天然含换行与制表符，原样入库会让
/// 一条审计记录在 `psql` 里裂成十几行，既没法 grep 也没法导出成单行日志。
pub fn sanitize_for_audit(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4) + 8);
    for ch in s.chars() {
        if out.chars().count() >= max {
            // 显式标记被截断，否则「限长」会被误读成「原文就这么长」
            out.push('…');
            break;
        }
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                // 其余 C0/C1 控制字符一律丢弃，只保留可见部分
            }
            c => out.push(c),
        }
    }
    out
}

/// 提取写入 `op_audit.target` 的描述串。
///
/// 每个工具取**语义上最关键的参数**，与「给人看的通知」规则保持一致：
///   · `bash`     → 命令串（最需要被追问的东西）
///   · `read`/`write`/`edit` → 文件路径
///   · `delegate` → 委派目标（`goal`）
///   · 其它/缺参 → 参数键名列表（能看出模型想干什么，又不泄露正文）
///
/// **只取参数、不取结果**：结果可能包含整份文件内容，审计表装不下。
/// 结果的成败由 `outcome` / `error_summary` 两列单独承载。
pub fn audit_target(tool: &str, args: &serde_json::Value) -> String {
    let picked = match tool {
        "bash" => args.get("command").and_then(|v| v.as_str()),
        "read" | "write" | "edit" => args.get("path").and_then(|v| v.as_str()),
        "delegate" => args.get("goal").and_then(|v| v.as_str()),
        _ => None,
    };

    match picked {
        Some(s) if !s.trim().is_empty() => sanitize_for_audit(s, AUDIT_TARGET_MAX_CHARS),
        // 参数缺失或为空 → 退化成键名列表
        _ => sanitize_for_audit(&arg_key_list(args), AUDIT_TARGET_MAX_CHARS),
    }
}

/// 列出参数键名（模型给了参数但没给对字段时的兜底描述）。
fn arg_key_list(args: &serde_json::Value) -> String {
    match args.as_object() {
        Some(obj) if !obj.is_empty() => {
            let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
            format!("({})", keys.join(","))
        }
        _ => "(无参数)".to_string(),
    }
}

/// 生成 `error_summary`；成功返回 `None`（该列留空）。
pub fn audit_error_summary(is_error: bool, content: &str, is_interrupted: bool) -> Option<String> {
    if is_interrupted {
        // /stop 打断不是工具自身报错，单独标注，避免被误读成 bash 失败
        return Some("（执行被 /stop 中断）".to_string());
    }
    if !is_error {
        return None;
    }
    let text = content.trim();
    if text.is_empty() {
        return Some("(工具返回空错误)".to_string());
    }
    Some(sanitize_for_audit(text, AUDIT_ERROR_MAX_CHARS))
}

/// 一次危险操作的记录（对应 `op_audit` 表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DangerOp {
    /// 操作唯一标识
    pub op_id: String,
    /// 操作类型
    pub op_type: OpType,
    /// 目标路径 / 命令
    pub target: String,
    /// 备份状态
    pub backup_state: BackupState,
    /// 备份产物路径（完成后填充）
    pub backup_path: Option<PathBuf>,
    /// 是否已通知
    pub notified: bool,
}

/// 备份任务（内部使用）。
struct BackupTask {
    op: DangerOp,
    /// 待备份的源路径
    source: PathBuf,
    /// 完成后回执（可选，便于测试同步）
    done_tx: Option<oneshot::Sender<BackupOutcome>>,
}

/// 备份结果。
#[derive(Debug, Clone)]
pub struct BackupOutcome {
    /// 操作 ID
    pub op_id: String,
    /// 结果状态
    pub state: BackupState,
    /// 产物路径
    pub artifact: Option<PathBuf>,
    /// 错误信息
    pub error: Option<String>,
}

/// 备份队列配置。
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// 备份根目录
    pub backup_dir: PathBuf,
    /// 保留天数
    pub retention_days: u32,
    /// 单次备份超时（6 小时窗口）
    pub window: Duration,
}

impl Default for BackupConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Self {
            backup_dir: PathBuf::from(home).join(".yunqiagent/backups"),
            retention_days: 7,
            window: Duration::from_secs(6 * 60 * 60),
        }
    }
}

/// 备份队列句柄。
pub struct BackupQueue {
    tx: mpsc::Sender<BackupTask>,
}

impl BackupQueue {
    /// 启动后台备份 worker（独立低优先级 tokio task）。
    pub fn spawn(config: BackupConfig) -> Self {
        let (tx, mut rx) = mpsc::channel::<BackupTask>(64);

        tokio::spawn(async move {
            tracing::info!("备份 worker 已启动，目录 {:?}", config.backup_dir);
            while let Some(task) = rx.recv().await {
                let outcome = run_backup(&task, &config).await;
                if let Some(tx) = task.done_tx {
                    let _ = tx.send(outcome);
                }
            }
            tracing::info!("备份 worker 退出：队列已关闭");
        });

        Self { tx }
    }

    /// 入队一次备份。
    ///
    /// **立即返回**（`try_send`），绝不阻塞危险操作的执行路径。
    /// 队列满或关闭时返回 `false`，调用方据此决定是否降级（如直接落盘提示）。
    pub fn enqueue(&self, op: DangerOp, source: PathBuf) -> bool {
        let task = BackupTask {
            op,
            source,
            done_tx: None,
        };
        self.tx.try_send(task).is_ok()
    }

    /// 入队并等待完成（仅用于测试 / 手动 `yq rollback` 前的同步备份）。
    pub async fn enqueue_and_wait(&self, op: DangerOp, source: PathBuf) -> Option<BackupOutcome> {
        let (done_tx, done_rx) = oneshot::channel();
        let task = BackupTask {
            op,
            source,
            done_tx: Some(done_tx),
        };
        if self.tx.send(task).await.is_err() {
            return None;
        }
        done_rx.await.ok()
    }
}

/// 执行一次备份（在 worker task 中运行）。
async fn run_backup(task: &BackupTask, config: &BackupConfig) -> BackupOutcome {
    let op_id = task.op.op_id.clone();
    let source = task.source.clone();

    if !source.exists() {
        return BackupOutcome {
            op_id,
            state: BackupState::Failed,
            artifact: None,
            error: Some(format!("源路径不存在: {}", source.display())),
        };
    }

    let date = Local::now().format("%Y-%m-%d").to_string();
    let out_dir = config.backup_dir.join(&date);
    if let Err(e) = tokio::fs::create_dir_all(&out_dir).await {
        return BackupOutcome {
            op_id,
            state: BackupState::Failed,
            artifact: None,
            error: Some(format!("创建备份目录失败: {}", e)),
        };
    }

    let artifact = out_dir.join(format!("{}.tar.zst", op_id));

    // 低优先级：nice +19，ionice idle
    // tar 打包源目录 → zstd 压缩
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let name = source
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());

    let cmd = format!(
        "nice -n 19 ionice -c3 tar -C {} -cf - {} | nice -n 19 ionice -c3 zstd -T1 -3 -q -o {}",
        shell_quote(&parent.to_string_lossy()),
        shell_quote(&name),
        shell_quote(&artifact.to_string_lossy()),
    );

    let fut = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .output();

    match tokio::time::timeout(config.window, fut).await {
        Ok(Ok(output)) if output.status.success() => BackupOutcome {
            op_id,
            state: BackupState::Done,
            artifact: Some(artifact),
            error: None,
        },
        Ok(Ok(output)) => BackupOutcome {
            op_id,
            state: BackupState::Failed,
            artifact: None,
            error: Some(String::from_utf8_lossy(&output.stderr).to_string()),
        },
        Ok(Err(e)) => BackupOutcome {
            op_id,
            state: BackupState::Failed,
            artifact: None,
            error: Some(format!("执行备份命令失败: {}", e)),
        },
        Err(_) => BackupOutcome {
            op_id,
            state: BackupState::Failed,
            artifact: None,
            error: Some("备份超时（超出 6 小时窗口）".to_string()),
        },
    }
}

/// 清理超过保留天数的备份目录。
pub async fn cleanup_old_backups(config: &BackupConfig) -> Result<usize, std::io::Error> {
    let cutoff = Local::now().date_naive() - chrono::Duration::days(config.retention_days as i64);
    let mut removed = 0usize;

    let mut rd = match tokio::fs::read_dir(&config.backup_dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(0),
    };

    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&name, "%Y-%m-%d") {
            if date < cutoff {
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// 生成操作 ID（含时间戳，**注意：此 ID 绝不进入 KV-cache 前缀**）。
pub fn new_op_id(op_type: OpType) -> String {
    format!(
        "{}-{}",
        op_type.as_str(),
        Local::now().format("%Y%m%d%H%M%S%3f")
    )
}

/// 简单的 shell 引号包裹。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_five_tools_map_to_stable_strings() {
        // 五个工具的审计取值必须与 `op_audit.op_type` 约定一致
        assert_eq!(AuditTool::from_tool_name("read").as_str(), "read");
        assert_eq!(AuditTool::from_tool_name("write").as_str(), "write");
        assert_eq!(AuditTool::from_tool_name("edit").as_str(), "edit");
        assert_eq!(AuditTool::from_tool_name("bash").as_str(), "bash");
        assert_eq!(AuditTool::from_tool_name("delegate").as_str(), "delegate");
    }

    #[test]
    fn unknown_tool_keeps_its_own_name() {
        // 未知工具名原样保留：模型幻觉出的工具名本身就是审计线索
        let t = AuditTool::from_tool_name("frobnicate");
        assert_eq!(t.as_str(), "frobnicate");
    }

    #[test]
    fn bash_target_is_the_command() {
        let t = audit_target(
            "bash",
            &json!({"command": "rm -rf /tmp/x", "timeout_secs": 60}),
        );
        assert_eq!(t, "rm -rf /tmp/x");
    }

    #[test]
    fn file_tools_target_is_the_path() {
        for tool in ["read", "write", "edit"] {
            let t = audit_target(tool, &json!({"path": "/etc/hosts", "content": "x"}));
            assert_eq!(t, "/etc/hosts", "{}", tool);
        }
    }

    #[test]
    fn delegate_target_is_the_goal() {
        let t = audit_target(
            "delegate",
            &json!({"goal": "分析磁盘占用", "context": "忽略"}),
        );
        assert_eq!(t, "分析磁盘占用");
    }

    #[test]
    fn missing_arg_falls_back_to_key_names() {
        let t = audit_target("bash", &json!({"cmd": "ls"}));
        assert_eq!(t, "(cmd)");
    }

    #[test]
    fn empty_args_render_as_no_args() {
        assert_eq!(audit_target("read", &json!({})), "(无参数)");
        assert_eq!(audit_target("read", &json!(null)), "(无参数)");
    }

    #[test]
    fn target_is_bounded_so_audit_table_cannot_blow_up() {
        // 模型可能吐出超长命令（管道、base64 粘贴）；审计表绝不能跟着膨胀
        let long = "a".repeat(20_000);
        let t = audit_target("bash", &json!({ "command": long }));
        assert!(
            t.chars().count() <= AUDIT_TARGET_MAX_CHARS + 1,
            "实得 {}",
            t.chars().count()
        );
        assert!(t.ends_with('…'), "被截断时应留下可见标记");
    }

    #[test]
    fn sanitize_escapes_newlines_to_keep_one_row_per_call() {
        // 换行原样入库会让一条审计在 psql 里裂成多行，grep/导出都会失效
        let t = audit_target("bash", &json!({ "command": "echo a\necho b\tc" }));
        assert_eq!(t, "echo a\\necho b\\tc");
        assert!(!t.contains('\n'), "target 不得含真实换行");
    }

    #[test]
    fn sanitize_drops_other_control_chars() {
        let s = sanitize_for_audit("ok\u{7}\u{0}done", 100);
        assert_eq!(s, "okdone");
    }

    #[test]
    fn sanitize_respects_char_bound_not_byte_bound() {
        // 中文按字符计：不得把多字节切坏（末尾 … 为截断标记）
        let s = "审".repeat(10);
        let out = sanitize_for_audit(&s, 4);
        assert_eq!(out, "审审审审…");
    }

    #[test]
    fn outcome_string_and_classification() {
        assert_eq!(AuditOutcome::from_is_error(false).as_str(), "ok");
        assert_eq!(AuditOutcome::from_is_error(true).as_str(), "error");
    }

    #[test]
    fn success_has_no_error_summary() {
        assert!(audit_error_summary(false, "读取成功", false).is_none());
    }

    #[test]
    fn failure_keeps_a_bounded_one_line_summary() {
        let e = audit_error_summary(true, &format!("工具执行失败: {}", "x".repeat(5000)), false)
            .unwrap();
        assert!(e.chars().count() <= AUDIT_ERROR_MAX_CHARS + 1);
        assert!(!e.contains('\n'));
    }

    #[test]
    fn empty_error_body_gets_a_placeholder() {
        assert_eq!(
            audit_error_summary(true, "   ", false).as_deref(),
            Some("(工具返回空错误)")
        );
    }

    #[test]
    fn interrupt_is_labelled_distinctly_from_failure() {
        // /stop 打断不是 bash 自身报错，标注区分开才不会被误判成执行失败
        assert_eq!(
            audit_error_summary(true, "（执行被 /stop 中断）", true).as_deref(),
            Some("（执行被 /stop 中断）")
        );
    }
}
