//! 能力校验 —— 插件权限声明的**唯一**执行点
//!
//! 设计原则（SPEC 第十一节）：**宿主端做能力校验，超出声明范围拒绝**。
//!
//! 校验三个维度：
//!   1. **文件路径**：请求路径必须落在声明的路径前缀白名单内（防目录穿越）
//!   2. **网络域名**：请求域名必须在白名单内（也支持通配子域 `*.example.com`）
//!   3. **命令**：命令首词必须在白名单内
//!
//! 所有校验都是**纯函数式**的 —— 不碰文件系统、不发网络请求，因此完全可单测。

use std::path::{Component, Path, PathBuf};

use crate::manifest::PermissionDecl;

/// 一次能力请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// 访问文件路径
    FilePath(String),
    /// 访问网络域名
    NetworkDomain(String),
    /// 执行命令
    Command(String),
}

/// 能力声明集合（由清单编译而来，校验用）。
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    /// 允许的文件路径前缀（已规范化）
    file_paths: Vec<PathBuf>,
    /// 允许的网络域名
    network_domains: Vec<String>,
    /// 允许的命令
    commands: Vec<String>,
}

impl CapabilitySet {
    /// 由清单权限声明构建。
    pub fn from_decl(decl: &PermissionDecl) -> Self {
        Self {
            file_paths: decl
                .file_paths
                .iter()
                .map(|p| normalize_path(Path::new(p)))
                .collect(),
            network_domains: decl.network_domains.clone(),
            commands: decl.commands.clone(),
        }
    }

    /// 是否声明了任何能力。
    pub fn is_empty(&self) -> bool {
        self.file_paths.is_empty() && self.network_domains.is_empty() && self.commands.is_empty()
    }

    /// 校验一个能力请求。
    ///
    /// 返回 `Ok(())` 表示**在声明范围内**；否则给出具体拒绝原因。
    pub fn check(&self, cap: &Capability) -> Result<(), CapabilityError> {
        match cap {
            Capability::FilePath(p) => self.check_path(p),
            Capability::NetworkDomain(d) => self.check_domain(d),
            Capability::Command(c) => self.check_command(c),
        }
    }

    /// 路径校验：必须落在某个声明前缀之下。
    ///
    /// 同时防御目录穿越：`/allowed/../../etc/passwd` 规范化后为 `/etc/passwd`，
    /// 不再落在 `/allowed` 之下 → 拒绝。
    fn check_path(&self, requested: &str) -> Result<(), CapabilityError> {
        let req = normalize_path(Path::new(requested));

        for allowed in &self.file_paths {
            if req.starts_with(allowed) {
                return Ok(());
            }
        }

        Err(CapabilityError::Denied {
            capability: format!("文件路径 {}", requested),
            reason: "不在声明的 file_paths 白名单内（或含目录穿越）".to_string(),
        })
    }

    /// 域名校验：精确匹配或通配子域匹配。
    fn check_domain(&self, requested: &str) -> Result<(), CapabilityError> {
        let req = requested.trim().to_ascii_lowercase();

        for pattern in &self.network_domains {
            let pat = pattern.trim().to_ascii_lowercase();
            if let Some(suffix) = pat.strip_prefix("*.") {
                // 通配：要求以 .suffix 结尾，且不是 suffix 裸域本身
                if req.ends_with(&format!(".{}", suffix)) {
                    return Ok(());
                }
            } else if req == pat {
                return Ok(());
            }
        }

        Err(CapabilityError::Denied {
            capability: format!("网络域名 {}", requested),
            reason: "不在声明的 network_domains 白名单内".to_string(),
        })
    }

    /// 命令校验：首词（可执行名）必须命中白名单。
    fn check_command(&self, requested: &str) -> Result<(), CapabilityError> {
        // split_whitespace 本身就会跳过首尾空白并按任意空白串切分，
        // 前置 trim() 是多余的中间遍历。
        let first = requested
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();

        // 允许"仅可执行名"或"含路径的完整名"两种写法命中
        let base = Path::new(&first)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| first.clone());

        if self.commands.iter().any(|c| c == &first || c == &base) {
            return Ok(());
        }

        Err(CapabilityError::Denied {
            capability: format!("命令 {}", requested),
            reason: format!("可执行文件 `{}` 不在声明的 commands 白名单内", base),
        })
    }
}

/// 能力校验错误。
#[derive(Debug, Clone, thiserror::Error)]
pub enum CapabilityError {
    /// 越权：声明外请求
    #[error("越权拒绝：{capability} —— {reason}")]
    Denied {
        /// 被拒的能力描述
        capability: String,
        /// 拒绝原因
        reason: String,
    },
}

/// 规范化路径：解析 `.` / `..`，去掉多余分隔符（不访问文件系统）。
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // 只在有可弹出元素时弹出，防止越过根
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_set() -> CapabilitySet {
        CapabilitySet::from_decl(&PermissionDecl {
            file_paths: vec!["/tmp/yq-plugin".to_string()],
            network_domains: vec!["api.example.com".to_string(), "*.trusted.org".to_string()],
            commands: vec!["python3".to_string(), "echo".to_string()],
        })
    }

    #[test]
    fn allows_path_inside_declared_prefix() {
        let set = sample_set();
        assert!(
            set.check(&Capability::FilePath(
                "/tmp/yq-plugin/data.json".to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn denies_path_outside_prefix() {
        let set = sample_set();
        assert!(
            set.check(&Capability::FilePath("/etc/passwd".to_string()))
                .is_err()
        );
    }

    #[test]
    fn denies_directory_traversal() {
        let set = sample_set();
        // /tmp/yq-plugin/../../etc/passwd → /etc/passwd → 拒绝
        assert!(
            set.check(&Capability::FilePath(
                "/tmp/yq-plugin/../../etc/passwd".to_string()
            ))
            .is_err()
        );
    }

    #[test]
    fn allows_exact_domain() {
        let set = sample_set();
        assert!(
            set.check(&Capability::NetworkDomain("api.example.com".to_string()))
                .is_ok()
        );
    }

    #[test]
    fn denies_unlisted_domain() {
        let set = sample_set();
        assert!(
            set.check(&Capability::NetworkDomain("evil.com".to_string()))
                .is_err()
        );
    }

    #[test]
    fn wildcard_domain_matches_subdomain_but_not_bare() {
        let set = sample_set();
        assert!(
            set.check(&Capability::NetworkDomain("a.trusted.org".to_string()))
                .is_ok()
        );
        assert!(
            set.check(&Capability::NetworkDomain("trusted.org".to_string()))
                .is_err()
        );
    }

    #[test]
    fn allows_whitelisted_command() {
        let set = sample_set();
        assert!(
            set.check(&Capability::Command("python3 plugin.py".to_string()))
                .is_ok()
        );
        assert!(
            set.check(&Capability::Command(
                "/usr/bin/python3 plugin.py".to_string()
            ))
            .is_ok()
        );
    }

    #[test]
    fn denies_non_whitelisted_command() {
        let set = sample_set();
        assert!(
            set.check(&Capability::Command("rm -rf /".to_string()))
                .is_err()
        );
    }

    #[test]
    fn empty_set_denies_everything() {
        let set = CapabilitySet::default();
        assert!(set.is_empty());
        assert!(
            set.check(&Capability::FilePath("/tmp".to_string()))
                .is_err()
        );
        assert!(
            set.check(&Capability::NetworkDomain("a.com".to_string()))
                .is_err()
        );
        assert!(set.check(&Capability::Command("ls".to_string())).is_err());
    }

    #[test]
    fn normalize_collapses_dots() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(normalize_path(Path::new("/a/./b")), PathBuf::from("/a/b"));
    }
}
