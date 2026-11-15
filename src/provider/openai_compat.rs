//! OpenAI-compatible chat completions with function calling.
//!
//! Request assembly lives in `crate::core::prompt`; this module serializes it, adds the
//! provider-specific reasoning field (ProxyAPI uses `reasoning_effort`, AITUNNEL uses
//! `reasoning: { "effort": ... }`), offers tool schemas, and parses the answer including
//! `tool_calls`. Field order in the serialized request is part of the prompt-cache
//! contract and must not change (`AGENTS.md` §2, §6).

use std::collections::BTreeMap;

use futures_util::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Reasoning;
use crate::provider::registry::{Provider, ReasoningStyle};
use crate::provider::usage::Usage;

/// One tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Provider call id, echoed in the tool result message.
    pub id: String,
    /// Function name.
    pub name: String,
    /// JSON arguments as a string.
    pub arguments: String,
}

/// One message in a request or a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// `system`, `user`, `assistant`, or `tool`.
    pub role: &'static str,
    /// Message text; `None` for an assistant message that only calls tools.
    pub content: Option<String>,
    /// Tool calls made by an assistant message.
    pub tool_calls: Vec<ToolCall>,
    /// Id of the call this tool message answers.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// A system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }

    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }

    /// An assistant message with optional text and tool calls.
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant",
            content,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// A tool result.
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool",
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    fn text(role: &'static str, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// A parsed answer.
#[derive(Debug, Clone)]
pub struct Completion {
    /// The assistant message, including any tool calls.
    pub message: ChatMessage,
    /// Provider-reported usage.
    pub usage: Usage,
    /// Provider-reported charge in RUB, when the aggregator returns it.
    pub cost_rub: Option<f64>,
}

impl Completion {
    /// Assistant text, or an empty string for a pure tool-call turn.
    pub fn text(&self) -> &str {
        self.message.content.as_deref().unwrap_or_default()
    }
}

/// Full chat-completions URL.
pub fn endpoint(provider: &Provider) -> String {
    format!(
        "{}{}",
        provider.base_url.trim_end_matches('/'),
        provider.chat_path
    )
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningField<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'a str>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct ReasoningField<'a> {
    effort: &'a str,
}

fn build<'a>(
    provider: &Provider,
    model: &'a str,
    messages: &'a [ChatMessage],
    reasoning: Reasoning,
    tools: Option<&[Value]>,
    stream: bool,
) -> Request<'a> {
    let effort = reasoning.effort();
    let (reasoning_effort, reasoning) = match provider.reasoning {
        ReasoningStyle::EffortField => {
            // ProxyAPI rejects function tools together with a thinking effort on
            // /v1/chat/completions; `none` is its documented escape hatch.
            let effort = if tools.is_some() {
                Some("none")
            } else {
                effort
            };
            (effort, None)
        }
        ReasoningStyle::EffortObject => (None, effort.map(|effort| ReasoningField { effort })),
    };

    Request {
        model,
        messages: messages.iter().map(WireMessage::from).collect(),
        reasoning_effort,
        reasoning,
        tools: tools.map(<[Value]>::to_vec),
        tool_choice: tools.map(|_| "auto"),
        stream,
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
    }
}

impl<'a> From<&'a ChatMessage> for WireMessage<'a> {
    fn from(message: &'a ChatMessage) -> Self {
        Self {
            role: message.role,
            content: message.content.as_deref(),
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| WireToolCall {
                    id: &call.id,
                    kind: "function",
                    function: WireFunction {
                        name: &call.name,
                        arguments: &call.arguments,
                    },
                })
                .collect(),
            tool_call_id: message.tool_call_id.as_deref(),
        }
    }
}

/// Request body as JSON; used by tests and by [`complete`].
pub fn request_value(
    provider: &Provider,
    model: &str,
    messages: &[ChatMessage],
    reasoning: Reasoning,
    tools: Option<&[Value]>,
) -> Value {
    serde_json::to_value(build(provider, model, messages, reasoning, tools, false))
        .expect("request serializes")
}

/// Sends one non-streaming chat request and parses the answer.
pub async fn complete(
    client: &reqwest::Client,
    provider: &Provider,
    key: &SecretString,
    model: &str,
    messages: &[ChatMessage],
    reasoning: Reasoning,
    tools: Option<&[Value]>,
) -> Result<Completion, ChatError> {
    let request = build(provider, model, messages, reasoning, tools, false);
    let response = client
        .post(endpoint(provider))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", key.expose_secret()),
        )
        .json(&request)
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(ChatError::Status {
            status: status.as_u16(),
            body: truncate(&body, 400),
        });
    }

    let parsed: Response = serde_json::from_str(&body)
        .map_err(|error| ChatError::Shape(format!("{error}: {}", truncate(&body, 200))))?;
    let Response { choices, usage } = parsed;

    let choice = choices
        .into_iter()
        .next()
        .ok_or_else(|| ChatError::Shape("no choices in the response".to_owned()))?;
    let content = choice.message.content;
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|call| {
            let arguments = match call.function.arguments {
                Some(Value::String(text)) => text,
                Some(value) => value.to_string(),
                None => "{}".to_owned(),
            };
            ToolCall {
                id: call.id,
                name: call.function.name,
                arguments,
            }
        })
        .collect::<Vec<_>>();

    if content.is_none() && tool_calls.is_empty() {
        return Err(ChatError::Shape(
            "the answer has neither text nor tool calls".to_owned(),
        ));
    }

    let cost_rub = usage.as_ref().and_then(|usage| usage.cost_rub);
    let usage = usage.map(Usage::from).unwrap_or_default();

    Ok(Completion {
        message: ChatMessage::assistant(content, tool_calls),
        usage,
        cost_rub,
    })
}

/// Sends one streaming chat request and forwards text deltas as they arrive.
///
/// Tool calls are accumulated silently; the final [`Completion`] is identical in shape to
/// [`complete`], so the agent loop does not need a streaming-specific path.
#[allow(clippy::too_many_arguments)]
pub async fn complete_stream(
    client: &reqwest::Client,
    provider: &Provider,
    key: &SecretString,
    model: &str,
    messages: &[ChatMessage],
    reasoning: Reasoning,
    tools: Option<&[Value]>,
    mut on_text: impl FnMut(String),
) -> Result<Completion, ChatError> {
    let request = build(provider, model, messages, reasoning, tools, true);
    let response = client
        .post(endpoint(provider))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", key.expose_secret()),
        )
        .json(&request)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await?;
        return Err(ChatError::Status {
            status: status.as_u16(),
            body: truncate(&body, 400),
        });
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut text = String::new();
    let mut calls: BTreeMap<usize, ToolCall> = BTreeMap::new();
    let mut usage: Option<WireUsage> = None;
    let mut done = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if consume_buffer_lines(
            &mut buffer,
            &chunk,
            &mut text,
            &mut calls,
            &mut usage,
            &mut on_text,
        )? {
            done = true;
            break;
        }
    }

    if !done && !buffer.is_empty() {
        let line = String::from_utf8_lossy(&buffer);
        let _ = consume_stream_line(
            line.trim_end_matches(['\r', '\n']),
            &mut text,
            &mut calls,
            &mut usage,
            &mut on_text,
        )?;
    }

    let tool_calls = calls
        .into_iter()
        .map(|(index, mut call)| {
            if call.id.is_empty() {
                call.id = format!("call_{index}");
            }
            call
        })
        .collect::<Vec<_>>();
    if text.is_empty() && tool_calls.is_empty() {
        return Err(ChatError::Shape(
            "the streamed answer has neither text nor tool calls".to_owned(),
        ));
    }

    let cost_rub = usage.as_ref().and_then(|usage| usage.cost_rub);
    let usage = usage.map(Usage::from).unwrap_or_default();
    Ok(Completion {
        message: ChatMessage::assistant(
            if text.is_empty() { None } else { Some(text) },
            tool_calls,
        ),
        usage,
        cost_rub,
    })
}

/// Feeds one network chunk into the SSE byte buffer and consumes every complete line.
///
/// Lines are split on `\n` in the byte buffer and each whole line is decoded afterwards,
/// so a multi-byte UTF-8 character split across network chunks is never decoded
/// piecewise (`from_utf8_lossy` on a chunk would corrupt it). Returns `true` when
/// `[DONE]` was seen; the remaining half-line stays in `buffer` for the next call.
fn consume_buffer_lines(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    text: &mut String,
    calls: &mut BTreeMap<usize, ToolCall>,
    usage: &mut Option<WireUsage>,
    on_text: &mut impl FnMut(String),
) -> Result<bool, ChatError> {
    buffer.extend_from_slice(chunk);
    while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
        let raw: Vec<u8> = buffer.drain(..=position).collect();
        let line = String::from_utf8_lossy(&raw);
        if consume_stream_line(
            line.trim_end_matches(['\r', '\n']),
            text,
            calls,
            usage,
            on_text,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn consume_stream_line(
    line: &str,
    text: &mut String,
    calls: &mut BTreeMap<usize, ToolCall>,
    usage: &mut Option<WireUsage>,
    on_text: &mut impl FnMut(String),
) -> Result<bool, ChatError> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(false);
    };
    let data = data.trim();
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        return Ok(true);
    }

    let chunk: StreamChunk = serde_json::from_str(data)
        .map_err(|error| ChatError::Shape(format!("{error}: {}", truncate(data, 200))))?;
    if let Some(value) = chunk.usage {
        *usage = Some(value);
    }
    for choice in chunk.choices {
        if let Some(content) = choice.delta.content
            && !content.is_empty()
        {
            text.push_str(&content);
            on_text(content);
        }
        for call in choice.delta.tool_calls {
            let entry = calls.entry(call.index).or_insert_with(|| ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            if let Some(id) = call.id
                && !id.is_empty()
            {
                entry.id = id;
            }
            if let Some(function) = call.function {
                if let Some(name) = function.name {
                    entry.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    entry.arguments.push_str(&arguments);
                }
            }
        }
    }
    Ok(false)
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunction>,
}

#[derive(Deserialize)]
struct StreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireResponseToolCall>,
}

#[derive(Deserialize)]
struct WireResponseToolCall {
    id: String,
    function: WireResponseFunction,
}

#[derive(Deserialize)]
struct WireResponseFunction {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionDetails>,
    #[serde(default)]
    cost_rub: Option<f64>,
}

#[derive(Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        let cached = usage
            .prompt_tokens_details
            .map_or(0, |details| details.cached_tokens);
        let reasoning = usage
            .completion_tokens_details
            .map_or(0, |details| details.reasoning_tokens);

        Self {
            input: usage.prompt_tokens.saturating_sub(cached),
            output: usage.completion_tokens,
            reasoning,
            cache_read: cached,
            cache_write: 0,
        }
    }
}

/// Request and response errors. Messages never include the API key.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned HTTP {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Truncated response body.
        body: String,
    },
    #[error("unexpected response: {0}")]
    Shape(String),
}

fn truncate(text: &str, limit: usize) -> String {
    let mut shortened: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        shortened.push('…');
    }
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::registry;

    fn messages() -> Vec<ChatMessage> {
        vec![ChatMessage::system("rules"), ChatMessage::user("hello")]
    }

    #[test]
    fn stream_lines_accumulate_text_and_tool_arguments() {
        let mut text = String::new();
        let mut calls = BTreeMap::new();
        let mut usage = None;
        let mut deltas = Vec::new();
        let mut on_text = |delta: String| deltas.push(delta);

        for line in [
            r#"data: {"choices":[{"delta":{"content":"hel"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_file","arguments":"{\"path\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        ] {
            assert!(
                !consume_stream_line(line, &mut text, &mut calls, &mut usage, &mut on_text)
                    .expect("line")
            );
        }

        assert_eq!(text, "hello");
        assert_eq!(deltas, vec!["hel", "lo"]);
        let call = calls.get(&0).expect("tool call");
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "get_file");
        assert_eq!(call.arguments, r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_is_not_corrupted() {
        // "ё" is two UTF-8 bytes (0xD1 0x91). Splitting the SSE line in the middle of
        // the character and decoding each half separately (the pre-fix behaviour) turns
        // it into U+FFFD; `consume_buffer_lines` must decode the whole line instead.
        let mut buffer = Vec::new();
        let mut text = String::new();
        let mut calls = BTreeMap::new();
        let mut usage = None;
        let mut deltas = Vec::new();
        let mut on_text = |delta: String| deltas.push(delta);

        let mut line = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xD1\x91\"}}]}".to_vec();
        line.push(b'\n');
        let split = line
            .windows(2)
            .position(|window| window == [0xD1, 0x91])
            .expect("the ё bytes");
        let (first, second) = line.split_at(split + 1);

        assert!(
            !consume_buffer_lines(
                &mut buffer,
                first,
                &mut text,
                &mut calls,
                &mut usage,
                &mut on_text,
            )
            .expect("first chunk")
        );
        assert_eq!(text, "", "the half-line must wait for its second byte");
        assert!(
            !consume_buffer_lines(
                &mut buffer,
                second,
                &mut text,
                &mut calls,
                &mut usage,
                &mut on_text,
            )
            .expect("second chunk")
        );

        assert_eq!(text, "ё");
        assert_eq!(deltas, vec!["ё"]);
    }

    #[test]
    fn aitunnel_uses_the_reasoning_object_from_its_curl_example() {
        let provider = registry::get("aitunnel").expect("aitunnel");
        let value = request_value(provider, "gpt-6-luna", &messages(), Reasoning::Medium, None);

        assert_eq!(value["model"], "gpt-6-luna");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "hello");
        assert_eq!(value["reasoning"]["effort"], "medium");
        assert!(value.get("reasoning_effort").is_none());
        assert_eq!(value["stream"], false);
        assert_eq!(
            endpoint(provider),
            "https://api.aitunnel.ru/v1/chat/completions"
        );
    }

    #[test]
    fn proxyapi_uses_the_effort_field() {
        let provider = registry::get("proxyapi").expect("proxyapi");
        let value = request_value(
            provider,
            "openai/gpt-6-luna",
            &messages(),
            Reasoning::High,
            None,
        );

        assert_eq!(value["reasoning_effort"], "high");
        assert!(value.get("reasoning").is_none());
    }

    #[test]
    fn proxyapi_tools_force_reasoning_effort_off() {
        let provider = registry::get("proxyapi").expect("proxyapi");
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": { "name": "get_file", "parameters": {"type": "object"} }
        })];
        let value = request_value(
            provider,
            "openai/gpt-6-luna",
            &messages(),
            Reasoning::Medium,
            Some(&tools),
        );

        assert_eq!(value["reasoning_effort"], "none");
        assert!(value.get("reasoning").is_none());
    }

    #[test]
    fn reasoning_off_sends_neither_field() {
        let provider = registry::get("aitunnel").expect("aitunnel");
        let value = request_value(provider, "gpt-6-luna", &messages(), Reasoning::Off, None);

        assert!(value.get("reasoning").is_none());
        assert!(value.get("reasoning_effort").is_none());
    }

    #[test]
    fn tool_schemas_and_calls_are_serialized() {
        let provider = registry::get("aitunnel").expect("aitunnel");
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": { "name": "read_file", "parameters": {"type": "object"} }
        })];
        let messages = vec![
            ChatMessage::user("read it"),
            ChatMessage::assistant(
                None,
                vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"src/main.rs\"}".to_owned(),
                }],
            ),
            ChatMessage::tool("call_1", "fn main() {}"),
        ];

        let value = request_value(
            provider,
            "gpt-6-luna",
            &messages,
            Reasoning::Off,
            Some(&tools),
        );

        assert_eq!(value["tool_choice"], "auto");
        assert_eq!(value["tools"][0]["function"]["name"], "read_file");
        assert_eq!(value["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(value["messages"][1]["content"], Value::Null);
        assert_eq!(value["messages"][2]["role"], "tool");
        assert_eq!(value["messages"][2]["tool_call_id"], "call_1");
    }

    #[test]
    fn usage_moves_cached_tokens_out_of_the_uncached_input() {
        let usage = Usage::from(WireUsage {
            prompt_tokens: 1_000,
            completion_tokens: 200,
            prompt_tokens_details: Some(PromptDetails { cached_tokens: 400 }),
            completion_tokens_details: Some(CompletionDetails {
                reasoning_tokens: 50,
            }),
            cost_rub: Some(1.5),
        });

        assert_eq!(usage.input, 600);
        assert_eq!(usage.cache_read, 400);
        assert_eq!(usage.output, 200);
        assert_eq!(usage.reasoning, 50);
    }
}
