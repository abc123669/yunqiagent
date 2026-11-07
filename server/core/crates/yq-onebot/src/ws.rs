//! OneBot11 WebSocket 连接层
//!
//! 支持两种模式（SPEC 第十五节）：
//!   · **正向（forward）**：本进程作为客户端，连到 OneBot 实现（NapCat 等）的 WS 服务端
//!   · **反向（reverse）**：本进程起 WS 服务端，OneBot 实现反向连入
//!
//! 解耦要点：本模块**只做协议收发**，不含任何 QQ 业务逻辑（路由/权限/格式在 yq-qq）。

use std::net::SocketAddr;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::protocol::{Action, ApiResponse, Event};

/// 连接模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 正向：客户端连出去
    Forward,
    /// 反向：服务端接受连入
    Reverse,
}

/// 事件流（从 OneBot 收到的消息事件）。
pub type EventStream = mpsc::Receiver<Event>;

/// 动作流（待发送到 OneBot 的动作）。
pub type ActionStream = mpsc::Receiver<Action>;

/// OneBot 连接句柄。
pub struct OneBotConnection {
    /// 收到的事件流
    pub events: EventStream,
    /// 待发送动作的发送端
    pub actions: mpsc::Sender<Action>,
    /// 模式
    pub mode: Mode,
}

/// 启动 OneBot 连接（无鉴权，兼容 NapCat 等本地实现）。
pub async fn connect(mode: Mode, endpoint: &str) -> Result<OneBotConnection> {
    connect_with_token(mode, endpoint, None).await
}

/// 启动 OneBot 连接，可带 Bearer token。
///
/// 正向模式：`endpoint` 形如 `ws://127.0.0.1:3001`；
///          需要鉴权的服务端（如 Kenkin Chat）用 `token` 传令牌。
/// 反向模式：`endpoint` 形如 `0.0.0.0:6700`（监听地址）；`token` 忽略。
pub async fn connect_with_token(
    mode: Mode,
    endpoint: &str,
    token: Option<String>,
) -> Result<OneBotConnection> {
    let (event_tx, event_rx) = mpsc::channel::<Event>(64);
    let (action_tx, action_rx) = mpsc::channel::<Action>(64);

    match mode {
        Mode::Forward => {
            let url = endpoint.to_string();
            let token = token.clone();
            tokio::spawn(async move {
                if let Err(e) = forward_loop(&url, token, event_tx, action_rx).await {
                    tracing::error!("正向 WS 连接结束: {}", e);
                }
            });
        }
        Mode::Reverse => {
            let addr: SocketAddr = endpoint
                .parse()
                .with_context(|| format!("反向监听地址不合法: {}", endpoint))?;
            tokio::spawn(async move {
                if let Err(e) = reverse_loop(addr, event_tx, action_rx).await {
                    tracing::error!("反向 WS 服务结束: {}", e);
                }
            });
        }
    }

    Ok(OneBotConnection {
        events: event_rx,
        actions: action_tx,
        mode,
    })
}

/// 正向连接循环。
///
/// `bearer_token` 非空时，会用 `Authorization: Bearer <token>` 完成握手。
///
/// ## 为什么需要自定义 header（2026-10-06 加）
///
/// Kenkin Chat 的反向 WS 端点要求 `Authorization: Bearer <token>` 鉴权；
/// 而 NapCat 之类的本地 OneBot 实现通常不需要。因此把 token 做成可选参数，
/// 两种场景共用同一条正向连接路径。
async fn forward_loop(
    url: &str,
    bearer_token: Option<String>,
    event_tx: mpsc::Sender<Event>,
    mut action_rx: ActionStream,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = url
        .into_client_request()
        .with_context(|| format!("构造 WS 请求失败: {}", url))?;

    if let Some(token) = bearer_token.as_deref() {
        if !token.trim().is_empty() {
            let value = format!("Bearer {}", token.trim());
            request.headers_mut().insert(
                "Authorization",
                value
                    .parse()
                    .context("Authorization 头构造失败（token 含非法字符？）")?,
            );
            tracing::debug!("正向 WS 已附加 Authorization 头");
        }
    }

    let (ws_stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("无法连接正向 WS: {}", url))?;
    tracing::info!("正向 WS 已连接: {}", url);

    let (mut write, mut read) = ws_stream.split();

    // 读循环
    let read_task = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Text(text)) => {
                    if let Ok(ev) = serde_json::from_str::<Event>(&text) {
                        if event_tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(WsMessage::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    // 写循环
    while let Some(action) = action_rx.recv().await {
        let text = serde_json::to_string(&action)?;
        if write.send(WsMessage::Text(text)).await.is_err() {
            break;
        }
    }

    read_task.abort();
    Ok(())
}

/// 反向服务循环。
async fn reverse_loop(
    addr: SocketAddr,
    event_tx: mpsc::Sender<Event>,
    mut action_rx: ActionStream,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("无法监听反向 WS 地址: {}", addr))?;
    tracing::info!("反向 WS 服务已监听: {}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        let ws = tokio_tungstenite::accept_async(stream).await?;
        tracing::info!("反向 WS 已接入: {}", peer);

        let (mut write, mut read) = ws.split();
        let event_tx2 = event_tx.clone();
        let peer2 = peer;

        // 读循环（每个连接一个）
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(WsMessage::Text(text)) => {
                        if let Ok(ev) = serde_json::from_str::<Event>(&text) {
                            if event_tx2.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(WsMessage::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            tracing::info!("反向 WS 连接断开: {}", peer2);
        });

        // 写循环：本连接存活期间消费共享动作流
        let send_res = send_actions(&mut write, &mut action_rx).await;
        if let Err(e) = send_res {
            tracing::warn!("反向 WS 发送循环结束: {}", e);
        }
    }
}

/// 从动作流取动作并发往 WS。
async fn send_actions<S>(write: &mut S, action_rx: &mut ActionStream) -> Result<()>
where
    S: SinkExt<WsMessage> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    while let Some(action) = action_rx.recv().await {
        let text = serde_json::to_string(&action)?;
        write.send(WsMessage::Text(text)).await?;
    }
    Ok(())
}

/// 解析 OneBot 响应文本。
pub fn parse_response(text: &str) -> Option<ApiResponse> {
    serde_json::from_str(text).ok()
}
