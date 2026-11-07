//! LLM 配置 —— 从环境变量 / TOML 读取。
//!
//! 环境变量（SCREAMING_SNAKE_CASE，与 SPEC 命名铁律一致）：
//!   · `YQ_LLM_BASE_URL`  如 `http://127.0.0.1:3000/v1`（自建 new-api 中转）
//!   · `YQ_LLM_API_KEY`   API 密钥
//!   · `YQ_LLM_MODEL`     模型名，如 `deepseek-chat`
//!   · `YQ_LLM_TEMPERATURE`（可选，默认 0.7）
//!   · `YQ_LLM_MAX_TOKENS`（可选，默认 16384）
//!   · `YQ_LLM_TIMEOUT_SECS`（可选，默认 120）

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::client::LlmError;

/// 降级链中的一档。
///
/// 主模型失败时按顺序尝试这些备用档。`base_url` / `api_key` 为空时
/// **继承主配置**（同一网关换个模型名的常见场景）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackEntry {
    /// 备用模型名
    pub model: String,
    /// 备用接口地址（留空则继承主配置）
    #[serde(default)]
    pub base_url: Option<String>,
    /// 备用密钥（留空则继承主配置）
    #[serde(default)]
    pub api_key: Option<String>,
}

impl FallbackEntry {
    /// 仅指定模型名（继承地址与密钥）。
    pub fn model_only(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: None,
            api_key: None,
        }
    }
}

/// LLM 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// 接口根地址（含 `/v1`）
    pub base_url: String,
    /// API 密钥
    pub api_key: String,
    /// 模型名
    pub model: String,
    /// 采样温度
    pub temperature: f32,
    /// 单次最大生成 token
    pub max_tokens: u32,
    /// 请求超时（秒）
    pub timeout_secs: u64,
    /// **降级链**：主模型失败时按序尝试
    #[serde(default)]
    pub fallback: Vec<FallbackEntry>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:3000/v1".to_string(),
            api_key: String::new(),
            model: "deepseek-chat".to_string(),
            temperature: 0.7,
            // **注意**：默认 16384 而不是更小的值 —— 2026-10-11 线上事故：
            // 工具调用 JSON 输出到 8192 就被腰斩（finish_reason=length），
            // 截断的 <tool_call> 块导致解析失败、连环重试。
            // 已实测网关对 16384 / 32768 均返回 HTTP 200，取 16384 兜底。
            max_tokens: 16384,
            timeout_secs: 120,
            fallback: Vec::new(),
        }
    }
}

impl LlmConfig {
    /// 按降级链生成候选配置列表（第 0 个是主配置）。
    ///
    /// 每档继承主配置里未覆写的字段。
    ///
    /// **注意**：返回的每个候选都**不带 fallback**（含第 0 档）——
    /// 否则拿候选去再展开就会无限递归。
    pub fn candidates(&self) -> Vec<LlmConfig> {
        // 主配置也要清空链，避免"候选自带链"的隐患
        let mut main = self.clone();
        main.fallback = Vec::new();
        let mut out = vec![main];

        for entry in &self.fallback {
            let mut c = self.clone();
            c.model = entry.model.clone();
            if let Some(u) = &entry.base_url {
                if !u.trim().is_empty() {
                    c.base_url = u.trim().to_string();
                }
            }
            if let Some(k) = &entry.api_key {
                if !k.trim().is_empty() {
                    c.api_key = k.trim().to_string();
                }
            }
            c.fallback = Vec::new();
            out.push(c);
        }
        out
    }

    /// 降级链长度（不含主配置）。
    pub fn fallback_count(&self) -> usize {
        self.fallback.len()
    }
    /// 从环境变量读取（缺失项用默认值）。
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = std::env::var("YQ_LLM_BASE_URL") {
            if !v.trim().is_empty() {
                cfg.base_url = v.trim().to_string();
            }
        }
        if let Ok(v) = std::env::var("YQ_LLM_API_KEY") {
            cfg.api_key = v.trim().to_string();
        }
        if let Ok(v) = std::env::var("YQ_LLM_MODEL") {
            if !v.trim().is_empty() {
                cfg.model = v.trim().to_string();
            }
        }
        if let Ok(v) = std::env::var("YQ_LLM_TEMPERATURE") {
            if let Ok(f) = v.trim().parse::<f32>() {
                cfg.temperature = f;
            }
        }
        if let Ok(v) = std::env::var("YQ_LLM_MAX_TOKENS") {
            if let Ok(n) = v.trim().parse::<u32>() {
                cfg.max_tokens = n;
            }
        }
        if let Ok(v) = std::env::var("YQ_LLM_TIMEOUT_SECS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                cfg.timeout_secs = n;
            }
        }

        cfg
    }

    /// 从 TOML 文件读取（不存在或解析失败则回退环境变量）。
    pub fn from_toml_file(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<LlmConfig>(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!("LLM 配置解析失败（{}），改用环境变量", e);
                    Self::from_env()
                }
            },
            Err(_) => Self::from_env(),
        }
    }

    /// 超时时长。
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    /// 校验配置是否可用（缺 api_key 时给出明确错误）。
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.base_url.trim().is_empty() {
            return Err(LlmError::Config("base_url 为空".to_string()));
        }
        if self.api_key.trim().is_empty() {
            return Err(LlmError::Config(
                "api_key 为空，请设置 YQ_LLM_API_KEY".to_string(),
            ));
        }
        if self.model.trim().is_empty() {
            return Err(LlmError::Config("model 为空".to_string()));
        }
        Ok(())
    }

    /// 拼接 chat completions 端点。
    pub fn completions_endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{}/chat/completions", base)
    }
}
