use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent, Role,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderPricing {
    pub input_million_tokens_micros: u64,
    pub output_million_tokens_micros: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiCompatibleConfig {
    pub endpoint: String,
    pub model: String,
    pub pricing: ProviderPricing,
    pub response_timeout: Duration,
    pub stream_idle_timeout: Duration,
}

#[derive(Clone)]
pub struct ProviderCredential(Zeroizing<String>);

impl ProviderCredential {
    pub fn bearer(value: impl Into<String>) -> Result<Self, ProviderExecutionError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "provider bearer token must not be blank".into(),
            ));
        }
        Ok(Self(Zeroizing::new(value)))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub(crate) fn redact(&self, message: String) -> String {
        message.replace(self.0.as_str(), "[REDACTED]")
    }
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderCredential([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProviderExecutionError {
    #[error("provider execution was cancelled")]
    Cancelled,
    #[error("invalid provider configuration: {0}")]
    InvalidConfiguration(String),
    #[error("provider event consumer closed")]
    ConsumerClosed,
    #[error("provider request failed: {message}")]
    Provider {
        kind: ModelErrorKind,
        retryable: bool,
        status: Option<u16>,
        /// Parsed HTTP `Retry-After`, bounded later by the frozen Runtime
        /// policy. Kept protocol-neutral so every Adapter reports the same
        /// scheduling hint.
        retry_after_ms: Option<u64>,
        message: String,
    },
}

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleAdapter {
    client: Client,
    endpoint: Url,
    model: String,
    pricing: ProviderPricing,
    response_timeout: Duration,
    stream_idle_timeout: Duration,
}

impl OpenAiCompatibleAdapter {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self, ProviderExecutionError> {
        let endpoint = Url::parse(&config.endpoint).map_err(|error| {
            ProviderExecutionError::InvalidConfiguration(format!(
                "provider endpoint is not a valid URL: {error}"
            ))
        })?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
        {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "provider endpoint must be HTTP(S) and must not contain credentials".into(),
            ));
        }
        if config.model.trim().is_empty()
            || config.response_timeout.is_zero()
            || config.stream_idle_timeout.is_zero()
        {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "provider model and timeouts must be configured".into(),
            ));
        }
        let client = Client::builder()
            .connect_timeout(config.response_timeout)
            .build()
            .map_err(|error| {
                ProviderExecutionError::InvalidConfiguration(format!(
                    "provider HTTP client could not be built: {error}"
                ))
            })?;
        Ok(Self {
            client,
            endpoint,
            model: config.model,
            pricing: config.pricing,
            response_timeout: config.response_timeout,
            stream_idle_timeout: config.stream_idle_timeout,
        })
    }

    pub async fn execute(
        &self,
        request: &ModelRequest,
        credential: &ProviderCredential,
        cancellation: CancellationToken,
        events: mpsc::Sender<ModelStreamEvent>,
    ) -> Result<(), ProviderExecutionError> {
        let payload = self.request_payload(request)?;
        let send = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(credential.expose())
            .json(&payload)
            .send();
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err(ProviderExecutionError::Cancelled),
            response = tokio::time::timeout(self.response_timeout, send) => {
                response.map_err(|_| provider_error(
                    ModelErrorKind::Timeout,
                    true,
                    None,
                    "provider did not return response headers before the configured timeout",
                ))?.map_err(classify_transport_error)?
            },
        };
        if !response.status().is_success() {
            return Err(classify_http_error(response, credential).await);
        }

        let mut stream = response.bytes_stream().eventsource();
        let mut tool_calls = BTreeMap::<u64, PartialToolCall>::new();
        let mut finish_reason = None;
        loop {
            let next = tokio::select! {
                _ = cancellation.cancelled() => return Err(ProviderExecutionError::Cancelled),
                next = tokio::time::timeout(self.stream_idle_timeout, stream.next()) => {
                    next.map_err(|_| provider_error(
                        ModelErrorKind::Timeout,
                        true,
                        None,
                        "provider stream was idle beyond the configured timeout",
                    ))?
                },
            };
            let Some(event) = next else {
                return Err(provider_error(
                    ModelErrorKind::Protocol,
                    false,
                    None,
                    "provider stream ended without [DONE]",
                ));
            };
            let event = event.map_err(|error| {
                provider_error(
                    ModelErrorKind::Protocol,
                    false,
                    None,
                    format!("invalid provider SSE stream: {error}"),
                )
            })?;
            if event.data == "[DONE]" {
                let reason = finish_reason.ok_or_else(|| {
                    provider_error(
                        ModelErrorKind::Protocol,
                        false,
                        None,
                        "provider stream completed without finish_reason",
                    )
                })?;
                flush_tool_calls(&events, tool_calls).await?;
                emit(&events, ModelStreamEvent::Completed { reason }).await?;
                return Ok(());
            }
            let chunk: Value = serde_json::from_str(&event.data).map_err(|error| {
                provider_error(
                    ModelErrorKind::Protocol,
                    false,
                    None,
                    format!("provider SSE data is not valid JSON: {error}"),
                )
            })?;
            consume_chunk(
                &events,
                &mut tool_calls,
                &mut finish_reason,
                &chunk,
                self.pricing,
            )
            .await?;
        }
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    fn request_payload(&self, request: &ModelRequest) -> Result<Value, ProviderExecutionError> {
        let messages = request
            .messages
            .iter()
            .map(message_payload)
            .collect::<Result<Vec<_>, _>>()?;
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema
                    }
                })
            })
            .collect::<Vec<_>>();
        let mut payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": request.max_output_tokens
        });
        if !tools.is_empty() {
            payload["tools"] = Value::Array(tools);
            payload["tool_choice"] = Value::String("auto".into());
        }
        if let Some(schema) = &request.output_schema {
            payload["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": "agent_output", "strict": true, "schema": schema }
            });
        }
        Ok(payload)
    }
}

fn message_payload(message: &Message) -> Result<Value, ProviderExecutionError> {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    if message.role == Role::Tool {
        if message.content.len() != 1 {
            return Err(capability_error(
                "tool messages must contain exactly one tool result",
            ));
        }
        if let ContentPart::ToolResult {
            tool_call_id,
            content,
        } = &message.content[0]
        {
            let content = content
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| content.to_string());
            return Ok(json!({
                "role": role,
                "tool_call_id": tool_call_id,
                "content": content
            }));
        }
        return Err(capability_error("tool role requires a tool result"));
    }

    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => content.push(json!({"type":"text","text":text})),
            ContentPart::Image { source, .. } if is_provider_safe_image_source(source) => {
                content.push(json!({"type":"image_url","image_url":{"url":source}}));
            }
            ContentPart::Image { .. } => {
                return Err(capability_error(
                    "image source must be an HTTP(S) URL or data URL resolved by the model gateway",
                ));
            }
            ContentPart::Audio { .. } => {
                return Err(capability_error(
                    "audio input is not supported by the first chat-completions adapter",
                ));
            }
            ContentPart::ToolResult { .. } => {
                return Err(capability_error(
                    "tool results are only valid in tool-role messages",
                ));
            }
            ContentPart::ToolCall {
                tool_call_id,
                name,
                arguments,
            } if message.role == Role::Assistant => {
                tool_calls.push(json!({
                    "id": tool_call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(arguments).expect("JSON value is serializable")
                    }
                }));
            }
            ContentPart::ToolCall { .. } => {
                return Err(capability_error(
                    "tool calls are only valid in assistant-role messages",
                ));
            }
            ContentPart::Reasoning { .. } => {}
            ContentPart::Refusal { text } if message.role == Role::Assistant => {
                content.push(json!({"type":"text","text":text}));
            }
            ContentPart::Refusal { .. } => {
                return Err(capability_error(
                    "refusals are only valid in assistant-role messages",
                ));
            }
        }
    }
    let content = if content.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else if content.len() == 1 && content[0]["type"] == "text" {
        content.remove(0)["text"].clone()
    } else {
        Value::Array(content)
    };
    let mut payload = json!({"role":role,"content":content});
    if !tool_calls.is_empty() {
        payload["tool_calls"] = Value::Array(tool_calls);
    }
    Ok(payload)
}

pub(crate) fn is_provider_safe_image_source(source: &str) -> bool {
    if source.starts_with("data:") {
        return true;
    }
    Url::parse(source).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
    })
}

async fn consume_chunk(
    events: &mpsc::Sender<ModelStreamEvent>,
    tool_calls: &mut BTreeMap<u64, PartialToolCall>,
    finish_reason: &mut Option<ModelFinishReason>,
    chunk: &Value,
    pricing: ProviderPricing,
) -> Result<(), ProviderExecutionError> {
    for choice in chunk["choices"].as_array().into_iter().flatten() {
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            // The choice's own index. This protocol has one content stream per
            // choice, so the choice *is* the block, and a provider asked for
            // more than one completion produces more than one.
            let block = choice["index"]
                .as_u64()
                .map(|index| u32::try_from(index).unwrap_or(u32::MAX));
            emit(
                events,
                ModelStreamEvent::TextDelta {
                    text: text.into(),
                    block,
                },
            )
            .await?;
        }
        for fragment in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = fragment["index"].as_u64().ok_or_else(|| {
                provider_error(
                    ModelErrorKind::Protocol,
                    false,
                    None,
                    "streamed tool call is missing its index",
                )
            })?;
            let partial = tool_calls.entry(index).or_default();
            if let Some(id) = fragment["id"].as_str() {
                partial.id.push_str(id);
            }
            if let Some(name) = fragment["function"]["name"].as_str() {
                partial.name.push_str(name);
            }
            if let Some(arguments) = fragment["function"]["arguments"].as_str() {
                partial.arguments.push_str(arguments);
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            *finish_reason = Some(match reason {
                "stop" => ModelFinishReason::Stop,
                "tool_calls" | "function_call" => ModelFinishReason::ToolCalls,
                "length" => ModelFinishReason::Length,
                "content_filter" => ModelFinishReason::ContentFilter,
                value => {
                    return Err(provider_error(
                        ModelErrorKind::Protocol,
                        false,
                        None,
                        format!("unsupported provider finish_reason {value}"),
                    ));
                }
            });
        }
    }
    if let Some(usage) = chunk["usage"].as_object() {
        let input_tokens = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output_tokens = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        emit(
            events,
            ModelStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cost_micros: calculate_cost(input_tokens, output_tokens, pricing),
            },
        )
        .await?;
    }
    Ok(())
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

async fn flush_tool_calls(
    events: &mpsc::Sender<ModelStreamEvent>,
    tool_calls: BTreeMap<u64, PartialToolCall>,
) -> Result<(), ProviderExecutionError> {
    for (_, tool_call) in tool_calls {
        if tool_call.id.is_empty() || tool_call.name.is_empty() {
            return Err(provider_error(
                ModelErrorKind::Protocol,
                false,
                None,
                "streamed tool call is missing id or function name",
            ));
        }
        let arguments = serde_json::from_str(&tool_call.arguments).map_err(|error| {
            provider_error(
                ModelErrorKind::Protocol,
                false,
                None,
                format!("streamed tool arguments are invalid JSON: {error}"),
            )
        })?;
        emit(
            events,
            ModelStreamEvent::ToolCall {
                id: tool_call.id,
                name: tool_call.name,
                arguments,
            },
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn emit(
    events: &mpsc::Sender<ModelStreamEvent>,
    event: ModelStreamEvent,
) -> Result<(), ProviderExecutionError> {
    events
        .send(event)
        .await
        .map_err(|_| ProviderExecutionError::ConsumerClosed)
}

pub(crate) fn calculate_cost(
    input_tokens: u64,
    output_tokens: u64,
    pricing: ProviderPricing,
) -> u64 {
    let total = u128::from(input_tokens) * u128::from(pricing.input_million_tokens_micros)
        + u128::from(output_tokens) * u128::from(pricing.output_million_tokens_micros);
    let rounded_up = total.saturating_add(999_999) / 1_000_000;
    u64::try_from(rounded_up).unwrap_or(u64::MAX)
}

pub(crate) async fn classify_http_error(
    response: reqwest::Response,
    credential: &ProviderCredential,
) -> ProviderExecutionError {
    let status = response.status();
    let retry_after_ms = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after_ms);
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "provider returned an unreadable error body".into());
    let message = credential.redact(body.chars().take(2048).collect());
    let (kind, retryable) = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => (ModelErrorKind::Authentication, false),
        StatusCode::PAYMENT_REQUIRED => (ModelErrorKind::Billing, false),
        StatusCode::TOO_MANY_REQUESTS => (ModelErrorKind::RateLimited, true),
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => {
            (ModelErrorKind::Timeout, true)
        }
        status if status.is_server_error() => (ModelErrorKind::Unavailable, true),
        _ if looks_like_context_overflow(&message) => (ModelErrorKind::ContextOverflow, false),
        _ => (ModelErrorKind::Protocol, false),
    };
    ProviderExecutionError::Provider {
        kind,
        retryable,
        status: Some(status.as_u16()),
        retry_after_ms,
        message,
    }
}

pub(crate) fn classify_transport_error(error: reqwest::Error) -> ProviderExecutionError {
    let kind = if error.is_timeout() {
        ModelErrorKind::Timeout
    } else if error.is_connect() {
        ModelErrorKind::Unavailable
    } else {
        ModelErrorKind::Protocol
    };
    let retryable = matches!(kind, ModelErrorKind::Timeout | ModelErrorKind::Unavailable);
    provider_error(
        kind,
        retryable,
        None,
        format!("provider transport error: {error}"),
    )
}

pub(crate) fn capability_error(message: impl Into<String>) -> ProviderExecutionError {
    provider_error(ModelErrorKind::CapabilityMismatch, false, None, message)
}

pub(crate) fn provider_error(
    kind: ModelErrorKind,
    retryable: bool,
    status: Option<u16>,
    message: impl Into<String>,
) -> ProviderExecutionError {
    ProviderExecutionError::Provider {
        kind,
        retryable,
        status,
        retry_after_ms: None,
        message: message.into(),
    }
}

fn parse_retry_after_ms(value: &str) -> Option<u64> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(seconds.saturating_mul(1_000));
    }
    let deadline = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
    let remaining = deadline.with_timezone(&chrono::Utc) - chrono::Utc::now();
    u64::try_from(remaining.num_milliseconds().max(0)).ok()
}

fn looks_like_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("context length")
        || message.contains("context window")
        || message.contains("too many tokens")
}
