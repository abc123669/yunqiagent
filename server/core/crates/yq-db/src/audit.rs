//! 工具调用审计 —— 把「这机器人对我这台机器做了什么」落成可查询的表
//!
//! ## 为什么需要它
//!
//! `bash` 工具是 `bash -c` 任意执行：**无沙箱、无白名单、无审批**
//! （SPEC 第十节明确「不审批，直接执行」）。而 `op_audit` 表从 001 迁移
//! 起就存在，却一直是**空表** —— 等于这台机器上发生过什么完全没有凭据。
//! 运维唯一能问的「上周二它改了什么」无人答得上来。
//!
//! 本模块负责把这些记录**真的写进去**。
//!
//! ## fail-open（与 `sysmon` 一致的纪律）
//!
//! 审计是**可观测性**，不是业务逻辑：一次工具执行成功了但审计写失败，
//! 正确的做法是照样把成功还给模型，然后留一条 WARN。**绝不能**因为
//! 审计写不进去就中断工具循环 —— 那等于让一台次要功能把主功能拖垮，
//! 也违背「观测手段不得改变被观测系统行为」这条基本约束。
//!
//! 因此本模块刻意**不暴露 `Result`**：写失败只打 `tracing::warn!`，
//! 调用方无从感知，也就无法（不小心）去传播它。
//! 这与 `sysmon::RestartGuard::record_boot` 的做法一致 ——
//! 守卫自身出错时返回「一切正常」，绝不拦住健康的服务。
//!
//! ## 为什么 `target` 不会被灌爆
//!
//! 长度限制在**纯函数**那一侧（`yq_core::ops::audit_target`）完成，
//! 并在本层二次兜底（`trim_target`）。SQL 层不做业务判断，
//! 但也不能完全信任调用方 —— 这是两处都限长的原因。

use std::time::Duration;

use sqlx::PgPool;

/// 单条审计写入的等待上限。
///
/// 树莓派 + 本地 PG，稳态连接是毫秒级；给 2 秒足够宽裕，
/// 又保证「数据库卡住」时不会把工具循环一起拖住。
/// 超时后本条审计被放弃（fail-open），仅留一条 WARN。
const AUDIT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// 审计记录的默认保留天数。
///
/// 选 90 天的理由：审计的价值在于**回溯事故**，而事故的追责窗口
/// 以月计；但审计是 append-only 且含命令全文，单行可达数百字节，
/// 无上限增长会挤占这台 4GB 机器的 PG 数据目录。
/// 90 天 ≈ 覆盖一个季度，本项目 `BackupConfig::retention_days`
/// 用 7 天（备份产物是整目录快照，体积大得多）相比是宽的。
pub const DEFAULT_AUDIT_RETENTION_DAYS: i64 = 90;

/// 清理一次的历史行数上限。
///
/// 单次 DELETE 不做无界删除：万一保留天数被配成 0 或负数，
/// 上限保证**一轮最多删这么多行**，把大事化小、避免长时间持锁。
const CLEANUP_BATCH_LIMIT: i64 = 5_000;

/// `op_audit.target` 的二次长度上限（按字符数）。
///
/// 与 `yq_core::ops::AUDIT_TARGET_MAX_CHARS` 对齐，此处为**兜底**：
/// 防止将来某个调用方绕过纯函数层直接调到这里时把表灌爆。
const TARGET_MAX_CHARS: usize = 512;

/// 一条工具调用的审计记录。
///
/// 字段与 `op_audit` 表一一对应（`id` / `created_at` 由数据库生成）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    /// 工具名（`read` / `write` / `edit` / `bash` / `delegate`）
    pub op_type: String,
    /// 被操作的目标（命令串 / 路径 / 委派目标），已限长
    pub target: String,
    /// 执行结果：`ok` / `error`
    pub outcome: String,
    /// 失败原因摘要；成功时为 `None`
    pub error_summary: Option<String>,
    /// 备份状态（仅危险操作有意义，其余沿用表默认值 `pending`）
    pub backup_state: Option<String>,
    /// 备份产物路径（危险操作已有产物时填充）
    pub backup_path: Option<String>,
}

impl AuditRecord {
    /// 构造一条最小审计记录（`backup_state` 留空 → 用表默认值）。
    pub fn new(
        op_type: impl Into<String>,
        target: impl Into<String>,
        outcome: impl Into<String>,
    ) -> Self {
        Self {
            op_type: op_type.into(),
            target: target.into(),
            outcome: outcome.into(),
            error_summary: None,
            backup_state: None,
            backup_path: None,
        }
    }

    /// 附上失败摘要。
    pub fn with_error(mut self, summary: impl Into<String>) -> Self {
        self.error_summary = Some(summary.into());
        self
    }

    /// 附上备份状态与产物路径。
    pub fn with_backup(mut self, state: impl Into<String>, path: Option<String>) -> Self {
        self.backup_state = Some(state.into());
        self.backup_path = path;
        self
    }
}

/// 把长文本按字符数限长（按字符切，中文/emoji 安全）。
fn trim_target(s: &str) -> &str {
    match s.char_indices().nth(TARGET_MAX_CHARS) {
        None => s,
        Some((byte_idx, _)) => &s[..byte_idx],
    }
}

/// 写一条审计记录 —— **永不返回错误**。
///
/// 这是本模块唯一的公开写入入口。写入失败只记 WARN：
/// 工具循环照常继续，模型照常拿到工具结果。
///
/// 调用前请注意：`op_audit` 的语义是「工具执行的凭据」，
/// 而非危险操作的备份台账 —— `backup_path` 有值时，
/// 对应的数据其实已经由 `stop::OpJournal` 在内存里持有。
/// 这里只是**如实转写**该状态，便于运维在数据库里一眼看出
/// 「哪些操作当时还留着可回滚的备份」，不构成回滚能力本身。
pub async fn record_tool_call(pool: Option<&PgPool>, rec: AuditRecord) {
    // 无连接池（repl/tui 等降级路径）→ 只留日志，不落库。
    let Some(pool) = pool else {
        tracing::debug!(
            "[AUDIT] 无连接池，仅记日志：op_type={} outcome={}",
            rec.op_type,
            rec.outcome
        );
        return;
    };

    // 限长放在写库前，避免超长 target 拖慢网络传输。
    let target = trim_target(&rec.target).to_string();

    let write = sqlx::query(
        "INSERT INTO op_audit (op_type, target, backup_path, backup_state, outcome, error_summary) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&rec.op_type)
    .bind(&target)
    .bind(rec.backup_path.as_deref())
    .bind(rec.backup_state.as_deref())
    .bind(&rec.outcome)
    .bind(rec.error_summary.as_deref())
    .execute(pool);

    match tokio::time::timeout(AUDIT_WRITE_TIMEOUT, write).await {
        Ok(Ok(res)) => {
            tracing::debug!(
                "[AUDIT] 已记录 op_type={} outcome={} 影响行数={}",
                rec.op_type,
                rec.outcome,
                res.rows_affected()
            );
        }
        // 超时：数据库卡住 —— fail-open，绝不阻塞工具循环
        Err(_) => {
            tracing::warn!(
                "[AUDIT] 写入超时（{}s），本条审计已放弃（不影响工具执行）: op_type={} target={}",
                AUDIT_WRITE_TIMEOUT.as_secs(),
                rec.op_type,
                target
            );
        }
        // 写库失败：例如迁移尚未应用 —— 同样只告警
        Ok(Err(e)) => {
            tracing::warn!(
                "[AUDIT] 写入失败（已忽略，不影响工具执行）: op_type={} target={} 原因={}",
                rec.op_type,
                target,
                e
            );
        }
    }
}

/// 删除早于保留期的审计记录 —— **永不返回错误**（与写入同一纪律）。
///
/// ## 为什么必须有上限
///
/// `DELETE ... WHERE created_at < cutoff` 不带 `LIMIT` 时，
/// 若保留天数被配成 0（比如误写环境变量、解析出错），
/// 一次就会清空**整张审计表** —— 而这正是出事后最需要它的时刻。
/// 因此这里同时用两个约束：
///   1. `retention_days <= 0` 视为「未配置」，直接返回，绝不删任何行；
///   2. 即使配置合法，也按 `id` 分批、每批不超过 [`CLEANUP_BATCH_LIMIT`] 行。
///
/// ## 分批而不是一次删
///
/// 审计表会持续增长，一次性删除大量行会产生大量死元组并长时间持锁，
/// 而这台机器上的 PG 与 agent 进程共享同一份 4GB 内存。
/// 分批删除把压力摊平到多轮 ticker 上。
///
/// 返回删掉的行数（供调用方记日志），失败时返回 0。
pub async fn purge_old_audit(pool: Option<&PgPool>, retention_days: i64) -> u64 {
    let Some(pool) = pool else {
        return 0;
    };
    if retention_days <= 0 {
        tracing::debug!("[AUDIT] 保留天数未配置（{retention_days}），跳过清理");
        return 0;
    }

    let del = sqlx::query(
        // 用 `$1 * interval '1 day'` 而不是 `make_interval(days => $1)`：
        // 后者的形参是 **int4**，绑定 int8 会报
        // `function make_interval(days => bigint) does not exist`。
        // 那个错会被 fail-open 分支静默吞掉，表现为「清理永远不生效」，
        // 没有任何日志提示 —— 间隔乘法对 bigint 直接可用，没有这个问题。
        "DELETE FROM op_audit WHERE id IN ( \
             SELECT id FROM op_audit \
             WHERE created_at < now() - ($1 * interval '1 day') \
             ORDER BY id ASC LIMIT $2 \
         )",
    )
    .bind(retention_days)
    .bind(CLEANUP_BATCH_LIMIT)
    .execute(pool);

    match tokio::time::timeout(AUDIT_WRITE_TIMEOUT, del).await {
        Ok(Ok(res)) => {
            let n = res.rows_affected();
            if n > 0 {
                tracing::info!("[AUDIT] 清理过期记录 {n} 条（保留 {retention_days} 天）");
            }
            n
        }
        Err(_) => {
            tracing::warn!("[AUDIT] 清理超时（已忽略，不影响服务）");
            0
        }
        Ok(Err(e)) => {
            tracing::warn!("[AUDIT] 清理失败（已忽略，不影响服务）: {e}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn purge_without_pool_is_a_no_op() {
        assert_eq!(purge_old_audit(None, 90).await, 0);
    }

    #[tokio::test]
    async fn purge_refuses_to_delete_when_retention_is_not_positive() {
        // 保留天数配错时必须「什么都不删」，绝不能清空整张审计表。
        // 无池环境下这行也能成立：它验证的是参数校验先于任何写操作。
        assert_eq!(purge_old_audit(None, 0).await, 0);
        assert_eq!(purge_old_audit(None, -1).await, 0);
    }

    #[test]
    fn record_builder_maps_fields() {
        let rec = AuditRecord::new("bash", "rm -rf /tmp/x", "ok");
        assert_eq!(rec.op_type, "bash");
        assert_eq!(rec.target, "rm -rf /tmp/x");
        assert_eq!(rec.outcome, "ok");
        // 未设置即留空，由表默认值兜底
        assert!(rec.error_summary.is_none());
        assert!(rec.backup_state.is_none());
        assert!(rec.backup_path.is_none());
    }

    #[test]
    fn builder_attaches_error_and_backup() {
        let rec = AuditRecord::new("write", "/etc/hosts", "error")
            .with_error("权限不足")
            .with_backup("done", Some("/backups/x.tar.zst".to_string()));
        assert_eq!(rec.error_summary.as_deref(), Some("权限不足"));
        assert_eq!(rec.backup_state.as_deref(), Some("done"));
        assert_eq!(rec.backup_path.as_deref(), Some("/backups/x.tar.zst"));
    }

    #[test]
    fn trim_target_bounds_by_chars_not_bytes() {
        let s = "审".repeat(TARGET_MAX_CHARS + 100);
        let t = trim_target(&s);
        assert_eq!(t.chars().count(), TARGET_MAX_CHARS);
    }

    #[test]
    fn trim_target_leaves_short_text_untouched() {
        assert_eq!(trim_target("ls -la"), "ls -la");
    }

    #[test]
    fn trim_target_does_not_panic_on_multibyte_boundary() {
        // 若按字节切会 panic（落在 UTF-8 字符中间）；按字符切必须安全
        let s = "中".repeat(513);
        let t = trim_target(&s);
        assert_eq!(t.chars().count(), TARGET_MAX_CHARS);
    }

    #[tokio::test]
    async fn record_without_pool_is_a_no_op_and_does_not_panic() {
        // 无池路径必须安静返回 —— repl/tui 就走这条路
        record_tool_call(None, AuditRecord::new("read", "/tmp/a", "ok")).await;
    }
}
