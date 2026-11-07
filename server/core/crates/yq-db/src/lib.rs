//! yq-db —— yunqiagent 数据层
//!
//! 职责：
//!   · 构建 PostgreSQL 连接池（sqlx）
//!   · 运行数据库迁移（sqlx::migrate!，迁移文件内嵌进二进制）
//!   · 统一错误类型
//!   · 工具调用审计落库（audit 模块，写 `op_audit` 表）
//!
//! 规范：
//!   · 所有标识符一律 snake_case
//!   · 连接池上限与 SPEC 第五节的 max_connections=20 保持一致

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

pub mod audit;

/// yq-db 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// 连接池构建失败
    #[error("构建数据库连接池失败: {0}")]
    Pool(#[from] sqlx::Error),

    /// 迁移执行失败
    #[error("执行数据库迁移失败: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// 缺少 DATABASE_URL 环境变量
    #[error("未找到环境变量 DATABASE_URL，请检查 .env 或部署配置")]
    MissingDatabaseUrl,
}

/// 连接池默认最大连接数 —— 与 SPEC 第五节 `max_connections = 20` 对齐。
pub const DEFAULT_MAX_CONNECTIONS: u32 = 20;

/// 获取数据库连接串。
///
/// 优先读环境变量 `DATABASE_URL`；若进程工作目录存在 `.env` 则先加载。
pub fn database_url() -> Result<String, DbError> {
    // dotenvy 加载失败（无 .env）不视为错误
    let _ = dotenvy::dotenv();
    std::env::var("DATABASE_URL").map_err(|_| DbError::MissingDatabaseUrl)
}

/// 按给定连接串构建连接池。
///
/// 树莓派内存敏感，连接池保持小规模、短超时。
pub async fn connect(database_url: &str) -> Result<PgPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(DEFAULT_MAX_CONNECTIONS)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Some(Duration::from_secs(600)))
        .max_lifetime(Some(Duration::from_secs(1800)))
        .connect(database_url)
        .await?;

    tracing::info!(
        "数据库连接池已建立（max_connections={}）",
        DEFAULT_MAX_CONNECTIONS
    );
    Ok(pool)
}

/// 便捷入口：读环境变量并构建连接池。
pub async fn connect_from_env() -> Result<PgPool, DbError> {
    let url = database_url()?;
    connect(&url).await
}

/// 运行内嵌的数据库迁移。
///
/// 迁移文件位于 `crates/yq-db/migrations/`，随二进制编译内嵌，
/// 部署时无须携带 SQL 文件。
pub async fn run_migrations(pool: &PgPool) -> Result<(), DbError> {
    sqlx::migrate!("./migrations").run(pool).await?;
    tracing::info!("数据库迁移已执行完毕");
    Ok(())
}

/// 检查连通性（供启动自检与健康检查使用）。
pub async fn ping(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}
