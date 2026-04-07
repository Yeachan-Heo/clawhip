use std::error::Error;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::config::ProvidersConfig;

const MAX_INPUT_CHARS: usize = 4000;
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-4o-mini";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const HTTP_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentMode {
    Summary,
    Raw,
}

impl ContentMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Raw => "raw",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizedContent {
    pub summary: String,
    pub raw_truncated: String,
    pub backend: String,
    pub content_mode: ContentMode,
}

/// Trait for tmux content summarizers/transformers.
#[async_trait]
pub trait Summarizer: Send + Sync {
    fn name(&self) -> &str;

    async fn summarize(
        &self,
        content: &str,
        session: &str,
    ) -> Result<SummarizedContent, Box<dyn Error + Send + Sync>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizerSpec {
    Gemini { model: String },
    OpenRouter { model: String },
    OpenAiCompatible { model: String },
    Raw,
}

pub fn parse_summarizer_spec(
    summarizer: &str,
) -> Result<SummarizerSpec, Box<dyn Error + Send + Sync>> {
    let trimmed = summarizer.trim();
    let gemini_default = || SummarizerSpec::Gemini {
        model: DEFAULT_GEMINI_MODEL.to_string(),
    };
    if trimmed.eq_ignore_ascii_case("raw") {
        return Ok(SummarizerSpec::Raw);
    }
    if trimmed.eq_ignore_ascii_case("openrouter") {
        return Ok(SummarizerSpec::OpenRouter {
            model: DEFAULT_OPENROUTER_MODEL.to_string(),
        });
    }
    if let Some(model) = trimmed.strip_prefix("openrouter:") {
        return Ok(SummarizerSpec::OpenRouter {
            model: default_if_empty(model, DEFAULT_OPENROUTER_MODEL),
        });
    }
    if trimmed.eq_ignore_ascii_case("openai") || trimmed.eq_ignore_ascii_case("openai-compatible") {
        return Ok(SummarizerSpec::OpenAiCompatible {
            model: DEFAULT_OPENAI_MODEL.to_string(),
        });
    }
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("gemini") {
        return Ok(gemini_default());
    }
    if let Some(model) = trimmed
        .strip_prefix("openai:")
        .or_else(|| trimmed.strip_prefix("openai-compatible:"))
    {
        return Ok(SummarizerSpec::OpenAiCompatible {
            model: default_if_empty(model, DEFAULT_OPENAI_MODEL),
        });
    }
    if let Some(model) = trimmed.strip_prefix("gemini:") {
        return Ok(SummarizerSpec::Gemini {
            model: default_if_empty(model, DEFAULT_GEMINI_MODEL),
        });
    }
    Err(format!("unsupported summarizer backend '{trimmed}'").into())
}

pub fn build_summarizer(
    summarizer: &str,
    providers: &ProvidersConfig,
) -> Result<Box<dyn Summarizer>, Box<dyn Error + Send + Sync>> {
    match parse_summarizer_spec(summarizer)? {
        SummarizerSpec::Gemini { model } => Ok(Box::new(GeminiCli { model })),
        SummarizerSpec::OpenRouter { model } => {
            Ok(Box::new(OpenAiCompatibleSummarizer::new_openrouter(
                model,
                providers.openrouter.api_key.as_deref(),
            )?))
        }
        SummarizerSpec::OpenAiCompatible { model } => {
            Ok(Box::new(OpenAiCompatibleSummarizer::new_openai_compatible(
                model,
                providers.openai.api_key.as_deref(),
                providers.openai.base_url.as_deref(),
            )?))
        }
        SummarizerSpec::Raw => Ok(Box::new(RawPassthroughSummarizer)),
    }
}

/// Truncate content to the last `MAX_INPUT_CHARS` characters.
pub fn truncate_for_summarizer(content: &str) -> &str {
    if content.len() <= MAX_INPUT_CHARS {
        return content;
    }
    let start = content.len() - MAX_INPUT_CHARS;
    let start = content[start..]
        .char_indices()
        .next()
        .map(|(i, _)| start + i)
        .unwrap_or(start);
    &content[start..]
}

fn resolve_key(
    config_key: Option<&str>,
    env_var: &str,
    backend: &str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    if let Some(key) = config_key.filter(|k| !k.trim().is_empty()) {
        return Ok(key.to_string());
    }
    std::env::var(env_var).map_err(|_| {
        format!(
            "{env_var} is required for {backend} summarizer; set it via [providers.{backend}].api_key in config.toml or as an environment variable"
        )
        .into()
    })
}

fn default_if_empty(value: &str, default: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn summarize_system_prompt() -> &'static str {
    "You summarize tmux session output for developer monitoring. \
     Focus on what the agent is doing, any errors encountered, and current status. \
     Keep it concise (2-5 sentences).\n\n\
     IMPORTANT: If the terminal output indicates the session is waiting for user input — for example: \
     a [Y/n] confirmation prompt, 'Press enter to continue', 'Allow, Deny, Always allow' tool approval \
     (Claude Code style), 'continue?', 'proceed?', 'overwrite?', an interactive menu asking for a choice, \
     or a shell/REPL prompt awaiting a command — begin your response with exactly this line:\n\
     STATUS: WAITING_FOR_INPUT\n\n\
     Otherwise do not include a STATUS line. Respond in plain text."
}

fn summarize_user_prompt(session: &str, content: &str) -> String {
    format!("Session: {session}\n\n{}", truncate_for_summarizer(content))
}

fn summarize_result(summary: String, raw_truncated: String, backend: &str) -> SummarizedContent {
    SummarizedContent {
        summary,
        raw_truncated,
        backend: backend.to_string(),
        content_mode: ContentMode::Summary,
    }
}

fn raw_result(raw_truncated: String, backend: &str) -> SummarizedContent {
    SummarizedContent {
        summary: raw_truncated.clone(),
        raw_truncated,
        backend: backend.to_string(),
        content_mode: ContentMode::Raw,
    }
}

fn openai_chat_request(model: &str, session: &str, content: &str) -> Value {
    json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": summarize_system_prompt(),
            },
            {
                "role": "user",
                "content": summarize_user_prompt(session, content),
            }
        ],
        "temperature": 0.2
    })
}

fn extract_openai_response_text(value: &Value) -> Option<String> {
    let content = value.pointer("/choices/0/message/content")?;
    match content {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                })
                .collect::<Vec<_>>()
                .join("\n");
            if joined.trim().is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

pub struct GeminiCli {
    model: String,
}

#[async_trait]
impl Summarizer for GeminiCli {
    fn name(&self) -> &str {
        "gemini-cli"
    }

    async fn summarize(
        &self,
        content: &str,
        session: &str,
    ) -> Result<SummarizedContent, Box<dyn Error + Send + Sync>> {
        let raw_truncated = truncate_for_summarizer(content).to_string();
        let prompt = format!(
            "{}\n\n{}",
            summarize_system_prompt(),
            summarize_user_prompt(session, content)
        );
        let output = Command::new("gemini")
            .arg("-m")
            .arg(&self.model)
            .arg("-p")
            .arg(&prompt)
            .output()
            .await
            .map_err(|e| format!("failed to spawn gemini CLI: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "gemini CLI exited with status {}: {}",
                output.status,
                stderr.trim()
            )
            .into());
        }

        let summary = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if summary.is_empty() {
            return Err("gemini returned an empty summary".into());
        }

        Ok(summarize_result(summary, raw_truncated, self.name()))
    }
}

pub struct RawPassthroughSummarizer;

#[async_trait]
impl Summarizer for RawPassthroughSummarizer {
    fn name(&self) -> &str {
        "raw"
    }

    async fn summarize(
        &self,
        content: &str,
        _session: &str,
    ) -> Result<SummarizedContent, Box<dyn Error + Send + Sync>> {
        Ok(raw_result(
            truncate_for_summarizer(content).to_string(),
            self.name(),
        ))
    }
}

struct OpenAiCompatibleSummarizer {
    client: Client,
    backend_name: &'static str,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiCompatibleSummarizer {
    fn new_openrouter(
        model: String,
        config_key: Option<&str>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let api_key = resolve_key(config_key, "OPENROUTER_API_KEY", "openrouter")?;
        Self::new(
            "openrouter",
            OPENROUTER_BASE_URL.to_string(),
            api_key,
            model,
        )
    }

    fn new_openai_compatible(
        model: String,
        config_key: Option<&str>,
        config_base_url: Option<&str>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let base_url = config_base_url
            .filter(|v| !v.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
        let api_key = resolve_key(config_key, "OPENAI_API_KEY", "openai")?;
        Self::new("openai-compatible", base_url, api_key, model)
    }

    fn new(
        backend_name: &'static str,
        base_url: String,
        api_key: String,
        model: String,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let client = Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()?;
        Ok(Self {
            client,
            backend_name,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        })
    }

    fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[async_trait]
impl Summarizer for OpenAiCompatibleSummarizer {
    fn name(&self) -> &str {
        self.backend_name
    }

    async fn summarize(
        &self,
        content: &str,
        session: &str,
    ) -> Result<SummarizedContent, Box<dyn Error + Send + Sync>> {
        let raw_truncated = truncate_for_summarizer(content).to_string();
        let response = self
            .client
            .post(self.chat_completions_url())
            .bearer_auth(&self.api_key)
            .json(&openai_chat_request(&self.model, session, content))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "{} API request failed with {}: {}",
                self.backend_name, status, body
            )
            .into());
        }

        let payload = response.json::<Value>().await?;
        let Some(summary) = extract_openai_response_text(&payload) else {
            return Err(format!("{} API returned no summary text", self.backend_name).into());
        };

        Ok(summarize_result(summary, raw_truncated, self.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_content_unchanged() {
        let content = "hello world";
        assert_eq!(truncate_for_summarizer(content), "hello world");
    }

    #[test]
    fn truncate_long_content() {
        let content = "A".repeat(5000);
        let truncated = truncate_for_summarizer(&content);
        assert_eq!(truncated.len(), 4000);
        assert_eq!(truncated, "A".repeat(4000));
    }

    #[test]
    fn truncate_at_char_boundary() {
        let prefix = "A".repeat(3998);
        let content = format!("{}€{}", prefix, "Z");
        let truncated = truncate_for_summarizer(&content);
        assert!(truncated.len() <= 4000);
    }

    #[test]
    fn parse_summarizer_spec_supports_raw() {
        assert_eq!(parse_summarizer_spec("raw").unwrap(), SummarizerSpec::Raw);
    }

    #[test]
    fn parse_summarizer_spec_supports_openrouter() {
        assert_eq!(
            parse_summarizer_spec("openrouter:google/gemini-2.5-flash").unwrap(),
            SummarizerSpec::OpenRouter {
                model: "google/gemini-2.5-flash".into()
            }
        );
    }

    #[test]
    fn parse_summarizer_spec_supports_openai() {
        assert_eq!(
            parse_summarizer_spec("openai:gpt-4.1-mini").unwrap(),
            SummarizerSpec::OpenAiCompatible {
                model: "gpt-4.1-mini".into()
            }
        );
    }

    #[test]
    fn parse_summarizer_spec_defaults_gemini() {
        assert_eq!(
            parse_summarizer_spec("gemini").unwrap(),
            SummarizerSpec::Gemini {
                model: DEFAULT_GEMINI_MODEL.into()
            }
        );
    }

    #[test]
    fn parse_summarizer_spec_with_gemini_model() {
        assert_eq!(
            parse_summarizer_spec("gemini:gemini-2.5-pro").unwrap(),
            SummarizerSpec::Gemini {
                model: "gemini-2.5-pro".into()
            }
        );
    }

    #[test]
    fn parse_summarizer_spec_empty_gemini_model_uses_default() {
        assert_eq!(
            parse_summarizer_spec("gemini:").unwrap(),
            SummarizerSpec::Gemini {
                model: DEFAULT_GEMINI_MODEL.into()
            }
        );
    }

    #[test]
    fn parse_summarizer_spec_rejects_unknown_prefix() {
        assert!(parse_summarizer_spec("custom:something").is_err());
    }

    #[tokio::test]
    async fn raw_passthrough_returns_truncated_content() {
        let output = RawPassthroughSummarizer
            .summarize("hello world", "issue-24")
            .await
            .unwrap();
        assert_eq!(output.summary, "hello world");
        assert_eq!(output.raw_truncated, "hello world");
        assert_eq!(output.backend, "raw");
        assert_eq!(output.content_mode, ContentMode::Raw);
    }

    #[test]
    fn openai_chat_request_contains_model_and_messages() {
        let payload = openai_chat_request("gpt-4o-mini", "issue-24", "build failed");
        assert_eq!(payload["model"], "gpt-4o-mini");
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][1]["role"], "user");
        assert!(
            payload["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("issue-24")
        );
    }

    #[test]
    fn extract_openai_response_text_supports_string_content() {
        let payload = json!({
            "choices": [
                {
                    "message": {
                        "content": "agent fixed the test"
                    }
                }
            ]
        });
        assert_eq!(
            extract_openai_response_text(&payload).as_deref(),
            Some("agent fixed the test")
        );
    }

    #[test]
    fn extract_openai_response_text_supports_content_parts() {
        let payload = json!({
            "choices": [
                {
                    "message": {
                        "content": [
                            {"type": "text", "text": "agent is compiling"},
                            {"type": "text", "text": "waiting on cargo"}
                        ]
                    }
                }
            ]
        });
        assert_eq!(
            extract_openai_response_text(&payload).as_deref(),
            Some("agent is compiling\nwaiting on cargo")
        );
    }

    #[test]
    fn build_summarizer_supports_raw() {
        assert!(build_summarizer("raw", &ProvidersConfig::default()).is_ok());
    }

    #[test]
    fn system_prompt_includes_waiting_for_input_status_marker() {
        let prompt = summarize_system_prompt();
        assert!(
            prompt.contains("STATUS: WAITING_FOR_INPUT"),
            "system prompt must instruct the LLM to emit STATUS: WAITING_FOR_INPUT"
        );
        // Confirm it covers the key OMC/OMX trigger patterns
        assert!(prompt.contains("[Y/n]") || prompt.contains("Allow, Deny"));
        assert!(prompt.contains("continue?") || prompt.contains("proceed?"));
    }
}
