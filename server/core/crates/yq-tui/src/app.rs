//! TUI 应用状态

use yq_core::message::Message;

/// 计划看板中的一个条目状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    /// 待办
    Todo,
    /// 进行中
    Doing,
    /// 已完成
    Done,
}

impl PlanStatus {
    /// 对应的状态图标。
    pub fn icon(self) -> &'static str {
        match self {
            PlanStatus::Todo => "⬜",
            PlanStatus::Doing => "⏳",
            PlanStatus::Done => "✅",
        }
    }
}

/// 计划看板条目。
#[derive(Debug, Clone)]
pub struct PlanItem {
    /// 内容
    pub title: String,
    /// 状态
    pub status: PlanStatus,
}

/// TUI 应用状态。
pub struct AppState {
    /// 主对话区消息
    pub messages: Vec<Message>,
    /// 当前输入缓冲
    pub input: String,
    /// 是否正在流式输出
    pub streaming: bool,
    /// 正在接收的流式文本（未提交到 messages）
    pub stream_buffer: String,

    /// 工具状态区内容
    pub tool_status: Vec<String>,
    /// 计划看板
    pub plan: Vec<PlanItem>,

    /// 缓存前缀 SHA256（状态栏展示前 8 位）
    pub cache_prefix_sha256: String,
    /// 记忆用量（条数）
    pub memory_usage: usize,
    /// 心跳倒计时（秒；None 表示未启用）
    pub heartbeat_countdown: Option<u64>,

    /// 是否请求退出
    pub should_quit: bool,
}

impl AppState {
    /// 新建应用状态。
    pub fn new(cache_prefix_sha256: String) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            streaming: false,
            stream_buffer: String::new(),
            tool_status: vec!["（暂无工具活动）".to_string()],
            plan: Vec::new(),
            cache_prefix_sha256,
            memory_usage: 0,
            heartbeat_countdown: None,
            should_quit: false,
        }
    }

    /// 追加一个流式 token。
    pub fn push_stream_token(&mut self, text: &str) {
        self.stream_buffer.push_str(text);
    }

    /// 提交当前流式内容为一条助手消息。
    pub fn commit_stream(&mut self) {
        if !self.stream_buffer.is_empty() {
            self.messages
                .push(Message::assistant(self.stream_buffer.clone()));
            self.stream_buffer.clear();
        }
        self.streaming = false;
    }

    /// 提交用户输入。
    pub fn submit_input(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.input.clear();
        self.messages.push(Message::user(text.clone()));
        Some(text)
    }

    /// 缓存前缀 SHA256 前 8 位（状态栏用）。
    pub fn short_prefix_hash(&self) -> String {
        self.cache_prefix_sha256.chars().take(8).collect()
    }
}
