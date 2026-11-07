//! LLM HTTP 客户端 —— 非流式 + SSE 流式。
//!
//! 流式接口返回 `mpsc::Receiver<TokenChunk>`，与 TUI 的 mock 驱动**同构**，
//! 便于把 mock 无缝替换为真实模型输出。
//!
//! ## 韧性策略（两级，`complete` 与 `complete_stream` 同构）
//!
//! 1. **同档重试**：瞬时故障（连接失败 / 超时 / 429 / 500 / 502 / 503 / 504）在
//!    **同一档模型内**先做指数退避重试：1s → 2s → 4s，±25% 抖动，尊重
//!    `Retry-After`，单次等待上限 30s，最多 3 次尝试（即 2 次重试）。
//!    网络抖一下不该让整个请求失败；429 更不该立刻把流量推给备用模型
//!    （主模型往往才是效果最好的那一档）。
//! 2. **跨档降级**：同档重试仍失败、且错误「值得降级」
//!    （见 [`LlmError::is_fallback_worthy`]）时，切到 `fallback` 链的下一档。
//!
//! ## 绝不重试的情形
//!
//! | 情形 | 同档重试 | 换档降级 | 原因 |
//! |---|---|---|---|
//! | connect error / 超时 | ✅ | ✅ | 端点抖动，等一下就好 |
//! | 429 限流 | ✅（读 `Retry-After`） | ✅ | 等窗口过去即可，主模型优先 |
//! | 500 / 502 / 503 / 504 | ✅ | ✅ | 服务端瞬时故障 |
//! | 501 及其他 5xx | ❌ | ✅ | 「永久不支持」重放必然同样失败，但换档可能换网关 |
//! | 404 / `model_not_found` | ❌ | ✅ | 本档模型没了，同档重放无意义，正该换下一档 |
//! | 400 / 401 / 403 等其余 4xx | ❌ | ❌ | 是我们的请求或密钥有问题，换谁都一样 |
//! | 请求体 / 响应解码错误 | ❌ | ❌ | 内容级问题，重放不改变结果 |
//! | 响应 JSON 解析失败 | ❌ | ❌ | 非模型可用性问题 |
//! | **流式已开始产出 token 后中断** | ❌ | ❌ | 见 [`LlmClient::complete_stream`] 的语义说明 |
//!
//! 退避 sleep 一律用 `tokio::time::sleep`（树莓派单核、异步运行时里**绝不阻塞**）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use tokio::sync::mpsc;

use yq_core::truncate_with_marker;

use crate::config::LlmConfig;
use crate::types::{ChatRequest, ChatResponse, StreamEvent, TokenChunk};

// ───────────────────────────── 重试策略常量 ─────────────────────────────

/// 单档模型内的最大尝试次数（1 次首发 + 2 次重试）。
///
/// 刻意保守：树莓派上 QQ 侧有响应时限，一档最多退避 ~3s（429 带 `Retry-After`
/// 时另说），叠 3 档降级也不会把一次对话拖成长征。
pub const MAX_ATTEMPTS: u32 = 3;

/// 指数退避基数（第 1 次重试等 1s，第 2 次 2s，第 3 次 4s……）。
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// 退避序列的上限（防御 `MAX_ATTEMPTS` 被调大后指数爆炸）。
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 单次等待总上限：退避与 `Retry-After` 取大后仍夹到这个值。
///
/// provider 常返回 `Retry-After: 3600`，老老实实等一小时等于服务挂死。
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// 抖动比例：±25%（实际实现是 `[0.75x, 1.25x]`）。
///
/// 没有抖动的话，服务恢复瞬间所有客户端同步重试（thundering herd），
/// 很容易把刚喘过气的网关再次打死。
const JITTER_RATIO: f64 = 0.25;

/// `Retry-After` 合理上限（秒）：超过这个值直接视为不可用（返回 `None`），
/// 交给退避序列决定等多久。防止病态值把 Duration 撑爆或语义漂移。
const RETRY_AFTER_SANITY_MAX_SECS: u64 = 86_400;

// ───────────────────────────── 错误分类（纯函数） ─────────────────────────────

/// 故障面：把「一个错误」归一化成重试决策需要的最小信息。
///
/// 单独造这个枚举而不是直接吃 `&LlmError`，是为了**可测**——
/// `reqwest::Error` 无法在单测里凭空构造（内部字段私有），
/// 于是先把传输错误提炼成信号，再对信号做纯函数判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureSignal {
    /// 连接建立失败（DNS 之后连不上 / 端口拒绝 / TLS 握手挂）
    Connect,
    /// 超时（reqwest 的 `TimedOut`，含整体请求超时与 body 读超时）
    Timeout,
    /// 请求发送阶段的错误（`Kind::Request`，多为连接复用踩到已关的 socket）
    Request,
    /// 请求体 / 响应体读写错误（`Kind::Body`）
    Body,
    /// 响应体解码错误（gzip / chunked 解码失败，`Kind::Decode`）
    Decode,
    /// HTTP 状态码（非 2xx）
    Status(u16),
    /// 其余一律按永久故障处理（认证、格式、解析、配置、重定向策略等）
    Other,
}

impl FailureSignal {
    /// 从 `LlmError` 提炼故障面。
    ///
    /// 判定顺序有讲究：`is_timeout()` 必须在 `is_body()` **之前**——
    /// reqwest 的整体超时常常同时挂在 body kind 上（`Kind::Body` + `TimedOut` source），
    /// 而超时是典型瞬时故障，必须重试；反之纯 body 错误（内容写坏）不该重试。
    pub fn of(err: &LlmError) -> Self {
        match err {
            LlmError::Http(e) => Self::of_reqwest(e),
            LlmError::Status { status, .. } => Self::Status(*status),
            LlmError::Parse(_) | LlmError::Config(_) | LlmError::StreamClosed => Self::Other,
        }
    }

    /// 从 reqwest 传输错误提炼故障面。
    pub fn of_reqwest(e: &reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout
        } else if e.is_body() {
            Self::Body
        } else if e.is_connect() {
            Self::Connect
        } else if e.is_decode() {
            Self::Decode
        } else if e.is_request() {
            Self::Request
        } else {
            Self::Other
        }
    }

    /// 该故障面**是否值得在同一档重试**。纯函数，无 IO，单测直接覆盖决策表。
    pub fn is_retryable(self) -> bool {
        match self {
            // 传输层瞬时故障
            Self::Connect | Self::Timeout | Self::Request => true,
            // 状态码：只认「等一下可能就好」的几个
            Self::Status(s) => is_retryable_status(s),
            // body/decode 是内容级问题；认证/格式/解析同理
            Self::Body | Self::Decode | Self::Other => false,
        }
    }
}

/// 可同档重试的 HTTP 状态码白名单。
///
/// 只列 429 / 500 / 502 / 503 / 504 这几个「网关与上游抖动」的常见码：
/// - **501 Not Implemented** 是服务端明确说「这功能我没实现」，重放必然同样结果；
/// - 其余 5xx（505/506/…）同理按永久处理，但**仍然**可以走降级链
///   （[`LlmError::is_fallback_worthy`] 对整段 5xx 判 true），换一档可能就是换个网关。
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// 是否还该再试一次（`attempt` 为**已完成的**尝试次数，1-based）。
pub fn should_retry_attempt(attempt: u32) -> bool {
    attempt < MAX_ATTEMPTS
}

/// 第 `attempt` 次尝试失败后的**基准**退避时长（不含抖动）。
///
/// `attempt` 是 1-based：第 1 次失败 → 1s，第 2 次 → 2s，第 3 次 → 4s。
/// 用 saturating 移位，`attempt` 很大时也不会溢出 panic，只会被 `BACKOFF_CAP` 夹住。
pub fn backoff_delay(attempt: u32) -> Duration {
    let base = BACKOFF_BASE.as_millis() as u64; // 1000ms
    // 指数：2^(attempt-1)，超过 40 位一律视为「顶到上限」，避免溢出
    let factor: u64 = if attempt == 0 || attempt > 41 {
        u64::MAX
    } else {
        1u64 << (attempt - 1)
    };
    let ms = base
        .saturating_mul(factor)
        .min(BACKOFF_CAP.as_millis() as u64);
    Duration::from_millis(ms)
}

/// 基准退避加 ±25% 抖动后的等待时长。
///
/// `seed` 由调用方提供（见 [`jitter_seed`]），这样函数本身是纯的、可测边界。
/// 结果落在 `[0.75x, 1.25x]` 闭区间内，`seed` 取 0 与取 `width` 分别命中两端。
pub fn jittered_delay(attempt: u32, seed: u64) -> Duration {
    let micros = backoff_delay(attempt).as_micros() as u64;
    let lo = micros - (micros as f64 * JITTER_RATIO).round() as u64;
    // 抖动窗口宽度 = 0.5x（从 0.75x 到 1.25x）
    let width = (micros as f64 * (JITTER_RATIO * 2.0)).round() as u64;
    let delta = if width == 0 { 0 } else { seed % (width + 1) };
    Duration::from_micros(lo.saturating_add(delta))
}

/// 解析 `Retry-After` 响应头（HTTP 的 delta-seconds 与工程实践里常见的小数秒）。
///
/// 只支持秒数：`"120"` → 120s，`"1.5"` → 1.5s。HTTP-date 形式（`"Wed, 21 Oct ..."`）
/// 需要日历解析、还要对齐服务端时钟，而 429 场景几乎没人用，故**不支持**（返回 `None`）。
/// 非数字、空串、负数、超过一天的病态值同样 `None`——宁可退回退避序列，不要等太久。
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<u64>() {
        return (secs <= RETRY_AFTER_SANITY_MAX_SECS).then(|| Duration::from_secs(secs));
    }
    let secs = v.parse::<f64>().ok()?;
    if !secs.is_finite() || secs < 0.0 || secs > RETRY_AFTER_SANITY_MAX_SECS as f64 {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

/// 最终等待时长：`max(带抖动的退避, Retry-After)`，再夹到 [`MAX_RETRY_DELAY`]。
///
/// 429 常带 `Retry-After`，那是服务端给的权威窗口，比我们的猜测准；
/// 但它也可能离谱（3600s），所以上限必须兜住。
pub fn effective_delay(attempt: u32, retry_after: Option<Duration>, seed: u64) -> Duration {
    let jittered = jittered_delay(attempt, seed);
    match retry_after {
        Some(ra) => ra.max(jittered).min(MAX_RETRY_DELAY),
        None => jittered.min(MAX_RETRY_DELAY),
    }
}

/// 抖动随机源：当前时间纳秒 与 进程内单调计数 混合（splitmix64 收尾）。
///
/// **为什么不加 `rand` 依赖**：
/// 1. yq-llm 目前不依赖 `rand`，为几毫秒的抖动新增依赖要动 `Cargo.lock`，
///    而当前工作区有多个同事并行改其他 crate，锁文件冲突成本远大于收益；
/// 2. 这里的随机**只求去同步**（避免 thundering herd），不要求密码学强度、
///    也不要求均匀分布的严格性，时间纳秒 + 计数器已经足够；
/// 3. 与 KV-cache 纪律不冲突——「固定区不得引入随机数污染」约束的是**进入 prompt 的字节**；
///    重试退避发生在运行期的 sleep 时长上，一个字节都不会进 prompt 或缓存前缀。
pub fn jitter_seed() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    splitmix64(nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// splitmix64 收尾混合：把「纳秒 + 序号」这种低位有规律的数打散，
/// 免得 `seed % (width+1)` 因为低位相关性导致抖动分布偏向某一侧。
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_8B3F);
    x ^ (x >> 31)
}

/// LLM 层错误。
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// 配置错误
    #[error("配置错误: {0}")]
    Config(String),
    /// HTTP 传输错误
    #[error("HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),
    /// 服务端返回非 2xx
    #[error("服务端返回 {status}: {body}")]
    Status {
        /// HTTP 状态码
        status: u16,
        /// 响应体
        body: String,
    },
    /// 响应解析失败
    #[error("响应解析失败: {0}")]
    Parse(String),
    /// 流式通道已关闭
    #[error("流式通道已关闭")]
    StreamClosed,
}

impl LlmError {
    /// 是否**值得降级重试**（换一档模型/端点再试）。
    ///
    /// 与「同档是否重试」（[`FailureSignal::is_retryable`]）是**两个独立维度**：
    /// 例如 404 同档重放毫无意义（不重试），但换一档正是解法（值得降级）；
    /// 505 同档不重放，换档可能换到另一个网关，仍值得降级。
    ///
    /// ## 判据
    ///
    /// | 错误 | 降级？ | 原因 |
    /// |---|---|---|
    /// | 网络/超时 | ✅ | 端点可能正好抖动 |
    /// | 5xx | ✅ | 服务端故障，别的模型可能正常 |
    /// | 429 限流 | ✅ | 换 channel 可能绕过限流 |
    /// | 404 / model_not_found | ✅ | **本档模型不可用，正该换下一档** |
    /// | 401/403 认证 | ❌ | 密钥问题，换模型同样失败（白耗时间） |
    /// | 400 请求格式 | ❌ | 是**我们的**请求有问题，换谁都一样 |
    /// | 解析失败 | ❌ | 响应格式问题，非模型可用性问题 |
    pub fn is_fallback_worthy(&self) -> bool {
        match self {
            LlmError::Http(_) => true,
            LlmError::Status { status, body } => {
                let s = *status;
                // 5xx / 429 / 404 → 值得换
                if (500..600).contains(&s) || s == 429 || s == 404 {
                    return true;
                }
                // 网关把「模型不存在」包成 400 时按内容判断
                let b = body.to_lowercase();
                b.contains("model_not_found")
                    || b.contains("no available channel")
                    || b.contains("模型不存在")
                    || b.contains("无可用渠道")
            }
            LlmError::Parse(_) | LlmError::Config(_) | LlmError::StreamClosed => false,
        }
    }
}

/// LLM 客户端。
///
/// 内部配置放在 `RwLock` 里，支持**运行时切换模型**（`/model` 命令需要），
/// 同时复用同一个 `reqwest::Client`（保留连接池）。
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    config: Arc<RwLock<LlmConfig>>,
}

impl LlmClient {
    /// 创建客户端（校验配置）。
    pub fn new(config: LlmConfig) -> Result<Self, LlmError> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .timeout(config.timeout())
            .build()?;
        Ok(Self {
            http,
            config: Arc::new(RwLock::new(config)),
        })
    }

    /// 当前配置快照。
    ///
    /// 返回值可能随 `/model` 切换而变化，故用 `String` 返回模型名。
    pub fn model(&self) -> String {
        self.config
            .read()
            .map(|c| c.model.clone())
            .unwrap_or_default()
    }

    /// 当前 base_url。
    pub fn base_url(&self) -> String {
        self.config
            .read()
            .map(|c| c.base_url.clone())
            .unwrap_or_default()
    }

    /// 切换模型（运行时生效）。
    pub fn set_model(&self, model: impl Into<String>) -> Result<(), LlmError> {
        let mut cfg = self
            .config
            .write()
            .map_err(|_| LlmError::Config("配置锁已毒化".to_string()))?;
        let model = model.into();
        if model.trim().is_empty() {
            return Err(LlmError::Config("模型名不能为空".to_string()));
        }
        cfg.model = model.trim().to_string();
        Ok(())
    }

    /// 以只读快照方式取配置（构造请求用；会随 `/model` 切换变化）。
    pub fn snapshot(&self) -> Result<LlmConfig, LlmError> {
        self.config
            .read()
            .map(|c| c.clone())
            .map_err(|_| LlmError::Config("配置锁已毒化".to_string()))
    }

    /// **同一档模型内**发送请求：瞬时故障按退避重试，耗尽后把最后一个错误交给调用方。
    ///
    /// 这是 `complete` / `complete_stream` 共用的底层原语，返回**未消费 body 的
    /// `reqwest::Response`**（状态码必为 2xx），于是：
    /// - 非流式路径拿它去 `.json()`；
    /// - 流式路径拿它去 `bytes_stream()` spawn 解析任务。
    ///
    /// 注意本函数**不做降级**——降级由调用方按 [`LlmError::is_fallback_worthy`] 决定，
    /// 这样「同档重试」与「跨档降级」两个维度不会互相纠缠。
    async fn send_with_retry(
        &self,
        cfg: &LlmConfig,
        request: &ChatRequest,
        streaming: bool,
    ) -> Result<reqwest::Response, LlmError> {
        let endpoint = cfg.completions_endpoint();
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;

            // RequestBuilder 不可复用（send 会消费掉），每次尝试重新构造。
            let resp = self
                .http
                .post(&endpoint)
                .bearer_auth(&cfg.api_key)
                .json(request)
                .send()
                .await;

            match resp {
                Ok(r) => {
                    let status = r.status().as_u16();
                    if r.status().is_success() {
                        return Ok(r);
                    }
                    // Retry-After 必须在读 body 之前从 header 拿（headers 随 Response 一起在）
                    let retry_after = r
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_retry_after);
                    let body = r.text().await.unwrap_or_default();
                    let err = LlmError::Status {
                        status,
                        body: truncate_with_marker(&body, 500, "…(截断)"),
                    };

                    let signal = FailureSignal::Status(status);
                    if signal.is_retryable() && should_retry_attempt(attempt) {
                        let delay = effective_delay(attempt, retry_after, jitter_seed());
                        tracing::warn!(
                            "模型 {} 返回 {}，{}（第 {}/{} 次尝试，{}ms 后重试）",
                            cfg.model,
                            status,
                            if retry_after.is_some() {
                                "按 Retry-After 退避"
                            } else {
                                "瞬时故障退避"
                            },
                            attempt,
                            MAX_ATTEMPTS,
                            delay.as_millis(),
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) => {
                    let signal = FailureSignal::of_reqwest(&e);
                    let err = LlmError::Http(e);
                    if signal.is_retryable() && should_retry_attempt(attempt) {
                        let delay = effective_delay(attempt, None, jitter_seed());
                        tracing::warn!(
                            "模型 {} {}请求传输失败（{}），第 {}/{} 次尝试，{}ms 后重试",
                            cfg.model,
                            if streaming { "流式" } else { "非流式" },
                            signal_desc(signal),
                            attempt,
                            MAX_ATTEMPTS,
                            delay.as_millis(),
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// 非流式补全（**同档重试 + 降级链**）。
    ///
    /// 每一档先做瞬时错误重试（见 [`Self::send_with_retry`]），仍失败且错误
    /// 「值得降级」时才切下一档。认证/请求格式这类换模型也没用的错误会**立即返回**，不白耗时间。
    pub async fn complete(&self, mut request: ChatRequest) -> Result<ChatResponse, LlmError> {
        // 非流式路径强制关闭 stream，避免服务端返回 SSE
        request.stream = None;
        request.stream_options = None;

        let base = self.snapshot()?;
        let candidates = base.candidates();
        let total = candidates.len();
        let mut last_err: Option<LlmError> = None;

        for (idx, cfg) in candidates.iter().enumerate() {
            let mut req = request.clone();
            req.model = cfg.model.clone();

            let resp = match self.send_with_retry(cfg, &req, false).await {
                Ok(r) => r,
                Err(err) => {
                    // 同档重试已耗尽，才考虑降级
                    if idx + 1 < total && err.is_fallback_worthy() {
                        tracing::warn!(
                            "模型 {} 调用失败（{}），降级到 {}（第 {}/{} 档）",
                            cfg.model,
                            err,
                            candidates[idx + 1].model,
                            idx + 2,
                            total
                        );
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            };

            // 2xx：解析 JSON。解析失败属于内容问题，不重试也不降级（判据见 is_fallback_worthy）
            return resp
                .json::<ChatResponse>()
                .await
                .map_err(|e| LlmError::Parse(e.to_string()));
        }

        Err(last_err.unwrap_or_else(|| LlmError::Config("降级链为空".to_string())))
    }

    /// 流式补全：返回 token 分片接收器（**同档重试 + 降级链**）。
    ///
    /// ## 为什么这里也必须有降级链
    ///
    /// QQ 生产主路径走流式（`serve` 逐 token 回复）。此前只有 `complete()` 有
    /// 降级链，流式第一档一挂就直接报错——**配置了备用模型等于没配**，
    /// 主模型限流/下线时机器人整段沉默。现在流式与非流式同构。
    ///
    /// ## 流式的重试边界（关键语义）
    ///
    /// 重试/降级**只发生在 spawn SSE 解析任务之前**，即「流建立之前」：
    /// 连接失败、超时、非 2xx 状态码。一旦 `send_with_retry` 返回 2xx 并进入
    /// 下方 `tokio::spawn`，就已经无法保证「用户还没看到任何 token」——
    /// 此时若中途断流去重放整段请求，用户会看到**重复的回答**（前半句两遍），
    /// 比一次报错更糟。所以流开始产出之后的中断**既不重放也不换档**，
    /// 只在解析任务里 `warn` 并补一个 `is_final` 终止片，保证调用方不会永久等待。
    ///
    /// 调用方用 `rx.recv().await` 或 `rx.try_recv()`（非阻塞）消费。
    pub async fn complete_stream(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<TokenChunk>, LlmError> {
        let mut request = request;
        request.stream = Some(true);
        request.stream_options = Some(crate::types::StreamOptions {
            include_usage: true,
        });

        let base = self.snapshot()?;
        let candidates = base.candidates();
        let total = candidates.len();

        for (idx, cfg) in candidates.iter().enumerate() {
            let mut req = request.clone();
            req.model = cfg.model.clone();

            // 只重试「建立流之前」的失败：连接 / 超时 / 非 2xx（带 Retry-After）
            let resp = match self.send_with_retry(cfg, &req, true).await {
                Ok(r) => r,
                Err(err) => {
                    if idx + 1 < total && err.is_fallback_worthy() {
                        tracing::warn!(
                            "模型 {} 流式调用失败（{}），降级到 {}（第 {}/{} 档）",
                            cfg.model,
                            err,
                            candidates[idx + 1].model,
                            idx + 2,
                            total
                        );
                        continue;
                    }
                    return Err(err);
                }
            };

            return Ok(Self::spawn_stream_reader(resp, cfg.model.clone()));
        }

        Err(LlmError::Config("降级链为空".to_string()))
    }

    /// 把已建立（2xx）的流式响应交给后台解析任务，返回接收器。
    ///
    /// 独立成函数是因为它一旦被调用就**越过了重试边界**：从这里开始只能
    /// 尽力把 token 吐干净，不能回头重放请求。
    fn spawn_stream_reader(resp: reqwest::Response, model: String) -> mpsc::Receiver<TokenChunk> {
        let (tx, rx) = mpsc::channel::<TokenChunk>(64);

        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut buffer = Vec::<u8>::new();
            let mut sent_any = false;

            loop {
                let item = tokio::select! {
                    _ = tx.closed() => return,
                    item = stream.next() => item,
                };
                let Some(item) = item else { break; };
                let chunk = match item {
                    Ok(c) => c,
                    Err(e) => {
                        // 已经可能吐过 token 了：此处**绝不重放/换档**，
                        // 否则用户会看到重复内容。见 complete_stream 的语义说明。
                        tracing::warn!(
                            "流式读取出错（模型 {}，已发送 {} 个分片）: {}",
                            model,
                            if sent_any { ">0" } else { "0" },
                            e
                        );
                        break;
                    }
                };

                buffer.extend_from_slice(&chunk);

                // SSE 以行为单位；保留最后一段不完整行
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&buffer[..pos]).trim_end_matches('\r').to_string();
                    buffer.drain(..=pos);

                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        let _ = tx
                            .send(TokenChunk {
                                text: String::new(),
                                is_final: true,
                            })
                            .await;
                        return;
                    }

                    match serde_json::from_str::<StreamEvent>(data) {
                        Ok(ev) => {
                            if let Some(text) = ev.delta_text() {
                                if !text.is_empty() {
                                    sent_any = true;
                                    if tx
                                        .send(TokenChunk {
                                            text: text.to_string(),
                                            is_final: false,
                                        })
                                        .await
                                        .is_err()
                                    {
                                        return; // 接收端关闭
                                    }
                                }
                            }
                            if ev.is_final() {
                                let _ = tx
                                    .send(TokenChunk {
                                        text: String::new(),
                                        is_final: true,
                                    })
                                    .await;
                                return;
                            }
                        }
                        Err(e) => {
                            tracing::debug!("跳过无法解析的 SSE 数据段: {} ({})", data, e);
                        }
                    }
                }
            }

            // 流意外结束：补一个终止片，保证调用方不会永久等待
            if !sent_any {
                tracing::warn!("流式响应未产出任何文本（模型 {}）", model);
            }
            let _ = tx
                .send(TokenChunk {
                    text: String::new(),
                    is_final: true,
                })
                .await;
        });

        rx
    }

    /// 便捷方法：流式并汇总为完整文本（回调式，避免调用方处理 channel）。
    pub async fn stream_to_string<F>(
        &self,
        request: ChatRequest,
        mut on_delta: F,
    ) -> Result<String, LlmError>
    where
        F: FnMut(&str),
    {
        let mut rx = self.complete_stream(request).await?;
        let mut out = String::new();
        while let Some(chunk) = rx.recv().await {
            if chunk.is_final {
                break;
            }
            out.push_str(&chunk.text);
            on_delta(&chunk.text);
        }
        Ok(out)
    }
}

/// 日志里用的故障面短描述（避免把 reqwest 那串长文案重复贴三遍）。
fn signal_desc(signal: FailureSignal) -> &'static str {
    match signal {
        FailureSignal::Connect => "连接失败",
        FailureSignal::Timeout => "超时",
        FailureSignal::Request => "请求发送失败",
        FailureSignal::Body => "请求/响应体错误",
        FailureSignal::Decode => "响应解码错误",
        FailureSignal::Status(_) => "HTTP 状态错误",
        FailureSignal::Other => "不可重试错误",
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;
    use crate::config::{FallbackEntry, LlmConfig};

    fn status(code: u16, body: &str) -> LlmError {
        LlmError::Status {
            status: code,
            body: body.to_string(),
        }
    }

    #[test]
    fn server_errors_are_fallback_worthy() {
        assert!(status(500, "internal").is_fallback_worthy());
        assert!(status(503, "unavailable").is_fallback_worthy());
        assert!(status(502, "bad gateway").is_fallback_worthy());
    }

    #[test]
    fn rate_limit_is_fallback_worthy() {
        assert!(status(429, "too many requests").is_fallback_worthy());
    }

    #[test]
    fn missing_model_is_fallback_worthy() {
        // 这正是降级链存在的意义：本档模型没了，换下一个
        assert!(status(404, "not found").is_fallback_worthy());
        assert!(status(400, r#"{"code":"model_not_found"}"#).is_fallback_worthy());
        assert!(status(400, "No available channel for model X").is_fallback_worthy());
        assert!(status(400, "当前分组下模型无可用渠道").is_fallback_worthy());
    }

    #[test]
    fn auth_errors_are_not_fallback_worthy() {
        // 密钥问题换模型同样失败，白耗时间
        assert!(!status(401, "unauthorized").is_fallback_worthy());
        assert!(!status(403, "forbidden").is_fallback_worthy());
    }

    #[test]
    fn bad_request_is_not_fallback_worthy() {
        // 我们的请求有问题，换谁都一样
        assert!(!status(400, "invalid messages format").is_fallback_worthy());
    }

    #[test]
    fn parse_error_is_not_fallback_worthy() {
        assert!(!LlmError::Parse("bad json".into()).is_fallback_worthy());
    }

    #[test]
    fn candidates_include_main_first() {
        let cfg = LlmConfig {
            model: "main-model".into(),
            fallback: vec![
                FallbackEntry::model_only("backup-1"),
                FallbackEntry::model_only("backup-2"),
            ],
            ..LlmConfig::default()
        };

        let c = cfg.candidates();
        assert_eq!(c.len(), 3, "主 + 2 备 = 3 档");
        assert_eq!(c[0].model, "main-model");
        assert_eq!(c[1].model, "backup-1");
        assert_eq!(c[2].model, "backup-2");
    }

    #[test]
    fn candidates_inherit_missing_fields() {
        let cfg = LlmConfig {
            base_url: "http://gateway/v1".into(),
            api_key: "sk-main".into(),
            model: "main".into(),
            // 只给模型名 → 地址与密钥应继承主配置
            fallback: vec![FallbackEntry::model_only("backup")],
            ..LlmConfig::default()
        };

        let c = cfg.candidates();
        assert_eq!(c[1].base_url, "http://gateway/v1");
        assert_eq!(c[1].api_key, "sk-main");
        assert_eq!(c[1].model, "backup");
    }

    #[test]
    fn candidates_allow_overriding_endpoint() {
        let cfg = LlmConfig {
            base_url: "http://gateway-a/v1".into(),
            model: "main".into(),
            fallback: vec![FallbackEntry {
                model: "backup".into(),
                base_url: Some("http://gateway-b/v1".into()),
                api_key: Some("sk-other".into()),
            }],
            ..LlmConfig::default()
        };

        let c = cfg.candidates();
        assert_eq!(c[1].base_url, "http://gateway-b/v1");
        assert_eq!(c[1].api_key, "sk-other");
    }

    #[test]
    fn candidates_do_not_recurse() {
        // 候选自身不该再带降级链（否则会无限展开）
        let cfg = LlmConfig {
            fallback: vec![FallbackEntry::model_only("backup")],
            ..LlmConfig::default()
        };
        let c = cfg.candidates();
        for cand in &c {
            assert!(cand.fallback.is_empty(), "候选不应携带降级链");
        }
    }

    #[test]
    fn no_fallback_yields_single_candidate() {
        let cfg = LlmConfig::default();
        assert_eq!(cfg.candidates().len(), 1);
        assert_eq!(cfg.fallback_count(), 0);
    }

    #[test]
    fn blank_override_falls_back_to_inheritance() {
        // 显式给了空串 → 视为未覆写，仍继承
        let cfg = LlmConfig {
            base_url: "http://gateway/v1".into(),
            fallback: vec![FallbackEntry {
                model: "backup".into(),
                base_url: Some("   ".into()),
                api_key: Some("".into()),
            }],
            ..LlmConfig::default()
        };
        let c = cfg.candidates();
        assert_eq!(c[1].base_url, "http://gateway/v1");
    }
}

#[cfg(test)]
mod resilience_tests {
    //! 重试/退避/`Retry-After` 的纯函数覆盖：决策表 + 边界 + 封顶。
    use super::*;

    fn st(code: u16) -> LlmError {
        LlmError::Status {
            status: code,
            body: String::new(),
        }
    }

    #[test]
    fn transient_status_codes_are_retryable() {
        for code in [429u16, 500, 502, 503, 504] {
            assert!(
                FailureSignal::of(&st(code)).is_retryable(),
                "{code} 应可同档重试"
            );
            assert!(is_retryable_status(code));
        }
    }

    #[test]
    fn permanent_status_codes_are_not_retryable() {
        // 404 / 400 / 401 / 403：重放必然同样失败
        for code in [400u16, 401, 403, 404, 405, 408, 422] {
            assert!(
                !FailureSignal::of(&st(code)).is_retryable(),
                "{code} 不应重试"
            );
        }
        // 501 是「服务端明确没实现」；其余 5xx 也走白名单外
        assert!(!FailureSignal::of(&st(501)).is_retryable());
        assert!(!FailureSignal::of(&st(505)).is_retryable());
        assert!(!FailureSignal::of(&st(507)).is_retryable());
    }

    #[test]
    fn no_retry_but_fallback_worthy_is_a_real_combination() {
        // 404 / 505：同档不重放，但换档正是解法 —— 两个维度必须独立
        for code in [404u16, 505] {
            let e = st(code);
            assert!(!FailureSignal::of(&e).is_retryable(), "{code} 同档不该重试");
            assert!(e.is_fallback_worthy(), "{code} 该降级");
        }
    }

    #[test]
    fn transport_signals_classify_retryability() {
        assert!(FailureSignal::Connect.is_retryable());
        assert!(FailureSignal::Timeout.is_retryable());
        assert!(FailureSignal::Request.is_retryable());
        // is_body 对应的内容级错误绝不重试
        assert!(!FailureSignal::Body.is_retryable());
        assert!(!FailureSignal::Decode.is_retryable());
        assert!(!FailureSignal::Other.is_retryable());
    }

    #[test]
    fn non_http_errors_map_to_non_retryable_other() {
        assert_eq!(
            FailureSignal::of(&LlmError::Parse("x".into())),
            FailureSignal::Other
        );
        assert!(!FailureSignal::of(&LlmError::Parse("x".into())).is_retryable());
        assert!(!FailureSignal::of(&LlmError::Config("x".into())).is_retryable());
        assert!(!FailureSignal::of(&LlmError::StreamClosed).is_retryable());
    }

    #[test]
    fn backoff_sequence_is_1s_2s_4s() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(5), Duration::from_secs(16));
    }

    #[test]
    fn backoff_is_capped_and_never_panics_on_huge_attempt() {
        assert_eq!(backoff_delay(6), BACKOFF_CAP);
        assert_eq!(backoff_delay(64), BACKOFF_CAP);
        assert_eq!(backoff_delay(u32::MAX), BACKOFF_CAP);
        assert_eq!(backoff_delay(0), BACKOFF_CAP); // 异常入参兜到上限，不 panic
        assert!(jittered_delay(u32::MAX, 12345) <= BACKOFF_CAP * 2);
    }

    #[test]
    fn jitter_stays_within_25_percent_and_hits_both_ends() {
        for attempt in 1..=5u32 {
            let base = backoff_delay(attempt);
            let lo = base.mul_f64(0.75);
            let hi = base.mul_f64(1.25);
            // 抖动窗口宽度 = hi - lo；seed 取 0 与 width 恰命中两端
            let micros = base.as_micros() as u64;
            let width = (micros as f64 * 0.5).round() as u64;
            assert_eq!(jittered_delay(attempt, 0), lo, "抖动下界应为 0.75x");
            assert_eq!(jittered_delay(attempt, width), hi, "抖动上界应为 1.25x");
            // 扫遍窗口内取样的种子，结果必须始终落在 [lo, hi]
            for seed in (0..=width).step_by(((width / 97).max(1)) as usize) {
                let d = jittered_delay(attempt, seed);
                assert!(
                    d >= lo && d <= hi,
                    "attempt={attempt} seed={seed} → {d:?} 越界 [{lo:?},{hi:?}]"
                );
            }
        }
    }

    #[test]
    fn attempts_are_capped_at_three() {
        assert_eq!(MAX_ATTEMPTS, 3);
        assert!(should_retry_attempt(1));
        assert!(should_retry_attempt(2));
        // 第 3 次尝试失败后封顶：不再重试
        assert!(!should_retry_attempt(3));
        assert!(!should_retry_attempt(4));
        assert!(!should_retry_attempt(u32::MAX));
    }

    #[test]
    fn parse_retry_after_delta_seconds() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 42 "), Some(Duration::from_secs(42)));
        assert_eq!(parse_retry_after("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("0.25"), Some(Duration::from_millis(250)));
    }

    #[test]
    fn parse_retry_after_rejects_junk() {
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("   "), None);
        assert_eq!(parse_retry_after("abc"), None);
        assert_eq!(parse_retry_after("-5"), None);
        assert_eq!(parse_retry_after("1.2.3"), None);
        assert_eq!(parse_retry_after("nan"), None);
        assert_eq!(parse_retry_after("inf"), None);
        // HTTP-date 形式不支持（需要日历解析 + 时钟对齐）
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        // 病态大值（超过一天）视为不可用，避免 Duration 撑爆/语义漂移
        assert_eq!(parse_retry_after("99999999999999999999999"), None);
        assert_eq!(parse_retry_after("86401"), None);
    }

    #[test]
    fn retry_after_can_be_respected_up_to_the_cap() {
        // 服务端的权威窗口比我们的猜测优先
        let d = effective_delay(1, Some(Duration::from_secs(3)), 0);
        assert_eq!(d, Duration::from_secs(3));
        // 但绝不超过 30s
        let d = effective_delay(1, Some(Duration::from_secs(3600)), 0);
        assert_eq!(d, MAX_RETRY_DELAY);
        let d = effective_delay(1, Some(Duration::from_secs(3600)), 7);
        assert_eq!(d, MAX_RETRY_DELAY);
    }

    #[test]
    fn effective_delay_falls_back_to_backoff() {
        // Retry-After 比退避小 → 用退避（含抖动，取两端验证）
        // attempt=2 基准 2s：抖动窗口宽度 = 2s × 0.5 = 1_000_000µs，seed 取 0 / width 命中两端
        let lo = effective_delay(2, Some(Duration::from_millis(100)), 0);
        let hi = effective_delay(2, Some(Duration::from_millis(100)), 1_000_000);
        assert_eq!(lo, Duration::from_secs(2).mul_f64(0.75));
        assert_eq!(hi, Duration::from_secs(2).mul_f64(1.25));
        // 无 Retry-After → 纯抖动退避
        assert_eq!(
            effective_delay(1, None, 0),
            Duration::from_secs(1).mul_f64(0.75)
        );
        assert!(effective_delay(1, None, 7) <= MAX_RETRY_DELAY);
    }

    #[test]
    fn all_effective_delays_are_bounded() {
        // 任意 attempt / retry-after / seed 组合都不会让人等到地老天荒
        for attempt in [1u32, 2, 3, 9, u32::MAX] {
            for ra in [
                None,
                Some(Duration::ZERO),
                Some(Duration::from_secs(u64::MAX / 2)),
            ] {
                for seed in [0u64, 1, 999, u64::MAX] {
                    let d = effective_delay(attempt, ra, seed);
                    assert!(
                        d <= MAX_RETRY_DELAY,
                        "attempt={attempt} ra={ra:?} seed={seed} → {d:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn jitter_seed_is_spread_out() {
        // 连续取样的抖动种子不应大面积相同（否则抖动形同虚设）
        let mut prev = jitter_seed();
        let mut same = 0usize;
        for _ in 0..16 {
            let s = jitter_seed();
            if s == prev {
                same += 1;
            }
            prev = s;
        }
        assert!(same < 16, "抖动源退化：连续种子全同");
    }

    /// 真连一个已关闭的本地端口，验证 reqwest 的错误确实被归成可重试的瞬时故障。
    ///
    /// 之所以要这条：`FailureSignal::of_reqwest` 依赖 reqwest 的 `is_*` 语义，
    /// 纯枚举测试测不到「库真的这么报」这件事。
    #[tokio::test]
    async fn real_connect_refusal_is_classified_retryable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        drop(listener); // 端口释放 → 连接必然被拒

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client");
        let err = http
            .post(format!("http://{addr}/chat/completions"))
            .json(&serde_json::json!({"model": "x", "messages": []}))
            .send()
            .await
            .expect_err("端口已关闭，必然失败");

        let signal = FailureSignal::of_reqwest(&err);
        assert!(
            signal.is_retryable(),
            "连接被拒应判为瞬时故障可重试，实际 signal={signal:?} err={err}"
        );
        assert!(
            !err.is_body(),
            "连接失败不该被报成 body 错误（body 不重试）"
        );
        let e = LlmError::Http(err);
        assert!(e.is_fallback_worthy(), "传输错误同样值得降级");
    }
}
