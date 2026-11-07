//! yq-memory —— yunqiagent 分层记忆
//!
//! 四层结构（SPEC 第八节）：
//!   · 身份记忆  kind='user'           用户偏好、风格、项目背景
//!   · 工作记忆  kind='env'            环境事实、活跃项目状态
//!   · 程序记忆  kind='skill'          可复用流程
//!   · 归档记忆  kind='session_summary' 会话摘要 + 原始对话
//!   · 附加       kind='heartbeat'     心跳产生的记录
//!
//! 阶段一提供：写入、按 kind/scope 过滤查询。检索（RRF 融合）见 `search` 模块。

pub mod search;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// 记忆类型。序列化为数据库中使用的小写字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// 身份记忆
    User,
    /// 工作记忆
    Env,
    /// 程序记忆
    Skill,
    /// 会话摘要
    SessionSummary,
    /// 心跳记录
    Heartbeat,
}

impl MemoryKind {
    /// 转为数据库存储的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::User => "user",
            MemoryKind::Env => "env",
            MemoryKind::Skill => "skill",
            MemoryKind::SessionSummary => "session_summary",
            MemoryKind::Heartbeat => "heartbeat",
        }
    }

    /// 从数据库字符串解析。
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "user" => Some(MemoryKind::User),
            "env" => Some(MemoryKind::Env),
            "skill" => Some(MemoryKind::Skill),
            "session_summary" => Some(MemoryKind::SessionSummary),
            "heartbeat" => Some(MemoryKind::Heartbeat),
            _ => None,
        }
    }
}

impl std::fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条记忆记录（不含 embedding —— 阶段一不写向量）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: i64,
    pub kind: String,
    pub scope: Option<String>,
    pub content: String,
    pub importance: f32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 记忆仓储统一错误。
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),

    #[error("非法 kind: {0}（允许: user/env/skill/session_summary/heartbeat）")]
    InvalidKind(String),
}

/// 分层记忆仓储。持有从 yq-db 取得的连接池。
#[derive(Clone)]
pub struct Memories {
    pool: PgPool,
}

impl Memories {
    /// 使用给定连接池创建仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 引用底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Retrieve memories relevant to the current task rather than injecting
    /// the newest records indiscriminately.
    pub async fn retrieve(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<search::SearchHit>, MemoryError> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        search::Searcher::new(self.pool.clone())
            .search(query, None, limit)
            .await
    }

    /// 写入一条记忆。
    ///
    /// `scope` 为 `None` 表示全局。`embedding` 阶段一留空。
    pub async fn insert(
        &self,
        kind: MemoryKind,
        scope: Option<&str>,
        content: &str,
        importance: f32,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO memories (kind, scope, content, importance) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(kind.as_str())
        .bind(scope)
        .bind(content)
        .bind(importance)
        .fetch_one(&self.pool)
        .await?;

        tracing::debug!("写入记忆 kind={} id={}", kind, row.0);
        Ok(row.0)
    }

    /// 按 id 更新记忆正文（供 `memory` 工具做 replace）。
    ///
    /// 返回受影响行数：0 表示 id 不存在。
    pub async fn update_content(&self, id: i64, content: &str) -> Result<u64, MemoryError> {
        let res = sqlx::query("UPDATE memories SET content = $1, updated_at = now() WHERE id = $2")
            .bind(content)
            .bind(id)
            .execute(&self.pool)
            .await?;

        tracing::debug!("更新记忆 id={} 影响 {} 行", id, res.rows_affected());
        Ok(res.rows_affected())
    }

    /// 按 id 删除记忆（供 `memory` 工具做 remove）。
    pub async fn delete(&self, id: i64) -> Result<u64, MemoryError> {
        let res = sqlx::query("DELETE FROM memories WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;

        tracing::debug!("删除记忆 id={} 影响 {} 行", id, res.rows_affected());
        Ok(res.rows_affected())
    }

    /// 按 kind 过滤列出记忆（按更新时间倒序）。
    pub async fn list_by_kind(
        &self,
        kind: MemoryKind,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoryError> {
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                String,
                Option<String>,
                String,
                f32,
                DateTime<Utc>,
                DateTime<Utc>,
            ),
        >(
            "SELECT id, kind, scope, content, importance, created_at, updated_at \
             FROM memories WHERE kind = $1 ORDER BY updated_at DESC LIMIT $2",
        )
        .bind(kind.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Memory {
                id: r.0,
                kind: r.1,
                scope: r.2,
                content: r.3,
                importance: r.4,
                created_at: r.5,
                updated_at: r.6,
            })
            .collect())
    }

    /// 列出全部记忆（按更新时间倒序）。
    pub async fn list_all(&self, limit: i64) -> Result<Vec<Memory>, MemoryError> {
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                String,
                Option<String>,
                String,
                f32,
                DateTime<Utc>,
                DateTime<Utc>,
            ),
        >(
            "SELECT id, kind, scope, content, importance, created_at, updated_at \
             FROM memories ORDER BY updated_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Memory {
                id: r.0,
                kind: r.1,
                scope: r.2,
                content: r.3,
                importance: r.4,
                created_at: r.5,
                updated_at: r.6,
            })
            .collect())
    }

    /// 统计记忆总条数（REPL 中展示用）。
    pub async fn count(&self) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM memories")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// 解析用户输入的 kind 字符串。
    pub fn parse_kind(s: &str) -> Result<MemoryKind, MemoryError> {
        MemoryKind::from_str_opt(s).ok_or_else(|| MemoryError::InvalidKind(s.to_string()))
    }
}
