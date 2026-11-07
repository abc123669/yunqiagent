//! 中断信号与操作回滚 —— `/stop` 的两根支柱
//!
//! ## 为什么需要它
//!
//! 用户发 `/stop` 的场景往往是「任务跑偏了」或「它正要/已经做了危险操作」。
//! 因此这里提供两件事：
//!
//! 1. **立即中断**（[`StopSignal`]）——
//!    信号一到就停，包括**打断正在执行的工具**（如一个跑了很久的 bash），
//!    而不是等当前轮跑完。
//!
//! 2. **操作回滚**（[`OpJournal`]）——
//!    记录每个危险操作执行前的**快照**，`/stop` 时按逆序恢复，
//!    把系统退回操作发生之前。
//!
//! ## 与 SPEC 第十节的对应
//!
//! SPEC 要求「危险操作执行前把被操作的目录加入备份队列」——
//! 本项目已在 `yq-core::ops` 实现了备份队列（后台慢速、nice/ionice、zstd）。
//! 本模块负责**在工具执行前调用它**，并登记可回滚的操作日志。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// 中断信号（可克隆，跨 task 共享）。
///
/// 组合了「标志位」（轮询用）与「通知」（立即唤醒用）：
///   · 轮次/工具边界处 `is_stopped()` 轮询
///   · 等待中的 `select!` 用 `wait()` 立即响应
#[derive(Clone)]
pub struct StopSignal {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl StopSignal {
    /// 新建（未触发状态）。
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// 触发中断。
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        // 唤醒所有等待者
        self.notify.notify_waiters();
    }

    /// 是否已中断（轮询用，非阻塞）。
    pub fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 复位（新一轮开始时调用）。
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// 等待中断发生（供 `select!` 使用）。
    ///
    /// 若已中断则立即返回。
    pub async fn wait(&self) {
        if self.is_stopped() {
            return;
        }
        self.notify.notified().await;
    }
}

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// 一次可回滚的操作记录。
#[derive(Debug, Clone)]
pub struct OpRecord {
    /// 操作类型（bash / write / edit）
    pub op_type: String,
    /// 被操作的目标（文件路径或命令）
    pub target: String,
    /// 操作前备份产物的路径
    pub backup_path: PathBuf,
    /// 操作前目标是否已存在（不存在则回滚 = 删除新建的文件）
    pub existed_before: bool,
}

/// 操作日志 —— 记录本轮做过的可回滚操作。
///
/// 按**发生顺序**记录，回滚时**逆序**执行（后做的先撤），
/// 这样多层修改能正确还原。
#[derive(Default)]
pub struct OpJournal {
    records: Vec<OpRecord>,
}

impl OpJournal {
    /// 新建空日志。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次操作。
    pub fn push(&mut self, record: OpRecord) {
        self.records.push(record);
    }

    /// 本轮是否记录过操作。
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 已记录的操作数。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 清空（新一轮开始时）。
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// 取出全部记录（不消费）。
    pub fn records(&self) -> &[OpRecord] {
        &self.records
    }
}

/// 回滚结果。
#[derive(Debug, Default)]
pub struct RollbackReport {
    /// 成功恢复的操作数
    pub restored: usize,
    /// 删除了新建文件的操作数
    pub deleted: usize,
    /// 失败的项（目标, 原因）
    pub failures: Vec<(String, String)>,
}

impl RollbackReport {
    /// 是否全部成功。
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }

    /// 渲染成给人看的一行摘要。
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.restored > 0 {
            parts.push(format!("恢复 {} 个文件", self.restored));
        }
        if self.deleted > 0 {
            parts.push(format!("删除 {} 个新建文件", self.deleted));
        }
        if !self.failures.is_empty() {
            parts.push(format!("{} 项失败", self.failures.len()));
        }
        if parts.is_empty() {
            "无可回滚的操作".to_string()
        } else {
            parts.join("，")
        }
    }
}

/// 按逆序回滚操作日志。
///
/// 对每个记录：
///   · 若操作前**不存在**该文件 → 删除它（撤销"新建"）
///   · 若操作前**存在** → 用备份产物恢复
///
/// 备份产物是 tar.zst（见 `yq-core::ops`），这里用 `tar` 命令还原。
pub async fn rollback(journal: &OpJournal) -> RollbackReport {
    let mut report = RollbackReport::default();

    // 逆序：后做的先撤
    for rec in journal.records().iter().rev() {
        let target = Path::new(&rec.target);

        if !rec.existed_before {
            // 操作前不存在 → 回滚即删除（撤销新建）
            if target.exists() {
                match delete_path(target) {
                    Ok(()) => {
                        report.deleted += 1;
                        tracing::info!("回滚[{}]：删除新建的 {}", rec.op_type, target.display());
                    }
                    Err(e) => report
                        .failures
                        .push((rec.target.clone(), format!("删除失败: {}", e))),
                }
            }
            continue;
        }

        // 操作前存在 → 用备份还原
        if !rec.backup_path.exists() {
            report.failures.push((
                rec.target.clone(),
                format!("备份产物不存在: {}", rec.backup_path.display()),
            ));
            continue;
        }

        match restore_from_backup(&rec.backup_path, target).await {
            Ok(()) => {
                report.restored += 1;
                tracing::info!("回滚[{}]：已恢复 {}", rec.op_type, target.display());
            }
            Err(e) => report
                .failures
                .push((rec.target.clone(), format!("恢复失败: {}", e))),
        }
    }

    report
}

/// 删除文件或目录。
fn delete_path(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// 从 tar.zst 备份产物恢复目标。
///
/// 备份是由 `yq-core::ops` 用
/// `tar -C <parent> -cf - <name> | zstd` 生成的（只含目标自身的相对路径），
/// 因此还原时解到同一个 parent 目录即可覆盖。
async fn restore_from_backup(backup: &Path, target: &Path) -> Result<(), String> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));

    let cmd = format!(
        "zstd -d -c {} | tar -C {} -xf -",
        shell_quote(&backup.to_string_lossy()),
        shell_quote(&parent.to_string_lossy()),
    );

    let out = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

/// 简单的 shell 引号包裹。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn new_signal_is_not_stopped() {
        let s = StopSignal::new();
        assert!(!s.is_stopped());
    }

    #[test]
    fn trigger_sets_flag() {
        let s = StopSignal::new();
        s.trigger();
        assert!(s.is_stopped());
    }

    #[test]
    fn reset_clears_flag() {
        let s = StopSignal::new();
        s.trigger();
        assert!(s.is_stopped());
        s.reset();
        assert!(!s.is_stopped());
    }

    #[test]
    fn clones_share_state() {
        let a = StopSignal::new();
        let b = a.clone();
        a.trigger();
        assert!(b.is_stopped(), "克隆应共享同一标志");
    }

    #[tokio::test]
    async fn wait_returns_immediately_if_already_stopped() {
        let s = StopSignal::new();
        s.trigger();
        // 不应挂起
        tokio::time::timeout(Duration::from_millis(100), s.wait())
            .await
            .expect("已中断时应立即返回");
    }

    #[tokio::test]
    async fn wait_wakes_on_trigger() {
        let s = StopSignal::new();
        let s2 = s.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            s2.trigger();
        });
        tokio::time::timeout(Duration::from_millis(500), s.wait())
            .await
            .expect("应在触发时被唤醒");
    }

    #[test]
    fn journal_records_in_order() {
        let mut j = OpJournal::new();
        assert!(j.is_empty());
        j.push(OpRecord {
            op_type: "write".into(),
            target: "/tmp/a".into(),
            backup_path: "/tmp/b1".into(),
            existed_before: true,
        });
        j.push(OpRecord {
            op_type: "edit".into(),
            target: "/tmp/b".into(),
            backup_path: "/tmp/b2".into(),
            existed_before: false,
        });
        assert_eq!(j.len(), 2);
        assert_eq!(j.records()[0].target, "/tmp/a");
        assert_eq!(j.records()[1].target, "/tmp/b");
    }

    #[test]
    fn journal_clear_resets() {
        let mut j = OpJournal::new();
        j.push(OpRecord {
            op_type: "write".into(),
            target: "/tmp/a".into(),
            backup_path: "/tmp/b1".into(),
            existed_before: true,
        });
        j.clear();
        assert!(j.is_empty());
    }

    #[test]
    fn rollback_report_summary_is_plain_text() {
        let mut r = RollbackReport::default();
        assert_eq!(r.summary(), "无可回滚的操作");
        assert!(r.is_clean());

        r.restored = 2;
        r.deleted = 1;
        assert!(r.summary().contains("恢复 2 个文件"));
        assert!(r.summary().contains("删除 1 个新建文件"));
        assert!(!r.summary().contains('|'), "不应含 Markdown 表格符");
    }

    #[test]
    fn rollback_report_flags_failures() {
        let mut r = RollbackReport::default();
        r.failures.push(("/tmp/x".into(), "权限不足".into()));
        assert!(!r.is_clean());
        assert!(r.summary().contains("1 项失败"));
    }

    /// 端到端：真实文件的新建 → 回滚（删除）。
    #[tokio::test]
    async fn rollback_deletes_newly_created_file() {
        let dir = std::env::temp_dir().join(format!("yq_stop_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join("newly_created.txt");

        // 模拟：agent 新建了文件（操作前不存在）
        std::fs::write(&target, "内容").unwrap();
        assert!(target.exists());

        // 记一条「操作前不存在」的记录
        let mut j = OpJournal::new();
        j.push(OpRecord {
            op_type: "write".into(),
            target: target.to_string_lossy().to_string(),
            backup_path: PathBuf::new(),
            existed_before: false,
        });

        let report = rollback(&j).await;
        assert_eq!(report.deleted, 1, "应删除 1 个新建文件");
        assert!(!target.exists(), "文件应已被删除（回滚成功）");
        assert!(report.is_clean());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端：真实文件的备份 → 篡改 → 回滚（恢复原内容）。
    ///
    /// 备份产物格式与 `yq-core::ops` 一致：`tar -C <parent> -cf - <name> | zstd`。
    #[tokio::test]
    async fn rollback_restores_modified_file_from_backup() {
        let dir = std::env::temp_dir().join(format!("yq_stop_restore_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join("important.txt");
        let backup = dir.join("important.tar.zst");

        // 原始内容
        std::fs::write(&target, "原始内容").unwrap();

        // 用与 ops.rs 相同的方式生成备份
        let cmd = format!(
            "tar -C {} -cf - {} | zstd -q -3 -o {}",
            dir.to_string_lossy(),
            "important.txt",
            backup.to_string_lossy()
        );
        let st = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .status()
            .await
            .expect("备份命令应能执行");

        if !st.success() || !backup.exists() {
            eprintln!("跳过：环境缺少 tar/zstd，无法验证恢复路径");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        // 篡改文件
        std::fs::write(&target, "被改坏的內容").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "被改坏的內容");

        // 回滚
        let mut j = OpJournal::new();
        j.push(OpRecord {
            op_type: "edit".into(),
            target: target.to_string_lossy().to_string(),
            backup_path: backup.clone(),
            existed_before: true,
        });

        let report = rollback(&j).await;
        assert_eq!(report.restored, 1, "应恢复 1 个文件；报告: {:?}", report);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "原始内容",
            "文件内容应已恢复为备份时的状态"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_target_extraction_finds_absolute_paths() {
        // 新逻辑要求目标**真实存在**（不存在的文件没什么可备份的），
        // 因此这里建真实文件。
        let dir = std::env::temp_dir().join(format!("yq_rm_paths_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("aaa");
        let b = dir.join("bbb");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();

        let cmd = format!("rm -rf {} {}", a.display(), b.display());
        let paths = crate::agent::extract_bash_targets(&cmd);
        assert!(
            paths.contains(&a),
            "应识别 {}，实得 {:?}",
            a.display(),
            paths
        );
        assert!(
            paths.contains(&b),
            "应识别 {}，实得 {:?}",
            b.display(),
            paths
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_target_extraction_ignores_safe_commands() {
        assert!(crate::agent::extract_bash_targets("ls -la /tmp").is_empty());
        assert!(crate::agent::extract_bash_targets("cat /etc/hostname").is_empty());
    }

    #[test]
    fn bash_target_extraction_handles_redirect() {
        // 同上：目标需真实存在
        let f = std::env::temp_dir().join(format!("yq_redir_{}.txt", std::process::id()));
        std::fs::write(&f, "x").unwrap();

        let cmd = format!("echo hi > {}", f.display());
        let paths = crate::agent::extract_bash_targets(&cmd);
        assert!(paths.contains(&f), "应识别重定向目标，实得 {:?}", paths);

        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn bash_target_extraction_skips_variables_and_globs() {
        // 变量和通配符无法静态确定，不应返回
        assert!(crate::agent::extract_bash_targets("rm -rf $DIR").is_empty());
        assert!(crate::agent::extract_bash_targets("rm -rf /tmp/*").is_empty());
    }

    /// ⚠️ 事故回归：`2>/dev/null` 是**丢弃输出**，绝不能被当成"写入"。
    ///
    /// 2026-10-05：`du -xh --max-depth=1 / 2>/dev/null` 被误判为破坏性操作，
    /// 把 `/` 提取成备份目标并真的备份了根目录（1.28GB），撑爆系统盘。
    #[test]
    fn dev_null_redirect_is_not_destructive() {
        let paths = crate::agent::extract_bash_targets("du -xh --max-depth=1 / 2>/dev/null");
        assert!(
            paths.is_empty(),
            "2>/dev/null 不应产生任何备份目标，实得 {:?}",
            paths
        );

        // 常见变体也都要安全
        for cmd in [
            "ls -la / > /dev/null",
            "df -h 2>/dev/null",
            "du -sh /var &>/dev/null",
            "find / -name x 2>/dev/null",
        ] {
            let p = crate::agent::extract_bash_targets(cmd);
            assert!(p.is_empty(), "`{}` 不应触发备份，实得 {:?}", cmd, p);
        }
    }

    /// ⚠️ 事故回归：宽泛目录一律拒绝备份。
    #[test]
    fn broad_directories_are_never_backupable() {
        use std::path::Path;
        for bad in ["/", "/mnt", "/home", "/var", "/usr"] {
            assert!(
                !crate::agent::is_backupable_path(Path::new(bad)),
                "`{}` 不该被当作可备份目标",
                bad
            );
        }
    }

    /// 伪文件系统与敏感目录也拒绝。
    #[test]
    fn pseudo_filesystems_are_not_backupable() {
        use std::path::Path;
        for bad in [
            "/dev/null",
            "/proc/self",
            "/sys/kernel",
            "/run/x",
            "/etc/passwd",
        ] {
            assert!(
                !crate::agent::is_backupable_path(Path::new(bad)),
                "`{}` 不该被当作可备份目标",
                bad
            );
        }
    }

    /// 真正的重定向仍要能识别（不能因噎废食）。
    #[test]
    fn real_redirect_still_detected() {
        let tmp = std::env::temp_dir().join("yq_redirect_probe.txt");
        std::fs::write(&tmp, "x").unwrap();

        let cmd = format!("echo hi > {}", tmp.display());
        let paths = crate::agent::extract_bash_targets(&cmd);
        assert!(
            paths.iter().any(|p| p == &tmp),
            "真重定向目标应被识别，实得 {:?}",
            paths
        );

        let _ = std::fs::remove_file(&tmp);
    }

    /// `rm` 具体文件仍要能识别。
    #[test]
    fn rm_specific_file_still_detected() {
        let tmp = std::env::temp_dir().join("yq_rm_probe.txt");
        std::fs::write(&tmp, "x").unwrap();

        let cmd = format!("rm -f {}", tmp.display());
        let paths = crate::agent::extract_bash_targets(&cmd);
        assert!(
            paths.iter().any(|p| p == &tmp),
            "rm 的具体文件应被识别，实得 {:?}",
            paths
        );

        let _ = std::fs::remove_file(&tmp);
    }

    /// 体积上限常量合理（不能太大，否则又可能撑爆盘）。
    // 这是**故意的常量范围守卫**：断言对象是编译期常量，
    // 目的是常量被改坏时立刻失败。
    // clippy::assertions_on_constants 针对的是恒真/恒假的废断言，
    // 不适用于守卫，故显式放行。
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn backup_size_cap_is_sane() {
        assert!(
            crate::agent::MAX_BACKUP_TARGET_MB <= 500,
            "单目标备份上限不该超过 500MB"
        );
        assert!(crate::agent::MAX_BACKUP_TARGET_MB >= 10, "也不该小到没意义");
    }
}
