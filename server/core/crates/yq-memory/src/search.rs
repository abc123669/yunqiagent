//! RRF 混合检索（SPEC 第七节）
//!
//! 三路召回 → Reciprocal Rank Fusion 融合 → top-K：
//!
//! | 路 | 信号 | 说明 |
//! |---|---|---|
//! | 向量路 | pgvector HNSW 余弦距离 | embedding 为空时该路无贡献 |
//! | 全文路 | tsvector + GIN | 官方镜像无 zhparser，用 `simple` 配置占位 |
//! | 时间路 | 时间衰减 × importance | 让新鲜、重要的记忆优先 |
//!
//! 融合公式（RRF，k 取 60）：
//! ```text
//! score(d) = Σ_over_rankers  weight_i / (k + rank_i(d))
//! ```
//!
//! **设计要点**：打分是**纯函数**（`rrf_fuse`），不碰数据库，因此可完整单测；
//! SQL 只负责取三路各自的 top-N 候选，融合在 Rust 侧完成，
//! 从而避免把复杂加权写死在 SQL 里（难测、难改）。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::MemoryError;

/// RRF 常数 k（业界惯例 60）。
pub const RRF_K: f64 = 60.0;

/// 单路召回条数（融合前候选池）。
pub const PER_ROUTE_LIMIT: i64 = 50;

/// 时间衰减半衰期（天）。
pub const DECAY_HALF_LIFE_DAYS: f64 = 30.0;

/// 三路权重（向量 / 全文 / 时间）。
#[derive(Debug, Clone, Copy)]
pub struct RouteWeights {
    /// 向量路权重
    pub vector: f64,
    /// 全文路权重
    pub fulltext: f64,
    /// 时间路权重
    pub recency: f64,
}

impl Default for RouteWeights {
    fn default() -> Self {
        Self {
            vector: 1.0,
            fulltext: 1.0,
            recency: 0.5,
        }
    }
}

/// 一个候选记忆（融合前）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// 记忆 ID
    pub id: i64,
    /// 类型
    pub kind: String,
    /// 内容
    pub content: String,
    /// 重要度
    pub importance: f32,
    /// 创建时间
    pub created_at: DateTime<Utc>,
}

/// 融合后的检索结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// 记忆 ID
    pub id: i64,
    /// 类型
    pub kind: String,
    /// 内容
    pub content: String,
    /// 融合得分
    pub score: f64,
    /// 各路名次（None 表示该路未召回）
    pub vector_rank: Option<usize>,
    /// 全文路名次
    pub fulltext_rank: Option<usize>,
    /// 时间路名次
    pub recency_rank: Option<usize>,
}

/// 计算时间衰减因子：`0.5^(age_days / half_life)`。
///
/// 越新越接近 1.0，越旧越趋近 0。
pub fn time_decay(created_at: DateTime<Utc>, now: DateTime<Utc>, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let age_days = (now - created_at).num_seconds() as f64 / 86400.0;
    let age_days = age_days.max(0.0); // 未来时间按 0 处理
    0.5_f64.powf(age_days / half_life_days)
}

/// 时间路排序键：`time_decay × importance`。
pub fn recency_score(created_at: DateTime<Utc>, importance: f32, now: DateTime<Utc>) -> f64 {
    time_decay(created_at, now, DECAY_HALF_LIFE_DAYS) * (importance as f64)
}

/// 按给定打分函数对候选排序，返回「id → 名次(从 1 开始)」映射。
///
/// 名次从 1 开始（RRF 公式惯例）。分数相同时按 id 升序稳定排序，
/// 保证结果**可复现**。
pub fn rank_by<F>(candidates: &[Candidate], score_fn: F) -> Vec<(i64, usize)>
where
    F: Fn(&Candidate) -> f64,
{
    let mut scored: Vec<(i64, f64)> = candidates.iter().map(|c| (c.id, score_fn(c))).collect();

    // 分数降序；同分按 id 升序（稳定、可复现）
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    scored
        .into_iter()
        .enumerate()
        .map(|(idx, (id, _))| (id, idx + 1))
        .collect()
}

/// 一条 RRF 融合结果。
///
/// 三路名次用 `Option` 表示「该路未命中此文档」——这是可解释性的关键：
/// 排查「为什么这条排在前面」时，需要能看出它是三路通吃还是单路独中。
/// 用结构体而非五元组，是因为 `(i64, f64, Option<usize>, Option<usize>,
/// Option<usize>)` 这种类型一旦写错顺序编译器抓不到（三个 Option 同型），
/// 命名字段把这类错误变成编译错误。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusedDoc {
    /// 记忆 id
    pub id: i64,
    /// 融合得分（降序排列依据）
    pub score: f64,
    /// 向量路名次（未命中为 None）
    pub vector_rank: Option<usize>,
    /// 全文路名次
    pub fulltext_rank: Option<usize>,
    /// 时间路名次
    pub recency_rank: Option<usize>,
}

/// RRF 融合 —— **纯函数**，三路名次表 → 融合得分。
///
/// 参数为三路各自的 `id → rank`：
///   · `vector`   向量路名次
///   · `fulltext` 全文路名次
///   · `recency`  时间路名次
///
/// 返回按融合得分降序的 [`FusedDoc`]。
pub fn rrf_fuse(
    vector: &[(i64, usize)],
    fulltext: &[(i64, usize)],
    recency: &[(i64, usize)],
    weights: RouteWeights,
) -> Vec<FusedDoc> {
    use std::collections::BTreeMap;

    /// 三枚名次（向量 / 全文 / 时间），None = 该路未命中
    type Ranks = (Option<usize>, Option<usize>, Option<usize>);

    // id → 三枚名次
    let mut table: BTreeMap<i64, Ranks> = BTreeMap::new();

    for (id, rank) in vector {
        table.entry(*id).or_insert((None, None, None)).0 = Some(*rank);
    }
    for (id, rank) in fulltext {
        table.entry(*id).or_insert((None, None, None)).1 = Some(*rank);
    }
    for (id, rank) in recency {
        table.entry(*id).or_insert((None, None, None)).2 = Some(*rank);
    }

    let mut out: Vec<FusedDoc> = table
        .into_iter()
        .map(|(id, (v, f, r))| {
            let mut score = 0.0;
            if let Some(rank) = v {
                score += weights.vector / (RRF_K + rank as f64);
            }
            if let Some(rank) = f {
                score += weights.fulltext / (RRF_K + rank as f64);
            }
            if let Some(rank) = r {
                score += weights.recency / (RRF_K + rank as f64);
            }
            FusedDoc {
                id,
                score,
                vector_rank: v,
                fulltext_rank: f,
                recency_rank: r,
            }
        })
        .collect();

    // 得分降序；同分按 id 升序（可复现）
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });

    out
}

/// 检索器：持有连接池。
#[derive(Clone)]
pub struct Searcher {
    pool: PgPool,
    weights: RouteWeights,
}

impl Searcher {
    /// 创建检索器。
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            weights: RouteWeights::default(),
        }
    }

    /// 设置三路权重。
    pub fn with_weights(mut self, weights: RouteWeights) -> Self {
        self.weights = weights;
        self
    }

    /// 单条 SQL 取三路候选（避免多次往返）。
    ///
    /// 说明：向量路需要查询向量；当 `query_vec` 为 `None` 时该路返回空，
    /// 融合自动退化为「全文 + 时间」双路（embedding 尚未写入时的正常状态）。
    async fn fetch_candidates(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        limit: i64,
    ) -> Result<(Vec<Candidate>, Vec<(i64, i32, usize)>), MemoryError> {
        // 三路各自取 top-N，用 UNION ALL 合并成一次往返。
        // 每行带 route 标记（0=向量 1=全文 2=时间），Rust 侧分路排名。
        let rows = sqlx::query_as::<_, (i64, String, String, f32, DateTime<Utc>, i32, i64)>(
            r#"
            WITH vec AS (
                SELECT id, kind, content, importance, created_at, 0 AS route,
                       row_number() OVER (ORDER BY embedding <=> $2::vector) AS route_rank
                FROM memories
                WHERE $2::vector IS NOT NULL AND embedding IS NOT NULL
                  AND (expires_at IS NULL OR expires_at > now())
                ORDER BY embedding <=> $2::vector
                LIMIT $3
            ),
            ft AS (
                SELECT id, kind, content, importance, created_at, 1 AS route,
                       row_number() OVER (ORDER BY ts_rank(fts, plainto_tsquery('simple', $1)) DESC) AS route_rank
                FROM memories
                WHERE fts @@ plainto_tsquery('simple', $1)
                  AND (expires_at IS NULL OR expires_at > now())
                ORDER BY ts_rank(fts, plainto_tsquery('simple', $1)) DESC
                LIMIT $3
            ),
            rec AS (
                SELECT id, kind, content, importance, created_at, 2 AS route,
                       row_number() OVER (ORDER BY created_at DESC, importance DESC) AS route_rank
                FROM memories
                WHERE expires_at IS NULL OR expires_at > now()
                ORDER BY created_at DESC, importance DESC
                LIMIT $3
            )
            SELECT id, kind, content, importance, created_at, route, route_rank
            FROM (
                SELECT * FROM vec
                UNION ALL
                SELECT * FROM ft
                UNION ALL
                SELECT * FROM rec
            ) AS merged
            "#,
        )
        .bind(query)
        .bind(query_vec.map(|v| {
            // 以 pgvector 文本字面量形式绑定：[0.1,0.2,...]
            format!(
                "[{}]",
                v.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut candidates = Vec::with_capacity(rows.len());
        let mut ranks = Vec::with_capacity(rows.len());
        for r in rows {
            candidates.push(Candidate { id:r.0, kind:r.1, content:r.2, importance:r.3, created_at:r.4 });
            ranks.push((r.0, r.5, r.6 as usize));
        }
        Ok((candidates, ranks))
    }

    /// 执行混合检索，返回 top-K。
    pub async fn search(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        top_k: usize,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        let (candidates, route_ranks) = self
            .fetch_candidates(query, query_vec, PER_ROUTE_LIMIT)
            .await?;

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let now = Utc::now();

        // 全文路：fts 已由 SQL 排序，这里按原序给名次（保持 SQL 的 ts_rank 顺序）
        let fulltext_ranks: Vec<(i64, usize)> = route_ranks.iter()
            .filter(|(_, route, _)| *route == 1)
            .map(|(id, _, rank)| (*id, *rank))
            .collect();

        // 时间路：time_decay × importance
        let recency_candidates: Vec<Candidate> = candidates.iter()
            .filter(|c| route_ranks.iter().any(|(id, route, _)| *id == c.id && *route == 2))
            .cloned().collect();
        let recency_ranks = rank_by(&recency_candidates, |c| {
            recency_score(c.created_at, c.importance, now)
        });

        // 向量路：SQL 已按距离排序；此处按候选顺序给名次。
        // 注：真正的向量路名次来自 SQL 的 `<=>` 排序，当前候选集已含全部三路，
        // 因此这里仅在 query_vec 存在时启用（否则该路不参与融合）。
        let vector_ranks: Vec<(i64, usize)> = if query_vec.is_some() {
            route_ranks.iter()
                .filter(|(_, route, _)| *route == 0)
                .map(|(id, _, rank)| (*id, *rank))
                .collect()
        } else {
            Vec::new()
        };

        let fused = rrf_fuse(&vector_ranks, &fulltext_ranks, &recency_ranks, self.weights);

        // 组装结果（带上内容）
        let by_id: std::collections::BTreeMap<i64, &Candidate> =
            candidates.iter().map(|c| (c.id, c)).collect();

        let hits = fused
            .into_iter()
            .take(top_k)
            .filter_map(|doc| {
                by_id.get(&doc.id).map(|c| SearchHit {
                    id: doc.id,
                    kind: c.kind.clone(),
                    content: c.content.clone(),
                    score: doc.score,
                    vector_rank: doc.vector_rank,
                    fulltext_rank: doc.fulltext_rank,
                    recency_rank: doc.recency_rank,
                })
            })
            .collect();

        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn cand(id: i64, importance: f32, age_days: i64) -> Candidate {
        Candidate {
            id,
            kind: "env".to_string(),
            content: format!("内容 {id}"),
            importance,
            created_at: Utc::now() - Duration::days(age_days),
        }
    }

    #[test]
    fn time_decay_is_one_at_age_zero() {
        let now = Utc::now();
        let d = time_decay(now, now, 30.0);
        assert!((d - 1.0).abs() < 1e-9);
    }

    #[test]
    fn time_decay_halves_at_half_life() {
        let now = Utc::now();
        let d = time_decay(now - Duration::days(30), now, 30.0);
        assert!((d - 0.5).abs() < 1e-6, "半衰期处应约为 0.5，实得 {}", d);
    }

    #[test]
    fn time_decay_approaches_zero_for_ancient() {
        let now = Utc::now();
        let d = time_decay(now - Duration::days(3650), now, 30.0);
        assert!(d < 0.001, "十年旧数据应几乎为 0，实得 {}", d);
    }

    #[test]
    fn recency_score_prefers_important_and_fresh() {
        let now = Utc::now();
        let fresh_important = recency_score(now - Duration::days(1), 0.9, now);
        let old_unimportant = recency_score(now - Duration::days(300), 0.1, now);
        assert!(fresh_important > old_unimportant);
    }

    #[test]
    fn rank_by_is_deterministic_on_ties() {
        let cands = vec![cand(10, 0.5, 0), cand(5, 0.5, 0), cand(7, 0.5, 0)];
        // 分数完全相同时按 id 升序
        let ranks = rank_by(&cands, |_| 1.0);
        assert_eq!(ranks, vec![(5, 1), (7, 2), (10, 3)]);
    }

    #[test]
    fn rank_by_orders_by_score_desc() {
        let cands = vec![cand(1, 0.1, 0), cand(2, 0.9, 0), cand(3, 0.5, 0)];
        let ranks = rank_by(&cands, |c| c.importance as f64);
        assert_eq!(ranks[0].0, 2, "最高重要度应排第一");
        assert_eq!(ranks[2].0, 1, "最低重要度应排最后");
    }

    #[test]
    fn rrf_fuse_rewards_documents_appearing_in_multiple_routes() {
        // id=1 在两路都是第 1；id=2 只在单路第 1
        let vector = vec![(1, 1), (2, 2)];
        let fulltext = vec![(1, 1)];
        let recency = vec![];

        let fused = rrf_fuse(&vector, &fulltext, &recency, RouteWeights::default());
        assert_eq!(fused[0].id, 1, "多路命中的文档应排第一");
        assert!(fused[0].score > fused[1].score, "融合得分应更高");
    }

    #[test]
    fn rrf_fuse_empty_routes_yield_nothing() {
        let fused = rrf_fuse(&[], &[], &[], RouteWeights::default());
        assert!(fused.is_empty());
    }

    #[test]
    fn rrf_fuse_single_route_preserves_order() {
        let recency = vec![(3, 1), (2, 2), (1, 3)];
        let fused = rrf_fuse(&[], &[], &recency, RouteWeights::default());
        assert_eq!(
            fused.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    #[test]
    fn rrf_weights_affect_ranking() {
        let vector = vec![(1, 1)];
        let fulltext = vec![(2, 1)];

        let vec_heavy = RouteWeights {
            vector: 10.0,
            fulltext: 0.1,
            recency: 0.0,
        };
        let fused = rrf_fuse(&vector, &fulltext, &[], vec_heavy);
        assert_eq!(fused[0].id, 1, "向量权重高时向量路文档应胜出");

        let ft_heavy = RouteWeights {
            vector: 0.1,
            fulltext: 10.0,
            recency: 0.0,
        };
        let fused2 = rrf_fuse(&vector, &fulltext, &[], ft_heavy);
        assert_eq!(fused2[0].id, 2, "全文权重高时全文路文档应胜出");
    }

    #[test]
    fn rrf_rank_lower_is_better() {
        // 名次 1 应比名次 10 得分高
        let a = rrf_fuse(&[], &[(1, 1)], &[], RouteWeights::default());
        let b = rrf_fuse(&[], &[(1, 10)], &[], RouteWeights::default());
        assert!(a[0].score > b[0].score);
    }
}
