//! yq-qqofficial —— QQ 开放平台**官方机器人**接入
//!
//! ## 与 `yq-onebot` 的区别
//!
//! | | yq-onebot | yq-qqofficial |
//! |---|---|---|
//! | 对端 | 第三方客户端（NapCat 等） | 腾讯官方服务端 |
//! | 协议 | OneBot11 | QQ Bot API v2 |
//! | 需要 QQ 客户端 | 是 | **否** |
//! | 鉴权 | Token / 无 | Access Token（2 小时过期，自动刷新） |
//! | 事件来源 | 本地 WS | 官方 WebSocket 网关 |
//!
//! ## 模块
//!
//! - `auth`     —— Access Token 获取与自动刷新（`POST /app/getAppAccessToken`）
//! - `protocol` —— WebSocket opcode、intents、事件类型定义
//! - `api`      —— REST API（发群消息 / 私聊 / 查询机器人信息）
//! - `gateway`  —— WebSocket 网关（Hello → Identify → Heartbeat → 事件分发 → Resume）
//! - `adapter`  —— 把官方事件转换为 `yq-qq` 的通用消息模型
//!
//! ## 快速用法
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use yq_qqofficial::{OfficialConfig, gateway};
//!
//! let cfg = OfficialConfig::from_env()?;
//! let mut events = gateway::connect(&cfg).await?;
//! while let Some(ev) = events.recv().await {
//!     println!("收到事件: {:?}", ev.event_type);
//! }
//! # Ok(())
//! # }
//! ```

pub mod adapter;
pub mod api;
pub mod auth;
pub mod config;
pub mod gateway;
pub mod protocol;

pub use adapter::{OfficialMessage, official_to_incoming};
pub use api::OfficialApi;
pub use auth::TokenManager;
pub use config::OfficialConfig;
pub use protocol::{EventType, Intents, OpCode, Payload};
