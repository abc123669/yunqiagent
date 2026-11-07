-- ═══════════════════════════════════════════════════════════════
--  002_delivery_ledger.sql —— 消息投递台账
--
--  解决的问题（参考 Hermes gateway/delivery_ledger.py）：
--    一条**已生成但尚未确认送达**的回复，是系统唯一会「无痕丢失」的产物——
--    tokens 已经烧掉，文本只存在于进程内存里，一旦在 finalize 与平台 ACK
--    之间发生崩溃/重启，它就彻底消失了，谁也不知道曾有过这么一条。
--
--  做法：发送前后写检查点，重启时扫出「孤儿」记录重新投递。
--
--  状态机：
--    pending     发送尚未开始   → 重投**不带**标记（绝无重复风险）
--    attempting  崩在 await 中  → 平台**可能已收到**，重投**带可见标记**
--                                （诚实做到 at-least-once，绝不静默重复）
--    delivered   已确认送达     → 无需处理，等待清理
--    failed      被明确拒绝一次 → 重投带标记（重启即天然重试边界）
--    abandoned   重试次数耗尽   → 保留片刻供排查，随后清理
--
--  owner_boot：本进程的启动时间戳。新进程启动时，owner_boot 不等于自己的
--  记录即为「孤儿」（上一个进程没走到 delivered 就没了）。
--  这样无需查 /proc 即可判定「原进程已死」。
-- ═══════════════════════════════════════════════════════════════

CREATE TABLE IF NOT EXISTS delivery_ledger (
    id              BIGSERIAL PRIMARY KEY,
    -- 会话键（group:xxx / c2c:xxx），便于按对话排查
    chat_key        TEXT        NOT NULL,
    -- 场景：group / c2c / guild
    scene           TEXT        NOT NULL,
    -- 投递目标（群 openid 或用户 openid）
    target          TEXT        NOT NULL,
    -- 被动回复要锚定的消息 ID（可为空）
    reply_to_msg_id TEXT,
    -- 待投递的正文
    content         TEXT        NOT NULL,
    -- 状态机
    state           TEXT        NOT NULL DEFAULT 'pending',
    -- 已尝试投递次数（超过上限则 abandoned，防止毒记录自旋）
    attempts        INT         NOT NULL DEFAULT 0,
    -- 归属进程的启动时间戳（判孤儿用）
    owner_boot      BIGINT      NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 扫描孤儿时按状态 + 归属过滤
CREATE INDEX IF NOT EXISTS idx_delivery_ledger_state_owner
    ON delivery_ledger (state, owner_boot);

-- 清理过期记录用
CREATE INDEX IF NOT EXISTS idx_delivery_ledger_updated
    ON delivery_ledger (updated_at);
