use crate::openai_compatible::{
    ProviderCredential, ProviderExecutionError, ProviderPricing, calculate_cost, capability_error,
    classify_http_error, classify_transport_error, emit, is_provider_safe_image_source,
    looks_like_exhausted_quota, provider_error,
};
use agent_protocol::{
    ContentPart, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent,
    ProviderPrivateState, ReasoningPolicy, Role,
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
        let provider_id = "direct-openai-responses";
        let (request, omissions) = crate::prepare_request_for_provider(
            request,
            provider_id,
            "openai_responses",
            &self.model,
        )?;
        for omission in omissions {
            emit(&events, omission).await?;
        }
        self.execute_for_provider(provider_id, &request, credential, cancellation, events)
            .await
    }

    pub(crate) async fn execute_for_provider(
        &self,
        provider_id: &str,
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
                        // `output_index` is this API's name for which output
                        // item the delta belongs to, which is the same question
                        // the other adapters answer with a block index.
                        let block = value["output_index"]
                            .as_u64()
                            .map(|index| u32::try_from(index).unwrap_or(u32::MAX));
                        emit(
                            &events,
                            ModelStreamEvent::TextDelta {
                                text: text.into(),
                                block,
                            },
                        )
                        .await?;
                    }
                }
                "response.refusal.done" => {
                    if let Some(text) = value["refusal"].as_str().filter(|text| !text.is_empty()) {
                        emit(&events, ModelStreamEvent::Refusal { text: text.into() }).await?;
                    }
                }
                "response.output_item.done" if value["item"]["type"] == "reasoning" => {
                    let item = &value["item"];
                    let summary = item["summary"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part["text"].as_str())
                        .filter(|text| !text.is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>();
                    let private_state = match item["encrypted_content"].as_str() {
                        Some(encrypted_content) if !encrypted_content.is_empty() => {
                            let id = required_string(item, "id", "reasoning item is missing id")?;
                            Some(ProviderPrivateState {
                                provider_id: provider_id.to_owned(),
                                protocol: "openai_responses".into(),
                                model: self.model.clone(),
                                format: "openai.responses.reasoning.v1".into(),
                                data: json!({
                                    "id": id,
                                    "encrypted_content": encrypted_content,
                                })
                                .to_string(),
                            })
                        }
                        _ => None,
                    };
                    if !summary.is_empty() || private_state.is_some() {
                        emit(
                            &events,
                            ModelStreamEvent::Reasoning {
                                summary,
                                private_state,
                            },
                        )
                        .await?;
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
                    // A call with no arguments is a call with no arguments,
                    // the same as in the chat-completions adapter. This one
                    // was the second instance of that defect and had no test
                    // at all; the name goes into the failure for the same
                    // reason it does there -- "invalid JSON" alone does not say
                    // which call broke.
                    let trimmed = arguments.trim();
                    let arguments = if trimmed.is_empty() {
                        Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(trimmed).map_err(|error| {
                            provider_error(
                                ModelErrorKind::Protocol,
                                false,
                                None,
                                format!(
                                    "function call arguments for {name} are invalid JSON: {error}"
                                ),
                            )
                        })?
                    };
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
                // Two endings arrive on this one event, and the response says
                // which: `incomplete_details.reason` is `max_output_tokens` or
                // `content_filter`. Reading neither reported both as `Length`,
                // and `Length` is a *success* -- the kernel ends the Run
                // `run.succeeded` carrying the truncation. So a prompt the
                // provider's safety filter refused was drawn as an answer that
                // merely ran long, sending the person to shorten a prompt that
                // was never too long, and the one failure only they can act on
                // was never named.
                //
                // openclaw splits exactly this event, and states the rule:
                // a content-filtered turn is a provider error rather than a
                // truncated answer
                // (`packages/ai/src/providers/openai-responses-terminal-usage.ts:87-105`).
                "response.incomplete" => {
                    emit_usage(&events, &value["response"]["usage"], self.pricing).await?;
                    let reason = match value["response"]["incomplete_details"]["reason"].as_str() {
                        Some("content_filter") => ModelFinishReason::ContentFilter,
                        // Anything else, including a response that omits the
                        // detail entirely, keeps the ending this branch has
                        // always reported. An unnamed incomplete turn is a
                        // short answer we still have the text of; calling it a
                        // filter block would be inventing a reason.
                        _ => ModelFinishReason::Length,
                    };
                    emit(&events, ModelStreamEvent::Completed { reason }).await?;
                    return Ok(());
                }
                // Several different endings arrive on this one event, and the
                // `error` object says which. Reporting `Protocol` for all of
                // them was not a naming quibble: `Protocol` is neither
                // retryable nor in any `fallback_on` set, so an exhausted
                // account ended the Run on the spot, was told "回复的格式不对"
                // when nothing was malformed, and never reached the second
                // candidate that exists for exactly that case.
                //
                // codex splits the same event on the same field --
                // `response.failed` reads `error.code` and answers
                // `insufficient_quota` with `QuotaExceeded` rather than the
                // generic stream error
                // (`codex-rs/codex-api/src/sse/responses.rs:387-400`, with
                // `is_quota_exceeded_error` at `:629-631`), covered end to end
                // by `codex-rs/core/tests/suite/quota_exceeded.rs`. We agree
                // and take the same split, with one difference: codex matches
                // `error.code` exactly, while this reads the code *and* the
                // sentence through `looks_like_exhausted_quota`, because the
                // 429 path in this gateway already had to -- OpenAI-compatible
                // servers in this family are not consistent about which field
                // carries the name.
                "response.failed" => {
                    let message = value["response"]["error"]["message"]
                        .as_str()
                        .unwrap_or("OpenAI Responses request failed");
                    let code = value["response"]["error"]["code"]
                        .as_str()
                        .unwrap_or_default();
                    let (kind, retryable) = if looks_like_refused_content(code) {
                        // Never retried and never carried elsewhere: the
                        // content is what was refused, so a second attempt and
                        // a second vendor both fail the same way -- and the
                        // second vendor would have been shown content the
                        // first one declined. `ContentFilter` is deliberately
                        // outside the `fallback_on` whitelist
                        // (`RuntimeExecutionPolicySnapshot::is_bounded_and_safe`).
                        (ModelErrorKind::ContentFilter, false)
                    } else if looks_like_exhausted_quota(code)
                        || looks_like_exhausted_quota(message)
                    {
                        // Not retryable on the account that reported it, and
                        // that is the point: `Billing` crosses to another
                        // provider anyway (`failover.rs`,
                        // `crosses_to_another_provider`), because a different
                        // provider is a different account.
                        (ModelErrorKind::Billing, false)
                    } else if looks_like_transient_response_failure(code) {
                        // The same fault must not end differently for arriving
                        // a moment later. Reaching us before the stream opens,
                        // a server fault is `HTTP 500` -> `(Unavailable, true)`
                        // and is both retried and carried to the next
                        // candidate. Arriving after the 200 as
                        // `response.failed { code: "server_error" }` it used to
                        // fall in the unnamed arm below and become fatal -- no
                        // retry, second candidate never called. Arrival timing
                        // is not a property of the failure.
                        (ModelErrorKind::Unavailable, true)
                    } else if code.eq_ignore_ascii_case("rate_limit_exceeded") {
                        (ModelErrorKind::RateLimited, true)
                    } else if code.eq_ignore_ascii_case("context_length_exceeded") {
                        (ModelErrorKind::ContextOverflow, false)
                    } else {
                        // Unnamed, and deliberately still fatal. codex defaults
                        // this position to `Retryable`
                        // (`codex-rs/codex-api/src/sse/responses.rs:387-410`)
                        // and we do not follow it that far: retrying a fault
                        // this build cannot describe spends someone's money on
                        // a guess. What changed is that the transient codes
                        // above are no longer unnamed.
                        (ModelErrorKind::Protocol, false)
                    };
                    return Err(provider_error(
                        kind,
                        retryable,
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
            "reasoning": {"effort": effort},
            "include": ["reasoning.encrypted_content"]
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

    pub(crate) fn model(&self) -> &str {
        &self.model
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
                ContentPart::Reasoning {
                    summary,
                    private_state: Some(state),
                } if state.format == "openai.responses.reasoning.v1" => {
                    let private: Value = serde_json::from_str(&state.data).map_err(|_| {
                        provider_error(
                            ModelErrorKind::Protocol,
                            false,
                            None,
                            "OpenAI reasoning continuation state is invalid",
                        )
                    })?;
                    let id = required_string(
                        &private,
                        "id",
                        "OpenAI reasoning continuation state is missing id",
                    )?;
                    let encrypted_content = required_string(
                        &private,
                        "encrypted_content",
                        "OpenAI reasoning continuation state is missing encrypted content",
                    )?;
                    input.push(json!({
                        "type": "reasoning",
                        "id": id,
                        "summary": summary.iter().map(|text| json!({
                            "type": "summary_text",
                            "text": text,
                        })).collect::<Vec<_>>(),
                        "encrypted_content": encrypted_content,
                    }));
                }
                ContentPart::Reasoning { .. } => {}
                ContentPart::Refusal { text } if message.role == Role::Assistant => {
                    message_content.push(json!({"type":"refusal","refusal":text}));
                }
                ContentPart::Refusal { .. } => {
                    return Err(capability_error(
                        "refusals are only valid in assistant messages",
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
            // Zero because this adapter does not read its own cache field yet,
            // not because a Responses turn never hits cache -- it reports
            // `input_tokens_details.cached_tokens`, and reading it here is its
            // own entry in the sweep.
            cost_micros: calculate_cost(input_tokens, output_tokens, 0, pricing),
        },
    )
    .await
}

/// Whether a `response.failed` names a policy refusal rather than a fault.
///
/// The provider's own codes, matched exactly, and nothing looser. A substring
/// search would be actively harmful here: the destination is the one kind that
/// must never be handed to a second provider, so a false positive silently
/// ends a Run that a fallback would have answered. `invalid_prompt` is what
/// OpenAI returns for a prompt its usage policy declined; `bio_policy` and
/// `cyber_policy` are the two topic-specific refusals codex parses out of this
/// same event (`codex-rs/codex-api/src/sse/responses.rs:398-405`);
/// `content_filter` and `content_policy_violation` are what other servers in
/// this family spell it.
/// Codes that name a fault the provider expects to pass.
///
/// Kept narrow and exact. The point of this list is that these are the codes
/// whose out-of-band twin is an HTTP 5xx, which this gateway already treats as
/// `Unavailable` and retries; anything not on it stays fatal.
fn looks_like_transient_response_failure(code: &str) -> bool {
    matches!(
        code.to_ascii_lowercase().as_str(),
        "server_error" | "server_overloaded" | "slow_down" | "internal_error"
    )
}

fn looks_like_refused_content(code: &str) -> bool {
    matches!(
        code.to_ascii_lowercase().as_str(),
        "invalid_prompt"
            | "bio_policy"
            | "cyber_policy"
            | "content_filter"
            | "content_policy_violation"
    )
}
