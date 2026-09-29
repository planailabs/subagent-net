//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use subnet_core::chat::{Delta, Message, Role, ToolCallDelta, ToolDef, Usage};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    /// Name of the env var holding the API key; the key itself never leaves the spawner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Provider can continue a trailing assistant message (vLLM `continue_final_message`).
    #[serde(default)]
    pub prefill: bool,
    /// Extra request body fields (temperature, max_tokens, …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http {status}: {body}")]
    Status { status: u16, body: String },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("stream: {0}")]
    Stream(String),
    #[error("provider error: {0}")]
    Provider(Value),
    #[error("bad chunk: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    fn retryable(&self) -> bool {
        match self {
            Error::Status { status, .. } => *status == 429 || *status >= 500,
            Error::Http(e) => e.is_connect() || e.is_timeout(),
            _ => false,
        }
    }
}

pub type DeltaStream = BoxStream<'static, Result<Delta, Error>>;

/// Appended when a partial answer must be continued on a provider without prefill.
pub const INTERRUPTED: &str = "[interrupted]";
const CONTINUE: &str = "Your previous answer was interrupted. Continue exactly where it stopped, without repeating it.";

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    cfg: ModelConfig,
    api_key: Option<String>,
    retries: u32,
}

impl Client {
    pub fn new(cfg: ModelConfig, api_key: Option<String>) -> Self {
        Self { http: reqwest::Client::new(), cfg, api_key, retries: 3 }
    }

    /// Reads the key from `cfg.api_key_env`; a configured but unset variable is an error.
    pub fn from_env(cfg: ModelConfig) -> Result<Self, std::env::VarError> {
        let key = cfg.api_key_env.as_deref().map(std::env::var).transpose()?;
        Ok(Self::new(cfg, key))
    }

    pub fn config(&self) -> &ModelConfig {
        &self.cfg
    }

    /// Request body. A trailing assistant message is a partial answer to continue.
    pub fn body(&self, messages: &[Message], tools: &[ToolDef]) -> Value {
        let mut messages = messages.to_vec();
        let mut body = self.cfg.params.clone();
        if messages.last().is_some_and(|m| m.role == Role::Assistant) {
            if self.cfg.prefill {
                body.insert("continue_final_message".into(), json!(true));
                body.insert("add_generation_prompt".into(), json!(false));
            } else {
                tracing::info!(model = %self.cfg.model, "no prefill support, continuing partial via user message");
                let last = messages.last_mut().unwrap();
                let c = last.content.get_or_insert_default();
                c.push_str(INTERRUPTED);
                messages.push(Message::user(CONTINUE));
            }
        }
        body.insert("model".into(), json!(self.cfg.model));
        body.insert("messages".into(), json!(messages));
        body.insert("stream".into(), json!(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
        if !tools.is_empty() {
            let tools: Vec<Value> = tools
                .iter()
                .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.parameters}}))
                .collect();
            body.insert("tools".into(), json!(tools));
        }
        Value::Object(body)
    }

    /// Starts a streamed completion. Connection failures, 429 and 5xx are retried
    /// with backoff before the stream starts; mid-stream errors are not (the
    /// caller keeps the partial and decides).
    pub async fn stream(&self, messages: &[Message], tools: &[ToolDef]) -> Result<DeltaStream, Error> {
        let body = self.body(messages, tools);
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let mut attempt = 0;
        let resp = loop {
            match self.send(&url, &body).await {
                Err(e) if e.retryable() && attempt < self.retries => {
                    attempt += 1;
                    let wait = Duration::from_millis(250 << attempt);
                    tracing::warn!(error = %e, attempt, ?wait, "llm request failed, retrying");
                    tokio::time::sleep(wait).await;
                }
                r => break r?,
            }
        };
        Ok(resp
            .bytes_stream()
            .eventsource()
            .take_while(|ev| futures::future::ready(!matches!(ev, Ok(e) if e.data.trim() == "[DONE]")))
            .filter_map(|ev| async move {
                match ev {
                    Err(e) => Some(Err(Error::Stream(e.to_string()))),
                    Ok(e) if e.data.trim().is_empty() => None,
                    Ok(e) => Some(parse_chunk(&e.data)),
                }
            })
            .boxed())
    }

    async fn send(&self, url: &str, body: &Value) -> Result<reqwest::Response, Error> {
        let mut req = self.http.post(url).json(body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status { status: status.as_u16(), body });
        }
        Ok(resp)
    }
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: ChoiceDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChoiceDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Deserialize)]
struct WireToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunctionDelta>,
}

#[derive(Deserialize)]
struct WireFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

pub fn parse_chunk(data: &str) -> Result<Delta, Error> {
    let c: Chunk = serde_json::from_str(data)?;
    if let Some(e) = c.error {
        return Err(Error::Provider(e));
    }
    let mut d = Delta { usage: c.usage, ..Default::default() };
    if let Some(ch) = c.choices.into_iter().next() {
        d.content = ch.delta.content.filter(|s| !s.is_empty());
        d.finish_reason = ch.finish_reason;
        d.tool_calls = ch
            .delta
            .tool_calls
            .into_iter()
            .map(|t| {
                let (name, arguments) = t.function.map(|f| (f.name, f.arguments)).unwrap_or_default();
                ToolCallDelta { index: t.index, id: t.id, name, arguments }
            })
            .collect();
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(prefill: bool) -> ModelConfig {
        ModelConfig { base_url: "http://x".into(), model: "m".into(), api_key_env: None, prefill, params: Map::new() }
    }

    #[test]
    fn parses_text_chunk() {
        let d = parse_chunk(r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}"#).unwrap();
        assert_eq!(d.content.as_deref(), Some("hi"));
        assert!(d.tool_calls.is_empty());
    }

    #[test]
    fn parses_tool_call_chunk() {
        let d = parse_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c","type":"function","function":{"name":"f","arguments":"{"}}]}}]}"#,
        )
        .unwrap();
        assert_eq!(
            d.tool_calls,
            vec![ToolCallDelta { index: 1, id: Some("c".into()), name: Some("f".into()), arguments: Some("{".into()) }]
        );
    }

    #[test]
    fn parses_usage_only_chunk() {
        let d = parse_chunk(r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#).unwrap();
        assert_eq!(d.usage, Some(Usage { prompt_tokens: 5, completion_tokens: 2 }));
    }

    #[test]
    fn provider_error_in_stream() {
        assert!(matches!(parse_chunk(r#"{"error":{"message":"overloaded"}}"#), Err(Error::Provider(_))));
    }

    #[test]
    fn body_includes_tools_and_params() {
        let mut c = cfg(false);
        c.params.insert("temperature".into(), json!(0.2));
        let tools = [ToolDef { name: "f".into(), description: "d".into(), parameters: json!({"type":"object"}) }];
        let b = Client::new(c, None).body(&[Message::user("hi")], &tools);
        assert_eq!(b["temperature"], 0.2);
        assert_eq!(b["stream"], true);
        assert_eq!(b["tools"][0]["function"]["name"], "f");
        assert!(b.get("continue_final_message").is_none());
    }

    #[test]
    fn body_prefill_continues_partial() {
        let b = Client::new(cfg(true), None).body(&[Message::user("q"), Message::assistant("half")], &[]);
        assert_eq!(b["continue_final_message"], true);
        assert_eq!(b["messages"].as_array().unwrap().len(), 2);
        assert_eq!(b["messages"][1]["content"], "half");
    }

    #[test]
    fn body_without_prefill_marks_interrupted() {
        let b = Client::new(cfg(false), None).body(&[Message::user("q"), Message::assistant("half")], &[]);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"], format!("half{INTERRUPTED}"));
        assert_eq!(msgs[2]["role"], "user");
        assert!(b.get("continue_final_message").is_none());
    }

    #[test]
    fn from_env_errors_on_missing_var() {
        let mut c = cfg(false);
        c.api_key_env = Some("SUBNET_TEST_SURELY_UNSET_VAR".into());
        assert!(Client::from_env(c).is_err());
    }
}
