-- ═══════════════════════════════════════════════════════════════
--  003_tool_audit.sql —— 工具调用审计（补全 op_audit 的「结果」维度）
--
--  解决的问题：
--    `op_audit` 表自 001 起就存在，但**从来没有一行记录** —— 五个工具
--    （read/write/edit/bash/delegate）的执行完全没有留痕。运维问
--    「这机器人上周二对我这台机器做了什么」，只能翻聊天记录去猜。
--    更麻烦的是：`bash` 直接 `bash -c` 任意执行、无沙箱、无白名单，
--    出了事没有任何事后追查的凭据。
--
--  做法：
--    在 Agent 工具循环里对**每一次**工具执行写一行（不是只记危险操作 ——
--    完整轨迹才有排查价值）。本迁移只补两件事：
--      1. 结果维度：`outcome`（成功/失败）+ `error_summary`（截断的错误摘要）
--      2. 审计查询索引：按时间倒查、按工具类型筛、按失败筛
--
--  为什么结果单独存列而不塞进 `target`：
--    `target` 存的是**参数**（命令串 / 路径 / 委派目标），
--    混进结果会让「这条操作做了什么」与「结果如何」无法分别检索。
--    此外 `target` 在写入侧已被**限长**（见 yq-core::ops::audit_target），
--    杜绝把几 MB 的命令输出灌进这张表。
--
--  兼容性说明（重要）：
--    本迁移**全部是增量操作**：只 ADD COLUMN（带 DEFAULT，PG11+ 对已有行
--    直接按默认值回填、不重写全表）与 CREATE INDEX IF NOT EXISTS。
--    没有任何 DROP / TRUNCATE / RENAME / 类型收紧，
--    因此对既有数据完全安全，也不影响 001 中该表的既有语义。
--    `op_type` 的原注释写着 `'bash' | 'write' | 'edit'`，
--    但本次会把 `read` / `delegate` 也写进来 ——
--    这属于**扩大**取值范围而非改变语义，原有三种取值仍然有效。
-- ═══════════════════════════════════════════════════════════════

-- 执行结果：'ok' = 工具正常返回；'error' = 工具报错 / 未知工具 / 被 /stop 打断。
-- 允许 NULL（表内既有的历史行没有这个信息），不做 NOT NULL 约束。
ALTER TABLE op_audit ADD COLUMN IF NOT EXISTS outcome TEXT;

-- 失败原因摘要（已限长，且控制字符已转义，保证单行可读）。
ALTER TABLE op_audit ADD COLUMN IF NOT EXISTS error_summary TEXT;

-- 审计主查询是「按时间倒序翻看最近干了什么」；
-- 原表除主键外无索引，全表扫在审计数据涨起来后不可接受。
CREATE INDEX IF NOT EXISTS idx_op_audit_created_at ON op_audit (created_at DESC);

-- 「上周二它跑了哪些 bash」—— 按工具类型筛历史。
CREATE INDEX IF NOT EXISTS idx_op_audit_op_type_created ON op_audit (op_type, created_at DESC);

-- 「哪些操作失败了」—— 排查故障时的高频查询。
CREATE INDEX IF NOT EXISTS idx_op_audit_outcome_created ON op_audit (outcome, created_at DESC);
