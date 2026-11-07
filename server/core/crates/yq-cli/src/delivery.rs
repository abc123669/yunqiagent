//! 消息投递台账 —— 保证「生成好的回复」不会无痕丢失
//!
//! 参考 Hermes 的 `gateway/delivery_ledger.py`（那边引了 3 个真实 issue）。
//!
//! ## 问题
//!
//! 一条**已生成但未确认送达**的回复，是系统唯一会无痕丢失的产物：
//! tokens 已经烧掉、文本只存在于内存，若在「发送」与「平台确认」之间崩溃，
//! 它就凭空消失了——用户白等，日志也没痕迹。
//!
//! ## 做法
//!
//! 发送前后写检查点（落库），重启时扫「孤儿」记录重投：
//!
//! ```text
//! record_pending()   发送前             state='pending'
//! mark_attempting()  即将 await 发送     state='attempting'
//! mark_delivered()   平台确认成功        state='delivered'
//! mark_failed()      被明确拒绝          state='failed'
//! sweep_orphans()    启动时扫描          owner_boot != 自己 → 孤儿
//! ```
//!
//! ## 语义诚实性（at-least-once，绝不静默重复）
//!
//! · `pending` 重投 → **不带标记**（发送根本没开始，不可能重复）
//! · `attempting` / `failed` 重投 → **带可见标记**（平台可能已收到，
//!   必须让用户知道这条是补发的，而不是悄悄再发一遍）
//!
//! ## 毒记录防自旋
//!
//! 投递次数超过上限 → 转 `abandoned`（保留片刻供排查，之后清理）。
//! 台账本身**尽力而为**：任何台账故障都不得阻塞或延迟真正的发送。

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// 投递状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// 发送尚未开始（重投绝对安全）
    Pending,
    /// 崩在 await 中（平台可能已收到 → 重投需标记）
    Attempting,
    /// 已确认送达
    Delivered,
    /// 被明确拒绝一次
    Failed,
    /// 重试耗尽，放弃（保留供排查）
    Abandoned,
}

impl DeliveryState {
    /// 数据库中的字符串表示。
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryState::Pending => "pending",
            DeliveryState::Attempting => "attempting",
            DeliveryState::Delivered => "delivered",
            DeliveryState::Failed => "failed",
            DeliveryState::Abandoned => "abandoned",
        }
    }

    /// 由字符串解析（未知值视为 `Pending`，宁可重投也不丢）。
    pub fn from_str(s: &str) -> Self {
        match s {
            "attempting" => DeliveryState::Attempting,
            "delivered" => DeliveryState::Delivered,
            "failed" => DeliveryState::Failed,
            "abandoned" => DeliveryState::Abandoned,
            _ => DeliveryState::Pending,
        }
    }

    /// 重投时是否需要加「补发」标记。
    ///
    /// `attempting`/`failed` 意味着平台**可能**已经收到过，
    /// 必须让用户看出来这是补发，避免造成"发重了"的困惑。
    pub fn needs_recovery_marker(self) -> bool {
        matches!(self, DeliveryState::Attempting | DeliveryState::Failed)
    }
}

/// 一条投递记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryRecord {
    /// 行 ID
    pub id: i64,
    /// 会话键
    pub chat_key: String,
    /// 场景
    pub scene: String,
    /// 投递目标（群/用户 openid）
    pub target: String,
    /// 被动回复锚定的消息 ID
    pub reply_to_msg_id: Option<String>,
    /// 正文
    pub content: String,
    /// 状态
    pub state: DeliveryState,
    /// 已尝试次数
    pub attempts: i32,
    /// 归属进程的启动时间戳
    pub owner_boot: i64,
}

/// 单条记录的投递尝试上限（超过即放弃，防止毒记录无限自旋）。
pub const MAX_DELIVERY_ATTEMPTS: i32 = 3;

/// 台账错误（尽力而为，调用方应忽略而非中断发送）。
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// 数据库错误
    #[error("台账数据库错误: {0}")]
    Db(#[from] sqlx::Error),
}

/// 投递台账。
#[derive(Clone)]
pub struct DeliveryLedger {
    pool: PgPool,
    /// 本进程的启动时间戳（判孤儿的依据）
    owner_boot: i64,
}

impl DeliveryLedger {
    /// 创建台账（`owner_boot` 用当前时间戳）。
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            owner_boot: chrono::Utc::now().timestamp(),
        }
    }

    /// 当前归属标识。
    #[allow(dead_code)]
    pub fn owner_boot(&self) -> i64 {
        self.owner_boot
    }

    /// 登记一条「待发送」的回复。返回行 ID。
    pub async fn record_pending(
        &self,
        chat_key: &str,
        scene: &str,
        target: &str,
        reply_to_msg_id: Option<&str>,
        content: &str,
    ) -> Result<i64, LedgerError> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO delivery_ledger
               (chat_key, scene, target, reply_to_msg_id, content, state, owner_boot)
             VALUES ($1, $2, $3, $4, $5, 'pending', $6)
             RETURNING id",
        )
        .bind(chat_key)
        .bind(scene)
        .bind(target)
        .bind(reply_to_msg_id)
        .bind(content)
        .bind(self.owner_boot)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// 标记为「正在尝试发送」（即将 await）。
    pub async fn mark_attempting(&self, id: i64) -> Result<(), LedgerError> {
        sqlx::query(
            "UPDATE delivery_ledger
                SET state = 'attempting', attempts = attempts + 1, updated_at = now()
              WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 标记为「已送达」（只在平台确认成功时调用）。
    pub async fn mark_delivered(&self, id: i64) -> Result<(), LedgerError> {
        sqlx::query(
            "UPDATE delivery_ledger
                SET state = 'delivered', updated_at = now()
              WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 标记为「被拒绝」。
    pub async fn mark_failed(&self, id: i64) -> Result<(), LedgerError> {
        sqlx::query(
            "UPDATE delivery_ledger
                SET state = 'failed', updated_at = now()
              WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 扫描「孤儿」记录：不是本进程写的、且尚未送达的那些。
    ///
    /// 这些是上一个进程没走完的记录，需要在新进程里补救。
    pub async fn sweep_orphans(&self) -> Result<Vec<DeliveryRecord>, LedgerError> {
        let rows = sqlx::query_as::<_, (i64, String, String, String, Option<String>, String, String, i32, i64)>(
            "SELECT id, chat_key, scene, target, reply_to_msg_id, content, state, attempts, owner_boot
               FROM delivery_ledger
              WHERE owner_boot <> $1
                AND state IN ('pending', 'attempting', 'failed')
              ORDER BY id ASC
              LIMIT 50",
        )
        .bind(self.owner_boot)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| DeliveryRecord {
                id: r.0,
                chat_key: r.1,
                scene: r.2,
                target: r.3,
                reply_to_msg_id: r.4,
                content: r.5,
                state: DeliveryState::from_str(&r.6),
                attempts: r.7,
                owner_boot: r.8,
            })
            .collect())
    }

    /// 放弃一条（重试耗尽）。
    pub async fn mark_abandoned(&self, id: i64) -> Result<(), LedgerError> {
        sqlx::query(
            "UPDATE delivery_ledger
                SET state = 'abandoned', updated_at = now()
              WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 清理：已送达的旧记录 + 放弃很久的记录。
    ///
    /// 返回清理条数。
    pub async fn cleanup(&self, retain_hours: i64) -> Result<u64, LedgerError> {
        let res = sqlx::query(
            "DELETE FROM delivery_ledger
              WHERE (state = 'delivered' AND updated_at < now() - make_interval(hours => $1))
                 OR (state = 'abandoned' AND updated_at < now() - make_interval(hours => $1))",
        )
        .bind(retain_hours as i32)
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected())
    }

    /// 为「补发」的记录生成带可见标记的正文。
    ///
    /// `attempting`/`failed` 的重投必须让用户看出来是补发 ——
    /// 因为平台可能已经收到过，静默重复会造成"为什么发了两遍"的困惑。
    pub fn with_recovery_marker(record: &DeliveryRecord) -> String {
        if record.state.needs_recovery_marker() {
            format!("（补发 · 此前这条可能未送达成功）\n{}", record.content)
        } else {
            // pending：发送根本没开始过，不带标记
            record.content.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip() {
        for s in [
            DeliveryState::Pending,
            DeliveryState::Attempting,
            DeliveryState::Delivered,
            DeliveryState::Failed,
            DeliveryState::Abandoned,
        ] {
            assert_eq!(DeliveryState::from_str(s.as_str()), s);
        }
    }

    #[test]
    fn unknown_state_falls_back_to_pending() {
        // 宁可重投，也不当成"已送达"而丢掉
        assert_eq!(DeliveryState::from_str("wat"), DeliveryState::Pending);
        assert_eq!(DeliveryState::from_str(""), DeliveryState::Pending);
    }

    #[test]
    fn pending_needs_no_recovery_marker() {
        assert!(!DeliveryState::Pending.needs_recovery_marker());
    }

    #[test]
    fn attempting_and_failed_need_recovery_marker() {
        // 这两种状态下平台**可能**已收到，必须标记
        assert!(DeliveryState::Attempting.needs_recovery_marker());
        assert!(DeliveryState::Failed.needs_recovery_marker());
    }

    #[test]
    fn delivered_needs_no_marker() {
        assert!(!DeliveryState::Delivered.needs_recovery_marker());
    }

    #[test]
    fn recovery_marker_only_applied_when_needed() {
        let mut rec = DeliveryRecord {
            id: 1,
            chat_key: "c2c:u1".into(),
            scene: "c2c".into(),
            target: "u1".into(),
            reply_to_msg_id: None,
            content: "正文内容".into(),
            state: DeliveryState::Pending,
            attempts: 0,
            owner_boot: 0,
        };

        // pending → 原样
        let out = DeliveryLedger::with_recovery_marker(&rec);
        assert_eq!(out, "正文内容");
        assert!(!out.contains("补发"));

        // attempting → 带标记
        rec.state = DeliveryState::Attempting;
        let out = DeliveryLedger::with_recovery_marker(&rec);
        assert!(out.contains("补发"), "应标明是补发: {}", out);
        assert!(out.contains("正文内容"), "原文必须保留");
    }

    // 这是**故意的常量范围守卫**：断言对象是编译期常量，
    // 目的是常量被改坏时立刻失败。
    // clippy::assertions_on_constants 针对的是恒真/恒假的废断言，
    // 不适用于守卫，故显式放行。
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn max_attempts_is_positive() {
        assert!(MAX_DELIVERY_ATTEMPTS > 0);
    }
}
