//! 会话映射 —— 群 / 私聊会话键 → agent 会话 UUID。
//!
//! 同一 QQ 会话的多轮对话必须映射到同一个 agent 会话，才能共享历史与记忆。
//! 映射关系**持久化到磁盘**（JSON），进程重启后不丢上下文。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// 会话映射表。
///
/// 内部用 `BTreeMap`（**保序**）而非 `HashMap` —— 序列化结果稳定，
/// 便于人工检视与版本控制；且与 SPEC 的「禁无序集合」纪律一致。
pub struct SessionMap {
    /// 会话键 → 会话 UUID
    map: BTreeMap<String, Uuid>,
    /// 持久化路径
    path: PathBuf,
}

impl SessionMap {
    /// 打开（不存在则创建空表）。
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let map = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<BTreeMap<String, Uuid>>(&text).unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        };
        Self { map, path }
    }

    /// 取（或新建）某个会话键对应的会话 UUID。
    pub fn get_or_create(&mut self, key: &str) -> Uuid {
        if let Some(id) = self.map.get(key) {
            return *id;
        }
        let id = Uuid::new_v4();
        self.map.insert(key.to_string(), id);
        self.save();
        id
    }

    /// 仅查询，不新建。
    pub fn get(&self, key: &str) -> Option<Uuid> {
        self.map.get(key).copied()
    }

    /// 重置某个会话（下次说话开新会话）。
    pub fn reset(&mut self, key: &str) {
        self.map.remove(key);
        self.save();
    }

    /// 当前会话数量。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 落盘（原子写：先写临时文件再 rename）。
    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(text) = serde_json::to_string_pretty(&self.map) else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "yq_qq_sessionmap_test_{}_{}.json",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn same_key_returns_same_uuid() {
        let path = tmp_path("same");
        let mut map = SessionMap::open(&path);
        let a = map.get_or_create("group:931409187");
        let b = map.get_or_create("group:931409187");
        assert_eq!(a, b);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mapping_persists_across_reopen() {
        let path = tmp_path("persist");
        let id = {
            let mut map = SessionMap::open(&path);
            map.get_or_create("private:10001")
        };
        let reopened = SessionMap::open(&path);
        assert_eq!(reopened.get("private:10001"), Some(id));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reset_clears_mapping() {
        let path = tmp_path("reset");
        let mut map = SessionMap::open(&path);
        map.get_or_create("group:1");
        assert_eq!(map.len(), 1);
        map.reset("group:1");
        assert_eq!(map.len(), 0);
        let _ = std::fs::remove_file(&path);
    }
}
