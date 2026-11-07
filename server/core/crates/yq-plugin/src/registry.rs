//! 插件注册表 —— 加载 / 卸载 / 热插拔
//!
//! SPEC 第十一节「热插拔」要求：
//!   · 插件目录 `plugins/`
//!   · 支持运行时加载/卸载
//!   · **卸载时完整撤销副作用**
//!   · 插件变化时**重新解析依赖，不重启 agent**
//!
//! 本模块维护插件生命周期状态机，并**在卸载时确保资源回收**
//! （进程关停 / wasm 实例丢弃），不留悬挂副作用。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::capability::CapabilitySet;
use crate::manifest::PluginManifest;

/// 插件生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// 已发现，尚未加载
    Discovered,
    /// 已加载（进程/实例就绪）
    Loaded,
    /// 加载失败
    Failed,
}

impl PluginState {
    /// 中文描述（便于日志与界面展示）。
    pub fn label(self) -> &'static str {
        match self {
            PluginState::Discovered => "已发现",
            PluginState::Loaded => "已加载",
            PluginState::Failed => "加载失败",
        }
    }
}

/// 注册表中的一个插件条目。
#[derive(Debug, Clone)]
pub struct PluginEntry {
    /// 清单
    pub manifest: PluginManifest,
    /// 清单所在目录
    pub dir: PathBuf,
    /// 当前状态
    pub state: PluginState,
    /// 失败原因（state=Failed 时）
    pub error: Option<String>,
}

impl PluginEntry {
    /// 该插件的权限集合。
    pub fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::from_decl(&self.manifest.permissions)
    }

    /// 插件暴露的工具名列表。
    pub fn tool_names(&self) -> Vec<&str> {
        self.manifest
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect()
    }
}

/// 插件注册表。
///
/// 内部用 `BTreeMap`（**保序**）而非 `HashMap` —— 序列化与遍历顺序稳定，
/// 与项目「禁无序集合」纪律一致，也让"重新解析依赖"的结果可复现。
pub struct PluginRegistry {
    /// 插件目录
    dir: PathBuf,
    /// 插件名 → 条目
    entries: BTreeMap<String, PluginEntry>,
}

impl PluginRegistry {
    /// 创建注册表（不扫描）。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            entries: BTreeMap::new(),
        }
    }

    /// 插件目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 扫描插件目录，发现所有 `*/plugin.toml`。
    ///
    /// 对应「插件变化时重新解析依赖」——可随时重扫，无需重启 agent。
    /// 返回本次新发现的数量。
    pub fn scan(&mut self) -> usize {
        let mut found = 0usize;

        let Ok(read_dir) = std::fs::read_dir(&self.dir) else {
            tracing::debug!("插件目录不存在或不可读: {}", self.dir.display());
            return 0;
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.toml");
            if !manifest_path.exists() {
                continue;
            }

            match PluginManifest::from_file(&manifest_path) {
                Ok(manifest) => {
                    let name = manifest.name.clone();
                    if self.entries.contains_key(&name) {
                        continue; // 已发现过
                    }
                    tracing::info!(
                        "发现插件: {} v{}（{}）",
                        name,
                        manifest.version,
                        manifest.kind
                    );
                    self.entries.insert(
                        name,
                        PluginEntry {
                            manifest,
                            dir: path,
                            state: PluginState::Discovered,
                            error: None,
                        },
                    );
                    found += 1;
                }
                Err(e) => {
                    tracing::warn!("插件清单解析失败（{}）: {}", manifest_path.display(), e);
                }
            }
        }

        found
    }

    /// 标记插件为已加载。
    pub fn mark_loaded(&mut self, name: &str) -> bool {
        match self.entries.get_mut(name) {
            Some(entry) => {
                entry.state = PluginState::Loaded;
                entry.error = None;
                true
            }
            None => false,
        }
    }

    /// 标记插件加载失败。
    pub fn mark_failed(&mut self, name: &str, error: impl Into<String>) -> bool {
        match self.entries.get_mut(name) {
            Some(entry) => {
                entry.state = PluginState::Failed;
                entry.error = Some(error.into());
                true
            }
            None => false,
        }
    }

    /// 卸载插件：从注册表移除。
    ///
    /// **调用方责任**：在调用本方法前，必须先完成实际资源回收
    /// （`ProcessPlugin::shutdown()` 等），以确保「卸载时完整撤销副作用」。
    /// 本方法返回被移除的条目，便于调用方核对。
    pub fn unload(&mut self, name: &str) -> Option<PluginEntry> {
        let removed = self.entries.remove(name);
        if removed.is_some() {
            tracing::info!("已从注册表卸载插件: {}", name);
        }
        removed
    }

    /// 列出全部插件条目（按名字保序）。
    pub fn entries(&self) -> Vec<&PluginEntry> {
        self.entries.values().collect()
    }

    /// 按名查询。
    pub fn get(&self, name: &str) -> Option<&PluginEntry> {
        self.entries.get(name)
    }

    /// 已加载插件数量。
    pub fn loaded_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.state == PluginState::Loaded)
            .count()
    }

    /// 插件总数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 汇总所有已加载插件暴露的工具（供动态区注入）。
    ///
    /// **注意**：插件工具**不进 KV-cache 固定区**（数量随插件变化），
    /// 只作为动态区内容注入，保证固定区前缀字节稳定。
    pub fn all_tool_names(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for entry in self.entries.values() {
            if entry.state != PluginState::Loaded {
                continue;
            }
            for tool in &entry.manifest.tools {
                out.push((entry.manifest.name.clone(), tool.name.clone()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yq_plugin_registry_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_plugin(dir: &Path, name: &str, extra: &str) {
        let pdir = dir.join(name);
        fs::create_dir_all(&pdir).unwrap();
        let content = format!(
            "name = \"{}\"\nversion = \"1.0.0\"\nkind = \"process\"\ncommand = [\"echo\"]\n{}",
            name, extra
        );
        fs::write(pdir.join("plugin.toml"), content).unwrap();
    }

    #[test]
    fn scan_discovers_plugins() {
        let dir = tmp_dir("scan");
        write_plugin(&dir, "alpha", "");
        write_plugin(&dir, "beta", "");

        let mut reg = PluginRegistry::new(&dir);
        let found = reg.scan();
        assert_eq!(found, 2);
        assert_eq!(reg.len(), 2);

        // 重复扫描不重复计数
        assert_eq!(reg.scan(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_skips_malformed_manifest() {
        let dir = tmp_dir("bad");
        write_plugin(&dir, "good", "");
        let bad_dir = dir.join("bad_plugin");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("plugin.toml"), "name = \"BadName\"\n").unwrap();

        let mut reg = PluginRegistry::new(&dir);
        let found = reg.scan();
        assert_eq!(found, 1, "非法清单应被跳过");
        assert!(reg.get("good").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unload_removes_entry() {
        let dir = tmp_dir("unload");
        write_plugin(&dir, "temp_plugin", "");

        let mut reg = PluginRegistry::new(&dir);
        reg.scan();
        assert!(reg.get("temp_plugin").is_some());

        let removed = reg.unload("temp_plugin");
        assert!(removed.is_some());
        assert!(reg.get("temp_plugin").is_none());
        assert!(reg.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_loaded_plugins_expose_tools() {
        let dir = tmp_dir("tools");
        write_plugin(
            &dir,
            "with_tool",
            "\n[[tools]]\nname = \"do_thing\"\ndescription = \"干点事\"\n",
        );

        let mut reg = PluginRegistry::new(&dir);
        reg.scan();

        // 未加载 → 不暴露工具
        assert!(reg.all_tool_names().is_empty());

        reg.mark_loaded("with_tool");
        let tools = reg.all_tool_names();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].1, "do_thing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_state_is_recorded() {
        let dir = tmp_dir("failed");
        write_plugin(&dir, "broken", "");
        let mut reg = PluginRegistry::new(&dir);
        reg.scan();
        reg.mark_failed("broken", "进程启动失败");

        let entry = reg.get("broken").unwrap();
        assert_eq!(entry.state, PluginState::Failed);
        assert_eq!(entry.error.as_deref(), Some("进程启动失败"));
        assert_eq!(reg.loaded_count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
