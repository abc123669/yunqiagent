//! TUI 渲染 —— 四区布局
//!
//! 布局：
//!   ┌──────────────────────────┬───────────────┐
//!   │  主对话区                 │  工具状态区    │
//!   │                          ├───────────────┤
//!   │                          │  计划看板      │
//!   ├──────────────────────────┴───────────────┤
//!   │  输入框                                   │
//!   ├───────────────────────────────────────────┤
//!   │  状态栏：心跳 / 记忆 / 前缀 SHA256 前 8 位 │
//!   └───────────────────────────────────────────┘

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::AppState;

/// 渲染整屏。
pub fn render(frame: &mut Frame, app: &AppState) {
    let area = frame.area();

    // 竖向：主体 / 输入 / 状态栏
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let body_area = vertical[0];
    let input_area = vertical[1];
    let status_area = vertical[2];

    // 主体横向：主对话区 / 右侧（工具状态 + 计划看板）
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(body_area);

    let chat_area = horizontal[0];
    let right_area = horizontal[1];

    // 右侧竖向：工具状态 / 计划看板
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(right_area);

    render_chat(frame, chat_area, app);
    render_tool_status(frame, right[0], app);
    render_plan(frame, right[1], app);
    render_input(frame, input_area, app);
    render_status(frame, status_area, app);
}

/// 主对话区。
fn render_chat(frame: &mut Frame, area: Rect, app: &AppState) {
    let mut lines: Vec<Line> = Vec::new();
    for msg in &app.messages {
        let prefix = match msg.role {
            yq_core::message::Role::User => "你",
            yq_core::message::Role::Assistant => "AI",
            yq_core::message::Role::Tool => "工具",
            yq_core::message::Role::System => "系统",
        };
        let style = match msg.role {
            yq_core::message::Role::User => Style::default().fg(Color::Cyan),
            yq_core::message::Role::Assistant => Style::default().fg(Color::Green),
            _ => Style::default().fg(Color::DarkGray),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{}│ ", prefix), style.add_modifier(Modifier::BOLD)),
            Span::raw(msg.content.clone()),
        ]));
    }

    // 流式缓冲区
    if app.streaming || !app.stream_buffer.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "AI│ ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                app.stream_buffer.clone(),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("▌", Style::default().fg(Color::Green)),
        ]));
    }

    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" 对话 "))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

/// 工具状态区。
fn render_tool_status(frame: &mut Frame, area: Rect, app: &AppState) {
    let lines: Vec<Line> = app
        .tool_status
        .iter()
        .map(|s| Line::from(Span::raw(s.clone())))
        .collect();
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" 工具状态 "))
        .wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

/// 计划看板。
fn render_plan(frame: &mut Frame, area: Rect, app: &AppState) {
    let lines: Vec<Line> = if app.plan.is_empty() {
        vec![Line::from(Span::styled(
            "（暂无计划）",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.plan
            .iter()
            .map(|item| Line::from(format!("{} {}", item.status.icon(), item.title)))
            .collect()
    };
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" 计划 "))
        .wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

/// 输入框。
fn render_input(frame: &mut Frame, area: Rect, app: &AppState) {
    let para = Paragraph::new(Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Cyan)),
        Span::raw(app.input.clone()),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" 输入 "));
    frame.render_widget(para, area);
}

/// 状态栏：心跳倒计时 / 记忆用量 / 前缀 SHA256 前 8 位。
fn render_status(frame: &mut Frame, area: Rect, app: &AppState) {
    let heartbeat = match app.heartbeat_countdown {
        Some(secs) => format!("心跳 {}s", secs),
        None => "心跳 --".to_string(),
    };
    let line = Line::from(vec![
        Span::styled(" KV ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("前缀 {} ", app.short_prefix_hash()),
            Style::default().fg(Color::Magenta),
        ),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("记忆 {} ", app.memory_usage), Style::default()),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(heartbeat, Style::default().fg(Color::Blue)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}
