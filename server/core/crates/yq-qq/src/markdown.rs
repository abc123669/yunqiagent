//! Markdown → 纯文本转换 —— QQ 官方机器人消息渲染修复
//!
//! ## 问题（2026-10-05 实测）
//!
//! QQ 开放平台的群/私聊消息 `content` 字段是**纯文本**模式，
//! 但模型输出的回复带 Markdown 标记，原样发出后用户看到的是：
//!
//! ```text
//! **核心**
//! | 模块 | 说明 |
//! |---|---|
//! ```
//!
//! 也就是"一堆星号和一堆竖杠"。消息平台的富文本渲染由**发送方**决定，
//! 官方 Bot API 不会替我们渲染 Markdown。
//!
//! ## 本模块做什么
//!
//! 把 Markdown 降级为**人类可读的纯文本**：
//!   · 去粗体/斜体/删除线标记，保留文字
//!   · 去标题 `#`，保留文字
//!   · 行内代码去掉反引号
//!   · **表格转成缩进列表**（竖杠是用户吐槽的重点）
//!   · 列表符号统一成 `·`（原版字体友好）
//!   · 链接降级为 `文字 (url)`
//!
//! 注意：本项目另有约定「MC 菜单/标题要纯文字」——
//! 同理，发给 QQ 的文本也**不应含特殊装饰符**。

/// 把 Markdown 文本转换为适合 QQ 纯文本消息的形式。
pub fn markdown_to_plain(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];

        // ── 表格：连续的 | 开头行，整体转换 ──
        if is_table_line(line) {
            let mut table: Vec<&str> = Vec::new();
            while i < lines.len() && is_table_line(lines[i]) {
                table.push(lines[i]);
                i += 1;
            }
            out.push_str(&render_table(&table));
            out.push('\n');
            continue;
        }

        // ── 普通行 ──
        out.push_str(&convert_inline(line));
        out.push('\n');
        i += 1;
    }

    // 去掉结尾多余空行。
    //
    // **注意**：这里**不能**再补回换行 —— 消息是发给 QQ 的，
    // 尾部换行会让每条消息凭空多一个空行（用户实测反馈）。
    // 行间的结构性换行在循环里已经保留了。
    out.trim_end_matches('\n').to_string()
}

/// 判断是否为表格行（以 `|` 开头且含至少两个 `|`）。
fn is_table_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.matches('|').count() >= 2
}

/// 判断是否为表格分隔行（`|---|---|`）。
fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') {
        return false;
    }
    t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) && t.contains('-')
}

/// 把表格渲染成缩进列表。
///
/// 形如：
/// ```text
/// · 模块：说明
/// · 模块2：说明2
/// ```
fn render_table(table: &[&str]) -> String {
    if table.is_empty() {
        return String::new();
    }

    // 拆单元格
    let rows: Vec<Vec<String>> = table
        .iter()
        .filter(|l| !is_table_separator(l))
        .map(|l| split_row(l))
        .collect();

    if rows.is_empty() {
        return String::new();
    }

    let header = &rows[0];
    let body = &rows[1..];

    // 单列：直接列出
    if header.len() <= 1 {
        return rows
            .iter()
            .map(|r| format!("· {}", r.join(" ")))
            .collect::<Vec<_>>()
            .join("\n");
    }

    // 多列：首列作标题，其余列作 `列名：值`
    let mut out = Vec::new();
    for row in body {
        let title = row.first().cloned().unwrap_or_default();
        let mut parts = Vec::new();
        for (idx, cell) in row.iter().enumerate().skip(1) {
            let key = header.get(idx).cloned().unwrap_or_default();
            if cell.trim().is_empty() {
                continue;
            }
            if key.trim().is_empty() {
                parts.push(cell.trim().to_string());
            } else {
                parts.push(format!("{}：{}", key.trim(), cell.trim()));
            }
        }
        if parts.is_empty() {
            out.push(format!("· {}", title.trim()));
        } else {
            out.push(format!("· {}（{}）", title.trim(), parts.join("；")));
        }
    }

    if out.is_empty() {
        // 只有表头没有内容行：退化为列出表头
        return format!("· {}", header.join(" / "));
    }

    out.join("\n")
}

/// 按 `|` 拆分一行，去掉首尾空单元格。
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim().trim_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// 处理行内 Markdown 标记。
fn convert_inline(line: &str) -> String {
    let mut s = line.to_string();

    // 标题：`### 标题` → `标题`
    let trimmed_start = s.trim_start().to_string();
    if let Some(rest) = strip_heading(&trimmed_start) {
        let indent = s.len() - trimmed_start.len();
        s = format!("{}{}", " ".repeat(indent), rest);
    }

    // 引用：`> 内容` → `内容`
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix("> ") {
        s = rest.to_string();
    } else if let Some(rest) = t.strip_prefix('>') {
        s = rest.to_string();
    }

    // 列表符号统一：`- ` / `* ` / `+ ` → `· `
    let t = s.trim_start();
    let indent: String = s[..s.len() - t.len()].to_string();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            s = format!("{}· {}", indent, rest);
            break;
        }
    }
    // 有序列表保持数字，只规范空格
    // 任务列表 `- [ ] x` → `· [ ] x`
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix("· [ ] ") {
        s = format!("{}· 待办 {}", indent, rest);
    } else if let Some(rest) = t.strip_prefix("· [x] ") {
        s = format!("{}· 完成 {}", indent, rest);
    }

    // 行内代码 `` `code` `` → `code`
    s = remove_inline_code(&s);

    // 粗体/斜体/删除线
    s = remove_emphasis(&s);

    // 链接 `[文字](url)` → `文字 (url)`
    s = convert_links(&s);

    // 水平线 `---` / `***` → 短横线
    let t = s.trim();
    if t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
    {
        s = "────────".to_string();
    }

    s.trim_end().to_string()
}

/// 去掉标题标记，返回正文。
fn strip_heading(line: &str) -> Option<String> {
    let hashes: usize = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    // `#标题` 或 `# 标题` 都算
    Some(rest.trim_start().to_string())
}

/// 去掉行内代码的反引号。
fn remove_inline_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_code = false;
    for ch in s.chars() {
        if ch == '`' {
            in_code = !in_code;
            continue;
        }
        out.push(ch);
    }
    out
}

/// 去掉粗体 `**x**`、斜体 `*x*`、删除线 `~~x~~`、下划线 `__x__` 的标记。
fn remove_emphasis(s: &str) -> String {
    let mut out = s.replace("**", "");
    out = out.replace("~~", "");
    // 下划线强调：仅当两侧都是字母/汉字时才当标记（避免误伤 snake_case）
    out = remove_underscore_emphasis(&out);

    // 单星号斜体：成对出现才处理
    out = remove_paired_single_asterisk(&out);
    out
}

/// 去掉 `_强调_`（**不碰** `foo_bar` 这种标识符）。
///
/// 判据（对齐 CommonMark）：`_` 强调要求**词边界** ——
/// 起始 `_` 前面必须是行首/空白/标点（不能紧邻字母数字），
/// 结束 `_` 后面同理。这样 `tool_call_id`、`snake_case` 这类标识符不会被误伤。
fn remove_underscore_emphasis(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] == '_' && is_word_boundary_before(&chars, i) {
            // 找配对的结束 `_`：中间不能有空白，且结束 `_` 后必须是词边界
            let mut j = i + 1;
            let mut found = None;
            while j < chars.len() {
                if chars[j] == '_' && is_word_boundary_after(&chars, j) {
                    found = Some(j);
                    break;
                }
                if chars[j].is_whitespace() {
                    break;
                }
                j += 1;
            }
            if let Some(end) = found {
                if end > i + 1 {
                    // 是强调：跳过标记、保留内容
                    for c in &chars[i + 1..end] {
                        out.push(*c);
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

/// 位置 `i` 之前是否为词边界（行首或非字母数字）。
fn is_word_boundary_before(chars: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = chars[i - 1];
    !prev.is_alphanumeric()
}

/// 位置 `i` 之后是否为词边界（行尾或非字母数字）。
fn is_word_boundary_after(chars: &[char], i: usize) -> bool {
    if i + 1 >= chars.len() {
        return true;
    }
    let next = chars[i + 1];
    !next.is_alphanumeric()
}

/// 去掉成对的单星号（斜体），保留文字。
fn remove_paired_single_asterisk(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut pending: Option<usize> = None; // 已遇到的单个 `*` 位置

    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '*' {
            match pending {
                None => {
                    pending = Some(out.len());
                    i += 1;
                    continue;
                }
                Some(mark) => {
                    // 配对成功：中间的文本已写入，只需忽略这对标记
                    let _ = mark;
                    pending = None;
                    i += 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }

    // 落单的 `*` 还原（避免吃掉乘号等）
    if let Some(mark) = pending {
        let mut with_star = out[..mark].to_string();
        with_star.push('*');
        with_star.push_str(&out[mark..]);
        return with_star;
    }

    out
}

/// `[文字](url)` → `文字 (url)`
fn convert_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] == '[' {
            // 找 `](`
            let mut j = i + 1;
            let mut text_end = None;
            while j + 1 < chars.len() {
                if chars[j] == ']' && chars[j + 1] == '(' {
                    text_end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(te) = text_end {
                // 找右括号
                let mut k = te + 2;
                let mut url_end = None;
                while k < chars.len() {
                    if chars[k] == ')' {
                        url_end = Some(k);
                        break;
                    }
                    k += 1;
                }
                if let Some(ue) = url_end {
                    let text: String = chars[i + 1..te].iter().collect();
                    let url: String = chars[te + 2..ue].iter().collect();
                    out.push_str(&text);
                    if !url.is_empty() {
                        out.push_str(&format!(" ({})", url));
                    }
                    i = ue + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bold_markers() {
        assert_eq!(markdown_to_plain("这是**粗体**文字").trim(), "这是粗体文字");
    }

    #[test]
    fn strips_italic_markers() {
        assert_eq!(markdown_to_plain("这是*斜体*文字").trim(), "这是斜体文字");
    }

    #[test]
    fn keeps_underscore_identifiers_intact() {
        // snake_case 不能被当成强调
        assert_eq!(
            markdown_to_plain("字段名 tool_call_id").trim(),
            "字段名 tool_call_id"
        );
    }

    #[test]
    fn strips_heading_markers() {
        assert_eq!(markdown_to_plain("## 核心").trim(), "核心");
        assert_eq!(markdown_to_plain("###没有空格").trim(), "没有空格");
    }

    #[test]
    fn strips_inline_code_backticks() {
        assert_eq!(
            markdown_to_plain("用 `cargo test` 验证").trim(),
            "用 cargo test 验证"
        );
    }

    #[test]
    fn converts_table_to_indented_list() {
        let md = "| 模块 | 说明 |\n|---|---|\n| yq-core | 五工具 |\n| yq-llm | 模型层 |";
        let plain = markdown_to_plain(md);
        assert!(!plain.contains('|'), "不应残留竖杠：{}", plain);
        assert!(plain.contains("yq-core"), "应保留内容");
        assert!(plain.contains("yq-llm"));
        assert!(plain.contains('·'), "应转成列表形式");
    }

    #[test]
    fn normalizes_list_markers() {
        assert_eq!(markdown_to_plain("- 项目一").trim(), "· 项目一");
        assert_eq!(markdown_to_plain("* 项目二").trim(), "· 项目二");
    }

    #[test]
    fn converts_links() {
        assert_eq!(
            markdown_to_plain("[文档](https://example.com)").trim(),
            "文档 (https://example.com)"
        );
    }

    #[test]
    fn strips_blockquote_marker() {
        assert_eq!(markdown_to_plain("> 引用内容").trim(), "引用内容");
    }

    #[test]
    fn horizontal_rule_becomes_line() {
        assert_eq!(markdown_to_plain("---").trim(), "────────");
    }

    #[test]
    fn real_world_llm_output() {
        let md = "**核心**\n- Velocity-CTD 4.2.1（代理层）\n- Paper 25566\n\n| 模块 | 职责 |\n|---|---|\n| auth | 令牌管理 |\n\n（注：反机器人机制较严）";
        let plain = markdown_to_plain(md);
        assert!(!plain.contains("**"), "不应残留粗体标记");
        assert!(!plain.contains('|'), "不应残留表格竖杠");
        assert!(plain.contains("核心"));
        assert!(plain.contains("Velocity-CTD 4.2.1"));
        assert!(plain.contains("auth"));
    }

    #[test]
    fn plain_text_passes_through_mostly_unchanged() {
        let txt = "这是一段普通文本，没有标记。";
        assert_eq!(markdown_to_plain(txt).trim(), txt);
    }

    #[test]
    fn empty_input_is_safe() {
        assert_eq!(markdown_to_plain(""), "");
    }

    #[test]
    fn preserves_code_block_content() {
        // 代码块的三反引号应被去掉，内容保留
        let md = "```rust\nfn main() {}\n```";
        let plain = markdown_to_plain(md);
        assert!(!plain.contains("```"));
        assert!(plain.contains("fn main() {}"));
    }
}
