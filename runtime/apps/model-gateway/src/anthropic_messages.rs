use crate::openai_compatible::{
    ProviderCredential, ProviderExecutionError, ProviderPricing, calculate_cost, capability_error,
    classify_http_error, classify_transport_error, emit, provider_error,
};
use agent_protocol::{
    ContentPart, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent, Role,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicMessagesConfig {
    pub endpoint: String,
    pub model: String,
    pub anthropic_version: String,
    pub pricing: ProviderPricing,
    pub response_timeout: Duration,
    pub stream_idle_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct AnthropicMessagesAdapter {
    client: Client,
    endpoint: Url,
    model: String,
    anthropic_version: String,
    pricing: ProviderPricing,
    response_timeout: Duration,
    stream_idle_timeout: Duration,
}

#[derive(Default)]
struct AnthropicStreamState {
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<ModelFinishReason>,
    tools: BTreeMap<u64, PartialToolUse>,
}

#[derive(Default)]
struct PartialToolUse {
    id: String,
    name: String,
    input_json: String,
}

impl AnthropicMessagesAdapter {
    pub fn new(config: AnthropicMessagesConfig) -> Result<Self, ProviderExecutionError> {
        let endpoint = parse_endpoint(&config.endpoint)?;
        if config.model.trim().is_empty()
            || config.anthropic_version.trim().is_empty()
            || config.response_timeout.is_zero()
            || config.stream_idle_timeout.is_zero()
        {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "provider model, Anthropic version, and timeouts must be configured".into(),
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
            anthropic_version: config.anthropic_version,
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
            .header("x-api-key", credential.expose())
            .header("anthropic-version", &self.anthropic_version)
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
        let mut state = AnthropicStreamState::default();
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
                    "provider stream ended without message_stop",
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
            let value: Value = serde_json::from_str(&event.data).map_err(|error| {
                provider_error(
                    ModelErrorKind::Protocol,
                    false,
                    None,
                    format!("provider SSE data is not valid JSON: {error}"),
                )
            })?;
            let event_type = value["type"].as_str().unwrap_or(event.event.as_str());
            match event_type {
                "message_start" => {
                    state.input_tokens = value["message"]["usage"]["input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                }
                "content_block_start" => {
                    consume_block_start(&events, &mut state, &value).await?;
                }
                "content_block_delta" => {
                    consume_block_delta(&events, &mut state, &value).await?;
                }
                "content_block_stop" => {
                    let index = required_index(&value)?;
                    if let Some(tool) = state.tools.remove(&index) {
                        emit_tool(&events, tool).await?;
                    }
                }
                "message_delta" => {
                    state.output_tokens = value["usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or(state.output_tokens);
                    if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                        state.stop_reason = Some(map_stop_reason(reason)?);
                    }
                }
                "message_stop" => {
                    if !state.tools.is_empty() {
                        return Err(provider_error(
                            ModelErrorKind::Protocol,
                            false,
                            None,
                            "message stopped before all tool blocks completed",
                        ));
                    }
                    let reason = state.stop_reason.ok_or_else(|| {
                        provider_error(
                            ModelErrorKind::Protocol,
                            false,
                            None,
                            "message_stop arrived without stop_reason",
                        )
                    })?;
                    emit(
                        &events,
                        ModelStreamEvent::Usage {
                            input_tokens: state.input_tokens,
                            output_tokens: state.output_tokens,
                            cost_micros: calculate_cost(
                                state.input_tokens,
                                state.output_tokens,
                                self.pricing,
                            ),
                        },
                    )
                    .await?;
                    emit(&events, ModelStreamEvent::Completed { reason }).await?;
                    return Ok(());
                }
                "error" => {
                    let error_type = value["error"]["type"].as_str().unwrap_or_default();
                    let (kind, retryable) = match error_type {
                        "overloaded_error" | "api_error" => (ModelErrorKind::Unavailable, true),
                        "rate_limit_error" => (ModelErrorKind::RateLimited, true),
                        "authentication_error" | "permission_error" => {
                            (ModelErrorKind::Authentication, false)
                        }
                        _ => (ModelErrorKind::Protocol, false),
                    };
                    let message = value["error"]["message"]
                        .as_str()
                        .unwrap_or("Anthropic Messages stream failed");
                    return Err(provider_error(
                        kind,
                        retryable,
                        None,
                        credential.redact(message.to_owned()),
                    ));
                }
                "ping" => {}
                _ => {}
            }
        }
    }

    fn request_payload(&self, request: &ModelRequest) -> Result<Value, ProviderExecutionError> {
        if request.output_schema.is_some() {
            return Err(capability_error(
                "structured output is not supported by the Anthropic Messages adapter",
            ));
        }
        let (system, messages) = anthropic_messages(request)?;
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema
                })
            })
            .collect::<Vec<_>>();
        let mut payload = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": request.max_output_tokens,
            "stream": true
        });
        if !system.is_empty() {
            payload["system"] = Value::Array(system);
        }
        if !tools.is_empty() {
            payload["tools"] = Value::Array(tools);
        }
        Ok(payload)
    }
}

fn anthropic_messages(
    request: &ModelRequest,
) -> Result<(Vec<Value>, Vec<Value>), ProviderExecutionError> {
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        if message.role == Role::System {
            for part in &message.content {
                match part {
                    ContentPart::Text { text } => {
                        system.push(json!({"type":"text","text":text}));
                    }
                    _ => {
                        return Err(capability_error(
                            "Anthropic system messages only support text in this adapter",
                        ));
                    }
                }
            }
            continue;
        }
        let role = if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        let content = anthropic_content(message.role, &message.content)?;
        if content.is_empty() {
            continue;
        }
        if let Some(previous) = messages.last_mut()
            && previous["role"] == role
        {
            previous["content"]
                .as_array_mut()
                .expect("adapter messages always contain content arrays")
                .extend(content);
        } else {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    Ok((system, messages))
}

fn anthropic_content(
    role: Role,
    parts: &[ContentPart],
) -> Result<Vec<Value>, ProviderExecutionError> {
    let mut content = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } if role != Role::Tool => {
                content.push(json!({"type":"text","text":text}));
            }
            ContentPart::ToolCall {
                tool_call_id,
                name,
                arguments,
            } if role == Role::Assistant => content.push(json!({
                "type":"tool_use",
                "id":tool_call_id,
                "name":name,
                "input":arguments
            })),
            ContentPart::ToolResult {
                tool_call_id,
                content: result,
            } if role == Role::Tool => content.push(json!({
                "type":"tool_result",
                "tool_use_id":tool_call_id,
                "content":result.as_str().map(ToOwned::to_owned).unwrap_or_else(|| result.to_string())
            })),
            ContentPart::Image { .. } => {
                return Err(capability_error(
                    "image input is not supported by the first Anthropic Messages adapter",
                ));
            }
            ContentPart::Audio { .. } => {
                return Err(capability_error(
                    "audio input is not supported by the Anthropic Messages adapter",
                ));
            }
            ContentPart::Text { .. } => {
                return Err(capability_error("tool messages require tool results"));
            }
            ContentPart::ToolCall { .. } => {
                return Err(capability_error(
                    "tool calls are only valid in assistant messages",
                ));
            }
            ContentPart::ToolResult { .. } => {
                return Err(capability_error(
                    "tool results are only valid in tool messages",
                ));
            }
        }
    }
    Ok(content)
}

async fn consume_block_start(
    events: &mpsc::Sender<ModelStreamEvent>,
    state: &mut AnthropicStreamState,
    value: &Value,
) -> Result<(), ProviderExecutionError> {
    let index = required_index(value)?;
    match value["content_block"]["type"].as_str() {
        Some("text") => {
            if let Some(text) = value["content_block"]["text"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                emit(events, ModelStreamEvent::TextDelta { text: text.into() }).await?;
            }
        }
        Some("tool_use") => {
            let block = &value["content_block"];
            let id = required_string(block, "id", "tool_use block is missing id")?;
            let name = required_string(block, "name", "tool_use block is missing name")?;
            let input_json = match block.get("input") {
                Some(Value::Object(map)) if !map.is_empty() => {
                    Value::Object(map.clone()).to_string()
                }
                _ => String::new(),
            };
            state.tools.insert(
                index,
                PartialToolUse {
                    id,
                    name,
                    input_json,
                },
            );
        }
        Some("thinking" | "redacted_thinking") => {}
        Some(kind) => {
            return Err(provider_error(
                ModelErrorKind::Protocol,
                false,
                None,
                format!("unsupported Anthropic content block {kind}"),
            ));
        }
        None => {
            return Err(provider_error(
                ModelErrorKind::Protocol,
                false,
                None,
                "content block is missing type",
            ));
        }
    }
    Ok(())
}

async fn consume_block_delta(
    events: &mpsc::Sender<ModelStreamEvent>,
    state: &mut AnthropicStreamState,
    value: &Value,
) -> Result<(), ProviderExecutionError> {
    match value["delta"]["type"].as_str() {
        Some("text_delta") => {
            if let Some(text) = value["delta"]["text"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                emit(events, ModelStreamEvent::TextDelta { text: text.into() }).await?;
            }
        }
        Some("input_json_delta") => {
            let index = required_index(value)?;
            let tool = state.tools.get_mut(&index).ok_or_else(|| {
                provider_error(
                    ModelErrorKind::Protocol,
                    false,
                    None,
                    "tool input delta arrived before tool_use block",
                )
            })?;
            if let Some(fragment) = value["delta"]["partial_json"].as_str() {
                tool.input_json.push_str(fragment);
            }
        }
        Some("thinking_delta" | "signature_delta") => {}
        Some(kind) => {
            return Err(provider_error(
                ModelErrorKind::Protocol,
                false,
                None,
                format!("unsupported Anthropic content delta {kind}"),
            ));
        }
        None => {
            return Err(provider_error(
                ModelErrorKind::Protocol,
                false,
                None,
                "content delta is missing type",
            ));
        }
    }
    Ok(())
}

async fn emit_tool(
    events: &mpsc::Sender<ModelStreamEvent>,
    tool: PartialToolUse,
) -> Result<(), ProviderExecutionError> {
    let input = if tool.input_json.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(&tool.input_json).map_err(|error| {
            provider_error(
                ModelErrorKind::Protocol,
                false,
                None,
                format!("streamed tool input is invalid JSON: {error}"),
            )
        })?
    };
    emit(
        events,
        ModelStreamEvent::ToolCall {
            id: tool.id,
            name: tool.name,
            arguments: input,
        },
    )
    .await
}

fn map_stop_reason(reason: &str) -> Result<ModelFinishReason, ProviderExecutionError> {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => Ok(ModelFinishReason::Stop),
        "tool_use" => Ok(ModelFinishReason::ToolCalls),
        "max_tokens" | "model_context_window_exceeded" => Ok(ModelFinishReason::Length),
        "refusal" => Ok(ModelFinishReason::ContentFilter),
        value => Err(provider_error(
            ModelErrorKind::Protocol,
            false,
            None,
            format!("unsupported Anthropic stop_reason {value}"),
        )),
    }
}

fn required_index(value: &Value) -> Result<u64, ProviderExecutionError> {
    value["index"].as_u64().ok_or_else(|| {
        provider_error(
            ModelErrorKind::Protocol,
            false,
            None,
            "content block event is missing index",
        )
    })
}

fn required_string(
    value: &Value,
    field: &str,
    message: &'static str,
) -> Result<String, ProviderExecutionError> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| provider_error(ModelErrorKind::Protocol, false, None, message))
}

fn parse_endpoint(value: &str) -> Result<Url, ProviderExecutionError> {
    let endpoint = Url::parse(value).map_err(|error| {
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
    Ok(endpoint)
}
