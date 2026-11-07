//! 错误上报 —— 把「出什么事了」如实送到用户面前
//!
//! ## 为什么需要它
//!
//! 原实现里大量失败路径是 `tracing::warn!` 或 `let _ =`：
//! 日志里有，**用户那边一片寂静** —— 任务不出结果、消息没回音，
//! 却不知道为什么。用户根本无从判断是"还在跑"还是"已经死了"。
//!
//! 本模块提供 [`Notifier`]，把关键失败**推回同一个对话**。
//!
//! ## 分级原则（防止刷屏）
//!
//! · **Error** —— 用户的请求真的失败了（agent 挂了、发不出去、回滚失败）→ 必须推
//! · **Warn**  —— 降级但仍能继续（记忆检索失败、备份没做成）→ 推一次
//! · **Info**  —— 系统噪音（心跳、重连抖动）→ 只进日志，不推
//!
//! ## 发送自身失败怎么办
//!
//! 推送失败时**没有第二渠道可退**（对话就是唯一通道）。因此：
//! · 发送重试 3 次（指数退避）
//! · 仍然失败 → 记 error 级日志（供事后排查），并放弃
//!   （继续重试只会堆积，反而拖垮后续消息）

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use yq_qqofficial::adapter::Scene;
use yq_qqofficial::api::{OfficialApi, SendMessageBody};

/// 把错误推回用户对话的辅助器。
#[derive(Clone)]
pub struct Notifier {
    api: OfficialApi,
    scene: Scene,
    group_openid: Option<String>,
    user_openid: String,
    /// 被动回复锚定的消息 ID
    msg_id: String,
    /// 消息序号计数器（QQ 靠它去重）
    seq: Arc<AtomicU32>,
}

impl Notifier {
    /// 构造。
    pub fn new(
        api: OfficialApi,
        scene: Scene,
        group_openid: Option<String>,
        user_openid: String,
        msg_id: String,
        seq: Arc<AtomicU32>,
    ) -> Self {
        Self {
            api,
            scene,
            group_openid,
            user_openid,
            msg_id,
            seq,
        }
    }

    /// 取下一个消息序号。
    fn next_seq(&self) -> u32 {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        (n % 10_000) + 1
    }

    /// 报告一个错误（用户可见）。
    ///
    /// `what` 是「哪一步出错了」，`detail` 是具体原因。
    pub async fn error(&self, what: &str, detail: &str) {
        let text = format!("⚠️ 出错了：{what}\n原因：{detail}");
        self.send(&text).await;
        tracing::error!("已向用户报告错误 —— {}: {}", what, detail);
    }

    /// 报告一个警告（降级但继续）。
    pub async fn warn(&self, text: &str) {
        let text = format!("⚠️ {text}");
        self.send(&text).await;
        tracing::warn!("已向用户报告警告 —— {}", text);
    }

    /// 发送一条提示（不做任何包装，调用方自己写好）。
    ///
    /// 预留给后续使用（如主动通知、心跳提醒等）。
    #[allow(dead_code)]
    pub async fn notice(&self, already_formatted: &str) {
        self.send(already_formatted).await;
    }

    /// 底层发送：转纯文本 → 分片 → 重试。
    async fn send(&self, text: &str) {
        let plain = yq_qq::markdown_to_plain(text);
        let parts = yq_qq::format_reply_with_limit(&plain, yq_qq::QQ_OFFICIAL_MAX_CHARS);

        for (idx, part) in parts.iter().enumerate() {
            let body = if idx == 0 && !self.msg_id.is_empty() {
                SendMessageBody::reply(part, &self.msg_id, self.next_seq())
            } else {
                SendMessageBody::text(part)
            };

            if let Err(e) = self.send_once(&body).await {
                tracing::error!("错误通知发送失败（用户不会看到这条）: {}", e);
            }
        }
    }

    /// 单次发送（带重试）。
    async fn send_once(&self, body: &SendMessageBody) -> Result<(), String> {
        const MAX_ATTEMPTS: u32 = 3;

        for attempt in 0..MAX_ATTEMPTS {
            let result = match self.scene {
                Scene::Group => {
                    self.api
                        .send_group_message(self.group_openid.as_deref().unwrap_or(""), body)
                        .await
                }
                _ => self.api.send_c2c_message(&self.user_openid, body).await,
            };

            match result {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    // 频率限制/去重这类错误，退避后重试有意义；
                    // 参数错误重试也没用，直接放弃
                    let retryable = msg.contains("频控")
                        || msg.contains("频率")
                        || msg.contains("500")
                        || msg.contains("timeout")
                        || msg.contains("超时");

                    if attempt + 1 == MAX_ATTEMPTS || !retryable {
                        return Err(format!("第 {} 次尝试后放弃: {}", attempt + 1, msg));
                    }

                    let backoff = std::time::Duration::from_millis(300 * 2u64.pow(attempt));
                    tracing::warn!("发送失败（{}），{:?} 后重试", msg, backoff);
                    tokio::time::sleep(backoff).await;
                }
            }
        }

        Err("重试耗尽".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_seq_stays_in_official_range() {
        let counter = Arc::new(AtomicU32::new(0));
        let api =
            yq_qqofficial::api::OfficialApi::new(&yq_qqofficial::OfficialConfig::new("1", "s"));
        // OfficialApi::new 需要能构造出来（不发请求）
        if let Ok(api) = api {
            let n = Notifier::new(api, Scene::C2c, None, "u".into(), "m".into(), counter);
            for _ in 0..5 {
                let s = n.next_seq();
                assert!((1..=10_000).contains(&s), "序号越界: {}", s);
            }
        }
    }
}
