//! 流式 token 源 —— 阶段二 mock 实现
//!
//! 硬性约束：
//!   · **不产生随机数 / 时间戳**（避免污染固定区）
//!   · 阶段二不接真实 LLM —— mock 源就是输入字符串本身
//!   · 阶段四将替换为真实 LLM 流式响应，但 `TokenChunk` / `mpsc` 接口不变

use std::time::Duration;

use tokio::sync::mpsc;

/// 一个流式输出的 token 块。
#[derive(Debug, Clone)]
pub struct TokenChunk {
    /// 文本片段（阶段二为单个字符；阶段四为 LLM 返回的 token）
    pub text: String,
    /// 是否为流的最后一个块
    pub is_final: bool,
}

/// 把字符串按字符切分成 token 流（mock）。
///
/// 用于 `repl` / `tui` 在**没有真实 LLM** 时演示逐 token 渲染。
/// 字符间隔固定（不引入随机延迟），保证行为可复现。
pub fn mock_token_stream(text: &str) -> mpsc::Receiver<TokenChunk> {
    mock_token_stream_with_delay(text, Duration::from_millis(20))
}

/// 同上，可指定字符间隔。
pub fn mock_token_stream_with_delay(text: &str, delay: Duration) -> mpsc::Receiver<TokenChunk> {
    let (tx, rx) = mpsc::channel(64);
    let owned = text.to_string();

    tokio::spawn(async move {
        let chars: Vec<char> = owned.chars().collect();
        let total = chars.len();
        for (idx, ch) in chars.into_iter().enumerate() {
            let chunk = TokenChunk {
                text: ch.to_string(),
                is_final: idx + 1 == total,
            };
            if tx.send(chunk).await.is_err() {
                // 接收端已关闭，停止发送
                return;
            }
            tokio::time::sleep(delay).await;
        }
        // 空字符串：发送一个 final 空块，保证接收端能感知结束
        if total == 0 {
            let _ = tx
                .send(TokenChunk {
                    text: String::new(),
                    is_final: true,
                })
                .await;
        }
    });

    rx
}
