use agent_protocol::{
    ContentPart, Message, ModelErrorKind, ModelFinishReason, ModelRequest, ModelStreamEvent,
    ReasoningPolicy, Role,
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
    /// What a prompt token served from the provider's cache costs, when the
    /// operator knows it.
    ///
    /// Separate from `input_million_tokens_micros` because it is a different
    /// price for the same token: OpenAI bills a cache hit at a tenth of a fresh
    /// prompt token, DeepSeek at roughly a tenth to a quarter. `None` is the
    /// honest unset -- a discount this adapter invented would be wrong for the
    /// next endpoint, and the safe direction for a number that ends Runs is the
    /// one we already charge. Unset means a cache hit is billed at the full
    /// input rate, which is what this adapter did before it read the field at
    /// all.
    pub cached_input_million_tokens_micros: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiCompatibleConfig {
    pub endpoint: String,
    pub model: String,
    pub pricing: ProviderPricing,
    pub response_timeout: Duration,
    pub stream_idle_timeout: Duration,
    /// The longest single reply this model will accept being asked for, when
    /// the operator knows it.
    ///
    /// Beside `model` and `endpoint` because that is what it is a property of.
    /// `None` means the field is not sent at all and the provider applies its
    /// own default, which is always valid -- unlike a number we would have had
    /// to guess.
    pub max_output_tokens: Option<u64>,
    /// Whether this endpoint accepts `reasoning_effort`, as the operator
    /// declared it.
    ///
    /// Off by default, and that default is the evidence-backed one for the
    /// servers this adapter talks to. `reasoning_effort` is an OpenAI spelling:
    /// DeepSeek reads `thinking: {type}`, Qwen reads `enable_thinking`, Z.ai
    /// reads its own object, and OpenRouter takes the nested `reasoning:
    /// {effort}` shape on this very endpoint (openclaw
    /// `providers/openai-completions.ts:826-861` branches five ways on exactly
    /// this). openclaw's own auto-detection turns the flat field *off* for
    /// proxy-like endpoints (`openai-completions-compat.ts:130-135`), which is
    /// the population this adapter exists to serve.
    ///
    /// So sending it unasked would put an unrecognised argument in front of
    /// every vLLM, SGLang and proxy we already talk to, to carry a policy that
    /// no caller can currently vary -- the sole producer hardcodes
    /// `Balanced` (`worker/src/lib.rs:6783`). Declared, it is sent and means
    /// what it says; undeclared, the request is byte-for-byte what it was.
    pub supports_reasoning_effort: bool,
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
    max_output_tokens: Option<u64>,
    supports_reasoning_effort: bool,
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
            max_output_tokens: config.max_output_tokens,
            supports_reasoning_effort: config.supports_reasoning_effort,
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
                // The stream ended. `[DONE]` is an SSE framing convention;
                // `finish_reason` is the provider saying the turn ended. A
                // server that closes cleanly after saying so has given us
                // everything, and refusing it threw away a complete,
                // already-paid-for answer -- the text had reached the person
                // and the Run failed anyway. opencode does not require the
                // sentinel at all: it is filtered out with the other
                // keep-alives before its parser sees it
                // (`opencode/packages/llm/src/protocols/shared.ts:247`).
                //
                // Without a finish_reason it is a different thing: a
                // connection dropped mid-answer looks exactly like a clean end
                // except for that, and reporting it as success would hand the
                // model half a turn as though it were whole. openclaw draws
                // the same line, in the same words -- "EOF without [DONE]
                // remains fail-closed".
                let Some(reason) = finish_reason else {
                    return Err(provider_error(
                        ModelErrorKind::Protocol,
                        false,
                        None,
                        "provider stream ended before the model said the turn was over",
                    ));
                };
                let reason = tool_turn(reason, &tool_calls);
                flush_tool_calls(&events, tool_calls).await?;
                emit(&events, ModelStreamEvent::Completed { reason }).await?;
                return Ok(());
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
                let reason = tool_turn(reason, &tool_calls);
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
            // A fault reported after the 200. This is the primary error
            // channel for the servers this adapter talks to -- vLLM, SGLang
            // and the proxies in front of them accept the request, open the
            // stream, and report a rate limit or an overrun as a data frame.
            // Unread, the frame was discarded and the Run failed with
            // "provider stream ended without [DONE]": a sentence about our own
            // framing, marked non-retryable, with the provider's own words
            // thrown away. The same shape is already handled one file over
            // (`anthropic_messages.rs`).
            if let Some(reported) = in_band_error(&chunk, credential) {
                return Err(reported);
            }
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
        });
        // What the caller asked for, capped by what the operator said this model
        // will accept.
        //
        // `request.max_output_tokens` carries two different intents through one
        // non-optional `u64`, and telling them apart here is not possible. Some
        // callers mean it: transcript compaction sets it to `max_summary_tokens`
        // (`worker/src/lib.rs:6940-6943`) because a summary really must be
        // short. The ordinary turn does not: `worker/src/lib.rs:6784` fills it
        // with the Run's *remaining budget*, and a desktop Run carries 400,000 --
        // which a real server rejects outright with "max_tokens=400000 cannot be
        // greater than max_model_len=204800", so no conversation could start.
        //
        // An earlier attempt at this dropped `max_tokens` unless a ceiling was
        // configured. That fixed the desktop and silently broke compaction,
        // which had a genuine reason for its 256 and stopped being able to say
        // so. The adapter now sends faithfully and the ceiling does the capping,
        // which is the only one of the two jobs the adapter has the standing to
        // do: it knows what the operator declared about this model, and it does
        // not know what the caller meant.
        //
        // Consequence worth stating plainly: with no ceiling configured, the Run
        // budget still goes out as `max_tokens` and a real server will still
        // reject it. The ceiling is what makes an OpenAI-compatible provider
        // usable, which is why the desktop now always writes one.
        payload["max_tokens"] = json!(match self.max_output_tokens {
            Some(ceiling) => ceiling.min(request.max_output_tokens),
            None => request.max_output_tokens,
        });
        // How hard to think, in the spelling this endpoint uses -- but only
        // where the operator said it has one.
        //
        // The same three values already go out as `reasoning: {effort}` on the
        // Responses adapter (`openai_responses.rs:330-334`). Unsent here, one
        // `ModelRequest` meant two different things depending on which adapter
        // failover picked, and `Thorough` bought nothing on this path.
        //
        // Flat rather than nested is what chat-completions takes (openclaw
        // `openai-completions-transport.ts:1927`, opencode
        // `protocols/openai-chat.ts:340`); the nested object is the Responses
        // shape, which OpenRouter confusingly also wants on this endpoint. That
        // is one of several reasons the field is gated rather than assumed:
        // this adapter has no model catalogue to tell OpenRouter from vLLM, so
        // the operator is the one who knows.
        if self.supports_reasoning_effort {
            payload["reasoning_effort"] = json!(match request.reasoning {
                ReasoningPolicy::Minimal => "low",
                ReasoningPolicy::Balanced => "medium",
                ReasoningPolicy::Thorough => "high",
            });
        }
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
        // Some OpenAI-compatible servers answer a `stream: true` request with
        // the non-streaming shape: the whole assistant turn arrives as
        // `message` where the streaming shape puts `delta`. Everything the
        // choice says hangs off that one field, so reading only `delta` found
        // nothing to emit and then completed the turn off the same choice's
        // `finish_reason` -- a Run that succeeded with the answer, the
        // thinking and the tool calls all missing, and nothing anywhere
        // saying so. openclaw normalises this in both of its code paths
        // (`openai-completions-transport.ts:672-674` and
        // `providers/openai-completions.ts:433-438`, whose comment names the
        // cause and notes refusal-only turns arrive this way too).
        let delta = match &choice["delta"] {
            Value::Null => &choice["message"],
            delta => delta,
        };
        // Two spellings, one fact. `reasoning` is what a vLLM-served Qwen3
        // emits; `reasoning_content` is DeepSeek's and what the SGLang and
        // vLLM reasoning parsers produce. Neither was read, so on a reasoning
        // model the entire thinking was dropped: one short answer from a real
        // server streamed 34 reasoning fragments and 2 content fragments, and
        // a person watched an empty screen for all of the first and then saw
        // four characters.
        let thinking = delta["reasoning"]
            .as_str()
            .or_else(|| delta["reasoning_content"].as_str())
            .filter(|text| !text.is_empty());
        if let Some(text) = thinking {
            let block = choice["index"]
                .as_u64()
                .map(|index| u32::try_from(index).unwrap_or(u32::MAX));
            emit(
                events,
                ModelStreamEvent::ReasoningDelta {
                    text: text.into(),
                    block,
                },
            )
            .await?;
        }
        // The choice's own index. This protocol has one content stream per
        // choice, so the choice *is* the block, and a provider asked for more
        // than one completion produces more than one.
        let block = choice["index"]
            .as_u64()
            .map(|index| u32::try_from(index).unwrap_or(u32::MAX));
        // `content` is typed `string | null` by OpenAI and sent as an array of
        // parts by several providers. Reading only the string meant an entire
        // answer could be skipped in silence: the Run reached `[DONE]` with a
        // finish_reason and reported success having emitted nothing, which is
        // the worst shape a failure can take because nothing says it happened.
        for said in content_parts(&delta["content"]) {
            let event = match said {
                Said::Text(text) => ModelStreamEvent::TextDelta { text, block },
                Said::Thinking(text) => ModelStreamEvent::ReasoningDelta { text, block },
            };
            emit(events, event).await?;
        }
        // A refusal is words the model produced, on its own field, with
        // `content` null beside it. Unread, a refusal was a successful empty
        // answer -- and the Run's own record said the model had said nothing.
        if let Some(text) = delta["refusal"].as_str().filter(|text| !text.is_empty()) {
            emit(
                events,
                ModelStreamEvent::Refusal {
                    text: text.to_owned(),
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
            // Replaced, not appended. Only `arguments` is streamed in
            // fragments; the id and the name are identity, and several
            // providers resend them whole with every fragment. Appending
            // turned one call into `call_1call_1call_1` naming
            // `read_fileread_file` -- a Tool that does not exist, refused by
            // the runtime, with the model told nothing it could act on.
            if let Some(id) = fragment["id"].as_str().filter(|id| !id.is_empty()) {
                partial.id.clear();
                partial.id.push_str(id);
            }
            if let Some(name) = fragment["function"]["name"]
                .as_str()
                .filter(|name| !name.is_empty())
            {
                partial.name.clear();
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
                // A terminal word this build has never seen still means the
                // turn ended. `eos_token`, `end_turn` and `COMPLETE` are all
                // in the wild, and refusing them destroyed a finished answer:
                // the text had already streamed to the person and the tokens
                // had already been paid for, and the Run failed anyway. None
                // of the three reference implementations does that.
                value => {
                    tracing::debug!(
                        finish_reason = value,
                        "provider ended the turn with a word this build does not know",
                    );
                    ModelFinishReason::Stop
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
        // The part of the prompt the provider served from its own cache, and
        // charged a fraction of the fresh rate for. It is a *subset* of
        // `prompt_tokens`, not a number beside it -- opencode states that
        // invariant outright (`packages/llm/src/schema/events.ts:7-60`) and
        // openclaw's arithmetic proves it by subtracting
        // (`providers/openai-completions.ts:1091`).
        //
        // Unread, the whole prompt was billed fresh. On an agent loop whose
        // prompt is mostly a cache hit every turn that is several times the real
        // charge, and `cost_micros` is not merely displayed: the worker spends
        // it against the Run's budget (`worker/src/lib.rs:6236-6246`), so Runs
        // died for money nobody was charged.
        //
        // The counts reported upward stay inclusive. A cache hit sits in the
        // context window exactly like a fresh token; only its price differs.
        let cached_input_tokens = usage
            .get("prompt_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        emit(
            events,
            ModelStreamEvent::Usage {
                input_tokens,
                output_tokens,
                cost_micros: calculate_cost(
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    pricing,
                ),
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
        // A call with no arguments is a call with no arguments. Models emit
        // these with `arguments` as `""` or omit the field, and parsing that
        // as JSON failed and took the whole turn down over a perfectly well
        // formed call. Both references treat empty as `{}`: opencode writes
        // `raw || "{}"` (`packages/llm/src/protocols/shared.ts:155-156`) and
        // openclaw returns `{}` for empty or whitespace
        // (`packages/ai/src/utils/json-parse.ts:130-132`).
        //
        // Arguments that were cut off mid-object stay a failure. openclaw
        // repairs and, failing that, silently substitutes `{}`; we do not
        // agree -- turning `{"path": "/etc/pas` into `{}` runs a call the
        // model never asked for, and for a write or an exec that is not the
        // smaller mistake. What we were missing is which call broke: "invalid
        // JSON" alone does not say which of eleven it was.
        let raw = tool_call.arguments.trim();
        let arguments = if raw.is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(raw).map_err(|error| {
                provider_error(
                    ModelErrorKind::Protocol,
                    false,
                    None,
                    format!(
                        "streamed tool arguments for {} are invalid JSON: {error}",
                        tool_call.name,
                    ),
                )
            })?
        };
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

/// What this turn cost, in micros.
///
/// `input_tokens` is the provider's inclusive prompt total and
/// `cached_input_tokens` is the subset of it served from cache. The two are made
/// disjoint before either is priced: charging the inclusive total at the fresh
/// rate *and* the cached subset at the cached rate would bill a cache hit twice,
/// which is the trap openclaw's `calculateCost` (`model-utils.ts:12-18`) avoids
/// by normalising to disjoint buckets at ingest.
///
/// A provider reporting more cache hits than prompt tokens is nonsense, and both
/// TypeScript references clamp it rather than trusting it (opencode's
/// `subtractTokens`, `protocols/shared.ts:61-76`). Here the clamp is not
/// cosmetic: unsigned subtraction would panic in a debug build and wrap to
/// something budget-ending in a release one. The reported cache hits are still
/// priced in full -- we clamp our own arithmetic, we do not silently rewrite
/// what the provider said.
pub(crate) fn calculate_cost(
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    pricing: ProviderPricing,
) -> u64 {
    // Unset is not a discount. Without a declared rate a cache hit costs what a
    // fresh prompt token costs, which is what this adapter charged before it
    // read the field -- and of the two ways to be wrong, over-charging ends a
    // Run early while under-charging quietly overspends a budget.
    let cached_rate = pricing
        .cached_input_million_tokens_micros
        .unwrap_or(pricing.input_million_tokens_micros);
    let fresh_input_tokens = input_tokens.saturating_sub(cached_input_tokens);
    let total = u128::from(fresh_input_tokens) * u128::from(pricing.input_million_tokens_micros)
        + u128::from(cached_input_tokens) * u128::from(cached_rate)
        + u128::from(output_tokens) * u128::from(pricing.output_million_tokens_micros);
    let rounded_up = total.saturating_add(999_999) / 1_000_000;
    u64::try_from(rounded_up).unwrap_or(u64::MAX)
}

/// A provider fault carried inside the stream rather than by the status line.
///
/// `None` when the chunk is an ordinary one, including a chunk that carries an
/// explicit `"error": null` -- several servers send that on every frame, and
/// treating it as a fault would fail every stream they produce.
///
/// The kind comes from the message where the message says so, because these
/// servers are not consistent about `type`: a context overrun arrives as
/// `invalid_request_error`, as `BadRequestError`, or with no type at all, and
/// the sentence is the one part that reliably names it.
/// A turn that asked for a tool ends as a tool turn, whatever word the provider
/// used to end it.
///
/// Plenty of servers in this family close a tool-calling turn with `stop`. Read
/// literally, the consequence is not cosmetic: `requested_tool_turn`
/// (`runtime/apps/worker/src/lib.rs:10089-10094`) matches only
/// `Completed { reason: ToolCalls }`, so nothing plans the tool, nothing runs
/// it, and the kernel marks the Run **succeeded** with the call abandoned --
/// and leaves a Tool call with no result in the committed transcript.
///
/// opencode promotes unconditionally
/// (`packages/llm/src/protocols/openai-chat.ts:465`). openclaw promotes only
/// when there was no visible text and the stream ended cleanly, and **drops**
/// the calls otherwise
/// (`packages/ai/src/transports/openai-completions-transport.ts:815-834`).
/// Either is coherent; what we do today is the one thing neither does, which is
/// to deliver the call and then abandon it. Taking opencode's form because
/// dropping a call the model asked for is a silent loss of its intent, while
/// running it is what the model asked for in the first place.
fn tool_turn(
    reason: ModelFinishReason,
    tool_calls: &BTreeMap<u64, PartialToolCall>,
) -> ModelFinishReason {
    if reason == ModelFinishReason::Stop && !tool_calls.is_empty() {
        return ModelFinishReason::ToolCalls;
    }
    reason
}

fn in_band_error(chunk: &Value, credential: &ProviderCredential) -> Option<ProviderExecutionError> {
    let error = chunk.get("error").filter(|value| !value.is_null())?;
    let kind_hint = error
        .get("type")
        .or_else(|| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let said = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("provider reported a fault with no message");
    let message = credential.redact(said.chars().take(2048).collect::<String>());
    let (kind, retryable) = if looks_like_context_overflow(&message) {
        (ModelErrorKind::ContextOverflow, false)
    } else if kind_hint.contains("rate_limit") || kind_hint.contains("overloaded") {
        (ModelErrorKind::RateLimited, true)
    } else if kind_hint.contains("quota") || kind_hint.contains("billing") {
        (ModelErrorKind::Billing, false)
    } else if kind_hint.contains("authentication") || kind_hint.contains("permission") {
        (ModelErrorKind::Authentication, false)
    } else if kind_hint.contains("timeout") {
        (ModelErrorKind::Timeout, true)
    } else if kind_hint.contains("server") || kind_hint.contains("unavailable") {
        (ModelErrorKind::Unavailable, true)
    } else {
        // Unrecognised, and deliberately not retried: a fault this build
        // cannot name is one it cannot say is safe to repeat.
        (ModelErrorKind::Protocol, false)
    };
    Some(ProviderExecutionError::Provider {
        kind,
        retryable,
        status: None,
        retry_after_ms: error
            .get("retry_after_ms")
            .and_then(Value::as_u64)
            .or_else(|| {
                error
                    .get("retry_after")
                    .and_then(Value::as_str)
                    .and_then(parse_retry_after_ms)
            }),
        message,
    })
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
        // Two different endings share this status code, and only the body
        // tells them apart. "Slow down" clears by waiting; "this account is
        // out of quota" does not, and retrying it is guaranteed-wasted time --
        // `RateLimited` is in the default `fallback_on` set and carries
        // `retryable: true`, so an exhausted key spent the full same-Provider
        // backoff *and* the whole fallback chain before the person was told,
        // wrongly, that they were calling too fast.
        StatusCode::TOO_MANY_REQUESTS if looks_like_exhausted_quota(&message) => {
            (ModelErrorKind::Billing, false)
        }
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

/// One thing the model said in a content part.
enum Said {
    Text(String),
    Thinking(String),
}

/// Everything a `delta.content` carries, whichever shape it came in.
///
/// OpenAI types this `string | null`; providers send an array of typed parts
/// as well. openclaw handles all three and names Mistral's thinking models
/// (`openclaw/packages/ai/src/transports/openai-completions-transport.ts:1061-1101`),
/// with a note that coercing the objects had produced literal "[object Object]"
/// in stored transcripts. Reading only the string is quieter and worse: the
/// answer simply is not there, and nothing says so.
fn content_parts(content: &Value) -> Vec<Said> {
    match content {
        Value::String(text) if !text.is_empty() => vec![Said::Text(text.clone())],
        Value::Array(parts) => parts.iter().flat_map(content_parts).collect(),
        Value::Object(part) => {
            let kind = part
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let text = part
                .get("text")
                .or_else(|| part.get("content"))
                .or_else(|| part.get("thinking"))
                .map(content_parts)
                .unwrap_or_default();
            // A thinking part is thinking wherever it appears. Routed to the
            // reasoning stream rather than the answer, because the two are
            // read by a person for different purposes and mixing them buries
            // the reply.
            if kind.contains("thinking") || kind.contains("reasoning") {
                text.into_iter()
                    .map(|said| match said {
                        Said::Text(text) | Said::Thinking(text) => Said::Thinking(text),
                    })
                    .collect()
            } else {
                text
            }
        }
        _ => Vec::new(),
    }
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

/// Whether a 429 body says the account has no allowance left, rather than that
/// it is calling too fast.
///
/// Deliberately the provider's own error codes and their canonical sentences,
/// not the word "quota". Vertex answers a *per-minute* rate limit with "Quota
/// exceeded for quota metric 'Generate Content API requests per minute'" --
/// which is the retryable case wearing the other one's vocabulary, and a loose
/// match on "quota" would stop retrying the one ending that waiting actually
/// fixes. The needles below are what the providers emit for an allowance that
/// is gone: OpenAI's `insufficient_quota` and the sentence it ships with, the
/// ChatGPT-plan `usage_limit_reached` / `usage_not_included` pair codex parses
/// out of this same status (`codex-rs/codex-api/src/api_bridge.rs:94-127`),
/// and Anthropic's balance sentence.
///
/// The in-band streaming path already made this split on the error's `type`
/// field (`in_band_error`, `kind_hint.contains("quota")`); here there is only
/// the body, because a provider that answers 429 answers with no stream at all.
fn looks_like_exhausted_quota(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("insufficient_quota")
        || message.contains("usage_limit_reached")
        || message.contains("usage_not_included")
        || message.contains("exceeded your current quota")
        || message.contains("credit balance is too low")
        || message.contains("billing_hard_limit_reached")
}
