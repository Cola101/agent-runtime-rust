use crate::openai_compatible::{
    ProviderCredential, ProviderExecutionError, ProviderPricing, calculate_cost, capability_error,
    classify_http_error, classify_transport_error, emit, is_provider_safe_image_source,
    provider_error,
};
use agent_protocol::{
    ContentPart, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent,
    ReasoningPolicy, Role,
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiResponsesConfig {
    pub endpoint: String,
    pub model: String,
    pub pricing: ProviderPricing,
    pub response_timeout: Duration,
    pub stream_idle_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct OpenAiResponsesAdapter {
    client: Client,
    endpoint: Url,
    model: String,
    pricing: ProviderPricing,
    response_timeout: Duration,
    stream_idle_timeout: Duration,
}

impl OpenAiResponsesAdapter {
    pub fn new(config: OpenAiResponsesConfig) -> Result<Self, ProviderExecutionError> {
        let endpoint = parse_endpoint(&config.endpoint)?;
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
        let mut saw_tool_call = false;
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
                    "provider stream ended without response.completed",
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
                "response.output_text.delta" => {
                    if let Some(text) = value["delta"].as_str().filter(|text| !text.is_empty()) {
                        emit(&events, ModelStreamEvent::TextDelta { text: text.into() }).await?;
                    }
                }
                "response.output_item.done" if value["item"]["type"] == "function_call" => {
                    let item = &value["item"];
                    let id = required_string(item, "call_id", "function call is missing call_id")?;
                    let name = required_string(item, "name", "function call is missing name")?;
                    let arguments = item["arguments"].as_str().ok_or_else(|| {
                        provider_error(
                            ModelErrorKind::Protocol,
                            false,
                            None,
                            "function call is missing arguments",
                        )
                    })?;
                    let arguments = serde_json::from_str(arguments).map_err(|error| {
                        provider_error(
                            ModelErrorKind::Protocol,
                            false,
                            None,
                            format!("function call arguments are invalid JSON: {error}"),
                        )
                    })?;
                    emit(
                        &events,
                        ModelStreamEvent::ToolCall {
                            id,
                            name,
                            arguments,
                        },
                    )
                    .await?;
                    saw_tool_call = true;
                }
                "response.completed" => {
                    emit_usage(&events, &value["response"]["usage"], self.pricing).await?;
                    emit(
                        &events,
                        ModelStreamEvent::Completed {
                            reason: if saw_tool_call {
                                ModelFinishReason::ToolCalls
                            } else {
                                ModelFinishReason::Stop
                            },
                        },
                    )
                    .await?;
                    return Ok(());
                }
                "response.incomplete" => {
                    emit_usage(&events, &value["response"]["usage"], self.pricing).await?;
                    emit(
                        &events,
                        ModelStreamEvent::Completed {
                            reason: ModelFinishReason::Length,
                        },
                    )
                    .await?;
                    return Ok(());
                }
                "response.failed" => {
                    let message = value["response"]["error"]["message"]
                        .as_str()
                        .unwrap_or("OpenAI Responses request failed");
                    return Err(provider_error(
                        ModelErrorKind::Protocol,
                        false,
                        None,
                        credential.redact(message.to_owned()),
                    ));
                }
                "error" => {
                    let message = value["message"]
                        .as_str()
                        .unwrap_or("OpenAI Responses stream failed");
                    return Err(provider_error(
                        ModelErrorKind::Protocol,
                        false,
                        None,
                        credential.redact(message.to_owned()),
                    ));
                }
                _ => {}
            }
        }
    }

    fn request_payload(&self, request: &ModelRequest) -> Result<Value, ProviderExecutionError> {
        let input = response_input(request)?;
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": true
                })
            })
            .collect::<Vec<_>>();
        let effort = match request.reasoning {
            ReasoningPolicy::Minimal => "low",
            ReasoningPolicy::Balanced => "medium",
            ReasoningPolicy::Thorough => "high",
        };
        let mut payload = json!({
            "model": self.model,
            "input": input,
            "stream": true,
            "store": false,
            "max_output_tokens": request.max_output_tokens,
            "reasoning": {"effort": effort}
        });
        if !tools.is_empty() {
            payload["tools"] = Value::Array(tools);
            payload["tool_choice"] = Value::String("auto".into());
        }
        if let Some(schema) = &request.output_schema {
            payload["text"] = json!({
                "format": {
                    "type": "json_schema",
                    "name": "agent_output",
                    "strict": true,
                    "schema": schema
                }
            });
        }
        Ok(payload)
    }
}

fn response_input(request: &ModelRequest) -> Result<Vec<Value>, ProviderExecutionError> {
    let mut input = Vec::new();
    for message in &request.messages {
        let role = match message.role {
            Role::System => "system",
            Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
        };
        let mut message_content = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text { text } => message_content.push(json!({
                    "type": if message.role == Role::Assistant {"output_text"} else {"input_text"},
                    "text": text
                })),
                ContentPart::Image { source, .. }
                    if message.role == Role::User && is_provider_safe_image_source(source) =>
                {
                    message_content.push(json!({"type":"input_image","image_url":source}));
                }
                ContentPart::Image { .. } => {
                    return Err(capability_error(
                        "image input must be a safe HTTP(S) or data URL in a user message",
                    ));
                }
                ContentPart::Audio { .. } => {
                    return Err(capability_error(
                        "audio input is not supported by the Responses adapter",
                    ));
                }
                ContentPart::ToolCall {
                    tool_call_id,
                    name,
                    arguments,
                } if message.role == Role::Assistant => input.push(json!({
                    "type": "function_call",
                    "call_id": tool_call_id,
                    "name": name,
                    "arguments": arguments.to_string()
                })),
                ContentPart::ToolResult {
                    tool_call_id,
                    content,
                } if message.role == Role::Tool => input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": content.as_str().map(ToOwned::to_owned).unwrap_or_else(|| content.to_string())
                })),
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
        if !message_content.is_empty() {
            input.push(json!({"role":role,"content":message_content}));
        }
    }
    Ok(input)
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

async fn emit_usage(
    events: &mpsc::Sender<ModelStreamEvent>,
    usage: &Value,
    pricing: ProviderPricing,
) -> Result<(), ProviderExecutionError> {
    let Some(usage) = usage.as_object() else {
        return Ok(());
    };
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
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
    .await
}
