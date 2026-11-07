//! 系统健康监控 —— 长跑服务的地基
//!
//! 参考 Hermes 的 `gateway/memory_monitor.py` 与 `gateway/restart_loop_guard.py`。
//!
//! ## 为什么需要这两个
//!
//! **1. 内存监控**（[`MemoryMonitor`]）
//!
//! 长跑进程会随缓存累积而慢慢吃内存（agent 实例、会话记录、工具 schema、
//! 向量缓存……）。单条日志看不出泄漏，**只有把 RSS 观测成时间序列才能发现**。
//! 本模块定时打一条 grep 友好的 `[MEMORY]` 行，便于事后画曲线。
//!
//! **2. 启动循环守卫**（[`RestartGuard`]）
//!
//! systemd `Restart=always` 遇上"启动即崩"的逻辑会变成**每几秒重启一次的死循环**，
//! 而且每轮都会重跑致崩的那段代码，人还没介入机器已经空转半天。
//! 本模块持久化记录每次启动的时间戳，检测到短窗口内启动过于频繁时
//! **判定为崩溃循环**，让调用方跳过「会致崩的恢复动作」，从而打破循环。
//!
//! 两者都遵循**fail-open** 原则：自身出错绝不影响主服务。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 内存快照。
#[derive(Debug, Clone, Default)]
pub struct MemorySnapshot {
    /// 常驻内存（MB）
    pub rss_mb: f64,
    /// 虚拟内存（MB）
    pub vsz_mb: f64,
}

impl MemorySnapshot {
    /// 采集当前进程的内存占用。
    ///
    /// 优先读 `/proc/self/statm`（Linux，无依赖）；失败则返回默认值
    /// （**绝不因为采集失败而影响主服务**）。
    pub fn capture() -> Self {
        #[cfg(target_os = "linux")]
        {
            if let Ok(text) = std::fs::read_to_string("/proc/self/statm") {
                let nums: Vec<u64> = text
                    .split_whitespace()
                    .filter_map(|t| t.parse::<u64>().ok())
                    .collect();
                // statm 前两项：size（虚拟）、resident（常驻），单位是页
                if nums.len() >= 2 {
                    let page_kb = 4.0; // aarch64/x86 常见 4KB 页
                    return Self {
                        vsz_mb: (nums[0] as f64 * page_kb) / 1024.0,
                        rss_mb: (nums[1] as f64 * page_kb) / 1024.0,
                    };
                }
            }
        }
        Self::default()
    }

    /// 渲染成 grep 友好的单行日志。
    pub fn log_line(&self, tag: &str) -> String {
        format!(
            "[MEMORY] {} rss_mb={:.1} vsz_mb={:.1}",
            tag, self.rss_mb, self.vsz_mb
        )
    }
}

/// 内存监控配置。
#[derive(Debug, Clone)]
pub struct MemoryMonitorConfig {
    /// 采样间隔
    pub interval: Duration,
    /// 单次采样占用高于此值（MB）时额外发 WARN
    pub warn_threshold_mb: f64,
}

impl Default for MemoryMonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300), // 5 分钟
            warn_threshold_mb: 100.0,
        }
    }
}

/// 启动内存监控（后台任务，随进程退出自动结束）。
///
/// 启动时先打一条基线，之后按间隔采样；
/// 超过阈值时打 WARN（便于在日志里直接搜到异常点）。
pub fn spawn_memory_monitor(config: MemoryMonitorConfig) {
    // 基线快照：任何时候 grep 日志都能看到"启动时多少"
    let baseline = MemorySnapshot::capture();
    tracing::info!("{}", baseline.log_line("baseline"));

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.interval);
        // 第一次 tick 会立刻触发，跳过它（基线已经打过了）
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let snap = MemorySnapshot::capture();

            if snap.rss_mb >= config.warn_threshold_mb {
                tracing::warn!(
                    "{}（超过告警阈值 {:.0}MB，留意是否泄漏）",
                    snap.log_line("high"),
                    config.warn_threshold_mb
                );
            } else {
                tracing::info!("{}", snap.log_line("sample"));
            }
        }
    });
}

// ═══════════════════════════════════════════════════════════════
//  启动循环守卫
// ═══════════════════════════════════════════════════════════════

/// 启动守卫配置。
#[derive(Debug, Clone)]
pub struct RestartGuardConfig {
    /// 判定窗口
    pub window: Duration,
    /// 窗口内允许的最大启动次数（超过即判定为循环）
    pub max_boots_in_window: usize,
    /// 历史保留的最大条数（防止文件无限增长）
    pub max_history: usize,
}

impl Default for RestartGuardConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(300), // 5 分钟
            max_boots_in_window: 5,           // 5 分钟内启动 5 次以上 → 循环
            max_history: 50,
        }
    }
}

/// 启动判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootVerdict {
    /// 正常启动
    Normal,
    /// 检测到崩溃循环
    CrashLoop {
        /// 窗口内的启动次数
        boots: usize,
    },
}

impl BootVerdict {
    /// 是否判定为崩溃循环。
    pub fn is_loop(&self) -> bool {
        matches!(self, BootVerdict::CrashLoop { .. })
    }

    /// 给人看的说明。
    pub fn describe(&self) -> String {
        match self {
            BootVerdict::Normal => "启动正常".to_string(),
            BootVerdict::CrashLoop { boots } => format!(
                "检测到崩溃循环：{} 秒内已启动 {} 次。\
                 已跳过可能致崩的恢复动作以打破循环，请检查服务日志排查根因。",
                boots, boots
            ),
        }
    }
}

/// 启动循环守卫。
pub struct RestartGuard {
    path: PathBuf,
    config: RestartGuardConfig,
}

impl RestartGuard {
    /// 创建（`path` 通常为 `~/.yunqiagent/restart_history.json`）。
    pub fn new(path: impl Into<PathBuf>, config: RestartGuardConfig) -> Self {
        Self {
            path: path.into(),
            config,
        }
    }

    /// 登记本次启动并判定是否处于崩溃循环。
    ///
    /// **fail-open**：历史文件读写失败时一律返回 `Normal`
    /// —— 一个坏掉的守卫绝不能拦住健康的服务。
    pub fn record_boot(&self) -> BootVerdict {
        let now = unix_now();
        let mut history = self.load_history();

        history.push(now);

        // 只保留窗口内的 + 限制总长度
        let cutoff = now.saturating_sub(self.config.window.as_secs());
        history.retain(|t| *t >= cutoff);
        if history.len() > self.config.max_history {
            let keep_from = history.len() - self.config.max_history;
            history.drain(..keep_from);
        }

        let boots = history.len();
        self.save_history(&history);

        if boots > self.config.max_boots_in_window {
            BootVerdict::CrashLoop { boots }
        } else {
            BootVerdict::Normal
        }
    }

    /// 读取历史（失败即当空）。
    fn load_history(&self) -> Vec<u64> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        // 简单格式：每行一个 unix 时间戳（避免引入 JSON 依赖面）
        text.lines()
            .filter_map(|l| l.trim().parse::<u64>().ok())
            .collect()
    }

    /// 写入历史（失败静默 —— 守卫自身不该影响主流程）。
    fn save_history(&self, history: &[u64]) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body: String = history
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(e) = std::fs::write(&self.path, body) {
            tracing::debug!("启动历史写入失败（不影响服务）: {}", e);
        }
    }

    /// 历史文件路径。
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 当前 unix 秒。
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_history(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yq_sysmon_test_{}_{}.txt", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn memory_snapshot_capture_does_not_panic() {
        let snap = MemorySnapshot::capture();
        // Linux 上应能读到非零值；其它平台允许为零
        #[cfg(target_os = "linux")]
        assert!(snap.rss_mb > 0.0, "Linux 上应能读到 RSS");
        let _ = snap.log_line("test");
    }

    #[test]
    fn memory_log_line_is_greppable() {
        let snap = MemorySnapshot {
            rss_mb: 12.5,
            vsz_mb: 100.0,
        };
        let line = snap.log_line("baseline");
        assert!(line.starts_with("[MEMORY]"), "应便于 grep: {}", line);
        assert!(line.contains("rss_mb=12.5"));
        assert!(line.contains("baseline"));
    }

    #[test]
    fn first_boot_is_normal() {
        let path = tmp_history("first");
        let guard = RestartGuard::new(&path, RestartGuardConfig::default());
        assert_eq!(guard.record_boot(), BootVerdict::Normal);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn repeated_boots_trip_the_loop_breaker() {
        let path = tmp_history("loop");
        let cfg = RestartGuardConfig {
            window: Duration::from_secs(300),
            max_boots_in_window: 3,
            max_history: 50,
        };
        let guard = RestartGuard::new(&path, cfg);

        // 前 3 次正常
        for _ in 0..3 {
            assert_eq!(guard.record_boot(), BootVerdict::Normal);
        }
        // 第 4 次触发（boots=4 > max=3）
        let v = guard.record_boot();
        assert!(v.is_loop(), "应判定为崩溃循环，实得 {:?}", v);
        if let BootVerdict::CrashLoop { boots } = v {
            assert_eq!(boots, 4);
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loop_verdict_describes_itself() {
        let v = BootVerdict::CrashLoop { boots: 7 };
        let text = v.describe();
        assert!(text.contains("崩溃循环"));
        assert!(text.contains('7'));
        assert!(!text.contains('|'), "不应含 Markdown 表格符");

        assert_eq!(BootVerdict::Normal.describe(), "启动正常");
    }

    #[test]
    fn stale_history_outside_window_is_ignored() {
        let path = tmp_history("stale");
        // 手工写入 10 条"很久以前"的记录（窗口外）
        let old = unix_now().saturating_sub(10_000);
        let body = (0..10)
            .map(|_| old.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();

        let guard = RestartGuard::new(
            &path,
            RestartGuardConfig {
                window: Duration::from_secs(300),
                max_boots_in_window: 3,
                max_history: 50,
            },
        );

        // 窗口外的旧记录应被清掉，本次启动只算 1 次
        assert_eq!(guard.record_boot(), BootVerdict::Normal);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_history_fails_open() {
        let path = tmp_history("corrupt");
        std::fs::write(&path, "这不是数字\n乱码!!!\n").unwrap();
        let guard = RestartGuard::new(&path, RestartGuardConfig::default());
        // 解析不出任何历史 → 只算本次 → 正常
        assert_eq!(guard.record_boot(), BootVerdict::Normal);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unwritable_path_fails_open() {
        // 指向一个不可能写入的路径
        let guard = RestartGuard::new(
            PathBuf::from("/proc/definitely/not/writable/history.txt"),
            RestartGuardConfig::default(),
        );
        // 关键：读不到历史 + 写不进去，也必须返回 Normal，不能 panic
        assert_eq!(guard.record_boot(), BootVerdict::Normal);
    }

    #[test]
    fn history_is_bounded() {
        let path = tmp_history("bounded");
        let guard = RestartGuard::new(
            &path,
            RestartGuardConfig {
                window: Duration::from_secs(86_400), // 大窗口，不清旧记录
                max_boots_in_window: 1000,
                max_history: 5,
            },
        );
        for _ in 0..20 {
            guard.record_boot();
        }
        let body = std::fs::read_to_string(&path).unwrap();
        let n = body.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(n <= 5, "历史应被限制在 5 条，实得 {}", n);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guard_path_is_exposed() {
        let p = tmp_history("expose");
        let guard = RestartGuard::new(&p, RestartGuardConfig::default());
        assert_eq!(guard.path(), p.as_path());
    }
}
