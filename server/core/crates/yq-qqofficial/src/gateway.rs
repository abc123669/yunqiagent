//! WebSocket 网关 —— 连接、鉴权、心跳、事件分发、断线重连。
//!
//! ## 连接时序
//!
//! ```text
//! 1. GET /gateway/bot          → 拿 wss 地址
//! 2. 建立 WebSocket
//! 3. 收 op=10 Hello            → 拿 heartbeat_interval
//! 4. 发 op=2 Identify          → token 必须是 "QQBot {access_token}"
//! 5. 起独立 task 按间隔发 op=1 Heartbeat
//! 6. 事件：op=0 Dispatch（带 t 事件名 + s 序号）
//! ```
//!
//! ## 重连纪律（踩坑换来的）
//!
//! - **4009（会话超时）必须清除 session 并重新 Identify**，不能 Resume ——
//!   会话已失效，Resume 只会无限失败。
//! - **4914/4915（机器人下线/封禁）是致命错误**，不应重连。
//! - 心跳任务必须与读循环解耦（独立 task），否则心跳会被事件处理阻塞。

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::api::OfficialApi;
use crate::config::OfficialConfig;
use crate::protocol::{CloseCode, EventType, HelloData, OpCode, Payload};

/// 网关错误。
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// API 调用失败
    #[error("获取网关地址失败: {0}")]
    Api(#[from] crate::api::ApiError),
    /// WebSocket 错误
    #[error("WebSocket 错误: {0}")]
    Ws(String),
    /// 连接关闭
    #[error("连接已关闭: code={code:?}")]
    Closed {
        /// 关闭码
        code: Option<u16>,
        /// 关闭原因
        reason: String,
    },
    /// 协议错误
    #[error("协议错误: {0}")]
    Protocol(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for GatewayError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        GatewayError::Ws(e.to_string())
    }
}

/// 一条从网关收到的事件。
#[derive(Debug, Clone)]
pub struct OfficialEvent {
    /// 事件类型
    pub event_type: EventType,
    /// 原始事件名
    pub raw_type: String,
    /// 事件载荷（`d` 字段）
    pub data: serde_json::Value,
    /// 事件序号（Resume 用）
    pub seq: Option<i64>,
}

/// 重连策略。
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// 最大重试次数（0 = 无限）
    pub max_retries: u32,
    /// 首次退避
    pub initial_backoff: Duration,
    /// 退避上限
    pub max_backoff: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0, // 无限重试（长跑服务）
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

impl ReconnectPolicy {
    /// 第 `attempt` 次重试的退避时长（指数退避，带上限）。
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 2u64.saturating_pow(attempt.min(6));
        let d = self.initial_backoff.saturating_mul(factor as u32);
        d.min(self.max_backoff)
    }
}

/// 建立网关连接，返回事件接收端。
///
/// 内部起一个**长驻后台任务**负责：连接 → 鉴权 → 心跳 → 读事件 → 断线重连。
/// 调用方只需消费返回的 `Receiver`。
pub async fn connect(
    config: &OfficialConfig,
) -> Result<mpsc::Receiver<OfficialEvent>, GatewayError> {
    config
        .validate()
        .map_err(|e| GatewayError::Protocol(e.to_string()))?;

    let (tx, rx) = mpsc::channel::<OfficialEvent>(256);
    let config = config.clone();

    tokio::spawn(async move {
        run_supervisor(config, tx).await;
    });

    Ok(rx)
}

/// 监督循环：负责连接并在断线后按策略重连。
async fn run_supervisor(config: OfficialConfig, tx: mpsc::Sender<OfficialEvent>) {
    let policy = ReconnectPolicy::default();
    let mut attempt: u32 = 0;
    // 会话状态（用于 Resume）
    let mut session_id: Option<String> = None;
    let mut last_seq: Option<i64> = None;

    loop {
        if !policy.max_retries == 0 && attempt >= policy.max_retries {
            tracing::error!("已达最大重连次数 {}，停止", policy.max_retries);
            return;
        }

        // 「本轮是否连通过」要跨 Err 分支回传，故用共享 AtomicBool 而非返回值。
        // 必须是 Sync 类型：run_once 里的 await 点要求整个 future 可 Send。
        let connected = std::sync::atomic::AtomicBool::new(false);

        match run_once(&config, &tx, session_id.clone(), last_seq, &connected).await {
            Ok(state) => {
                // 正常结束（接收端关闭）
                if tx.is_closed() {
                    tracing::info!("事件接收端已关闭，网关退出");
                    return;
                }
                session_id = state.session_id;
                last_seq = state.last_seq;
                attempt = 0; // 成功连过一轮，重置退避
            }
            Err(GatewayError::Closed { code, reason }) => {
                let cc = code.map(CloseCode::from_u16);
                if let Some(c) = cc {
                    if c.is_fatal() {
                        tracing::error!("致命关闭码 {:?}（{}），不再重连", c, reason);
                        return;
                    }
                    if c.must_reidentify() {
                        tracing::warn!(
                            "关闭码 {:?}（{}）：会话失效，清除 session 并重新 Identify",
                            c,
                            reason
                        );
                        session_id = None;
                        last_seq = None;
                    }
                }
                attempt = next_attempt(
                    attempt,
                    connected.load(std::sync::atomic::Ordering::Relaxed),
                );
                let backoff = policy.backoff(attempt);
                tracing::warn!(
                    "连接断开（{:?} {}），{} 秒后重连",
                    code,
                    reason,
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => {
                if tx.is_closed() {
                    return;
                }
                attempt = next_attempt(
                    attempt,
                    connected.load(std::sync::atomic::Ordering::Relaxed),
                );
                let backoff = policy.backoff(attempt);
                tracing::warn!("网关错误: {}；{} 秒后重连", e, backoff.as_secs());
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// 会话状态（供 Resume）。
struct SessionState {
    session_id: Option<String>,
    last_seq: Option<i64>,
}

/// 依据「本轮是否连通过」与「累计失败次数」决定下一档退避。
///
/// ## 为什么要单独抽出来
///
/// 退避计数器只在「正常结束」时归零是个陷阱：服务端主动重连（op=7）
/// 会让 `run_once` 以 `Err` 返回，于是**一条完全健康的长连接**每天被
/// 多罚一次退避：2s → 4s → 8s → … → 60s 上限后永久卡住。
/// 真正该被累加退避的只有「压根没连上」的失败。
///
/// 这个纯函数让该纪律可被单测覆盖（见 `backoff_resets_after_healthy_session`）。
fn next_attempt(attempt: u32, connected: bool) -> u32 {
    if connected {
        // 连通过 → 链路是通的，回到起点，别为服务端主动重连惩罚退避
        0
    } else {
        attempt.saturating_add(1)
    }
}

/// 单次连接生命周期。
///
/// `connected` 是给监督循环回传「本轮是否真的连通过」的出口：连接一旦
/// 收到 READY 就置位，之后即使以 `Err` 结束（服务端 op=7 主动重连、
/// 会话失效 4009……），监督循环也能据此把退避计数归零。
async fn run_once(
    config: &OfficialConfig,
    tx: &mpsc::Sender<OfficialEvent>,
    resume_session: Option<String>,
    resume_seq: Option<i64>,
    connected: &std::sync::atomic::AtomicBool,
) -> Result<SessionState, GatewayError> {
    let api = OfficialApi::new(config).map_err(|e| GatewayError::Protocol(e.to_string()))?;

    // ── 1. 取网关地址 ──
    let gateway_url = api.gateway_url().await?;
    tracing::info!("QQ 官方网关地址: {}", gateway_url);

    // ── 2. 建立 WebSocket ──
    let (ws, _) = tokio_tungstenite::connect_async(&gateway_url)
        .await
        .map_err(|e| GatewayError::Ws(format!("连接失败: {}", e)))?;
    let (mut write, mut read) = ws.split();

    // ── 3. 等 Hello，拿心跳间隔 ──
    let heartbeat_interval = wait_hello(&mut read).await?;
    tracing::info!("收到 Hello，心跳间隔: {:?}", heartbeat_interval);

    // ── 4. Identify 或 Resume ──
    let access_token = api
        .auth_header()
        .await?
        .trim_start_matches("QQBot ")
        .to_string();

    let payload = match (&resume_session, resume_seq) {
        (Some(sid), Some(seq)) => {
            tracing::info!("尝试 Resume（session={} seq={}）", sid, seq);
            Payload::resume(&access_token, sid, seq)
        }
        _ => {
            tracing::info!("发送 Identify（intents={}）", config.intents);
            Payload::identify(&access_token, config.intents, (0, 1))
        }
    };
    send_payload(&mut write, &payload).await?;

    // ── 5. 心跳 task（与读循环解耦）──
    //
    // 清理语义：本函数返回时 hb_tx 的所有克隆被 drop → hb_rx.recv() 返回 None
    // → task 自然结束；hb_timer 则在各返回路径上显式 abort()。
    let (hb_tx, mut hb_rx) = mpsc::channel::<Option<i64>>(4);
    let hb_write = write;
    let _hb_handle = tokio::spawn(async move {
        let mut w = hb_write;
        while let Some(seq) = hb_rx.recv().await {
            let p = Payload::heartbeat(seq);
            if let Err(e) = send_payload(&mut w, &p).await {
                tracing::warn!("心跳发送失败: {}", e);
                break;
            }
            tracing::debug!("心跳已发送（seq={:?}）", seq);
        }
        w
    });

    // 立刻发一次心跳（官方要求 Identify 后尽快开始心跳）
    let _ = hb_tx.send(resume_seq).await;

    // 独立定时器任务驱动心跳节奏
    let hb_timer = tokio::spawn({
        let interval = heartbeat_interval;
        let hb_tx = hb_tx.clone();
        async move {
            loop {
                tokio::time::sleep(interval).await;
                if hb_tx.send(None).await.is_err() {
                    break;
                }
            }
        }
    });

    // ── 6. 读循环 ──
    let mut session_id: Option<String> = resume_session;
    let mut last_seq: Option<i64> = resume_seq;

    loop {
        let msg = match read.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                hb_timer.abort();
                return Err(GatewayError::Ws(e.to_string()));
            }
            None => {
                hb_timer.abort();
                return Ok(SessionState {
                    session_id,
                    last_seq,
                });
            }
        };

        let text = match msg {
            WsMessage::Text(t) => t.to_string(),
            WsMessage::Close(frame) => {
                hb_timer.abort();
                let (code, reason) = match frame {
                    Some(f) => (Some(u16::from(f.code)), f.reason.to_string()),
                    None => (None, String::new()),
                };
                return Err(GatewayError::Closed { code, reason });
            }
            _ => continue,
        };

        let payload: Payload = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(
                    "无法解析的下行消息（已忽略）: {} | {}",
                    yq_core::truncate(&text, 200),
                    e
                );
                continue;
            }
        };

        match payload.opcode() {
            Some(OpCode::Dispatch) => {
                if let Some(s) = payload.s {
                    last_seq = Some(s);
                    // 同步最新序号给心跳任务
                    let _ = hb_tx.send(last_seq).await;
                }

                let raw_type = payload.t.clone().unwrap_or_default();
                let event_type = EventType::from_event_name(&raw_type);

                // READY 事件携带 session_id
                if raw_type == "READY" {
                    // 链路已确认可用 —— 退避计数可以归零
                    connected.store(true, std::sync::atomic::Ordering::Relaxed);
                    if let Some(d) = &payload.d {
                        session_id = d
                            .get("session_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        tracing::info!("已就绪（session={:?}）", session_id);
                    }
                }

                let ev = OfficialEvent {
                    event_type,
                    raw_type: raw_type.clone(),
                    data: payload.d.unwrap_or(serde_json::Value::Null),
                    seq: payload.s,
                };

                tracing::debug!("收到事件: {}", raw_type);

                if tx.send(ev).await.is_err() {
                    hb_timer.abort();
                    return Ok(SessionState {
                        session_id,
                        last_seq,
                    });
                }
            }
            Some(OpCode::HeartbeatAck) => {
                tracing::trace!("心跳已确认");
            }
            Some(OpCode::Reconnect) => {
                hb_timer.abort();
                tracing::warn!("服务端要求重连");
                return Err(GatewayError::Ws("服务端要求重连".to_string()));
            }
            Some(OpCode::InvalidSession) => {
                hb_timer.abort();
                tracing::warn!("会话无效，需重新 Identify");
                return Err(GatewayError::Closed {
                    code: Some(4009),
                    reason: "InvalidSession".to_string(),
                });
            }
            other => {
                tracing::trace!("未处理的 opcode: {:?}", other);
            }
        }
    }
}

/// 等待并解析 Hello。
async fn wait_hello<S>(read: &mut S) -> Result<Duration, GatewayError>
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    // 给 15 秒等 Hello，超时视为协议异常
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(GatewayError::Protocol("等待 Hello 超时".to_string()));
        }

        let msg = match tokio::time::timeout(remaining, read.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(GatewayError::Ws(e.to_string())),
            Ok(None) => {
                return Err(GatewayError::Closed {
                    code: None,
                    reason: "连接在 Hello 前关闭".to_string(),
                });
            }
            Err(_) => return Err(GatewayError::Protocol("等待 Hello 超时".to_string())),
        };

        let WsMessage::Text(text) = msg else {
            continue;
        };

        let Ok(payload) = serde_json::from_str::<Payload>(&text) else {
            continue;
        };

        if payload.op == OpCode::Hello as u8 {
            let data: HelloData = match payload.d {
                Some(d) => serde_json::from_value(d).unwrap_or(HelloData {
                    heartbeat_interval: 41250,
                }),
                None => HelloData {
                    heartbeat_interval: 41250,
                },
            };
            return Ok(data.interval());
        }
    }
}

/// 发送一个 payload。
async fn send_payload<S>(write: &mut S, payload: &Payload) -> Result<(), GatewayError>
where
    S: SinkExt<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    let text = serde_json::to_string(payload)
        .map_err(|e| GatewayError::Protocol(format!("序列化失败: {}", e)))?;
    write
        // 此处 WsMessage::Text 的载荷类型就是 String，`.into()` 是同型转换（空动作）
        .send(WsMessage::Text(text))
        .await
        .map_err(|e| GatewayError::Ws(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_with_cap() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.backoff(0), Duration::from_secs(1));
        assert_eq!(p.backoff(1), Duration::from_secs(2));
        assert_eq!(p.backoff(2), Duration::from_secs(4));
        assert_eq!(p.backoff(3), Duration::from_secs(8));
        // 上限 60 秒
        assert_eq!(p.backoff(10), Duration::from_secs(60));
    }

    #[test]
    fn backoff_saturates_on_huge_attempt() {
        let p = ReconnectPolicy::default();
        // 不应 panic（原来的 2u64.pow 会溢出）
        let d = p.backoff(100);
        assert_eq!(d, Duration::from_secs(60));
    }

    #[test]
    fn custom_policy_respects_initial_and_cap() {
        let p = ReconnectPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(5),
        };
        assert_eq!(p.backoff(0), Duration::from_millis(500));
        assert_eq!(p.backoff(1), Duration::from_secs(1));
        assert_eq!(p.backoff(5), Duration::from_secs(5));
    }

    // ── 退避重置纪律（回归测试，2026-10-06）──
    //
    // 生产实测：健康连接被服务端每小时要求重连一次（op=7），
    // 退避档位逐日 2s → 4s → 8s 递增，几天后永久卡在 60s 上限。
    // 根因是计数器只在 Ok 分支归零，而 op=7 走的是 Err 分支。

    #[test]
    fn backoff_resets_after_healthy_session() {
        // 服务端主动重连：连通过 → 计数归零
        assert_eq!(next_attempt(0, true), 0);
        assert_eq!(next_attempt(1, true), 0);
        assert_eq!(next_attempt(5, true), 0);
        assert_eq!(next_attempt(59, true), 0);
    }

    #[test]
    fn backoff_grows_only_when_never_connected() {
        // 压根没连上（DNS 失败 / 被墙 / 鉴权前断线）→ 继续累加
        assert_eq!(next_attempt(0, false), 1);
        assert_eq!(next_attempt(1, false), 2);
        assert_eq!(next_attempt(5, false), 6);
    }

    #[test]
    fn hourly_server_reconnect_never_escalates() {
        // 复现生产场景：每小时一次服务端主动重连，连一天都不该涨退避
        let mut attempt = 0u32;
        for _ in 0..24 {
            attempt = next_attempt(attempt, true);
        }
        assert_eq!(attempt, 0, "健康连接下退避计数必须始终为 0");
        assert_eq!(
            ReconnectPolicy::default().backoff(attempt),
            Duration::from_secs(1),
            "第一档退避不应被累积惩罚"
        );
    }

    #[test]
    fn genuine_outage_still_escalates_then_recovers() {
        // 真断网：连续失败应当逐级退避（别把真故障也压成 1s 硬重连）
        let mut attempt = 0u32;
        for _ in 0..4 {
            attempt = next_attempt(attempt, false);
        }
        assert_eq!(attempt, 4);
        // 网络恢复（这一轮连上了）→ 立刻回到起点
        assert_eq!(next_attempt(attempt, true), 0);
    }

    #[test]
    fn backoff_does_not_overflow_on_pathological_repeat() {
        // 连不上又连不上很久：饱和而非回绕
        let mut attempt = 0u32;
        for _ in 0..10_000 {
            attempt = next_attempt(attempt, false);
        }
        assert_eq!(attempt, 10_000);
        // 上限依然生效
        assert_eq!(
            ReconnectPolicy::default().backoff(attempt),
            Duration::from_secs(60)
        );
    }
}
