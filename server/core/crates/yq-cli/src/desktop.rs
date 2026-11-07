//! One request per SSH exec channel. stdout is exclusively NDJSON; stderr is logs.
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;
use tokio::io::{AsyncBufReadExt, BufReader};
use yq_core::message::Message;
use yq_memory::Memories;

#[derive(Deserialize)]
struct Request {
    kind: String,
    session: Option<String>,
    input: Option<String>,
}

fn emit(value: Value) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // A closed SSH channel must not panic the agent or print a partial JSON frame.
    if serde_json::to_writer(&mut out, &value).is_ok() {
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
}

pub async fn run() -> Result<()> {
    if let Err(e) = request().await {
        emit(json!({"type":"error", "message":format!("{e:#}")}));
    }
    Ok(())
}

fn history_with_tool_trace(content: String, evidence: Option<&Value>) -> String {
    let trace = evidence
        .and_then(|v| v.get("tool_trace"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match trace {
        Some(trace) => format!("{content}\n{trace}"),
        None => content,
    }
}

async fn request() -> Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    // The client caps input at 64 KiB. Also bound the server parser independently.
    use tokio::io::AsyncReadExt;
    let mut first = (&mut stdin).take(262145);
    first.read_line(&mut line).await?;
    anyhow::ensure!(line.len() <= 262144, "请求过大");
    let req: Request = serde_json::from_str(&line).context("请求 JSON 无效")?;
    let cfg = crate::AppConfig::load();
    if req.kind == "info" {
        cfg.validate()?;
        let pool = crate::pool().await?;
        sqlx::query("SELECT 1").execute(&pool).await?;
        emit(json!({"type":"info", "model":cfg.llm.model, "version":env!("CARGO_PKG_VERSION"),
            "transport":"SSH", "capabilities":["chat", "stream", "cancel", "memories", "tools"]}));
        return Ok(());
    }
    let pool = crate::pool().await?;
    let memories = Memories::new(pool.clone());
    if req.kind == "memories" {
        let rows = memories.list_all(200).await?;
        let items: Vec<Value> = rows.iter().map(|m| json!({"id":m.id,"kind":m.kind,
            "content":m.content,"importance":m.importance})).collect();
        emit(json!({"type":"memories", "items":items}));
        return Ok(());
    }
    anyhow::ensure!(req.kind == "chat", "未知请求类型");
    let input = req.input.context("缺少 input")?;
    anyhow::ensure!(!input.trim().is_empty() && input.len() <= 65536, "消息为空或超过 64 KiB");
    let session = uuid::Uuid::parse_str(&req.session.context("缺少 session")?)?;
    cfg.validate()?;
    let agent = crate::agent::Agent::new(cfg.llm, crate::build_static_prefix(), memories.clone(),
        crate::agent::AgentConfig { streaming: true, tool_protocol: yq_llm::ToolProtocol::Text,
            ..crate::agent::AgentConfig::default() }, Some(pool.clone()))?;
    let (rows, memory_rows) = tokio::join!(
        sqlx::query_as::<_, (String, String, Option<Value>)>(
            "SELECT role, COALESCE(content, ''), tool_calls FROM (SELECT id, role, content, tool_calls FROM sessions WHERE session_id=$1 ORDER BY id DESC LIMIT 40) recent ORDER BY id"
        )
        .bind(session).fetch_all(&pool),
        memories.retrieve(&input, 20),
    );
    let rows = rows?;
    let history: Vec<Message> = rows.into_iter().filter_map(|(role, content, evidence)| match role.as_str() {
        "user" => Some(Message::user(content)),
        "assistant" => Some(Message::assistant(history_with_tool_trace(content, evidence.as_ref()))),
        _ => None,
    }).collect();
    let injected = match memory_rows {
        Ok(rows) => {
            let items: Vec<(String,String)> = rows.into_iter().map(|m|(m.kind,m.content)).collect();
            let (block, _) = yq_core::tools::memory::build_memory_block(
                &items, yq_core::tools::memory::INJECTION_CHAR_BUDGET);
            if block.is_empty() { vec![] } else { vec![block] }
        },
        Err(e) => { tracing::warn!("desktop memory retrieval: {e}"); vec![] },
    };
    let stop = crate::stop::StopSignal::new();
    let response = agent.respond(&history, &injected, &input,
        |text| emit(json!({"type":"delta", "text":text})),
        |p| { if let Some(text) = p.render(true) { emit(json!({"type":"progress", "text":text})); } },
        stop.clone());
    tokio::pin!(response);
    let mut cancel = String::new();
    let reply = tokio::select! {
        result = &mut response => result,
        _ = stdin.read_line(&mut cancel) => {
            stop.trigger();
            response.await
        }
    }?;
    // Store a completed turn together. Retrying a failed request does not duplicate its user message.
    let mut tx = pool.begin().await?;
    for (role, text, evidence) in [
        ("user", input.as_str(), None),
        ("assistant", reply.text.as_str(), Some(json!({"tool_trace":reply.tool_trace}))),
    ] {
        sqlx::query("INSERT INTO sessions (session_id, role, content, tool_calls) VALUES ($1,$2,$3,$4)")
            .bind(session).bind(role).bind(text).bind(evidence).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    emit(json!({"type":"done", "text":reply.text, "tools":reply.tool_invocations,
        "prefix":reply.prefix_sha256, "stopped":stop.is_stopped()}));
    Ok(())
}

/// Hide text-protocol calls across arbitrary token boundaries while ordinary
/// prose continues to stream immediately. DeepSeek emits single-pipe DSML;
/// older gateways may emit the doubled-pipe form.
const TOOL_OPENERS: &[(&str, &str)] = &[
    ("<tool_call>", "</tool_call>"),
    ("<longcat_tool_call>", "</longcat_tool_call>"),
    ("<｜DSML｜ calls>", "</｜DSML｜ calls>"),
    ("<｜DSML｜calls>", "</｜DSML｜calls>"),
    ("<｜DSML｜ tool_calls>", "</｜DSML｜ tool_calls>"),
    ("<｜DSML｜tool_calls>", "</｜DSML｜tool_calls>"),
    ("<|DSML| calls>", "</|DSML| calls>"),
    ("<|DSML|calls>", "</|DSML|calls>"),
    ("<|DSML| tool_calls>", "</|DSML| tool_calls>"),
    ("<|DSML|tool_calls>", "</|DSML|tool_calls>"),
    ("<｜｜DSML｜｜ calls>", "</｜｜DSML｜｜ calls>"),
    ("<｜｜DSML｜｜calls>", "</｜｜DSML｜｜calls>"),
    ("<||DSML|| calls>", "</||DSML|| calls>"),
    ("<||DSML||calls>", "</||DSML||calls>"),
    ("<｜DSML｜ invoke ", "</｜DSML｜ invoke>"),
    ("<｜DSML｜invoke ", "</｜DSML｜invoke>"),
    ("<|DSML| invoke ", "</|DSML| invoke>"),
    ("<|DSML|invoke ", "</|DSML|invoke>"),
    ("<｜｜DSML｜｜ invoke ", "</｜｜DSML｜｜ invoke>"),
    ("<｜｜DSML｜｜invoke ", "</｜｜DSML｜｜invoke>"),
    ("<||DSML|| invoke ", "</||DSML|| invoke>"),
    ("<||DSML||invoke ", "</||DSML||invoke>"),
];

#[derive(Default)]
pub(crate) struct ToolTextFilter { pending: String, close: Option<String> }
impl ToolTextFilter {
    pub(crate) fn push(&mut self, delta: &str) -> String {
        self.pending.push_str(delta);
        let mut visible = String::new();
        loop {
            if let Some(close) = self.close.as_deref() {
                if let Some(at) = self.pending.find(close) {
                    self.pending.drain(..at + close.len());
                    self.close = None;
                    continue;
                }
                let retained = suffix_prefix_len(&self.pending, &[close]);
                self.pending.drain(..self.pending.len() - retained);
                break;
            }

            let next = TOOL_OPENERS.iter()
                .filter_map(|(open, close)| self.pending.find(open).map(|at| (at, *open, *close)))
                .min_by_key(|(at, _, _)| *at);
            if let Some((at, open, close)) = next {
                visible.push_str(&self.pending[..at]);
                self.pending.drain(..at + open.len());
                self.close = Some(close.to_string());
                continue;
            }

            let openers: Vec<&str> = TOOL_OPENERS.iter().map(|(open, _)| *open).collect();
            let retained = suffix_prefix_len(&self.pending, &openers);
            let emit_end = self.pending.len() - retained;
            visible.push_str(&self.pending[..emit_end]);
            self.pending.drain(..emit_end);
            break;
        }
        visible
    }

    pub(crate) fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        if self.close.is_some() || (pending.len() > 1 && TOOL_OPENERS.iter().any(|(open, _)| open.starts_with(&pending))) {
            String::new()
        } else {
            pending
        }
    }
}

fn suffix_prefix_len(text: &str, candidates: &[&str]) -> usize {
    let mut best = 0;
    for candidate in candidates {
        for end in candidate.char_indices().map(|(i, ch)| i + ch.len_utf8()) {
            if text.ends_with(&candidate[..end]) {
                best = best.max(end);
            }
        }
    }
    best
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persisted_tool_evidence_is_restored_only_for_model_history() {
        let evidence = json!({"tool_trace":"[TOOL_TRACE] read(src/lib.rs)✓"});
        assert_eq!(history_with_tool_trace("Done.".into(), Some(&evidence)), "Done.\n[TOOL_TRACE] read(src/lib.rs)✓");
        assert_eq!(history_with_tool_trace("Done.".into(), None), "Done.");
        assert_eq!(history_with_tool_trace("Done.".into(), Some(&json!({"other":1}))), "Done.");
    }
    #[test]
    fn hides_tools_at_every_unicode_token_boundary() {
        let text="你好<tool_call>{\"tool\":\"bash\"}</tool_call>世界";
        for split in text.char_indices().map(|(i,_)|i) {
            let mut f=ToolTextFilter::default();
            let mut visible=f.push(&text[..split]);visible.push_str(&f.push(&text[split..]));visible.push_str(&f.finish());
            assert_eq!(visible,"你好世界");
        }
    }
    #[test]
    fn incomplete_tool_body_never_leaks_but_plain_less_than_does() {
        let mut f=ToolTextFilter::default();assert_eq!(f.push("1 < 2"),"1 < 2");
        assert_eq!(f.push("<tool_call>secret"),"");assert_eq!(f.finish(),"");
    }

    #[test]
    fn hides_deepseek_single_fullwidth_pipe_dsml_at_every_token_boundary() {
        let text = "前言<｜DSML｜ calls><｜DSML｜ invoke name=\"memory\"><｜DSML｜ parameter name=\"action\">add</｜DSML｜ parameter></｜DSML｜ invoke></｜DSML｜ calls>结尾";
        for split in text.char_indices().map(|(i,_)|i) {
            let mut f=ToolTextFilter::default();
            let mut visible=f.push(&text[..split]);visible.push_str(&f.push(&text[split..]));visible.push_str(&f.finish());
            assert_eq!(visible,"前言结尾", "split at {split}");
        }
    }

    #[test]
    fn hides_unwrapped_ascii_dsml_invoke_and_keeps_surrounding_prose() {
        let mut f=ToolTextFilter::default();
        let mut visible=f.push("before <|DSML|invoke name=\"memory\"><|DSML|parameter name=\"content\">secret</|DSML|parameter></|DSML|invoke> after");
        visible.push_str(&f.finish());
        assert_eq!(visible,"before  after");
    }

    #[test]
    fn truncated_dsml_never_leaks_protocol_prefix_or_arguments() {
        let mut f=ToolTextFilter::default();
        assert_eq!(f.push("<｜DSML｜ calls><｜DSML｜ invoke name=\"memory\">secret"),"");
        assert_eq!(f.finish(),"");
        let mut partial=ToolTextFilter::default();
        assert_eq!(partial.push("answer<｜DSML｜ inv"),"answer");
        assert_eq!(partial.finish(),"");
    }
}
