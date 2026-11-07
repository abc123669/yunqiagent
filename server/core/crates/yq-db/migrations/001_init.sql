-- ═══════════════════════════════════════════════════════════════
--  yunqiagent 初始迁移 001_init.sql
--  依据 SPEC 第六节「数据库 Schema」
--
--  说明：
--   · pgvector 扩展由本迁移创建（版本 0.8.7，官方镜像自带）
--   · fts 生成列暂用 'simple' 配置 —— 官方 pgvector 镜像不含 zhparser，
--     阶段四将补装 zhparser 并改为 'chinese_zh'（见 TODO 注释）
--   · memories 按 created_at 月度分区，分区键必须包含在主键中
-- ═══════════════════════════════════════════════════════════════

CREATE EXTENSION IF NOT EXISTS vector;

-- ── 分层记忆统一表（按 created_at 月度分区）─────────────────────
CREATE TABLE memories (
    id           BIGSERIAL,
    kind         TEXT NOT NULL,        -- 'user' | 'env' | 'skill' | 'session_summary' | 'heartbeat'
    scope        TEXT,                 -- 全局为 NULL，否则项目名
    content      TEXT NOT NULL,
    embedding    vector(1024),
    importance   REAL DEFAULT 0.5,
    created_at   TIMESTAMPTZ DEFAULT now(),
    updated_at   TIMESTAMPTZ DEFAULT now(),
    expires_at   TIMESTAMPTZ,
    -- TODO(阶段四): 补装 zhparser 后改为 to_tsvector('chinese_zh', content)
    fts          tsvector GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED,
    meta         JSONB DEFAULT '{}',
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

-- 初始分区：2026-10
CREATE TABLE memories_2026_10 PARTITION OF memories
    FOR VALUES FROM ('2026-10-01') TO ('2026-11-01');

-- 兜底分区：2026-11（避免新月份数据无处可去）
CREATE TABLE memories_2026_11 PARTITION OF memories
    FOR VALUES FROM ('2026-11-01') TO ('2026-12-01');

-- 向量近邻索引（HNSW，余弦距离）
CREATE INDEX memories_embedding_hnsw_idx
    ON memories USING hnsw (embedding vector_cosine_ops);

-- 全文检索索引（GIN）
CREATE INDEX memories_fts_gin_idx ON memories USING gin (fts);

-- 常用过滤/排序索引
CREATE INDEX memories_kind_scope_updated_idx
    ON memories (kind, scope, updated_at DESC);

-- ── 原始会话归档 ────────────────────────────────────────────────
CREATE TABLE sessions (
    id          BIGSERIAL PRIMARY KEY,
    session_id  UUID NOT NULL,
    role        TEXT NOT NULL,        -- 'user' | 'assistant' | 'tool'
    content     TEXT,
    tool_calls  JSONB,
    created_at  TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX sessions_session_id_created_idx ON sessions (session_id, created_at);

-- ── 记忆冲突待确认 ──────────────────────────────────────────────
CREATE TABLE memory_conflicts (
    id             BIGSERIAL PRIMARY KEY,
    old_memory_id  BIGINT,
    new_content    TEXT NOT NULL,
    reason         TEXT,
    status         TEXT DEFAULT 'pending',
    created_at     TIMESTAMPTZ DEFAULT now()
);

-- ── 心跳执行日志 ────────────────────────────────────────────────
CREATE TABLE heartbeat_log (
    id         BIGSERIAL PRIMARY KEY,
    fired_at   TIMESTAMPTZ DEFAULT now(),
    status     TEXT NOT NULL,        -- 'ok' | 'alert' | 'error'
    summary    TEXT,
    delivered  BOOLEAN DEFAULT false
);

-- ── 危险操作审计 + 备份记录 ─────────────────────────────────────
CREATE TABLE op_audit (
    id           BIGSERIAL PRIMARY KEY,
    op_type      TEXT NOT NULL,      -- 'bash' | 'write' | 'edit'
    target       TEXT,
    backup_path  TEXT,
    backup_state TEXT DEFAULT 'pending',
    notified     BOOLEAN DEFAULT false,
    created_at   TIMESTAMPTZ DEFAULT now()
);
