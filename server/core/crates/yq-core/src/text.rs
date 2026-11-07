//! 文本工具 —— 全局共用的纯字符串处理
//!
//! 硬性约束：
//!   · 这些函数跑在**错误 / 日志路径**上，调用频繁但每次只需一行输出，
//!     因此**不得**为整个输入分配 `Vec<char>`
//!   · 按**字符数**（不是字节数）截断，中文/emoji 安全
//!   · 截断结果默认**不带标记**，需要标记的调用方显式选择 `truncate_with_marker`

/// 按字符数截断文本，超长时返回前 `max` 个字符，**不带任何标记**。
///
/// - `s` 长度（按字符）`<= max` 时原样返回
/// - 否则返回前 `max` 个字符，**不追加省略号**（调用方自行决定是否需要标记）
/// - `max == 0` 且输入非空时返回空串
///
/// 实现说明：用 `char_indices()` 定位第 `max` 个字符的字节边界再切片，
/// 避免 `chars().collect::<Vec<char>>()` 对整个输入的 O(n) 堆分配。
pub fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        // 没有第 `max` 个字符，说明按字符数没超长，原样返回
        None => s.to_string(),
        // `(byte_idx, _)` 是第 `max` 个字符的起始字节，切到它之前即为前 `max` 个字符
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
    }
}

/// 按字符数截断文本，超长时在结果末尾追加 `marker`（`…(截断)`）。
///
/// 与 [`truncate`] 的唯一区别是**截断时会追加标记**，用于错误体这类
/// 需要让用户一眼看出"内容被截过"的日志场景。
pub fn truncate_with_marker(s: &str, max: usize, marker: &str) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((byte_idx, _)) => {
            let mut t = String::with_capacity(byte_idx + marker.len());
            t.push_str(&s[..byte_idx]);
            t.push_str(marker);
            t
        }
    }
}

/// 按字符数截断文本，超长时追加单个省略号 `…`（**计入截断结果**）。
///
/// 与 [`truncate`] 的区别：截断时**会**多出一个 `…` 字符，
/// 因此结果长度可能为 `max + 1`。用于面向用户的展示位（保持既有观感）。
pub fn truncate_ellipsis(s: &str, max: usize) -> String {
    truncate_with_marker(s, max, "…")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 本模块的测试全部**迁移自**各 crate 的本地 truncate 测试，
    // 保证「按字符截断」这一行为的覆盖不因去重而丢失。

    /// 迁移自 api.rs `truncate_keeps_short_text_intact` + gateway.rs `truncate_helper_handles_cjk`
    /// （后者的 `truncate("短", 10)` 断言与此重复，合并为同一条）
    #[test]
    fn truncate_keeps_short_text_intact() {
        assert_eq!(truncate("短", 10), "短");
    }

    /// 迁移自 gateway.rs `truncate_helper_handles_cjk`：按字符切，不切坏多字节
    #[test]
    fn truncate_handles_cjk() {
        assert_eq!(truncate("你好世界", 2), "你好");
    }

    /// 迁移自 api.rs `truncate_cuts_long_text`：截断到上限附近。
    /// 无标记版长度恰为 `max`；省略号版为 `max + 1`（含 `…`）；
    /// 标记版保留 client.rs 的用户可见 `…(截断)` 字面量。
    #[test]
    fn truncate_cuts_long_text() {
        let long = "啊".repeat(500);
        assert_eq!(truncate(&long, 10).chars().count(), 10);
        assert_eq!(truncate_ellipsis(&long, 10).chars().count(), 11); // 10 + 省略号
        assert_eq!(
            truncate_with_marker(&"x".repeat(1000), 5, "…(截断)"),
            "xxxxx…(截断)"
        );
    }
}
