//! 响应格式化 —— `[心跳]` 前缀、≤200 字分片（SPEC 第九 / 十五节）。
//!
//! QQ 风控约束：
//!   · 单条消息不超过 200 字
//!   · 超长自动分片
//!   · 心跳消息加 `[心跳]` 前缀区分
//!   · 不用特殊字符堆砌，不刷屏

/// QQ 单条消息最大字符数（**OneBot11 / NapCat 路径**）。
///
/// 这是给第三方客户端路径的保守值（NapCat 风控较严）。
pub const QQ_MAX_MESSAGE_CHARS: usize = 200;

/// QQ **官方开放平台** 单条消息的字符上限。
///
/// 官方限制为 **2000 字**（远宽于 NapCat 路径的 200）。
/// 用这个值可让正常长度的回复一条发完，避免无谓的分片。
pub const QQ_OFFICIAL_MAX_CHARS: usize = 2000;

/// 心跳消息前缀（与 yq-heartbeat 保持一致）。
pub const HEARTBEAT_PREFIX: &str = "[心跳] ";

/// 分片标记（续条时附加）。
pub const CONTINUATION_MARK: &str = "（续）";

/// 把长文本按 ≤`max_chars` 分片。
///
/// 优先在**换行**处切分，其次在**句末标点**，最后硬切 —— 可读性更好。
pub fn split_for_qq(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![text.to_string()];
    }

    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return vec![text.to_string()];
    }

    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    // 按行处理，行内超长再按句末标点切
    for line in text.split_inclusive('\n') {
        for piece in split_line_for_qq(line, max_chars) {
            let piece_len = piece.chars().count();
            if current_len + piece_len > max_chars && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_len = 0;
            }
            current.push_str(&piece);
            current_len += piece_len;
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

/// 单行切分：优先在句末标点处断开。
fn split_line_for_qq(line: &str, max_chars: usize) -> Vec<String> {
    if line.chars().count() <= max_chars {
        return vec![line.to_string()];
    }

    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for ch in line.chars() {
        current.push(ch);
        current_len += 1;

        let is_break = matches!(ch, '。' | '！' | '？' | '；' | '\n' | '.' | '!' | '?');
        if current_len >= max_chars || (is_break && current_len >= max_chars / 2) {
            out.push(std::mem::take(&mut current));
            current_len = 0;
        }
    }

    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 格式化一次普通回复（分片 + 续条标记）。
pub fn format_reply(text: &str) -> Vec<String> {
    let parts = split_for_qq(text, QQ_MAX_MESSAGE_CHARS);
    mark_continuation(parts)
}

/// 格式化回复（指定单条上限）。
///
/// 官方开放平台路径应传 [`QQ_OFFICIAL_MAX_CHARS`]（900），
/// 避免正常长度的回复被无谓切分。
pub fn format_reply_with_limit(text: &str, max_chars: usize) -> Vec<String> {
    let parts = split_for_qq(text, max_chars);
    mark_continuation(parts)
}

/// 格式化心跳消息：加 `[心跳]` 前缀后分片。
pub fn format_heartbeat_message(text: &str) -> Vec<String> {
    let body = text.strip_prefix(HEARTBEAT_PREFIX).unwrap_or(text);
    let full = format!("{}{}", HEARTBEAT_PREFIX, body);
    let parts = split_for_qq(&full, QQ_MAX_MESSAGE_CHARS);
    mark_continuation(parts)
}

/// 给多片消息加续条标记（仅当多于一片时）。
fn mark_continuation(parts: Vec<String>) -> Vec<String> {
    let n = parts.len();
    if n <= 1 {
        return parts;
    }
    parts
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                p
            } else {
                format!("{}{}", CONTINUATION_MARK, p)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_single_chunk() {
        let parts = split_for_qq("你好", 200);
        assert_eq!(parts, vec!["你好".to_string()]);
    }

    #[test]
    fn long_text_is_split_and_each_piece_within_limit() {
        let text = "这是一句话。".repeat(80); // 约 480 字
        let parts = split_for_qq(&text, 200);
        assert!(parts.len() > 1, "应被分片");
        for p in &parts {
            assert!(
                p.chars().count() <= 200,
                "分片超长: {} 字",
                p.chars().count()
            );
        }
        // 分片内容拼回去应与原文一致（忽略可能的换行边界差异）
        let joined: String = parts.concat();
        assert_eq!(joined.chars().count(), text.chars().count());
    }

    #[test]
    fn heartbeat_prefix_is_added_once() {
        let parts = format_heartbeat_message("该浇花了");
        assert_eq!(parts.len(), 1);
        assert!(parts[0].starts_with(HEARTBEAT_PREFIX));
        assert_eq!(parts[0], "[心跳] 该浇花了");
    }

    #[test]
    fn heartbeat_prefix_not_duplicated() {
        let parts = format_heartbeat_message("[心跳] 该浇花了");
        assert_eq!(parts[0], "[心跳] 该浇花了");
    }

    #[test]
    fn continuation_mark_added_on_extra_parts() {
        let text = "甲".repeat(500);
        let parts = format_reply(&text);
        assert!(parts.len() > 1);
        assert!(!parts[0].starts_with(CONTINUATION_MARK));
        assert!(parts[1].starts_with(CONTINUATION_MARK));
    }

    #[test]
    fn cjk_measured_in_chars_not_bytes() {
        // 200 个汉字 = 600 字节，但只算 200 字符 → 不应分片
        let text = "汉".repeat(200);
        let parts = split_for_qq(&text, 200);
        assert_eq!(parts.len(), 1);
    }
}
