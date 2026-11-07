//! yq-cli —— yunqiagent 主二进制
//!
//! 命令：
//!   migrate                    运行数据库迁移
//!   repl                       交互式 REPL（已接入真实 LLM）
//!   tui                        全屏 TUI（ratatui 流式界面）
//!   serve                      启动 QQ 服务（OneBot11 正向/反向 WS + 心跳）
//!   mem add <kind> <content>   写入一条记忆
//!   mem list [--kind <k>]      列出记忆
//!
//! 全局开关：
//!   --debug-cache-prefix       输出固定区前缀 SHA256，验证 KV-cache 命中
//!
//! 运行示例：
//!   DATABASE_URL=postgres://yunqi:***@127.0.0.1:5432/yunqiagent \
//!     YQ_LLM_BASE_URL=http://127.0.0.1:3000/v1 YQ_LLM_API_KEY=sk-xxx \
//!     cargo run -p yq-cli -- --debug-cache-prefix repl

mod agent;
mod desktop;
mod config;
mod delivery;
mod notify;
mod stop;
mod sysmon;

use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use uuid::Uuid;
use yq_core::message::Message;
use yq_core::persona::Persona;
use yq_core::prompt::{STATIC_RULES, SYSTEM_PROMPT, StaticPrefix};
use yq_core::tools::ToolRegistry;
use yq_memory::Memories;

use crate::agent::{Agent, AgentConfig};
use crate::config::AppConfig;

/// 云栖 Agent —— 整合五家精华的 AI Agent
#[derive(Parser, Debug)]
#[command(
    name = "yunqiagent",
    version,
    about = "yunqiagent：整合五家精华的 AI Agent"
)]
struct Cli {
    /// 输出固定区前缀的 SHA256，便于验证 KV-cache 命中
    #[arg(long, global = true)]
    debug_cache_prefix: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Windows 桌面客户端使用的 SSH / NDJSON 桥接（日志输出到 stderr）。
    DesktopStdio,
    /// 运行数据库迁移
    Migrate,

    /// 启动交互式 REPL（真实 LLM）
    Repl {
        /// 指定会话 ID（默认随机生成）
        #[arg(long)]
        session: Option<String>,
    },

    /// 启动全屏 TUI（ratatui 流式界面）
    Tui,

    /// 启动 QQ 服务（OneBot11 + 心跳）
    Serve {
        /// 连接模式：forward（客户端连出）| reverse（服务端监听）；缺省读配置文件
        #[arg(long)]
        mode: Option<String>,
        /// 端点：forward 为 ws://host:port；reverse 为 0.0.0.0:port；缺省读配置文件
        #[arg(long)]
        endpoint: Option<String>,
        /// 心跳间隔（如 30m / 0m 禁用）；缺省读配置文件
        #[arg(long)]
        heartbeat: Option<String>,
    },

    /// 启动 **Kenkin Chat** 接入服务（好友自研聊天软件，OneBot v11）
    ///
    /// 凭据从 ~/.yunqiagent/config.toml 的 [kenkin] 段读取。
    ServeKenkin {
        /// 心跳间隔（如 30m / 0m 禁用）；缺省读配置文件
        #[arg(long)]
        heartbeat: Option<String>,
    },

    /// 启动 **QQ 开放平台官方机器人**服务（无需 QQ 客户端）
    ServeOfficial {
        /// 只测试连通性：获取 token + 查机器人信息后退出
        #[arg(long)]
        probe: bool,
    },

    /// 记忆相关操作
    Mem {
        #[command(subcommand)]
        action: MemAction,
    },

    /// 配置管理（生成模板 / 查看当前配置）
    Config {
        /// 生成配置模板到 ~/.yunqiagent/config.toml（已存在则不覆盖）
        #[arg(long)]
        init: bool,
    },
}

#[derive(Subcommand, Debug)]
enum MemAction {
    /// 写入一条记忆
    Add {
        /// 记忆类型：user / env / skill / session_summary / heartbeat
        kind: String,
        /// 记忆内容
        content: String,
        /// 重要度（0.0 ~ 1.0，默认 0.5）
        #[arg(long, default_value_t = 0.5)]
        importance: f32,
        /// 作用域（默认全局）
        #[arg(long)]
        scope: Option<String>,
    },
    /// 列出记忆
    List {
        /// 仅列出指定类型
        #[arg(long)]
        kind: Option<String>,
        /// 返回条数上限
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    // 尝试加载 .env（缺失不报错；主要用于 CI / 临时调试）
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    match cli.command {
        Command::DesktopStdio => desktop::run().await,
        Command::Migrate => cmd_migrate().await,
        Command::Repl { session } => cmd_repl(session, cli.debug_cache_prefix).await,
        Command::Tui => cmd_tui(cli.debug_cache_prefix).await,
        Command::Serve {
            mode,
            endpoint,
            heartbeat,
        } => cmd_serve(mode, endpoint, heartbeat, cli.debug_cache_prefix, false).await,

        Command::ServeKenkin { heartbeat } => {
            cmd_serve(None, None, heartbeat, cli.debug_cache_prefix, true).await
        }
        Command::Mem { action } => cmd_mem(action).await,
        Command::Config { init } => cmd_config(init),
        Command::ServeOfficial { probe } => cmd_serve_official(probe).await,
    }
}

/// `config --init`：生成配置模板（照 Hermes 章法，配置住家目录）。
fn cmd_config(init: bool) -> Result<()> {
    let path = AppConfig::default_path();

    if !init {
        // 不带 --init：只展示当前生效的配置（密钥遮蔽）
        let cfg = AppConfig::load();
        println!("配置文件: {}", path.display());
        println!(
            "  是否存在: {}",
            if path.exists() {
                "是"
            } else {
                "否（使用环境变量回退）"
            }
        );
        println!("数据库: {}", mask_url(&cfg.database.url));
        println!("LLM base_url: {}", cfg.llm.base_url);
        println!("LLM model: {}", cfg.llm.model);
        println!("LLM api_key: {}", mask_secret(&cfg.llm.api_key));
        println!("QQ mode: {}  端点: {}", cfg.qq.mode, cfg.qq.endpoint);
        println!("心跳: {}", cfg.qq.heartbeat);
        return Ok(());
    }

    if path.exists() {
        println!("⚠️ 配置文件已存在，未覆盖: {}", path.display());
        println!("   如需重建，请先手动备份并删除。");
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("无法创建 ~/.yunqiagent 目录")?;
    }

    let cfg = AppConfig::default();
    let text = format!(
        "# yunqiagent 配置（照 Hermes 章法：配置与代码分离）\n\
         # 权限应为 600。密钥只住此文件，绝不进项目目录。\n\
         # 生成后请把 [llm].api_key 填成你的 NewAPI 调用令牌。\n\n{}",
        cfg.to_toml_string()
    );

    std::fs::write(&path, &text).context("写入配置失败")?;

    // 收紧权限（Unix）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    println!("✅ 已生成配置模板: {}", path.display());
    println!("   请编辑 [llm].api_key 后即可运行 repl / serve。");
    Ok(())
}

/// 遮蔽连接串中的密码。
fn mask_url(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme_end), Some(at)) if at > scheme_end => {
            let head = &url[..scheme_end + 3];
            let tail = &url[at..];
            format!("{}***{}", head, tail)
        }
        _ => url.to_string(),
    }
}

/// 遮蔽密钥（只留前后各 4 字符）。
fn mask_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return "***".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}****{}", head, tail)
}

/// 构建固定区前缀（供 debug 输出与 TUI 状态栏使用）。
fn build_static_prefix() -> Arc<StaticPrefix> {
    // 用**含 memory** 的完整定义渲染固定区 —— 否则模型看不到 memory 的参数
    // schema，只能靠 STATIC_RULES 的自然语言猜（容易调错）。
    let tools_prefix = ToolRegistry::render_for_prefix_with_memory();
    let persona = load_persona();
    Arc::new(StaticPrefix::build(
        SYSTEM_PROMPT,
        &tools_prefix,
        &persona,
        STATIC_RULES,
    ))
}

/// 加载 persona：优先 `~/.yunqiagent/persona.toml`，缺失则用默认。
fn load_persona() -> Persona {
    let path = dirs_persona_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("persona 解析失败（{}），使用默认 persona", e);
            Persona::default()
        }),
        Err(_) => Persona::default(),
    }
}

/// persona 文件路径：`~/.yunqiagent/persona.toml`。
fn dirs_persona_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{}/.yunqiagent/persona.toml", home)
}

/// `~/.yunqiagent` 目录路径。
fn yq_home() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    format!("{}/.yunqiagent", home)
}

/// 若开启 --debug-cache-prefix，打印前缀诊断信息。
fn print_cache_prefix(prefix: &StaticPrefix) {
    println!("[cache-prefix] 固定区字节长度: {}", prefix.len());
    println!("[cache-prefix] SHA256={}", prefix.sha256());
}

/// 建立连接池（配置来自 ~/.yunqiagent/config.toml，回退环境变量）。
async fn pool() -> Result<sqlx::PgPool> {
    let cfg = AppConfig::load();
    let pool = yq_db::connect(&cfg.database.url)
        .await
        .context("无法连接数据库，请检查 ~/.yunqiagent/config.toml 的 [database].url")?;
    Ok(pool)
}

/// 构造 Agent（LLM 配置来自 ~/.yunqiagent/config.toml）。
///
/// `memories` 透传给 `Agent::new`，使 `memory` 工具对模型可用
/// （模型因此能主动 add/replace/remove/list 长期记忆）。
///
/// `pool` 透传给 `Agent` 用于**工具调用审计**落库（`op_audit` 表）。
/// 传 `None` 时审计降级为「只打日志」，Agent 其余行为完全不受影响。
fn build_agent(
    prefix: Arc<StaticPrefix>,
    memories: Memories,
    pool: Option<sqlx::PgPool>,
) -> Result<Agent> {
    let cfg = AppConfig::load();
    // 启动即校验：密钥缺失/为空时给出可操作的报错，而不是等到第一次对话才失败
    cfg.validate()?;
    Agent::new(cfg.llm, prefix, memories, AgentConfig::default(), pool)
}

/// `migrate`：执行迁移。
async fn cmd_migrate() -> Result<()> {
    let pool = pool().await?;
    yq_db::run_migrations(&pool).await?;
    println!("✅ 数据库迁移完成");
    Ok(())
}

/// `mem add` / `mem list`。
async fn cmd_mem(action: MemAction) -> Result<()> {
    let pool = pool().await?;
    let memories = Memories::new(pool);

    match action {
        MemAction::Add {
            kind,
            content,
            importance,
            scope,
        } => {
            let kind = Memories::parse_kind(&kind)?;
            let id = memories
                .insert(kind, scope.as_deref(), &content, importance)
                .await?;
            println!("✅ 已写入记忆 id={} kind={} content={}", id, kind, content);
        }
        MemAction::List { kind, limit } => {
            let rows = match kind {
                Some(k) => {
                    memories
                        .list_by_kind(Memories::parse_kind(&k)?, limit)
                        .await?
                }
                None => memories.list_all(limit).await?,
            };
            if rows.is_empty() {
                println!("（暂无记忆）");
            } else {
                println!("共 {} 条：", rows.len());
                for m in rows {
                    let scope = m.scope.unwrap_or_else(|| "全局".to_string());
                    println!(
                        "  [{}] id={} 重要度={:.2} 作用域={} | {}",
                        m.kind, m.id, m.importance, scope, m.content
                    );
                }
            }
        }
    }
    Ok(())
}

/// `tui`：启动全屏 TUI（接真实 LLM 流式）。
async fn cmd_tui(debug_cache_prefix: bool) -> Result<()> {
    // 固定区：与 repl / serve 共用同一构建函数，保证跨入口前缀字节一致
    let prefix = build_static_prefix();
    if debug_cache_prefix {
        print_cache_prefix(&prefix);
    }

    // LLM 配置：复用 `AppConfig::load()`（配置文件 → 环境变量回退），
    // 不另写一份加载逻辑；启动即校验，密钥缺失时给出可操作报错。
    let cfg = AppConfig::load();
    cfg.validate()?;

    // ── 记忆注入（动态区）：与 repl 同源，取预算内的条目拼成一段文本。 ──
    //
    // 记忆属动态区，不进 messages[0]，不影响 KV-cache 前缀。
    let _memories = load_tui_memories().await;

    // 2026-10-11 修复（编译错误 E0422）：
    //
    // 这里原先构造 `yq_tui::TuiLlm { client, prefix, memories }` 并传给
    // `run_tui`，但 **`yq_tui::TuiLlm` 从未存在过**（`git log -S` 全仓零命中，
    // 说明它从来没被定义、也从来没被引用成功）。也就是说这个接线是**未完成的
    // 半成品**，一直没人发现 —— 因为生产服务跑的是**旧二进制**，坏代码从没
    // 被编译过，`cargo check` 一开就暴露。
    //
    // 真实签名是 `run_tui(cache_prefix_sha256: String)`：TUI 只需固定区指纹，
    // 用于状态栏展示缓存命中情况（见 app.rs:108，取前 8 字符）。
    //
    // 本次按真实签名接线，让 `tui` 子命令至少**能编译能启动**。
    // TUI 目前仍是阶段二 mock 流式源（见 yq-tui/src/stream.rs），
    // 真正接上 LLM 流式是独立的后续任务，不在本次范围。
    yq_tui::run_tui(prefix.sha256().to_string()).await
}

/// 为 TUI 准备记忆注入块（动态区）。
///
/// 数据库不可用或检索失败时**降级为空注入**而不是让 TUI 起不来 ——
/// 聊天界面没有记忆也能用，起不来则完全没用。
async fn load_tui_memories() -> Vec<String> {
    let pool = match pool().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("TUI 记忆检索跳过（数据库不可用）: {}", e);
            return Vec::new();
        }
    };
    let memories = Memories::new(pool);
    match memories.list_all(agent_config_memory_limit()).await {
        Ok(rows) => {
            let items: Vec<(String, String)> = rows
                .iter()
                .map(|m| (m.kind.clone(), m.content.clone()))
                .collect();
            let (block, taken) = yq_core::tools::memory::build_memory_block(
                &items,
                yq_core::tools::memory::INJECTION_CHAR_BUDGET,
            );
            if taken < rows.len() {
                tracing::debug!("TUI 记忆注入裁剪：{} 条中纳入 {} 条", rows.len(), taken);
            }
            if block.trim().is_empty() {
                Vec::new()
            } else {
                vec![block]
            }
        }
        Err(e) => {
            tracing::warn!("TUI 记忆检索失败（已跳过注入）: {}", e);
            Vec::new()
        }
    }
}

/// REPL 输入模式：终端交互（单行回车即发送）还是管道/重定向（段落累积）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    /// TTY：人一行一行敲，必须保持「单行回车即发送」的手感（既有约束，不能改）。
    Tty,
    /// 非 TTY（`cat task.txt | yunqiagent repl` 之类）：连续行合成一条消息。
    Pipe,
}

/// `ReplInput::next_message` 的一次结果。
enum NextInput {
    /// 一条待提交的用户消息（已整体 trim，保证非空）
    Message(String),
    /// 用户显式输入了 exit / quit（应提交的累积块已先提交完）
    Quit,
    /// 输入流结束（EOF / Ctrl-D）
    Eof,
}

/// 把「逐行 stdin」翻译成「一条条用户消息」的读取器。
///
/// 为什么分模式（生产事故）：`repl` 原先逐行成消息，用
/// `(cat task.txt; printf '\n/exit\n') | yunqiagent repl` 喂多行任务书时，
/// 整份任务书被切成 ~20 条独立消息，模型对每行都答一句「写好了，下一个要求？」，
/// 最后还编造了从未执行的测试输出。因此非 TTY 改为段落累积：
/// 连续行合成一条，空行才提交；而 TTY 路径**逐行语义原样保留**。
struct ReplInput<I>
where
    I: Iterator<Item = io::Result<String>>,
{
    lines: I,
    mode: InputMode,
    /// 段落累积缓冲，只在 Pipe 模式使用；每项都是 trim 后非空的行
    buf: Vec<String>,
    /// 已决定退出、但还欠一条「先提交累积块」的结果（exit/quit 或 EOF 触发）
    pending: Option<NextInput>,
}

impl<I> ReplInput<I>
where
    I: Iterator<Item = io::Result<String>>,
{
    fn new(lines: I, mode: InputMode) -> Self {
        Self {
            lines,
            mode,
            buf: Vec::new(),
            pending: None,
        }
    }

    /// 取下一条要提交的消息；`Quit` / `Eof` 表示应当退出。
    fn next_message(&mut self) -> io::Result<NextInput> {
        // exit/quit、EOF 可能带着一条必须先提交的累积块：先还债，再退出
        if let Some(p) = self.pending.take() {
            return Ok(p);
        }
        match self.mode {
            InputMode::Tty => self.next_tty(),
            InputMode::Pipe => self.next_pipe(),
        }
    }

    /// TTY：单行即发，空行忽略，exit / quit 退出 —— 与历史行为逐字一致。
    fn next_tty(&mut self) -> io::Result<NextInput> {
        loop {
            let line = match self.lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => return Err(e),
                None => return Ok(NextInput::Eof),
            };
            let trimmed = line.trim();
            // 现状：空行 continue 忽略，不产生空消息
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "exit" || trimmed == "quit" {
                return Ok(NextInput::Quit);
            }
            return Ok(NextInput::Message(trimmed.to_string()));
        }
    }

    /// 非 TTY：连续行累积成一条消息，空行提交、exit/quit 与 EOF 先提交再退出。
    fn next_pipe(&mut self) -> io::Result<NextInput> {
        loop {
            let line = match self.lines.next() {
                Some(Ok(l)) => l,
                Some(Err(e)) => return Err(e),
                // EOF：有累积块就先交出去，下一轮再返回 Eof
                None => return Ok(self.settle(NextInput::Eof)),
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                // 空行 = 段落分隔符；累积块为空则忽略，绝不产生空消息
                if let Some(msg) = self.take_buf() {
                    return Ok(NextInput::Message(msg));
                }
                continue;
            }
            if trimmed == "exit" || trimmed == "quit" {
                // 顺序约束：先把当前非空累积块提交出去，再退出
                // （与单行模式「先处理输入、再退出」的语义一致）
                return Ok(self.settle(NextInput::Quit));
            }
            self.buf.push(line);
        }
    }

    /// 若有累积块则把它变成待返回的消息，退出结果挂到 pending；否则直接返回退出结果。
    fn settle(&mut self, exit: NextInput) -> NextInput {
        match self.take_buf() {
            Some(msg) => {
                self.pending = Some(exit);
                NextInput::Message(msg)
            }
            None => exit,
        }
    }

    /// 取出并清空累积块：整体 trim（多行保留内部换行与缩进），空块返回 None。
    fn take_buf(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let joined = self.buf.join("\n");
        self.buf.clear();
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.to_string())
    }
}

/// `repl`：交互循环（接入真实 LLM + 记忆注入闭环）。
async fn cmd_repl(session: Option<String>, debug_cache_prefix: bool) -> Result<()> {
    let prefix = build_static_prefix();
    if debug_cache_prefix {
        print_cache_prefix(&prefix);
    }

    let app_cfg = AppConfig::load();
    let llm_cfg = app_cfg.llm.clone();

    let pool = pool().await?;
    let memories = Memories::new(pool.clone());

    // 记忆仓储先建好再构造 Agent —— memory 工具需要它
    // pool 已有（供 Memories 用），顺带交给 Agent 做审计落库
    let agent = build_agent(prefix.clone(), memories.clone(), Some(pool.clone()))?;

    let session_id = match session {
        Some(s) => Uuid::parse_str(&s).context("session 参数不是合法 UUID")?,
        None => Uuid::new_v4(),
    };

    // 先判定输入模式：TTY 单行即发，管道则按段落累积（见 ReplInput 注释）
    let mode = if io::stdin().is_terminal() {
        InputMode::Tty
    } else {
        InputMode::Pipe
    };

    println!("═══════════════════════════════════════════");
    println!("  yunqiagent REPL");
    println!("  会话 ID: {}", session_id);
    println!("  模型: {} @ {}", llm_cfg.model, llm_cfg.base_url);
    println!("  输入内容开始对话；输入 exit / quit 或 Ctrl-D 退出");
    // 让用户知道 REPL 也认这些命令（此前打了会被当普通消息喂给模型）
    println!("  可用命令：/new /model /usage /compress /help");
    if mode == InputMode::Pipe {
        // 管道里没有回车即发送的直觉，必须明说怎么分隔多条消息
        println!("  （管道输入：空行分隔多行消息）");
    }
    println!("═══════════════════════════════════════════");

    let stdin = io::stdin();
    let mut input_src = ReplInput::new(stdin.lock().lines(), mode);
    // 本会话的历史（滚动窗口由 Agent 内部管理）
    let mut history: Vec<Message> = Vec::new();

    loop {
        print!("> ");
        io::stdout().flush().ok();

        // 到这里拿到的就是「一条完整消息」——后续入库 / 记忆注入 / respond 完全不变
        let input = match input_src.next_message() {
            Ok(NextInput::Message(m)) => m,
            Ok(NextInput::Quit) => {
                println!("👋 呀吼！那本堂主先告退啦，旅行者～");
                break;
            }
            Ok(NextInput::Eof) => {
                println!("\n（输入结束，退出）");
                break;
            }
            Err(e) => {
                eprintln!("读取输入出错: {e}");
                break;
            }
        };
        let input = input.as_str();

        // ── 斜杠命令：本地处理，不进 LLM、不入库 ──
        //
        // 为什么必须在入库和调模型之前拦下：REPL 此前**完全没有**命令处理，
        // 打 `/help` 会被当普通消息发给模型，让它即兴编一份帮助文本
        // （内容不可信，还可能漏掉真实命令）。文案一律取自 yq_qq 的共享
        // 构造函数，与 QQ serve 路径同一来源，绝不各写一份。
        match handle_repl_command(input, &agent) {
            ReplCommand::Handled(text) => {
                println!("{}", text);
                continue;
            }
            ReplCommand::ResetHistory(text) => {
                history.clear();
                println!("{}", text);
                continue;
            }
            ReplCommand::Compress => {
                let text = repl_compress(&agent, &mut history).await;
                println!("{}", text);
                continue;
            }
            ReplCommand::PassThrough => {}
        }

        // 1) 写入用户消息（原始会话归档）
        if let Err(e) =
            sqlx::query("INSERT INTO sessions (session_id, role, content) VALUES ($1, 'user', $2)")
                .bind(session_id)
                .bind(input)
                .execute(&pool)
                .await
        {
            tracing::warn!("写入用户消息失败: {}", e);
        }

        // 2) 检索相关记忆（阶段一为基础实现：按重要度取最近若干条）
        //
        // 用 build_memory_block 做**字符预算裁剪**：记忆条数会随使用增长，
        // 全量注入迟早吃爆 context。这里只把预算内的条目拼成一段文本。
        let injected: Vec<String> = match memories.retrieve(input, agent_config_memory_limit() as usize).await {
            Ok(rows) => {
                let items: Vec<(String, String)> = rows
                    .iter()
                    .map(|m| (m.kind.clone(), m.content.clone()))
                    .collect();
                let (block, taken) = yq_core::tools::memory::build_memory_block(
                    &items,
                    yq_core::tools::memory::INJECTION_CHAR_BUDGET,
                );
                if taken < rows.len() {
                    tracing::debug!("记忆注入裁剪：{} 条中纳入 {} 条", rows.len(), taken);
                }
                if block.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![block]
                }
            }
            Err(e) => {
                tracing::warn!("记忆检索失败（已跳过注入）: {}", e);
                Vec::new()
            }
        };

        // 3) 调用 Agent（流式回调直接打印）
        print!("🤖 ");
        io::stdout().flush().ok();

        let reply = match agent
            .respond(
                &history,
                &injected,
                input,
                |delta| {
                    print!("{}", delta);
                    io::stdout().flush().ok();
                },
                |progress| {
                    // REPL：把每轮进度直接打印（同一终端，不会刷屏）
                    if let Some(text) = progress.render(true) {
                        println!("\n   {}", text.replace('\n', "\n   "));
                    }
                },
                crate::stop::StopSignal::new(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                println!("\n（调用模型失败: {}）", e);
                continue;
            }
        };
        println!();

        if debug_cache_prefix {
            println!(
                "   [cache] prefix={} 工具调用={} 次",
                &reply.prefix_sha256[..8.min(reply.prefix_sha256.len())],
                reply.tool_invocations
            );
        }

        // 4) 写入助手消息
        if let Err(e) = sqlx::query(
            "INSERT INTO sessions (session_id, role, content) VALUES ($1, 'assistant', $2)",
        )
        .bind(session_id)
        .bind(&reply.text)
        .execute(&pool)
        .await
        {
            tracing::warn!("写入助手消息失败: {}", e);
        }

        // 5) 维护本地历史窗口
        //
        // 为什么助手消息要带 [TOOL_TRACE]（而第 4) 步入库仍只存 reply.text）：
        // 只存最终文本时，第二轮模型看不到上一轮真实跑过什么工具，面对自己
        // 第一轮「测试通过」的声称查无实据，会按执行纪律自我定罪「我编造了」
        // 并把活重干一遍（2026-10 生产实测的假认错事故）。工具证据是防假报
        // 闭环的最后一块拼图，与审计表 op_audit（id62-66）的证据链同源。
        // 这份证据只进**内存历史**给模型看：sessions 表是用户可见的原始对话
        // （上面第 4) 步 bind 的仍是 reply.text），不掺内部证据。
        history.push(Message::user(input));
        history.push(Message::assistant(reply.history_text()));
    }

    Ok(())
}

/// REPL 记忆注入条数上限。
fn agent_config_memory_limit() -> i64 {
    AgentConfig::default().memory_limit
}

/// `serve`：启动 QQ 服务（OneBot11 + 非阻塞心跳）。
///
/// 参数缺省时从 `~/.yunqiagent/config.toml` 的 `[qq]` 段读取，
/// 命令行参数仅作临时覆盖。
async fn cmd_serve(
    mode: Option<String>,
    endpoint: Option<String>,
    heartbeat: Option<String>,
    debug_cache_prefix: bool,
    use_kenkin: bool,
) -> Result<()> {
    use yq_onebot::ws::{Mode, connect_with_token};
    use yq_qq::{Router, SessionMap, TriggerPolicy};

    let app_cfg = AppConfig::load();

    // Kenkin Chat：从 [kenkin] 段读凭据，固定走 forward + Bearer token
    let (mode_str, endpoint, heartbeat, bearer) = if use_kenkin {
        let k = &app_cfg.kenkin;
        if !k.is_ready() {
            anyhow::bail!(
                "[kenkin] 配置不完整：需要 enabled=true、token、ws_url\n\
                 当前：enabled={} token={} ws_url={}",
                k.enabled,
                if k.token.is_empty() {
                    "（空）"
                } else {
                    "已设置"
                },
                k.ws_url
            );
        }
        println!("── Kenkin Chat 接入 ──");
        println!("  机器人: {} ", k.name);
        println!("  端点:   {}", k.ws_url);
        println!(
            "  令牌:   {}…",
            k.token.chars().take(10).collect::<String>()
        );
        (
            "forward".to_string(),
            k.ws_url.clone(),
            heartbeat.unwrap_or_else(|| app_cfg.qq.heartbeat.clone()),
            Some(k.token.clone()),
        )
    } else {
        (
            mode.unwrap_or_else(|| app_cfg.qq.mode.clone()),
            endpoint.unwrap_or_else(|| app_cfg.qq.endpoint.clone()),
            heartbeat.unwrap_or_else(|| app_cfg.qq.heartbeat.clone()),
            None,
        )
    };

    let prefix = build_static_prefix();
    if debug_cache_prefix {
        print_cache_prefix(&prefix);
    }

    let llm_cfg = app_cfg.llm.clone();

    let pool = pool().await?;
    let memories = Memories::new(pool.clone());

    // 记忆仓储先建好再构造 Agent —— memory 工具需要它
    let agent = build_agent(prefix.clone(), memories.clone(), Some(pool.clone()))?;

    // 会话映射（群/私聊 → agent 会话 UUID）
    let mut session_map = SessionMap::open(format!("{}/sessions.json", yq_home()));

    // 每个会话的历史（内存态；重启后靠 sessions 表回溯）
    let mut histories: std::collections::BTreeMap<String, Vec<Message>> =
        std::collections::BTreeMap::new();

    let mode = match mode_str.as_str() {
        "forward" => Mode::Forward,
        "reverse" => Mode::Reverse,
        other => anyhow::bail!("未知模式 `{}`（应为 forward 或 reverse）", other),
    };

    let conn = connect_with_token(mode, &endpoint, bearer).await?;
    let mut events = conn.events;
    let actions = conn.actions.clone();

    println!("═══════════════════════════════════════════");
    println!("  yunqiagent QQ 服务");
    println!("  模式: {:?}  端点: {}", mode, endpoint);
    println!("  模型: {} @ {}", llm_cfg.model, llm_cfg.base_url);
    println!("  心跳间隔: {}", heartbeat);
    println!("═══════════════════════════════════════════");

    // ── 非阻塞心跳：独立 task，结果经 mpsc 投递 ──
    let hb_config = yq_heartbeat::HeartbeatConfig {
        every: parse_interval(&heartbeat),
        ..yq_heartbeat::HeartbeatConfig::default()
    };
    let mut heartbeat_handle = yq_heartbeat::spawn(hb_config);

    // ── 审计保留清理：独立 task，主循环零感知 ──
    //
    // op_audit 语义上是 append-only（审计记录不能被随意抹掉），
    // 长期跑必然增长，因此在独立 task 里按天清理。
    // 与心跳一样遵守项目硬性约束：任何旁路都不许阻塞主流程。
    {
        let pool = pool.clone();
        let retention = std::env::var("YQ_AUDIT_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|d| *d > 0)
            .unwrap_or(yq_db::audit::DEFAULT_AUDIT_RETENTION_DAYS);
        tokio::spawn(async move {
            // 先空跑一拍：把第一次清理推到 24 小时后，
            // 避免与启动阶段的迁移、连接池建立抢资源。
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                yq_db::audit::purge_old_audit(Some(&pool), retention).await;
            }
        });
    }

    let policy = TriggerPolicy::default();
    let mut self_id: i64 = 0;

    loop {
        // 1) 非阻塞取心跳产出（绝不 await，绝不阻塞消息处理）
        while let Some(tick) = heartbeat_handle.try_recv() {
            if tick.should_deliver {
                if let Some(msg) = &tick.message {
                    let parts = yq_qq::format_heartbeat_message(msg);
                    for part in parts {
                        // 心跳默认发往最近活跃的群；无群则仅落盘
                        if let Some(gid) = last_group_id(&events_group_cache()) {
                            let _ = actions
                                .send(yq_onebot::Action::send_group_msg(gid, part))
                                .await;
                        }
                    }
                }
                // 归档（target=both 时也走这里）
                let now = chrono::Local::now();
                let hb_dir = std::path::PathBuf::from(format!("{}/heartbeat", yq_home()));
                if let Some(msg) = &tick.message {
                    let _ = yq_heartbeat::archive_to_local(&hb_dir, &now, msg).await;
                }
            }
            tracing::info!(
                "心跳: status={:?} deliver={}",
                tick.status,
                tick.should_deliver
            );
        }

        // 2) 等事件（带超时，保证心跳队列被及时消费）
        let event = match tokio::time::timeout(std::time::Duration::from_millis(500), events.recv())
            .await
        {
            Ok(Some(ev)) => ev,
            Ok(None) => break,  // 连接关闭
            Err(_) => continue, // 超时：回到循环顶部消费心跳
        };

        if let Some(id) = event.self_id {
            self_id = id;
        }

        // 3) 触发判定
        let decision = policy.decide(&event, self_id);
        let kind = match decision {
            yq_qq::TriggerDecision::Respond { kind } => kind,
            yq_qq::TriggerDecision::Ignore { reason } => {
                tracing::debug!("忽略消息: {}", reason);
                continue;
            }
        };

        let kind: yq_qq::MessageKind = kind.into();
        let Some(msg) = Router::build(&event, kind) else {
            continue;
        };
        if Router::is_empty_text(&msg) {
            continue;
        }

        // 4) 会话路由
        let key = msg.session_key();
        let session_id = session_map.get_or_create(&key);

        // 5) 检索记忆注入（同样按字符预算裁剪，避免长会话吃爆 context）
        let injected: Vec<String> = match memories.retrieve(&msg.text, agent_config_memory_limit() as usize).await {
            Ok(rows) => {
                let items: Vec<(String, String)> = rows
                    .iter()
                    .map(|m| (m.kind.clone(), m.content.clone()))
                    .collect();
                let (block, taken) = yq_core::tools::memory::build_memory_block(
                    &items,
                    yq_core::tools::memory::INJECTION_CHAR_BUDGET,
                );
                if taken < rows.len() {
                    tracing::debug!("记忆注入裁剪：{} 条中纳入 {} 条", rows.len(), taken);
                }
                if block.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![block]
                }
            }
            Err(e) => {
                tracing::warn!("记忆检索失败（已跳过注入）: {}", e);
                Vec::new()
            }
        };

        // 6) 取该会话历史
        let history = histories.get(&key).cloned().unwrap_or_default();

        // 7) 调 Agent
        let reply = match agent
            .respond(
                &history,
                &injected,
                &msg.text,
                |_| {},
                |progress| {
                    if let Some(text) = progress.render(false) {
                        tracing::info!("进度: {}", text.replace('\n', " | "));
                    }
                },
                crate::stop::StopSignal::new(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Agent 调用失败: {}", e);
                let _ = actions
                    .send(yq_onebot::Action::send_group_msg(
                        msg.group_id.unwrap_or(0),
                        format!("（出错了：{}）", e),
                    ))
                    .await;
                continue;
            }
        };

        // 8) 归档会话消息
        let _ =
            sqlx::query("INSERT INTO sessions (session_id, role, content) VALUES ($1, 'user', $2)")
                .bind(session_id)
                .bind(&msg.text)
                .execute(&pool)
                .await;
        let _ = sqlx::query(
            "INSERT INTO sessions (session_id, role, content) VALUES ($1, 'assistant', $2)",
        )
        .bind(session_id)
        .bind(&reply.text)
        .execute(&pool)
        .await;

        // 9) 更新历史窗口
        //
        // 历史带 [TOOL_TRACE]：给第二轮模型留下「上一轮真跑过什么」的证据，
        // 避免它对着自己的「测试通过」声称查无实据而假认错重干
        // （2026-10 事故，证据链同审计表 op_audit id62-66）。
        // 上面第 8) 步入库的 sessions 仍只存 reply.text —— 数据库归档是
        // 用户可见的原始对话，不掺内部证据；发给 QQ 的 reply.text 同样不变。
        let entry = histories.entry(key.clone()).or_default();
        entry.push(Message::user(msg.text.clone()));
        entry.push(Message::assistant(reply.history_text()));
        let window = AgentConfig::default().history_window;
        if entry.len() > window {
            let cut = entry.len() - window;
            entry.drain(..cut);
        }

        // 10) 分片发送
        let parts = yq_qq::format_reply(&reply.text);
        for part in parts {
            let action = match msg.kind {
                yq_qq::MessageKind::Group => {
                    yq_onebot::Action::send_group_msg(msg.group_id.unwrap_or(0), part)
                }
                yq_qq::MessageKind::Private => {
                    yq_onebot::Action::send_private_msg(msg.user_id, part)
                }
            };
            if let Err(e) = actions.send(action).await {
                tracing::warn!("发送消息失败: {}", e);
                break;
            }
        }

        tracing::info!("已回复 [{}] 会话 {}", msg.user_id, key);
    }

    Ok(())
}

/// `serve-official`：启动 QQ **开放平台官方机器人**服务。
///
/// 与 `serve`（OneBot11）并列的另一条接入路径：无需 QQ 客户端，
/// 直接走腾讯官方 WebSocket 网关 + REST API。
///
/// 凭据来自 `~/.yunqiagent/config.toml` 的 `[qq_official]` 段
/// （或环境变量 `QQ_OPEN_APPID` / `QQ_OPEN_SECRET`）。
async fn cmd_serve_official(probe: bool) -> Result<()> {
    use yq_qqofficial::{adapter, api::OfficialApi, gateway};

    let app_cfg = AppConfig::load();

    // 配置文件优先，其次环境变量
    let official_cfg = if app_cfg.qq_official.is_configured() {
        app_cfg.qq_official.to_official()
    } else {
        yq_qqofficial::OfficialConfig::from_env()
            .context("未配置官方机器人：请在 ~/.yunqiagent/config.toml 的 [qq_official] 段填写 app_id / client_secret，或设置 QQ_OPEN_APPID / QQ_OPEN_SECRET 环境变量")?
    };
    official_cfg.validate()?;

    println!("═══════════════════════════════════════════");
    println!("  yunqiagent QQ 官方机器人服务");
    println!("  AppID: {}", official_cfg.app_id);
    println!("  intents: {}", official_cfg.intents);
    println!("  沙箱: {}", official_cfg.sandbox);
    println!("═══════════════════════════════════════════");

    // ── 连通性探测：取 token + 查机器人信息 ──
    let api = OfficialApi::new(&official_cfg)?;

    let token = match api.auth_header().await {
        Ok(h) => h,
        Err(e) => {
            println!("❌ 获取 Access Token 失败: {}", e);
            return Err(e.into());
        }
    };
    println!(
        "✅ Access Token 获取成功（{}，前缀 {}...）",
        if token.len() > 20 {
            "已就绪"
        } else {
            "异常短"
        },
        &token[..token.len().min(16)]
    );

    match api.bot_identity().await {
        Ok(id) => {
            println!("✅ 机器人身份: {}（id={}）", id.username, id.id);
            if !id.share_url.is_empty() {
                println!("   分享链接: {}", id.share_url);
            }
        }
        Err(e) => println!("⚠️ 查询机器人信息失败（不影响连接）: {}", e),
    }

    match api.gateway_url().await {
        Ok(url) => println!("✅ 网关地址: {}", url),
        Err(e) => {
            println!("❌ 获取网关地址失败: {}", e);
            return Err(e.into());
        }
    }

    if probe {
        println!("\n（--probe 模式：连通性验证完成，退出）");
        return Ok(());
    }

    // ── 正式服务：连网关 + 处理事件 ──
    let pool = pool().await?;
    let memories = Memories::new(pool.clone());

    // 记忆仓储先建好再构造 Agent —— memory 工具需要它
    let agent = build_agent(build_static_prefix(), memories.clone(), Some(pool.clone()))?;

    // 投递台账：保证"已生成未送达"的回复不丢
    let ledger = Arc::new(crate::delivery::DeliveryLedger::new(pool.clone()));

    // ── 共享状态（支持并发处理 + 中断）──
    //
    // 每条消息在独立 task 中处理，因此会话状态必须共享加锁；
    // 同时为每个会话维护一个「停止标志」，供 `/stop` 设置。
    let sessions = Arc::new(tokio::sync::Mutex::new(yq_qq::SessionMap::open(format!(
        "{}/official_sessions.json",
        yq_home()
    ))));
    let histories: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, Vec<Message>>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new()));
    let stop_flags: Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, crate::stop::StopSignal>>,
    > = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // 详细模式开关（/verbose 控制）：开启后连思维链一起汇报
    let official_verbose = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // msg_seq 计数器：QQ 官方 API 靠它区分「同一条消息的多次被动回复」，
    // 用固定值会被判定为重复并丢弃（实测 code=40054005）。
    // 用全局递增计数器，范围 1..=10000（官方限制）。
    let msg_seq_counter = Arc::new(std::sync::atomic::AtomicU32::new(1));

    println!("连接官方网关...（Ctrl-C 退出）");

    // ── 系统健康：内存监控 + 启动循环守卫 ──
    //
    // 两者都在正式服务前启动：
    //   · 内存监控：给长跑留下 RSS 时间序列（泄漏只有看曲线才能发现）
    //   · 启动守卫：若检测到崩溃循环，跳过"会致崩的恢复动作"以打破循环
    crate::sysmon::spawn_memory_monitor(crate::sysmon::MemoryMonitorConfig::default());

    let guard = crate::sysmon::RestartGuard::new(
        format!("{}/restart_history.txt", yq_home()),
        crate::sysmon::RestartGuardConfig::default(),
    );
    let verdict = guard.record_boot();
    if verdict.is_loop() {
        // 醒目告警 + 留档，让人能立刻发现
        tracing::error!("⚠️ {}", verdict.describe());
        println!("⚠️ {}", verdict.describe());
    } else {
        tracing::debug!("{}", verdict.describe());
    }

    let mut events = gateway::connect(&official_cfg).await?;
    println!("✅ 已连接，等待事件（模型: {}）", agent.model());

    // ── 补发上次进程遗留的未投递回复 ──
    //
    // 这些是"生成好了但没能送出去"的消息（上个进程在发送过程中崩溃/重启）。
    // pending 的补发不带标记（发送没开始过）；attempting/failed 带「补发」标记
    // （平台可能已收到，必须诚实标明，绝不静默重复）。
    match ledger.sweep_orphans().await {
        Ok(orphans) if !orphans.is_empty() => {
            println!("🔁 发现 {} 条上次未投递的回复，正在补发…", orphans.len());
            let api_for_sweep = api.clone();
            let ledger_for_sweep = ledger.clone();
            let seq_for_sweep = msg_seq_counter.clone();
            for rec in orphans {
                if rec.attempts >= crate::delivery::MAX_DELIVERY_ATTEMPTS {
                    let _ = ledger_for_sweep.mark_abandoned(rec.id).await;
                    tracing::warn!("投递记录 #{} 重试耗尽，已放弃", rec.id);
                    continue;
                }

                let text = crate::delivery::DeliveryLedger::with_recovery_marker(&rec);
                let scene = match rec.scene.as_str() {
                    "group" => yq_qqofficial::adapter::Scene::Group,
                    _ => yq_qqofficial::adapter::Scene::C2c,
                };
                let group = if scene == yq_qqofficial::adapter::Scene::Group {
                    Some(rec.target.as_str())
                } else {
                    None
                };

                let _ = ledger_for_sweep.mark_attempting(rec.id).await;
                match send_with_ledger(
                    None, // 补发本身不再重复登记（已有记录）
                    &api_for_sweep,
                    scene,
                    SendRoute {
                        group_openid: group,
                        user_openid: &rec.target,
                        reply_to: rec.reply_to_msg_id.as_deref().unwrap_or(""),
                        chat_key: &rec.chat_key,
                    },
                    &seq_for_sweep,
                    &text,
                )
                .await
                {
                    Ok(()) => {
                        let _ = ledger_for_sweep.mark_delivered(rec.id).await;
                        println!("   ✅ 补发成功 #{}", rec.id);
                    }
                    Err(e) => {
                        let _ = ledger_for_sweep.mark_failed(rec.id).await;
                        println!("   ❌ 补发失败 #{}: {}", rec.id, e);
                    }
                }
            }
        }
        Ok(_) => tracing::debug!("无遗留未投递记录"),
        Err(e) => tracing::warn!("扫描投递台账失败（不影响服务）: {}", e),
    }

    // 顺手清理过期台账记录（尽力而为）
    match ledger.cleanup(24).await {
        Ok(n) if n > 0 => tracing::info!("已清理 {} 条过期投递记录", n),
        _ => {}
    }
    println!("   可用命令：/new /model /usage /compress /verbose /stop /help");

    // Agent 包成 Arc 以便跨 task 共享
    let agent = Arc::new(agent);

    while let Some(ev) = events.recv().await {
        let Some(msg) = adapter::official_to_incoming(ev.event_type, &ev.data) else {
            tracing::debug!("非消息事件（{}），跳过", ev.raw_type);
            continue;
        };

        // 触发判定：群聊必须 @机器人（公域群机器人会推送所有群消息）
        if !adapter::should_respond(&msg) {
            tracing::debug!(
                "未 @机器人，跳过（群 {}）",
                msg.group_openid.clone().unwrap_or_default()
            );
            continue;
        }

        let text = msg.stripped_content();
        if text.is_empty() {
            continue;
        }

        println!("📨 [{}] {}: {}", msg.scene_str(), msg.user_openid, text);

        let key = msg.session_key();

        // ── /stop：立即设置中断标志（不走 agent，也不排队）──
        //
        // 这条必须在「spawn 消息处理」之前同步处理，否则任务跑着时
        // 主循环读不到它（这正是之前 /stop 无效的原因）。
        if text.trim().eq_ignore_ascii_case("/stop") {
            // 1) 触发中断信号（立即打断正在执行的工具）
            let signal = {
                let mut flags = stop_flags.lock().await;
                flags
                    .entry(key.clone())
                    .or_insert_with(crate::stop::StopSignal::new)
                    .clone()
            };
            signal.trigger();

            // 2) 回滚本轮已执行的操作（恢复快照）
            let journal = agent.journal_snapshot();
            let report = if journal.is_empty() {
                None
            } else {
                Some(crate::stop::rollback(&journal).await)
            };
            agent.clear_journal();

            let notice = match &report {
                Some(r) => format!(
                    "已中断当前任务。\n回滚结果：{}{}",
                    r.summary(),
                    if r.is_clean() {
                        String::new()
                    } else {
                        format!(
                            "\n（失败项：{}）",
                            r.failures
                                .iter()
                                .map(|(t, e)| format!("{} — {}", t, e))
                                .collect::<Vec<_>>()
                                .join("；")
                        )
                    }
                ),
                None => "已中断当前任务（本轮没有需要回滚的操作）。".to_string(),
            };
            let plain = yq_qq::markdown_to_plain(&notice);
            let body = yq_qqofficial::api::SendMessageBody::reply(
                &plain,
                &msg.message_id,
                next_seq(&msg_seq_counter),
            );
            let _ = match msg.scene {
                yq_qqofficial::adapter::Scene::Group => {
                    api.send_group_message(msg.group_openid.as_deref().unwrap_or(""), &body)
                        .await
                }
                _ => api.send_c2c_message(&msg.user_openid, &body).await,
            };
            continue;
        }

        // ── 其他斜杠命令：同步处理 ──
        if yq_qq::looks_like_command(&text) {
            let response = {
                let mut sess = sessions.lock().await;
                let mut hist = histories.lock().await;
                handle_official_command(
                    &text,
                    &agent,
                    &mut sess,
                    &mut hist,
                    &key,
                    &official_verbose,
                )
            };

            let plain = yq_qq::markdown_to_plain(&response);
            let parts = yq_qq::format_reply_with_limit(&plain, yq_qq::QQ_OFFICIAL_MAX_CHARS);
            for (idx, part) in parts.iter().enumerate() {
                let body = if idx == 0 && !msg.message_id.is_empty() {
                    yq_qqofficial::api::SendMessageBody::reply(
                        part,
                        &msg.message_id,
                        next_seq(&msg_seq_counter),
                    )
                } else {
                    yq_qqofficial::api::SendMessageBody::text(part)
                };
                let r = match msg.scene {
                    yq_qqofficial::adapter::Scene::Group => {
                        api.send_group_message(msg.group_openid.as_deref().unwrap_or(""), &body)
                            .await
                    }
                    yq_qqofficial::adapter::Scene::C2c | yq_qqofficial::adapter::Scene::Guild => {
                        api.send_c2c_message(&msg.user_openid, &body).await
                    }
                };
                if let Err(e) = r {
                    tracing::warn!("发送命令回复失败: {}", e);
                    break;
                }
            }
            continue;
        }

        // ── 普通消息：spawn 独立 task 处理 ──
        //
        // 必须并发处理，否则 agent 跑长任务时主循环被占住，
        // /stop 与 /new 这类命令无法及时生效。
        {
            // 新消息进来时清除该会话的中断标志（上一轮的 stop 不该影响本轮）
            let signal = {
                let mut flags = stop_flags.lock().await;
                flags
                    .entry(key.clone())
                    .or_insert_with(crate::stop::StopSignal::new)
                    .clone()
            };
            signal.reset();
            // 新一轮开始，清空上一轮的操作日志
            agent.clear_journal();

            let agent = agent.clone();
            let api = api.clone();
            let pool = pool.clone();
            let memories = memories.clone();
            let sessions = sessions.clone();
            let histories = histories.clone();
            let verbose_flag = official_verbose.clone();
            let seq_counter = msg_seq_counter.clone();
            let ledger = ledger.clone();

            tokio::spawn(async move {
                // ── panic 保护 ──
                //
                // 处理逻辑万一 panic（unwrap 失败、索引越界等），
                // 用户本来会「什么都收不到」。这里捕获它并如实汇报，
                // 至少让用户知道"这次请求崩了"，而不是傻等。
                let panic_notifier = crate::notify::Notifier::new(
                    api.clone(),
                    msg.scene,
                    msg.group_openid.clone(),
                    msg.user_openid.clone(),
                    msg.message_id.clone(),
                    seq_counter.clone(),
                );

                let fut = handle_official_message(
                    msg,
                    agent,
                    api,
                    pool,
                    memories,
                    sessions,
                    histories,
                    signal,
                    verbose_flag,
                    seq_counter,
                    ledger,
                );

                use futures_util::FutureExt;
                let outcome = std::panic::AssertUnwindSafe(fut).catch_unwind().await;

                if outcome.is_err() {
                    panic_notifier
                        .error(
                            "任务处理意外崩溃",
                            "内部发生未预期的错误（panic），本次请求已中断。详情见服务日志。",
                        )
                        .await;
                }
            });
        }
    }

    Ok(())
}

/// 处理一条普通（非命令）消息：调 Agent、推送进度、回发结果。
///
/// 抽成独立函数以便在 task 中并发执行（不影响主循环接收 `/stop`）。
#[allow(clippy::too_many_arguments)]
async fn handle_official_message(
    msg: yq_qqofficial::adapter::OfficialMessage,
    agent: Arc<Agent>,
    api: yq_qqofficial::api::OfficialApi,
    pool: sqlx::PgPool,
    memories: Memories,
    sessions: Arc<tokio::sync::Mutex<yq_qq::SessionMap>>,
    histories: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, Vec<Message>>>>,
    stop_signal: crate::stop::StopSignal,
    verbose_flag: Arc<std::sync::atomic::AtomicBool>,
    seq_counter: Arc<std::sync::atomic::AtomicU32>,
    ledger: Arc<crate::delivery::DeliveryLedger>,
) {
    let text = msg.stripped_content();
    let key = msg.session_key();

    // 错误上报器（所有关键失败都推回这个对话）
    let notifier = crate::notify::Notifier::new(
        api.clone(),
        msg.scene,
        msg.group_openid.clone(),
        msg.user_openid.clone(),
        msg.message_id.clone(),
        seq_counter.clone(),
    );

    // 会话 + 历史（读）
    let (session_id, history) = {
        let mut sess = sessions.lock().await;
        let hist = histories.lock().await;
        (
            sess.get_or_create(&key),
            hist.get(&key).cloned().unwrap_or_default(),
        )
    };

    // 记忆注入（失败时降级但不静默）
    //
    // 按字符预算裁剪后再注入：官方 QQ 通道上下文更紧，全量记忆尤其危险。
    let injected: Vec<String> = match memories.retrieve(&text, agent_config_memory_limit() as usize).await {
        Ok(rows) => {
            let items: Vec<(String, String)> = rows
                .iter()
                .map(|m| (m.kind.clone(), m.content.clone()))
                .collect();
            let (block, taken) = yq_core::tools::memory::build_memory_block(
                &items,
                yq_core::tools::memory::INJECTION_CHAR_BUDGET,
            );
            if taken < rows.len() {
                tracing::debug!("记忆注入裁剪：{} 条中纳入 {} 条", rows.len(), taken);
            }
            if block.trim().is_empty() {
                Vec::new()
            } else {
                vec![block]
            }
        }
        Err(e) => {
            notifier
                .warn(&format!(
                    "记忆检索失败，本次对话将不带历史记忆\n原因：{}",
                    e
                ))
                .await;
            Vec::new()
        }
    };

    // ── 实时进度推送：闭包内非阻塞投递，独立 task 负责发送 ──
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<String>(32);

    let api_for_progress = api.clone();
    let seq_for_progress = seq_counter.clone();
    let scene = msg.scene;
    let group_openid = msg.group_openid.clone();
    let user_openid = msg.user_openid.clone();
    let msg_id = msg.message_id.clone();

    let sender = tokio::spawn(async move {
        let mut first = true;
        while let Some(t) = progress_rx.recv().await {
            if t.trim().is_empty() {
                continue;
            }
            let plain = yq_qq::markdown_to_plain(&yq_qq::sanitize_or_replace(&t));
            let parts = yq_qq::format_reply_with_limit(&plain, yq_qq::QQ_OFFICIAL_MAX_CHARS);
            for part in parts {
                let body = if first && !msg_id.is_empty() {
                    first = false;
                    yq_qqofficial::api::SendMessageBody::reply(
                        &part,
                        &msg_id,
                        next_seq(&seq_for_progress),
                    )
                } else {
                    yq_qqofficial::api::SendMessageBody::text(&part)
                };
                let r = match scene {
                    yq_qqofficial::adapter::Scene::Group => {
                        api_for_progress
                            .send_group_message(group_openid.as_deref().unwrap_or(""), &body)
                            .await
                    }
                    _ => api_for_progress.send_c2c_message(&user_openid, &body).await,
                };
                if let Err(e) = r {
                    tracing::warn!("发送进度失败: {}", e);
                    break;
                }
            }
        }
    });

    let verbose = verbose_flag.load(std::sync::atomic::Ordering::Relaxed);

    let result = agent
        .respond(
            &history,
            &injected,
            &text,
            |_| {},
            |progress| {
                if let Some(t) = progress.render(verbose) {
                    if progress_tx.try_send(t).is_err() {
                        tracing::debug!("进度队列已满，丢弃一条进度");
                    }
                }
            },
            stop_signal,
        )
        .await;

    drop(progress_tx);
    let _ = sender.await;

    let reply = match result {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Agent 调用失败: {}", e);
            let notice = yq_qq::markdown_to_plain(&format!("（调用模型失败：{}）", e));
            let body = yq_qqofficial::api::SendMessageBody::reply(
                &notice,
                &msg.message_id,
                next_seq(&seq_counter),
            );
            let _ = match msg.scene {
                yq_qqofficial::adapter::Scene::Group => {
                    api.send_group_message(msg.group_openid.as_deref().unwrap_or(""), &body)
                        .await
                }
                _ => api.send_c2c_message(&msg.user_openid, &body).await,
            };
            return;
        }
    };

    // 归档（失败时告知——否则用户以为对话被记录了，实际没有）
    if let Err(e) =
        sqlx::query("INSERT INTO sessions (session_id, role, content) VALUES ($1, 'user', $2)")
            .bind(session_id)
            .bind(&text)
            .execute(&pool)
            .await
    {
        notifier
            .warn(&format!(
                "用户消息归档失败（不影响本次回答，但历史会缺这条）\n原因：{e}"
            ))
            .await;
    }

    if let Err(e) =
        sqlx::query("INSERT INTO sessions (session_id, role, content) VALUES ($1, 'assistant', $2)")
            .bind(session_id)
            .bind(&reply.text)
            .execute(&pool)
            .await
    {
        tracing::warn!("助手回复归档失败: {}", e);
    }

    // 更新历史窗口
    //
    // 历史带 [TOOL_TRACE]（工具执行证据）：只存 reply.text 时第二轮模型
    // 看不到上一轮跑过什么，会对着自己的「测试通过」假定罪重干一遍
    // （2026-10 事故；证据同审计表 op_audit id62-66）。
    // 归档入库（上面 INSERT sessions）与发给用户的消息仍只用 reply.text。
    {
        let mut hist = histories.lock().await;
        let entry = hist.entry(key.clone()).or_default();
        entry.push(Message::user(text.clone()));
        entry.push(Message::assistant(reply.history_text()));
        let window = AgentConfig::default().history_window;
        if entry.len() > window {
            let cut = entry.len() - window;
            entry.drain(..cut);
        }
    }

    // ── 发送回复（经台账，保证不无痕丢失）──
    if let Err(e) = send_with_ledger(
        Some(&ledger),
        &api,
        msg.scene,
        SendRoute {
            group_openid: msg.group_openid.as_deref(),
            user_openid: &msg.user_openid,
            reply_to: &msg.message_id,
            chat_key: &key,
        },
        &seq_counter,
        &reply.text,
    )
    .await
    {
        // 发送失败没法再"通知用户"（渠道就是它本身），只能记 error 日志；
        // 但台账里留着 pending/attempting 记录，重启后会尝试补发。
        tracing::error!("回复发送失败（已记入台账，重启后将尝试补发）: {}", e);
    }
}

/// 发送一批分片，并**用台账记录每一次投递**。
///
/// 台账是「已生成但未确认送达」的保险：发送前记 pending，发送中记 attempting，
/// 成功记 delivered。若进程在这一过程中崩溃，重启后 sweep 会把这些孤儿捞出来
/// 补发（attempting/failed 的补发会带可见标记）。
///
/// **台账故障绝不影响真实发送**（尽力而为）——所有台账调用失败都只记日志。
/// 一次发送的**路由信息**（去往哪个会话、回复谁）。
///
/// 把这些参数打成一个结构体，是因为 `send_with_ledger` 曾经有 9 个位置参数：
/// 其中 `group_openid` 与 `user_openid`、`reply_to` 与 `chat_key` 都是
/// 同类型 `&str`，**顺序写错编译器抓不到**，而写错的后果是消息发到错误的
/// 会话（生产事故）。命名字段把这类错误变成编译错误。
#[derive(Clone, Copy)]
struct SendRoute<'a> {
    /// 群聊 openid（私聊时为 None）
    group_openid: Option<&'a str>,
    /// 用户 openid（群聊时是发送者，私聊时是接收者）
    user_openid: &'a str,
    /// 要回复的消息 id（空串表示不引用回复）
    reply_to: &'a str,
    /// 会话标识（台账主键的一部分）
    chat_key: &'a str,
}

async fn send_with_ledger(
    ledger: Option<&crate::delivery::DeliveryLedger>,
    api: &yq_qqofficial::api::OfficialApi,
    scene: yq_qqofficial::adapter::Scene,
    route: SendRoute<'_>,
    seq_counter: &std::sync::atomic::AtomicU32,
    text: &str,
) -> Result<(), String> {
    let SendRoute {
        group_openid,
        user_openid,
        reply_to,
        chat_key,
    } = route;

    // ── 净化：拦截模型"说漏嘴"的框架元信息（见 yq_qq::sanitize）──
    //
    // 实测模型偶发输出 Hermes 风格的会话压缩摘要（训练数据里见过类似产物），
    // 对用户来说是"bot 突然说莫名其妙的话"。这里在发送前拦下并换成诚实说明。
    let sanitized = yq_qq::sanitize_or_replace(text);
    let plain = yq_qq::markdown_to_plain(&sanitized);
    let parts = yq_qq::format_reply_with_limit(&plain, yq_qq::QQ_OFFICIAL_MAX_CHARS);

    let scene_str = match scene {
        yq_qqofficial::adapter::Scene::Group => "group",
        yq_qqofficial::adapter::Scene::C2c => "c2c",
        yq_qqofficial::adapter::Scene::Guild => "guild",
    };
    let target = group_openid.unwrap_or(user_openid);

    for (idx, part) in parts.iter().enumerate() {
        // ① 发送前：登记待投递（即使这之后立刻崩溃，也能被找回）
        let mut ledger_id = None;
        if let Some(l) = ledger {
            match l
                .record_pending(
                    chat_key,
                    scene_str,
                    target,
                    if reply_to.is_empty() {
                        None
                    } else {
                        Some(reply_to)
                    },
                    part,
                )
                .await
            {
                Ok(id) => ledger_id = Some(id),
                Err(e) => tracing::debug!("台账登记失败（不影响发送）: {}", e),
            }
        }

        // ② 发送中：标记 attempting（崩在这里，重启后会带「补发」标记重投）
        if let (Some(l), Some(id)) = (ledger, ledger_id) {
            if let Err(e) = l.mark_attempting(id).await {
                tracing::debug!("台账标记 attempting 失败: {}", e);
            }
        }

        let body = if idx == 0 && !reply_to.is_empty() {
            yq_qqofficial::api::SendMessageBody::reply(part, reply_to, next_seq(seq_counter))
        } else {
            yq_qqofficial::api::SendMessageBody::text(part)
        };

        let result = match scene {
            yq_qqofficial::adapter::Scene::Group => {
                api.send_group_message(group_openid.unwrap_or(""), &body)
                    .await
            }
            _ => api.send_c2c_message(user_openid, &body).await,
        };

        // ③ 收尾：成功记 delivered，失败记 failed
        match (result, ledger, ledger_id) {
            (Ok(_), Some(l), Some(id)) => {
                if let Err(e) = l.mark_delivered(id).await {
                    tracing::debug!("台账标记 delivered 失败: {}", e);
                }
            }
            (Err(ref e), Some(l), Some(id)) => {
                tracing::error!(
                    "❌ 回复发送失败（第 {}/{} 片）: {}",
                    idx + 1,
                    parts.len(),
                    e
                );
                if let Err(le) = l.mark_failed(id).await {
                    tracing::debug!("台账标记 failed 失败: {}", le);
                }
                return Err(e.to_string());
            }
            (Err(ref e), _, _) => {
                tracing::error!(
                    "❌ 回复发送失败（第 {}/{} 片）: {}",
                    idx + 1,
                    parts.len(),
                    e
                );
                return Err(e.to_string());
            }
            (Ok(_), _, _) => {}
        }
    }

    Ok(())
}

/// 场景的中文名（日志用）。
trait SceneLabel {
    /// 场景名
    fn scene_str(&self) -> &'static str;
}

impl SceneLabel for yq_qqofficial::adapter::OfficialMessage {
    fn scene_str(&self) -> &'static str {
        use yq_qqofficial::adapter::Scene;
        match self.scene {
            Scene::Group => "群",
            Scene::C2c => "私聊",
            Scene::Guild => "频道",
        }
    }
}

/// 取下一个 msg_seq（1..=10000 循环）。
///
/// QQ 官方 API 用 `msg_seq` 区分「同一条消息的多次被动回复」，
/// 固定值会被判定为重复并**直接丢弃**（实测 code=40054005）。
fn next_seq(counter: &std::sync::atomic::AtomicU32) -> u32 {
    let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    (n % 10_000) + 1
}

/// REPL 对一条输入的处理结果。
///
/// 与 QQ serve 路径共用 `yq_qq` 的命令解析与文案构造函数，
/// 但「怎么执行」因前端而异（REPL 清内存 vec，serve 清数据库 + 缓存）。
#[derive(Debug, PartialEq, Eq)]
enum ReplCommand {
    /// 已生成回复文本，直接打印，不进 LLM
    Handled(String),
    /// 重置对话历史（`/new`），文本是要打印的确认
    ResetHistory(String),
    /// 压缩历史（`/compress`）——需要跑一次模型，故单独一支
    Compress,
    /// 不是命令，按普通消息走 LLM
    PassThrough,
}

/// REPL 对一条输入的**意图判定**（纯函数，不碰 Agent）。
///
/// 与执行分离是为了可测：`Agent` 需要 LLM 配置与连接池才能构造，
/// 而「这条输入该走哪条路」是纯字符串判断，值得单独锁住。
#[derive(Debug, PartialEq, Eq)]
enum ReplIntent {
    /// `/verbose*`：REPL 无此概念，明确说明
    VerboseUnsupported,
    /// `/model` 查询当前模型
    ShowModel,
    /// `/model <名称>` 切换
    SetModel(String),
    /// `/usage` 查看用量
    Usage,
    /// `/compress` 压缩历史
    Compress,
    /// `/new` 重置对话
    ResetHistory,
    /// `/help`
    Help,
    /// 未知斜杠命令
    Unknown(String),
    /// 不是命令，按普通消息走 LLM
    PassThrough,
}

/// 判定一条 REPL 输入的意图。
fn classify_repl_input(input: &str) -> ReplIntent {
    use yq_qq::{Command, ParseResult};

    // `/verbose` 是 QQ 服务端的进度汇报开关；REPL 里进度本来就直接打印，
    // 故明确回绝而不是当普通消息（否则模型会编一段关于 verbose 的胡话）。
    let trimmed = input.trim().to_lowercase();
    if matches!(
        trimmed.as_str(),
        "/verbose" | "/verbose on" | "/verbose 1" | "/verbose off" | "/verbose 0"
    ) {
        return ReplIntent::VerboseUnsupported;
    }

    match yq_qq::parse(input) {
        ParseResult::NotACommand => ReplIntent::PassThrough,
        ParseResult::Command(Command::New) => ReplIntent::ResetHistory,
        ParseResult::Command(Command::Help) => ReplIntent::Help,
        ParseResult::Command(Command::ShowModel) => ReplIntent::ShowModel,
        ParseResult::Command(Command::SetModel(name)) => ReplIntent::SetModel(name),
        ParseResult::Command(Command::Usage) => ReplIntent::Usage,
        ParseResult::Command(Command::Compress) => ReplIntent::Compress,
        ParseResult::Command(Command::Unknown(name)) => ReplIntent::Unknown(name),
    }
}

/// 判定并处理 REPL 里的斜杠命令。
///
/// 文案一律取自 `yq_qq` 的共享构造函数，与 QQ serve 路径同一来源。
/// `/stop` 由主循环更早处处理，不到这里。
fn handle_repl_command(input: &str, agent: &Agent) -> ReplCommand {
    match classify_repl_input(input) {
        ReplIntent::PassThrough => ReplCommand::PassThrough,
        ReplIntent::VerboseUnsupported => ReplCommand::Handled(
            "详细汇报是 QQ 服务端专属功能，REPL 里每轮进度本来就直接打印，无需开关。".to_string(),
        ),
        ReplIntent::ShowModel => ReplCommand::Handled(yq_qq::model_text(&agent.model())),
        ReplIntent::SetModel(name) => match agent.set_model(&name) {
            Ok(()) => ReplCommand::Handled(yq_qq::model_switched_text(&agent.model())),
            Err(e) => ReplCommand::Handled(yq_qq::model_switch_failed_text(&e.to_string())),
        },
        ReplIntent::Usage => {
            let u = agent.usage();
            ReplCommand::Handled(yq_qq::format_usage(
                &agent.model(),
                u.prompt_tokens,
                u.completion_tokens,
                u.rounds,
            ))
        }
        ReplIntent::Compress => ReplCommand::Compress,
        ReplIntent::ResetHistory => ReplCommand::ResetHistory(yq_qq::new_conversation_text()),
        ReplIntent::Help => ReplCommand::Handled(yq_qq::help_text()),
        ReplIntent::Unknown(name) => ReplCommand::Handled(yq_qq::unknown_command_text(&name)),
    }
}

/// REPL 的 `/compress`：与 serve 路径共用提示词与文案，把历史压成一条摘要。
async fn repl_compress(agent: &Agent, history: &mut Vec<Message>) -> String {
    if history.is_empty() {
        return yq_qq::compress_empty_text();
    }
    let before = history.len();

    let mut transcript = String::new();
    for m in history.iter() {
        transcript.push_str(&format!("[{}] {}\n", m.role.as_str(), m.content));
    }
    let prompt = yq_qq::compress_prompt(&transcript);

    // REPL 已在 async 上下文，直接 await（serve 那边在同步上下文里才需要 block_in_place）
    match agent
        .respond(
            &[],
            &[],
            &prompt,
            |_| {},
            |_| {},
            crate::stop::StopSignal::new(),
        )
        .await
    {
        Ok(reply) => {
            let summary = reply.text.trim().to_string();
            *history = vec![
                Message::user(yq_qq::COMPRESS_PLACEHOLDER),
                Message::assistant(summary.clone()),
            ];
            yq_qq::compress_done_text(before, &summary)
        }
        Err(e) => yq_qq::compress_failed_text(&e.to_string()),
    }
}

/// 处理官方机器人收到的斜杠命令。
///
/// 返回面向用户的文本（由调用方转纯文本后发送）。
fn handle_official_command(
    text: &str,
    agent: &Agent,
    sessions: &mut yq_qq::SessionMap,
    histories: &mut std::collections::BTreeMap<String, Vec<Message>>,
    key: &str,
    verbose: &std::sync::atomic::AtomicBool,
) -> String {
    use std::sync::atomic::Ordering;
    use yq_qq::{Command, ParseResult};

    // `/verbose on|off` 单独处理（不在 yq-qq 的 Command 枚举里，
    // 因为它属于「服务层开关」而非通用命令）
    let trimmed = text.trim().to_lowercase();
    if trimmed == "/verbose" {
        return yq_qq::verbose_state_text(verbose.load(Ordering::Relaxed));
    }
    if trimmed == "/verbose on" || trimmed == "/verbose 1" {
        verbose.store(true, Ordering::Relaxed);
        return yq_qq::verbose_on_text();
    }
    if trimmed == "/verbose off" || trimmed == "/verbose 0" {
        verbose.store(false, Ordering::Relaxed);
        return yq_qq::verbose_off_text();
    }

    let cmd = match yq_qq::parse(text) {
        ParseResult::Command(c) => c,
        ParseResult::NotACommand => return String::new(),
    };

    match cmd {
        Command::New => {
            sessions.reset(key);
            histories.remove(key);
            yq_qq::new_conversation_text()
        }

        Command::ShowModel => yq_qq::model_text(&agent.model()),

        Command::SetModel(name) => match agent.set_model(&name) {
            Ok(()) => yq_qq::model_switched_text(&agent.model()),
            Err(e) => yq_qq::model_switch_failed_text(&e.to_string()),
        },

        Command::Usage => {
            let u = agent.usage();
            yq_qq::format_usage(
                &agent.model(),
                u.prompt_tokens,
                u.completion_tokens,
                u.rounds,
            )
        }

        Command::Compress => {
            let history = histories.get(key).cloned().unwrap_or_default();
            if history.is_empty() {
                return yq_qq::compress_empty_text();
            }

            let before = history.len();

            // 把历史拼成一段文本，交给模型总结
            let mut transcript = String::new();
            for m in &history {
                transcript.push_str(&format!("[{}] {}\n", m.role.as_str(), m.content));
            }

            let prompt = yq_qq::compress_prompt(&transcript);

            // 用 Agent 跑一次（此时不用历史，避免递归）
            let rt = tokio::runtime::Handle::current();
            let result = tokio::task::block_in_place(|| {
                rt.block_on(agent.respond(
                    &[],
                    &[],
                    &prompt,
                    |_| {},
                    |_| {},
                    crate::stop::StopSignal::new(),
                ))
            });

            match result {
                Ok(reply) => {
                    let summary = reply.text.trim().to_string();
                    // 用摘要替换历史（保留为用户+助手两条，维持结构）
                    histories.insert(
                        key.to_string(),
                        vec![
                            Message::user(yq_qq::COMPRESS_PLACEHOLDER),
                            Message::assistant(summary.clone()),
                        ],
                    );
                    yq_qq::compress_done_text(before, &summary)
                }
                Err(e) => yq_qq::compress_failed_text(&e.to_string()),
            }
        }

        Command::Help => yq_qq::help_text_full(),

        Command::Unknown(name) => yq_qq::unknown_command_text(&name),
    }
}

/// 最近活跃群号缓存（心跳投递目标）。
fn events_group_cache() -> Option<i64> {
    // 简化实现：由环境变量指定心跳目标群
    std::env::var("YQ_HEARTBEAT_GROUP")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// 从缓存的群号中取目标。
fn last_group_id(cache: &Option<i64>) -> Option<i64> {
    *cache
}

/// 解析间隔字符串：`30m` / `2h` / `45s` / `0m`（禁用）。
fn parse_interval(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.trim().parse().ok()?;
    if n == 0 {
        return None; // 0 表示禁用
    }
    match unit {
        "s" => Some(std::time::Duration::from_secs(n)),
        "m" => Some(std::time::Duration::from_secs(n * 60)),
        "h" => Some(std::time::Duration::from_secs(n * 3600)),
        _ => {
            // 纯数字按秒
            let n: u64 = s.parse().ok()?;
            if n == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(n))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_interval_minutes() {
        assert_eq!(
            parse_interval("30m"),
            Some(std::time::Duration::from_secs(1800))
        );
    }

    #[test]
    fn parse_interval_hours() {
        assert_eq!(
            parse_interval("2h"),
            Some(std::time::Duration::from_secs(7200))
        );
    }

    #[test]
    fn parse_interval_zero_disables() {
        assert_eq!(parse_interval("0m"), None);
    }

    #[test]
    fn parse_interval_seconds_fallback() {
        assert_eq!(
            parse_interval("45s"),
            Some(std::time::Duration::from_secs(45))
        );
        assert_eq!(
            parse_interval("12"),
            Some(std::time::Duration::from_secs(12))
        );
    }

    // ---------- ReplInput：段落累积逻辑（本次事故的核心验收） ----------

    /// 用一串固定行构造 Pipe 模式读取器（不碰真实 stdin，离线可测）。
    fn pipe(lines: &[&str]) -> ReplInput<std::vec::IntoIter<io::Result<String>>> {
        let rows: Vec<io::Result<String>> = lines.iter().map(|l| Ok(l.to_string())).collect();
        ReplInput::new(rows.into_iter(), InputMode::Pipe)
    }

    /// 同上，TTY 模式。
    fn tty(lines: &[&str]) -> ReplInput<std::vec::IntoIter<io::Result<String>>> {
        let rows: Vec<io::Result<String>> = lines.iter().map(|l| Ok(l.to_string())).collect();
        ReplInput::new(rows.into_iter(), InputMode::Tty)
    }

    /// 把读取器读干，按顺序收集全部结果。
    fn drain(r: &mut ReplInput<std::vec::IntoIter<io::Result<String>>>) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            match r.next_message() {
                Ok(NextInput::Message(m)) => out.push(format!("MSG:{m}")),
                Ok(NextInput::Quit) => {
                    out.push("QUIT".into());
                    break;
                }
                Ok(NextInput::Eof) => {
                    out.push("EOF".into());
                    break;
                }
                Err(e) => {
                    out.push(format!("ERR:{e}"));
                    break;
                }
            }
        }
        out
    }

    /// 管道单行：整块就是一条消息（与 TTY 单行结果一致）。
    #[test]
    fn pipe_single_line_message() {
        let mut r = pipe(&["你好"]);
        assert!(matches!(r.next_message(), Ok(NextInput::Message(m)) if m == "你好"));
        assert!(matches!(r.next_message(), Ok(NextInput::Eof)));
    }

    /// 管道多行 + 空行提交：多行合成**一条**消息 —— 事故场景的最小复现。
    #[test]
    fn pipe_multiline_committed_by_blank_line() {
        let mut r = pipe(&["第一行", "第二行", "", "第二条消息"]);
        assert!(matches!(r.next_message(), Ok(NextInput::Message(m)) if m == "第一行\n第二行"));
        // 第二条消息直到 EOF 才提交
        assert!(matches!(r.next_message(), Ok(NextInput::Message(m)) if m == "第二条消息"));
        assert!(matches!(r.next_message(), Ok(NextInput::Eof)));
    }

    /// 整体 trim：首尾空白去掉，块内部换行和缩进原样保留。
    #[test]
    fn pipe_trims_block_edges_only() {
        let mut r = pipe(&["  缩进行", "    保留缩进", "  "]);
        assert!(
            matches!(r.next_message(), Ok(NextInput::Message(m)) if m == "缩进行\n    保留缩进")
        );
    }

    /// 连续空行只提交一次，且绝不产生空消息。
    #[test]
    fn pipe_consecutive_blank_lines_never_produce_empty_message() {
        assert_eq!(
            drain(&mut pipe(&["", "   ", "内容", "", "", "尾巴", "", ""])),
            vec!["MSG:内容", "MSG:尾巴", "EOF"]
        );
        // 全是空行：一次消息都没有，直接 EOF
        assert_eq!(drain(&mut pipe(&["", "  ", ""])), vec!["EOF"]);
        // 完全空输入：EOF
        assert_eq!(drain(&mut pipe(&[])), vec!["EOF"]);
    }

    /// exit：先提交非空累积块，再退出（顺序是硬约束）。
    #[test]
    fn pipe_exit_commits_block_then_quits() {
        assert_eq!(
            drain(&mut pipe(&["任务第一行", "任务第二行", "exit"])),
            vec!["MSG:任务第一行\n任务第二行", "QUIT"]
        );
        // 累积块为空时直接退出，不产生空消息
        assert_eq!(drain(&mut pipe(&["exit"])), vec!["QUIT"]);
        // quit 同义
        assert_eq!(drain(&mut pipe(&["quit"])), vec!["QUIT"]);
        // exit 前的空行先提交掉，退出不吞掉已提交内容
        assert_eq!(
            drain(&mut pipe(&["已提交", "", "exit"])),
            vec!["MSG:已提交", "QUIT"]
        );
    }

    /// 只有整行（trim 后）等于 exit / quit 才算退出；正文里出现不误判。
    #[test]
    fn pipe_only_bare_exit_line_quits() {
        assert_eq!(
            drain(&mut pipe(&["echo exit && true", ""])),
            vec!["MSG:echo exit && true", "EOF"]
        );
        assert_eq!(
            drain(&mut pipe(&["say quit to exit"])),
            vec!["MSG:say quit to exit", "EOF"]
        );
        // 首尾空白不影响判定：trim 后仍是裸 quit
        assert_eq!(drain(&mut pipe(&["  quit  "])), vec!["QUIT"]);
    }

    /// EOF：先把累积块交出去，然后才退出（防止最后一段任务书被吞）。
    #[test]
    fn pipe_eof_commits_block_then_ends() {
        assert_eq!(
            drain(&mut pipe(&["没有空行结尾的任务书", "第二行"])),
            vec!["MSG:没有空行结尾的任务书\n第二行", "EOF"]
        );
    }

    /// 读取错误原样向上传（交给 cmd_repl 打印退出）。
    #[test]
    fn pipe_propagates_read_error() {
        let rows: Vec<io::Result<String>> =
            vec![Err(io::Error::new(io::ErrorKind::InvalidData, "坏了"))];
        let mut r = ReplInput::new(rows.into_iter(), InputMode::Pipe);
        assert!(r.next_message().is_err());
    }

    /// TTY 单行即发：历史行为不变（多行输入不会被攒起来）。
    #[test]
    fn tty_single_line_immediately() {
        let mut r = tty(&["第一行", "第二行"]);
        assert!(matches!(r.next_message(), Ok(NextInput::Message(m)) if m == "第一行"));
        assert!(matches!(r.next_message(), Ok(NextInput::Message(m)) if m == "第二行"));
        assert!(matches!(r.next_message(), Ok(NextInput::Eof)));
    }

    /// TTY 空行仍被忽略（不产生空消息），trim 规则不变。
    #[test]
    fn tty_blank_lines_still_ignored() {
        assert_eq!(
            drain(&mut tty(&["", "  ", "  你好  ", ""])),
            vec!["MSG:你好", "EOF"]
        );
    }

    /// TTY 的 exit / quit / EOF 语义不变。
    #[test]
    fn tty_exit_quit_eof_unchanged() {
        assert_eq!(drain(&mut tty(&["exit"])), vec!["QUIT"]);
        assert_eq!(drain(&mut tty(&["quit"])), vec!["QUIT"]);
        // exit 之前的普通行照常提交
        assert_eq!(
            drain(&mut tty(&["一句话", "exit"])),
            vec!["MSG:一句话", "QUIT"]
        );
        assert_eq!(drain(&mut tty(&[])), vec!["EOF"]);
    }

    // ── REPL 斜杠命令路由（此前 REPL 完全没有命令处理）──

    #[test]
    fn repl_routes_every_known_command() {
        assert_eq!(classify_repl_input("/new"), ReplIntent::ResetHistory);
        assert_eq!(classify_repl_input("/help"), ReplIntent::Help);
        assert_eq!(classify_repl_input("/model"), ReplIntent::ShowModel);
        assert_eq!(
            classify_repl_input("/model gpt-4o"),
            ReplIntent::SetModel("gpt-4o".to_string())
        );
        assert_eq!(classify_repl_input("/usage"), ReplIntent::Usage);
        assert_eq!(classify_repl_input("/compress"), ReplIntent::Compress);
        assert_eq!(
            classify_repl_input("/nope"),
            ReplIntent::Unknown("nope".to_string())
        );
    }

    #[test]
    fn repl_does_not_send_commands_to_the_model() {
        // 关键回归：这些曾经全都被当普通消息发给 LLM，模型会即兴编造
        // 帮助文本或模型名。任何以 / 开头的已知命令都不得 PassThrough。
        for cmd in ["/new", "/help", "/model", "/usage", "/compress", "/verbose"] {
            assert_ne!(
                classify_repl_input(cmd),
                ReplIntent::PassThrough,
                "{cmd} 被漏成普通消息了"
            );
        }
    }

    #[test]
    fn repl_leaves_ordinary_text_alone() {
        // 正文里出现斜杠不能被误判成命令
        for text in [
            "帮我看看 /tmp 目录",
            "访问 https://example.com 行吗",
            "cd /var/log 然后 tail",
            "你好啊",
        ] {
            assert_eq!(
                classify_repl_input(text),
                ReplIntent::PassThrough,
                "{text} 被误判成命令"
            );
        }
    }

    #[test]
    fn repl_is_case_insensitive_like_serve() {
        assert_eq!(classify_repl_input("/NEW"), ReplIntent::ResetHistory);
        assert_eq!(classify_repl_input("/Help"), ReplIntent::Help);
        assert_eq!(classify_repl_input("/MODEL"), ReplIntent::ShowModel);
    }

    #[test]
    fn repl_verbose_is_refused_explicitly_not_passed_through() {
        // /verbose 在 REPL 无意义，但必须明确回绝，不能喂给模型
        for text in ["/verbose", "/verbose on", "/verbose off"] {
            assert_eq!(classify_repl_input(text), ReplIntent::VerboseUnsupported);
        }
    }
}
