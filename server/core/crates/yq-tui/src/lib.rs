//! yq-tui —— 终端界面
//!
//! 全屏 ratatui + crossterm，逐 token 流式渲染。
//!
//! 快捷键：
//!   Enter     提交输入
//!   Ctrl+L    切换模型（阶段二占位，仅提示）
//!   Ctrl+R    重跑上一条（阶段二占位）
//!   Ctrl+H    手动触发心跳（阶段二占位）
//!   Ctrl+P    查看 persona（阶段二占位）
//!   Ctrl+C / Esc / Ctrl+Q  退出

pub mod app;
pub mod stream;
pub mod ui;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::app::AppState;
use crate::stream::{TokenChunk, mock_token_stream_with_delay};

/// 启动 TUI。
///
/// `cache_prefix_sha256` 由调用方（yq-cli）传入，用于状态栏展示。
pub async fn run_tui(cache_prefix_sha256: String) -> Result<()> {
    // 终端初始化
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, cache_prefix_sha256).await;

    // 保证异常时也能恢复终端
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// 主事件循环。
async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cache_prefix_sha256: String,
) -> Result<()> {
    let mut app = AppState::new(cache_prefix_sha256);

    // 当前流式接收端（None 表示无进行中的流）
    let mut stream_rx: Option<mpsc::Receiver<TokenChunk>> = None;

    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        if app.should_quit {
            break;
        }

        // 优先消费流式 token（非阻塞轮询）
        if let Some(rx) = stream_rx.as_mut() {
            while let Ok(chunk) = rx.try_recv() {
                app.push_stream_token(&chunk.text);
                if chunk.is_final {
                    app.commit_stream();
                    stream_rx = None;
                    break;
                }
            }
        }

        // 轮询键盘事件（超时 50ms，保证流式刷新流畅）
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                handle_key(&mut app, key, &mut stream_rx);
            }
        }
    }

    Ok(())
}

/// 处理按键。
fn handle_key(
    app: &mut AppState,
    key: KeyEvent,
    stream_rx: &mut Option<mpsc::Receiver<TokenChunk>>,
) {
    // 组合键
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('l') => {
                app.tool_status = vec!["Ctrl+L：切换模型（阶段二占位）".to_string()];
            }
            KeyCode::Char('r') => {
                app.tool_status = vec!["Ctrl+R：重跑上一条（阶段二占位）".to_string()];
            }
            KeyCode::Char('h') => {
                app.tool_status = vec!["Ctrl+H：手动心跳（阶段二占位）".to_string()];
            }
            KeyCode::Char('p') => {
                app.tool_status = vec!["Ctrl+P：查看 persona（阶段二占位）".to_string()];
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Enter => {
            if app.streaming {
                return;
            }
            if let Some(text) = app.submit_input() {
                // 阶段二：用 mock 流式源回显（chars 逐字）
                let reply = format!("（阶段二 mock 流式）收到：{}", text);
                app.streaming = true;
                *stream_rx = Some(mock_token_stream_with_delay(
                    &reply,
                    Duration::from_millis(15),
                ));
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) => {
            app.input.push(c);
        }
        _ => {}
    }
}
