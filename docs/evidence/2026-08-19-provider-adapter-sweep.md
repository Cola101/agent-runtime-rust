# Provider 适配器逐字段扫描（2026-08-19）

四条并行线拿 Codex / OpenClaw / opencode 的源码，把
`runtime/apps/model-gateway/src/openai_compatible.rs` 逐字段比对了一遍。
每一条都经过一次**对抗性复核**（默认判定为「推翻」，除非能从源码确认）；
下面只列**没有被推翻**的。

**这次扫描是被批评出来的。** 协议适配是最底层的一层，参考源码一直就在本地，
而我此前是靠一台真机偶然撞出 `delta.reasoning` 被丢掉才发现问题的——
正确做法从一开始就该是这种扫描。

| 严重度 | 条数 |
| --- | --- |
| **blocking** | 27 |
| **degraded** | 60 |
| **cosmetic** | 24 |
| 合计 | 111 |

## blocking（27 条）

### delta.content (array / typed-object form)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:691 -> getCompletionsContentDeltas at :1061-1101 (handles string | array | {type,text|content|thinking}, recursing; routes type containing 'thinking'/'reasoning' to a thinking block and 'text'/'output_text'/'*.output_text' to visible text)
- **我们**：nothing reads it — openai_compatible.rs:386 uses `delta["content"].as_str()`, which yields None for an array or object and silently drops the whole delta
- **后果**：Against any provider that streams structured content parts (openclaw names Mistral thinking models explicitly, and the same shape appears on some vLLM/LiteLLM builds), the entire assistant answer is dropped silently. The run reaches [DONE] with a finish_reason and reports success having emitted zero TextDelta. A person sees an empty answer with no error.
- **同不同意**：同意 —— openclaw's own code comment says coercing these objects produced persisted '[object Object]' text, so they hit this in production. Silently returning empty is worse than that. opencode does not handle it either, but opencode fails loudly (Schema.String decode error, openai-chat.test.ts:619-625 'fails on malformed stream events') rather than silently emitting nothing — either behaviour beats ours.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:386 — branch on Value::String vs Value::Array before `.as_str()`; at minimum return a Protocol error when content is present but not a string, so it fails loudly.
- **复核更正**：Line number only: the cited `openai_compatible.rs:386` is now :410 (commit cde0e90 "Show the model thinking instead of an empty screen" inserted the reasoning branch and its comment above it). `consume_chunk` starts at :375. Everything else in the gap is accurate as written. Two refinements to the CHANGE: (1) the useful branch is three-way, not two — string, array (recurse per part), and object (r

### delta.tool_calls[].id

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:140 (schema) -> tool-stream.ts:126 `const id = delta.id ?? current?.id` (replace/keep, never concatenate); openclaw .../openai-completions-transport.ts:771-773 `if (toolCall.id) { block.id = toolCall.id; ... }` (replace)
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:412-414 — `partial.id.push_str(id)` (CONCATENATE)
- **后果**：Both references replace; we append. Against any provider that re-sends the full tool_call id on every argument fragment — which several Azure and vLLM builds do — a 5-fragment call yields id 'call_abccall_abccall_abccall_abccall_abc'. That id is then sent back as tool_call_id on the tool-result message (openai_compatible.rs:290-294), the provider rejects the turn, and the tool loop breaks with a confusing 400.
- **同不同意**：同意 —— Two independent references chose replace semantics. Concatenation is only correct if ids are guaranteed to arrive split across fragments and never repeated, which the wire format does not promise. This is a live correctness bug, not a missing feature.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:412-414 — replace `partial.id.push_str(id)` with assignment (set if non-empty), matching tool-stream.ts:126.

### delta.tool_calls[].function.name

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:134 (schema), :434 -> tool-stream.ts:127 `const name = delta.name ?? current?.name` (replace/keep); openclaw .../openai-completions-transport.ts:776-778 `if (toolCall.function?.name) block.name = toolCall.function.name` (replace)
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:415-417 — `partial.name.push_str(name)` (CONCATENATE)
- **后果**：Same defect as the id. A provider that repeats the function name on each fragment produces 'read_fileread_fileread_file'; we then emit a ToolCall for a tool that does not exist and the tool runtime fails to resolve it. Even one repeat is fatal, because tool dispatch is an exact-name lookup.
- **同不同意**：同意 —— Both references replace. Neither treats the name as a streamed fragment, because no provider splits a function name mid-token — the only reason a name appears twice is repetition.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:415-417 — replace `partial.name.push_str(name)` with assignment.

### delta.refusal

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:709-717 — surfaced as visible text with the comment that Chat Completions puts safety/structured-output refusals in `refusal` with content null
- **我们**：nothing reads it
- **后果**：When the model refuses, `content` is null and `refusal` carries the text. We emit zero TextDelta, then see finish_reason 'stop', and report a successful run with a completely empty assistant turn. The person is told nothing — not that it was refused, not that anything happened. We have ModelStreamEvent::Refusal (runtime/crates/protocol/src/lib.rs:3054) and openai_responses.rs:177 already emits it; this adapter does not.
- **同不同意**：同意 —— An empty successful turn is the worst possible rendering of a refusal. openclaw's comment shows they fixed this exact silent-empty-turn symptom. opencode does not read refusal either, which is a gap in opencode, not a licence for ours.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:386 — read `delta["refusal"].as_str()` and emit ModelStreamEvent::Refusal { text }, mirroring openai_responses.rs:177.

### choices[].finish_reason == any other/unknown value

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:383 `return "unknown"` — the stream still finishes normally with everything already emitted; openclaw openai-stop-reason.ts:36-39 — stopReason 'error' with the raw reason preserved in the message
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:428-435 — hard Protocol error, non-retryable, run fails
- **后果**：We are the only one of the three that destroys a completed response over an unrecognized terminal word. Text already streamed to the person is followed by a failure; accumulated tool calls at :468-500 are never flushed. opencode finishes the turn; openclaw finishes it as an error but keeps the content.
- **同不同意**：同意 —— The finish_reason vocabulary is open-ended across OpenAI-compatible vendors — that is precisely why both references have a fallback arm instead of an error. Failing closed here converts a cosmetic unknown into total data loss.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:428-435 — replace the error return with a fallback terminal reason (flush tool calls, complete the turn) and record the raw value, rather than failing the run.

### finish_reason 'stop' promoted to tool-calls when tool calls were accumulated

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:465 (`state.finishReason === "stop" && hasToolCalls ? "tool-calls"`); openclaw .../openai-completions-transport.ts:818-833 (promotes on sawStopFinishReason, or on native tool_call deltas plus a clean [DONE]) and at :834-836 *discards* tool-call blocks if the turn did not end as toolUse
- **我们**：nothing reads it — openai_compatible.rs:468-500 flushes accumulated tool calls unconditionally, and :192-201 reports whatever reason arrived
- **后果**：When a provider emits tool_calls deltas but terminates with finish_reason 'stop' (openclaw names Evolink DeepSeek V4 for exactly this), we emit ToolCall events and then Completed{Stop}. The kernel sees a Stop and marks the run succeeded (runtime/crates/kernel/src/lib.rs:~525) while tool calls sit unexecuted. The agent silently stops mid-task instead of running its tools.
- **同不同意**：同意 —— Both references independently implement this promotion, and openclaw adds the inverse guard (drop tool blocks when the turn is not a tool turn) so the two states can never disagree. We have neither half.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:192-201 — before emitting Completed, promote Stop to ToolCalls when the tool_calls map is non-empty.
- **已关闭（2026-08-19）**：照做了，两条终局路径（收到 `[DONE]` 和干净 EOF）共用一个
  `tool_turn` 函数。取的是 opencode 那种无条件形式，不是 openclaw 的「没有可见文本 +
  干净终止才提升、否则丢掉调用」——两种都自洽，而**我们今天做的是两家都不做的那种**：
  把调用发出去然后弃掉它。丢掉一个模型明确要的调用是静默丢失它的意图，执行它才是它要的。
  两个方向各有守卫（不提升红 1 条，无条件提升红 3 条）。
  **顺带**：这条同时是我上一轮「悬空工具调用进不了冻结转录」那个结论的**反例**——
  那条测试驱动的两条路径确实进不去，而这是第三条：Run 被标成成功、调用没有结果、
  转录里就留下一个悬空调用。

### usage.prompt_tokens_details.cached_tokens

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:121-125 (schema), :393 and :398-399 (cacheReadInputTokens, plus a derived nonCachedInputTokens via subtractTokens); openclaw .../openai-completions-transport.ts:1942 and providers/openai-completions.ts:1079-1090 (subtracted out of `input` so buckets stay disjoint); codex reads the Responses equivalent at codex-api/src/sse/responses.rs:136,150
- **我们**：nothing reads it
- **后果**：We price the entire prompt at the full input rate. calculate_cost (openai_compatible.rs:512-521) multiplies prompt_tokens by input_million_tokens_micros with no cache discount, and cached tokens are typically billed at ~10% (OpenAI) or ~10-25% (DeepSeek). On a long-context agent loop where most of the prompt is a cache hit every turn, our reported cost_micros can be several times the real charge — and that inflated number is what drives budget exhaustion in the worker (runtime/apps/worker/src/lib.rs:6236-6246), so runs get killed for spending money they did not spend.
- **同不同意**：同意 —— All three references read it, and both TypeScript ones restructure their whole usage model around keeping cached tokens in a separate bucket. This is the single highest-value usage field we are missing, and it has a direct behavioural consequence, not just a reporting one.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:440-456 — read prompt_tokens_details.cached_tokens, subtract it from the billed input, and extend ProviderPricing/calculate_cost (:512-521) with a cached-input rate.

### [DONE] sentinel — is it required to terminate?

- **面**：流式分片全字段
- **他们**：opencode protocols/shared.ts:242-248 — [DONE] is filtered out as a keep-alive at :247 and never required; the stream terminates on the framed body ending and onHalt (openai-chat.ts:462-470), with the terminal condition being finish_reason (test at openai-chat.test.ts:594-616 'Provider stream ended without a terminal finish event'). openclaw does not require it for the normal path either: the SDK consumes it, and openclaw observes it out-of-band (openai-completions-transport.ts:114,117-160,830) purely as corroboration for the silent-tool-call promotion.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:175-182 — EOF without [DONE] is a hard Protocol error
- **后果**：We are the only one of the three that treats a missing [DONE] as fatal. Against any endpoint that closes cleanly after the final finish_reason chunk without emitting the sentinel — common among self-hosted vLLM builds, some Azure api-versions, and several gateways — every single run fails after the complete answer has already been streamed. The person sees the full text and then a protocol error.
- **同不同意**：同意 —— Neither reference makes [DONE] load-bearing, and both make finish_reason the terminal signal instead. [DONE] is a convention, not a guarantee; finish_reason is the thing the protocol actually defines.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:175-182 — on EOF, if a finish_reason was already seen, flush tool calls and complete normally; reserve the error for EOF with no finish_reason.
- **已关闭（2026-08-19）**：照上面这条改了。EOF 带着 finish_reason 就是完整的一轮，
  刷掉缓存的工具调用并正常终局；没有 finish_reason 才报错，措辞也换成了说事实的那句
  「provider stream ended before the model said the turn was over」。两个方向各有守卫
  （`a_clean_end_after_finish_reason_is_a_finished_turn` /
  `a_stream_cut_off_before_it_finished_is_still_a_failure`），各自破坏一次确认独立。
  **收到 `[DONE]` 但没有 finish_reason 那条仍然报错**，那是另一件事，没有动。

### top-level `error` object arriving mid-stream (HTTP 200, then data: {"error": {...}})

- **面**：流式分片全字段
- **他们**：codex-api/src/sse/responses.rs:387-421 is the closest full treatment — reads response.error and classifies code/message into ContextWindowExceeded, QuotaExceeded, UsageNotIncluded, CyberPolicy, InvalidRequest, ServerOverloaded, or Retryable{message, delay} (with try_parse_retry_after). openclaw inherits the openai-node SDK behaviour, which throws an APIError on a chunk carrying `error`. opencode's OpenAIChatEvent (openai-chat.ts:156-159) has no error member, so such a frame is a decode failure — loud, but unclassified.
- **我们**：nothing reads it — openai_compatible.rs:384 finds no `choices`, :439 finds no `usage`, and the chunk is silently discarded
- **后果**：The worst failure mode in the sweep. A provider that accepts the request (200), streams part of an answer, then hits a rate limit or context overflow and reports it in-band, produces: partial text emitted, error frame swallowed, then either 'provider stream ended without [DONE]' or '[DONE] without finish_reason'. The person gets a truncated answer plus a misleading protocol error. Worse, the real error is classified Protocol/non-retryable (:176-181, :192-199), so failover.rs never retries what was a textbook retryable rate limit — and the provider's own retry_after is discarded. Our classify_http_error (:523-556) does all this work correctly, but only for errors that arrive before the 200.
- **同不同意**：同意 —— codex treats the in-band error as the authoritative terminal event and maps it onto the same retryability and context-overflow taxonomy we already have in ModelErrorKind. We have the whole classification machinery at :523-556 and :603-608 and simply never reach it for mid-stream errors.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:212 — before consume_chunk, check for a top-level `error` object; classify code/message through the existing looks_like_context_overflow / ModelErrorKind mapping, parse any retry_after, and return a retryable ProviderExecutionError::Provider.

### stream ends after a valid finish_reason but before any terminal sentinel (clean EOF)

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:462-470 (onHalt/finishEvents — flushes accumulated tool-call events and finishes on the recorded reason); codex-api/src/sse/responses.rs:515-521 (EOF surfaces the stored response_error, or 'stream closed before response.completed')
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:175-182 — always a hard Protocol error, even when finish_reason was already recorded
- **后果**：Same blocking failure as the [DONE] row, stated from the other direction: we hold a valid terminal reason and a complete set of tool calls in memory at :161-162 and discard all of it because the sentinel never arrived. Both references drain what they have before failing.
- **同不同意**：同意 —— opencode's onHalt is exactly the seam we lack: a place to flush accumulated state when the transport ends rather than the protocol. Fixing this and the [DONE] row is a single change.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:175-182 — on EOF with a recorded finish_reason, run the same flush_tool_calls + Completed path as the [DONE] branch at :200-202.

### HTTP 429 — rate limit vs quota exhaustion

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:629-631 `is_quota_exceeded_error` (`code == "insufficient_quota"`) → ApiError::QuotaExceeded → CodexErr::QuotaExceeded → `is_retryable() == false` (protocol/src/error.rs:365-366); and codex-rs/codex-api/src/api_bridge.rs:94-124 — 429 with body `type == "usage_limit_reached"` becomes UsageLimitReached carrying plan_type + resets_at, also non-retryable. openclaw src/llm/utils/rate-limit-window.ts:10-11,68-70 — `insufficient_quota|quota exceeded|daily|weekly|monthly|usage limit|subscription` → window "long" → NOT eligible for same-model retry (assistant-failover.ts:196-203), goes straight to model fallback.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:565 — every 429 is `(RateLimited, true)`
- **后果**：OpenAI signals a spent account as HTTP 429 with `code: "insufficient_quota"`. We call it a retryable rate limit: the runtime-host burns its same-provider retries (lib.rs:4418-4447), opens a cooldown, walks the whole fallback chain, and the person is told "调得太快，被限流了" ("you're calling too fast, you got throttled") for an account that is out of money. Both references separate these and both make quota terminal.
- **同不同意**：同意 —— Retrying a spent quota can never succeed; it converts an instant, actionable answer into minutes of pointless backoff and the wrong sentence on screen. `ModelErrorKind::Billing` already exists and is exactly right for it.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:565 — inspect the body for `insufficient_quota` / quota wording before returning RateLimited; route to `ModelErrorKind::Billing, false`.

### Mid-stream error object in an SSE data line (`{"error": {...}}`)

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:387-419 — `response.failed` is unpacked by code: context_length_exceeded → fatal ContextWindowExceeded, insufficient_quota → fatal QuotaExceeded, usage_not_included → fatal, cyber_policy → fatal, invalid_prompt/bio_policy → InvalidRequest, server_is_overloaded/slow_down → ServerOverloaded, everything else → Retryable{message, delay}. Our own anthropic adapter does the equivalent (runtime/apps/model-gateway/src/anthropic_messages.rs:244-262).
- **我们**：nothing reads it — runtime/apps/model-gateway/src/openai_compatible.rs:377-461 `consume_chunk` touches only `choices[]` and `usage`; a chunk whose top level is `{"error":...}` parses as valid JSON, matches neither, and is silently dropped
- **后果**：vLLM, SGLang, Together, Groq and most OpenAI-compatible proxies report mid-stream faults exactly this way. We swallow the error object, the server then closes, and we report "provider stream ended without [DONE]" as `Protocol, false` (line 176-182) — the provider's actual sentence is discarded and the failure is made non-retryable. Our own Anthropic adapter is strictly better than our OpenAI-compatible one here.
- **同不同意**：同意 —— It is the primary error channel for the exact provider family this adapter targets, we already have the pattern in-tree, and the current behaviour both loses the message and mislabels the kind.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:377-384 — check `chunk["error"]` at the top of `consume_chunk` and classify as anthropic_messages.rs:244-262 does.

### Stream closes after finish_reason but without `[DONE]`

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:583-589 — the moment `response.completed` is parsed, the event is forwarded and the loop RETURNS; the stream is never required to signal end-of-stream (test `emits_completed_without_stream_end`, responses.rs:959-975).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:175-182 — `None` from the stream is unconditionally `"provider stream ended without [DONE]"`, `Protocol, false`, even when `finish_reason` was already captured
- **后果**：Azure OpenAI, several proxies, and any server that terminates the chunked body after the final chunk close without `data: [DONE]`. We turn a complete, successful turn into a non-retryable protocol failure, AFTER we have already emitted TextDelta — which means `committed_events > 0`, so `can_fallback` (runtime/apps/runtime-host/src/lib.rs:3965-3974) refuses to move on. The whole answer is on screen and the Run says failed.
- **同不同意**：同意 —— `[DONE]` is a convention, not a guarantee, and we already hold everything we need to complete the turn. Codex's rule — completion is a fact about the payload, not about the socket — is the correct one.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:175-182 — when `finish_reason.is_some()`, flush tool calls and emit Completed instead of erroring.

### Connection reset / transport error mid-stream

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:511-514 — a stream-level `Err` becomes `ApiError::Stream(e)` → CodexErr::Stream → retryable → whole turn re-sent even though deltas were already emitted (codex-rs/core/src/session/turn.rs:1341-1405 rebuilds the prompt from history and loops). openclaw treats the same as a transport failure and rotates providers.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:183-190 — `EventStreamError` → `Protocol, false`, "invalid provider SSE stream"
- **后果**：Two faults, one bucket: a TCP reset (transient, retryable) and a malformed SSE frame (a real protocol bug) both come out of `eventsource_stream` as an `Err` and we call both `Protocol, false`. A reset is therefore terminal and not in `fallback_on`. Codex retries it; we end the Run mid-sentence.
- **同不同意**：同意 —— `EventStreamError` distinguishes `Transport` from `Utf8`/`Parser`; we discard that distinction at the exact point it matters. The transport half should be `Unavailable, true`.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:183-190 — match on the `EventStreamError` variant instead of formatting it.

### Context-length error — how the provider's message is recognised

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:625-627 — exactly one rule: `error.code == "context_length_exceeded"`, structured, no text matching (tests cover the message containing an embedded newline, responses.rs:1039-1048, precisely because text matching is brittle). openclaw src/agents/embedded-agent-helpers/context-overflow.ts:26-80 — ~20 substring rules plus 5 Chinese phrases plus per-provider patterns (Bedrock/Azure/Ollama/Mistral/Cohere via matchesProviderContextOverflow), guarded against TPM false positives.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:627-632 — three substrings: "context length", "context window", "too many tokens"
- **后果**：We match OpenAI's phrasing and little else. Anthropic's "prompt is too long", Groq's `request_too_large`, Gemini's "input token count exceeds", and every Chinese-proxy wording fall through to `Protocol, false`. The downstream compaction machinery keys on ContextOverflow, so a missed match is a Run that dies instead of compacting. We also never read the structured `code` field that codex relies on exclusively.
- **同不同意**：同意 —— Both references agree the recognition must be broader than ours; they disagree only on method. Reading `error.code` (codex's way) is cheap, exact, and we already parse the body — we should do that first and keep substrings as the fallback.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:557-571 — parse the error body as JSON and test `error.code == "context_length_exceeded"` before falling back to runtime/apps/model-gateway/src/openai_compatible.rs:627-632.

### Content filter / safety refusal

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:399-407 — `cyber_policy` gets its own fatal ApiError with a human sentence and a fallback message when the provider sends an empty one (responses.rs:646-655); `invalid_prompt`/`bio_policy` → InvalidRequest with the provider's own text. Both are non-retryable (protocol/src/error.rs:359-381).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:451 — `finish_reason: "content_filter"` → `ModelFinishReason::ContentFilter` → runtime/crates/kernel/src/lib.rs:561-570 emits run.failed with `kind: ModelErrorKind::Protocol`
- **后果**：A person whose prompt was refused on safety grounds is told, by desktop/shell/src/surfaces/model.ts PROVIDER_FAILURE.protocol, "回复的格式不对" — "the reply's format is wrong". That is simply false, and it is the one failure where the person is the only one who can act. `reason: content_filter` IS in the payload at kernel/src/lib.rs:569 and the desktop's `failureReason` never looks at it (surfaces/model.ts, switch on `kind` only).
- **同不同意**：同意 —— Mislabelling a safety refusal as a wire bug is worse than saying nothing; the correct information is already in the event and simply unread.
- **改哪里**：runtime/crates/kernel/src/lib.rs:561-570 — content_filter deserves a kind of its own rather than Protocol; or desktop/shell/src/surfaces/model.ts `failureReason` must read `payload.reason`.

### `delta.refusal` (OpenAI streamed refusal text)

- **面**：错误与重试分类
- **他们**：our own runtime/apps/model-gateway/src/openai_responses.rs:175-178 reads `response.refusal.done` and emits `ModelStreamEvent::Refusal`; codex carries refusals as ordinary ResponseItems.
- **我们**：nothing reads it — runtime/apps/model-gateway/src/openai_compatible.rs:386-425 reads `delta.reasoning`, `delta.reasoning_content`, `delta.content`, `delta.tool_calls` and nothing else
- **后果**：On a chat-completions refusal the model streams `delta.refusal` and empty `delta.content`, finishing with `stop`. We emit zero text, zero refusal, and a successful Run. The person sees an empty answer and a green status — the worst available outcome, because nothing indicates anything went wrong. `ModelStreamEvent::Refusal` and `ContentPart::Refusal` both already exist (runtime/crates/protocol/src/lib.rs) and the request path even serialises refusals back (openai_compatible.rs:342-344).
- **同不同意**：同意 —— A silent empty success is unfalsifiable from the client's side, and the variant to fix it is already in the protocol and already used by our sibling adapter.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:410 — read `delta["refusal"]` beside `delta["content"]` and emit `ModelStreamEvent::Refusal`.

### Unknown / vendor `finish_reason` value

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:453-455 — unrecognised event kinds hit `_ => trace!("unhandled responses event")` and are ignored. openclaw src/agents/embedded-agent-helpers/errors.ts:953-957 — a provider-completed `finish_reason: error` is classified `server_error` (failover runs) rather than treated as a client bug.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:452-459 — any value outside stop/tool_calls/function_call/length/content_filter is a hard `Protocol, false`
- **后果**：`eos`, `end_turn`, `stop_sequence`, `error`, `max_tokens` are all live in the OpenAI-compatible ecosystem (llama.cpp, vLLM, DeepSeek, Mistral proxies). Each one ends the Run non-retryably, after text has already been committed — so no fallback either. This is strictness pointed at the wrong party: the vendor's spelling costs the person their turn.
- **同不同意**：同意 —— An unknown terminal reason still means the turn terminated; treating an unrecognised synonym for "stop" as a protocol violation is the most likely way this adapter fails against a real self-hosted server.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:452-459 — map unknown values to `Stop` (with a warn), or at minimum make them retryable.

### Provider's error message reaching the person (runtime-host path)

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/api_bridge.rs:33-45,131-144 — the body is carried into `UnexpectedResponseError { body, user_message, url, cf_ray, request_id }` and shown; openclaw src/agents/failover-error.ts:44 `rawError` carries the provider's own text through every wrapper for display and for api_health_log attribution.
- **我们**：runtime/apps/runtime-host/src/lib.rs:3961 — `message_digest: hex::encode(Sha256::digest(message))`; the terminal event at lib.rs:4498-4506 says only `"Provider {id} failed; diagnostic digest {sha256}"`
- **后果**：In the standalone runtime-host path the provider's sentence is destroyed at the boundary and replaced with a hash. Whatever the provider actually said — "You exceeded your current quota", "model `gpt-4o-mini-2024` does not exist", "prompt is too long: 210000 tokens > 200000" — is unrecoverable from the log. The desktop's `failureReason` (desktop/shell/src/surfaces/model.ts) has an explicit branch to print `payload.message` for kinds it does not recognise, and that branch can never fire on this path because the message is a hash. The gateway/gRPC path does forward the real message (runtime/apps/model-gateway/src/grpc.rs:363-375), so the two deployments tell the person different things about the same failure.
- **同不同意**：同意 —— The digest was presumably chosen to avoid leaking credentials, but `ProviderCredential::redact` (openai_compatible.rs:48-50) already handles that and the body is already truncated to 2048 chars. Hashing removes the only text that can explain a kind we did not anticipate.
- **改哪里**：runtime/apps/runtime-host/src/lib.rs:3961 — keep the redacted message beside the digest; runtime/apps/runtime-host/src/lib.rs:4501 — put it in the Failed event.

### `status` and `retry_after_ms` across the gRPC boundary

- **面**：错误与重试分类
- **他们**：codex-rs/protocol/src/error.rs:72,399-405 — `retry_delay` is a field on CodexErr itself and survives every mapping hop; codex-rs/core/src/responses_retry.rs:51 consumes it (`err.retry_delay().unwrap_or_else(|| backoff(n))`). openclaw threads `status` through FailoverError (src/agents/failover-error.ts:42).
- **我们**：runtime/apps/model-gateway/src/grpc.rs:360-380 `encode_provider_failure` destructures `{kind, retryable, message, ..}` — `status` and `retry_after_ms` are dropped; contracts/proto/model_gateway.proto:163-167 `Failed` has only kind/retryable/message
- **后果**：The Retry-After we carefully parse at openai_compatible.rs:552-556 is usable only in the in-process runtime-host path. In the deployed worker→gateway topology it is discarded on the wire, so the worker's retry (apps/worker/src/execution_supervisor.rs:143-158) can only reconstruct a coarse kind from a gRPC code and has no delay hint and no HTTP status at all. Half our classification is unreachable in the deployment that matters.
- **同不同意**：同意 —— We do the expensive part (parsing the header) and then throw it away one hop later. Both references keep the delay attached to the error through every mapping.
- **改哪里**：contracts/proto/model_gateway.proto:163-167 — add `optional uint32 status` and `optional uint64 retry_after_ms` to `Failed`; then runtime/apps/model-gateway/src/grpc.rs:360-380.

### Console (web) surfacing of any provider failure

- **面**：错误与重试分类
- **他们**：codex surfaces `StreamErrorEvent { message, codex_error_info, additional_details }` into the TUI (codex-rs/core/src/session/mod.rs:3934-3950); openclaw formats a user-facing sentence per reason (src/agents/embedded-agent-helpers/errors.ts:1290 formatAssistantErrorText, and the context-overflow copy at overflow-context-recovery.ts:274-276: "Try /reset (or /new) ... or use a larger-context model").
- **我们**：console/src/composables/useRuns.ts:38-48 polls `/v1/runs` for a status only; console/src/components/RunStatusBadge.vue:13,16 maps every failure to 失败 / 已超时. Nothing in console/src subscribes to `events_url` — the field exists (console/src/types/runtime.ts:46, api/runApi.ts:81) and is never used.
- **后果**：In the web console today, EVERY failure in this entire sweep — expired key, spent quota, context overflow, content filter, truncated stream, unknown finish_reason — renders identically as a red 失败 badge. No kind, no message, no retry, no remediation. The desktop shell is far ahead (desktop/shell/src/surfaces/model.ts has a per-kind sentence for all eight ModelErrorKind, plus retry and failover lines); the console has none of it.
- **同不同意**：同意 —— Every classification improvement above is invisible in the console until it reads the event stream. This is the single highest-leverage fix in the sweep: the data is already correct and already durable.
- **改哪里**：console/src/composables/useRuns.ts:38-48 — consume `events_url`; the per-kind vocabulary to reuse is desktop/shell/src/surfaces/model.ts PROVIDER_FAILURE and `failureReason`.

### reasoning_effort

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1927 (params.reasoning_effort = resolved effort, gated on compat.supportsReasoningEffort) and :1911 (forces "none" for GPT-5.6 + tools); opencode openai-chat.ts:99,335-341
- **我们**：not sent — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253 never reads request.reasoning, although ModelRequest carries it (crates/protocol/src/lib.rs:2976) and our Responses adapter does map it (openai_responses.rs:317-328)
- **后果**：The caller's ReasoningPolicy (Minimal/Balanced/Thorough) is silently discarded on the chat-completions path: a Minimal request pays for and waits on the server default effort, a Thorough request gets no extra thinking. The policy is a lie for every chat-completions provider. On GPT-5.6 chat-completions, openclaw found the server rejects function tools unless reasoning_effort is explicitly set — there we would fail outright.
- **同不同意**：同意 —— We already model the concept and already map it on the Responses adapter; dropping it on the sibling adapter makes the same ModelRequest mean two different things depending on which provider the failover picks.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253 — map ReasoningPolicy to reasoning_effort low/medium/high the way openai_responses.rs:317-320 already does.
- **复核更正**：Consequence is latent, not current. Nothing in the platform can emit a non-Balanced policy: the sole producer is runtime/apps/worker/src/lib.rs:6783 (`reasoning: ReasoningPolicy::Balanced as i32`, hardcoded). No API/config/DB field feeds it — contracts/openapi/openapi.yaml has no reasoning field, and ReasoningPolicy otherwise appears only in contracts/proto/model_gateway.proto:21,105, protocol/src

### delta.tool_calls[].function.arguments — empty string / absent for a no-arg tool

- **面**：工具调用组装边界
- **他们**：opencode /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/shared.ts:155-156 `parseToolInput = (route, name, raw) => parseJson(route, raw || "{}", ...)`; openclaw /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/utils/json-parse.ts:129-132 `if (!partialJson || partialJson.trim() === "") return {}`; openclaw /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:783 `if (toolCall.function?.arguments)` skips empty and leaves `arguments: {}` from line 756; codex /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/core/src/mcp_tool_call.rs:120-124 `// An empty string is OK` → `None`
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:481-488 — `let arguments = serde_json::from_str(&tool_call.arguments).map_err(|error| provider_error(ModelErrorKind::Protocol, false, None, format!("streamed tool arguments are invalid JSON: {error}")))?;` with no empty-string branch. `PartialToolCall::arguments` starts as `String::default()` (line 461-466) and stays `""` when the provider never sends an arguments fragment.
- **后果**：A tool with no parameters — the single most common shape for `list_*`, `get_status`, `finish` style tools — kills the run. `serde_json::from_str::<Value>("")` fails with "EOF while parsing a value", so `flush_tool_calls` returns a non-retryable `ModelErrorKind::Protocol` error at line 481, the `[DONE]` branch (line 200) never reaches `emit(Completed)`, and the whole attempt fails after the model already did the work. Note our own Anthropic adapter disagrees with us: /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/anthropic_messages.rs:676-678 does `let input = if tool.input_json.is_empty() { Value::Object(Map::new()) } else { ... }`. Same platform, same edge case, two answers.
- **同不同意**：同意 —— All three references treat empty arguments as `{}`, and our own Anthropic path already does. There is no defensible reading in which "the model called a zero-argument tool" is a protocol violation. This is the highest-value single-line fix in the file.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:481 — mirror anthropic_messages.rs:676: `let arguments = if tool_call.arguments.trim().is_empty() { Value::Object(Map::new()) } else { serde_json::from_str(&tool_call.arguments)... }`.
- **复核更正**：Two corrections, neither of which weakens the gap.

1. Line numbers have drifted about 24 lines; the quoted code is verbatim correct but sits lower in the file. The `serde_json::from_str` call is at `openai_compatible.rs:505-512`, not 481-488. `PartialToolCall` is at 485-490, not 461-466. The `[DONE]` branch calling `flush_tool_calls` is at line 200 as stated. The Anthropic comparison at `anthropi

### assembled function.arguments is not valid JSON (truncated or malformed)

- **面**：工具调用组装边界
- **他们**：openclaw /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/utils/json-parse.ts:129-147 — four-tier fallback: strict parse of repaired text, then `partial-json` parse, then partial parse of repaired text, then `{}`; never throws. Applied per-delta at /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:791 `block.arguments = parseStreamingJson(block.partialArgs)`. opencode /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/shared.ts:97-101 `parseJson` → `eventError` → LLMError fails the stream; deliberately hoisted to the finish boundary per the comment at /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:443-444 ("so JSON parse failures fail the stream at the boundary rather than at halt"). codex never assembles, but the equivalent failure — arguments that will not deserialize — goes back to the model: /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/core/src/tools/handlers/mod.rs:83-90 `FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))`, converted to a transcript item at /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/core/src/stream_events_utils.rs:362-382 with `output.needs_follow_up = true`.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:481-488 — `serde_json::from_str(&tool_call.arguments).map_err(|error| provider_error(ModelErrorKind::Protocol, false, None, format!("streamed tool arguments are invalid JSON: {error}")))?`
- **后果**：Three references, three different dispositions, and ours is a fourth: we fail the turn non-retryably with no repair and no feedback loop. openclaw repairs and proceeds; opencode fails the stream but at the finish boundary; codex hands the parse error back to the model so it can retry the call itself. Ours is the only one where a single malformed brace from the model — a thing models do — costs the entire run with no path to recovery. Note `retryable: false` (line 483), so the platform's retry machinery will not even re-roll the turn.
- **同不同意**：同意 —— Not because erroring is wrong — opencode errors too — but because `retryable: false` plus no repair plus no respond-to-model leaves zero recovery paths. Malformed tool arguments are model output, not provider misbehaviour, and every reference gives the model or the parser a second chance. At minimum this should be retryable; codex's RespondToModel shape is the better target because it costs one cheap turn instead of a full re-roll.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:481-488 — either set `retryable: true`, or add a repair pass, or (best) introduce a path that emits the parse failure as a tool result back into the transcript the way codex/stream_events_utils.rs:362 does.

### server emits delta.tool_calls but never sets finish_reason=tool_calls (sends "stop" instead)

- **面**：工具调用组装边界
- **他们**：openclaw handles this explicitly and by name. Transport /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:818-833: `if (output.stopReason === "stop" && hasToolCalls && !hasVisibleText && (sawStopFinishReason || (sawNativeToolCallDelta && (options?.sawStreamDONE?.() ?? false)))) { output.stopReason = "toolUse"; }` — the comment names the provider ("e.g. Evolink DeepSeek V4") and the rule: a clean `[DONE]` terminal proves the stream ended intentionally, so promote; EOF without `[DONE]` stays fail-closed. `sawStreamDONE` is a byte-level SSE line scanner at lines 118-163 that refuses to be fooled by a truncated long line (`lineOverflowed`). Provider path does the same without the DONE gate: /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-completions.ts:563-565. openclaw also runs the converse guard first — line 815/560, `stopReason === "toolUse" && !hasToolCalls → "stop"` — and hard-drops tool calls that failed to be promoted at lines 834-836 / 566-568. opencode promotes too, at /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:465: `const reason = state.finishReason === "stop" && hasToolCalls ? "tool-calls" : state.finishReason`. openclaw additionally maps the singular `"tool_call"` spelling when opted in (/Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-stop-reason.ts:25-29, enabled at transport line 662 `allowSingularToolCall: true`).
- **我们**：Nothing reads it. /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:422-437 maps only the literal strings `"stop" | "tool_calls" | "function_call" | "length" | "content_filter"`, and `flush_tool_calls` at line 200 runs unconditionally before `emit(Completed { reason })` at line 201 — so we emit the ToolCall events *and then* `Completed { reason: Stop }`, with no reconciliation between the two.
- **后果**：This is the worst outcome in the sweep because it is silent. The tool calls reach the worker and land in `pending_tool_calls` (runtime/apps/worker/src/lib.rs:6247-6255) — that arm does not inspect finish_reason. Then `Completed { reason: Stop }` reaches the kernel at /Users/cola/Documents/Code/agent-runtime-platform/runtime/crates/kernel/src/lib.rs:536-542, which transitions the Run to `RunStatus::Succeeded` and emits `run.succeeded`. Succeeded is terminal (kernel lib.rs:455-457). The `Completed{ToolCalls}` arm at worker lib.rs:6256-6285 is the *only* place tool calls are written into the assistant transcript message, so on the Stop path they are never recorded either. Net: the durable log shows `model.tool_call` events for tools that will never run, the transcript has no record of them, and the Run reports success. A user sees an agent that announced it was calling a tool and then declared victory. Note we do implement the converse guard — worker lib.rs:6176-6184 raises `EmptyToolTurn` when `Completed{ToolCalls}` arrives with nothing pending — so half the reconciliation exists; this is the missing half. We also do not accept the singular `"tool_call"` spelling that openclaw opts into.
- **同不同意**：同意 —— Both TypeScript references implement this promotion, one of them with a named provider in the comment, and openclaw went to the trouble of a byte-level `[DONE]` detector specifically so the promotion could be safe rather than guessy. We already own the same signal — our `[DONE]` branch at openai_compatible.rs:191 is the exact point openclaw's `sawStreamDONE` is proving — so the safe version of the promotion is nearly free for us and lands in code we already reach.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:191-203 — inside the `[DONE]` branch, before line 200, promote `ModelFinishReason::Stop` to `ToolCalls` when `!tool_calls.is_empty()`. Add `"tool_call"` to the match arm at line 425. The converse (ToolCalls with an empty map) is already covered downstream at runtime/apps/worker/src/lib.rs:6176-6184.

### choice.message instead of choice.delta (non-streaming-shaped chunk)

- **面**：工具调用组装边界
- **他们**：Both openclaw paths normalize it, with a comment naming the cause. Transport /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:672-674: `const choiceDelta = choice.delta ?? (choice as ... { message?: ... }).message;`. Provider /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-completions.ts:433-438 with the comment "Some OpenAI-compatible endpoints deliver a full `message` instead of `delta` (including refusal-only turns with content: null)". opencode reads `delta` only (/Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:413).
- **我们**：Nothing reads it. /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:385 — `let delta = &choice["delta"];` and every subsequent access indexes off it (lines 386, 402).
- **后果**：Against a server that answers a `stream: true` request with message-shaped chunks, `delta["content"]` and `delta["tool_calls"]` are both `Value::Null`, so `.as_str()` and `.as_array()` yield `None` and both loops are skipped. We still read `choice["finish_reason"]` at line 422, so the turn completes normally with `Completed { reason: Stop }` and *zero* content events. Silent, total data loss presenting as a successful empty turn — the one failure mode in this file with no error attached to it. openclaw considered this shape common enough to handle in two independent code paths.
- **同不同意**：同意 —— A silent empty success is worse than any of the loud failures above, and the fix is one fallback expression that openclaw ships twice. This is also the failure most likely to be misdiagnosed as a model problem rather than an adapter problem.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:385 — `let delta = if choice["delta"].is_null() { &choice["message"] } else { &choice["delta"] };`, matching openai-completions-transport.ts:672.

### disposition when assembly fails — drop, error the turn, or send back to the model

- **面**：工具调用组装边界
- **他们**：All three references, asked directly. codex: silently drops a malformed item (`debug!` at /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/codex-api/src/sse/responses.rs:331-337), skips a malformed frame (responses.rs:533-537), retries a truncated stream (protocol/src/error.rs:383, test core/tests/suite/stream_no_completed.rs:23), and sends every *semantic* failure back to the model — bad arguments (core/src/tools/handlers/mod.rs:83-90), unknown tool (core/src/tools/registry.rs:480-496), bad MCP arguments (core/src/mcp_tool_call.rs:120-135, `CallToolResult::from_error_text`) — all funnelling into `FunctionCallOutput` with `needs_follow_up = true` at core/src/stream_events_utils.rs:362-382. Only `FunctionCallError::Fatal` (tools/src/function_call_error.rs:8-9) kills the turn, reserved for internal invariant breaks like registry.rs:524. openclaw: repairs everything it can — invalid JSON to `{}` (json-parse.ts:129-147), missing/duplicate ids minted (attempt.tool-call-normalization.ts:668-675), blank names inferred from the id (lines 697-700) or rewritten into an assistant text message the model reads (lines 785-795, wired line 1016) — and hard-fails only on buffer caps (openai-completions-transport.ts:787). opencode: fails the stream on every assembly failure (tool-stream.ts:127, shared.ts:97-101), the strictest of the three.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs — every failure path in the file is `provider_error(ModelErrorKind::Protocol, false, None, ...)`: lines 176-181, 183-190, 192-199, 204-211, 403-410, 428-435, 473-480, 481-488. `retryable: false` in all eight. There is no drop path, no repair path, and no channel to hand a failure back to the model.
- **后果**：We have exactly one disposition where the references have three, and it is the most expensive one, applied uniformly to failures of very different character — a dropped TCP connection (line 176), a keep-alive frame we did not recognize (line 204), and a model that wrote a bad brace (line 481) all produce the identical non-retryable protocol error. That uniformity is what makes the individual entries above hard to fix in isolation: there is nowhere cheaper for a failure to go. codex's `RespondToModel`/`Fatal` split is the structural piece we lack — it lets a semantic failure cost one cheap turn instead of a whole run, and it is the reason codex can afford to be strict about stream termination while staying forgiving about content.
- **同不同意**：同意 —— The per-edge fixes above (empty arguments, message-vs-delta, stop-promotion) are each small, but they keep landing on the same missing structure: this adapter can only say "the turn is over and it failed". A three-way split — retryable transport failure, model-visible tool failure, genuine protocol violation — is what the references have in common despite disagreeing on where each case lands, and adopting it makes every other entry in this sweep a one-line decision rather than a judgement call.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:579-592 (`provider_error`) and its eight call sites — introduce the retryable/model-visible distinction; the transcript-injection half already exists downstream in the ToolResult path at openai_compatible.rs:281-295, so the wire format for feeding a failure back to the model is present, only unused.

## degraded（60 条）

### delta.reasoning_details[] (OpenRouter extension: array of {type, text}; type 'reasoning.text' is thinking, configured visible types 'response.output_text'/'response.text' are answer text)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:1125-1143, visible-type set configured at openai-completions-compat.ts:153
- **我们**：nothing reads it
- **后果**：On OpenRouter, reasoning_details takes precedence over the flat reasoning fields (openclaw sets usedReasoningThinkingDetails and skips the chain). Worse than losing thinking: for models where OpenRouter puts the *visible answer* in reasoning_details with type 'response.output_text', we would drop the answer itself, not just the thinking.
- **同不同意**：同意 —— This is a genuine provider extension with a real precedence rule, not decoration. Only openclaw handles it; opencode does not route OpenRouter through anything richer than the plain chat schema.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:386 — handle `delta["reasoning_details"]` as an array before falling back to the flat chain; map type=='reasoning.text' to ReasoningDelta and the configured visible types to TextDelta.

### delta.tool_calls[].index

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:139 (Schema.Number, required) used as the accumulator key at :432; openclaw .../openai-completions-transport.ts:740 (optional — `typeof toolCall.index === "number" ? ... : undefined`) with an id-keyed fallback at :742-743
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:403-411 — required; missing index is a hard Protocol error
- **后果**：We match opencode's strictness but not openclaw's tolerance. A provider that omits index and identifies calls only by id (openclaw carries an explicit fallback map, toolCallBlocksById) fails the whole run with 'streamed tool call is missing its index' instead of assembling the call.
- **同不同意**：同意 —— openclaw's dual-key lookup (index first, id second) costs one BTreeMap and covers a wire shape they clearly encountered. Failing the run on a recoverable omission is the wrong default for an adapter meant to speak to arbitrary OpenAI-compatible endpoints.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:403-411 — when index is absent, key the accumulator by fragment id instead of erroring.

### delta.tool_calls[].extra_content.google.thought_signature and delta.tool_calls[].function.thought_signature (Gemini via OpenAI-compat)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:1409-1424 (extractGoogleThoughtSignature), captured at :751-758 and :779-782, replayed back onto the assistant turn at :1475 and :1521
- **我们**：nothing reads it
- **后果**：Against Gemini's OpenAI-compatible endpoint the per-tool-call thought signature is dropped. Google requires it echoed back on the next turn; without it the model loses its own reasoning continuity across the tool loop, degrading multi-step tool use on exactly the models where it matters most.
- **同不同意**：同意 —— This is the OpenAI-compat analogue of the encrypted-reasoning replay our own protocol already models as ProviderPrivateState (openai_responses.rs:210 carries private_state). We built the concept and then did not wire it on this adapter.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:402-421 — read both locations into PartialToolCall and carry them as ProviderPrivateState on the emitted ToolCall / assistant replay.

### choices[].finish_reason == "tool_call" (singular)

- **面**：流式分片全字段
- **他们**：openclaw openai-stop-reason.ts:25-29, enabled by the caller at .../openai-completions-transport.ts:661-663 (allowSingularToolCall: true)
- **我们**：nothing reads it — openai_compatible.rs:428-435 falls into the catch-all and returns a hard Protocol error
- **后果**：A provider that spells the reason in the singular ends the run with 'unsupported provider finish_reason tool_call' after the model has already produced a complete, valid tool call. We throw away correct work and surface a protocol error to the person.
- **同不同意**：同意 —— openclaw gates it behind an explicit opt-in flag that the completions transport turns on, which means they met this wire value on a real endpoint. The singular form is unambiguous.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:425 — add "tool_call" to the ToolCalls arm.

### choices[].finish_reason == "end"

- **面**：流式分片全字段
- **他们**：openclaw openai-stop-reason.ts:17-19 (aliased to 'stop')
- **我们**：nothing reads it — hard Protocol error at openai_compatible.rs:428-435
- **后果**：Same shape as above: a normal, complete answer is discarded because the terminal word differs. Affects endpoints that borrow the 'end' spelling.
- **同不同意**：同意 —— A one-line alias in openclaw. There is no reading under which 'end' means anything but stop.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:424 — add "end" to the Stop arm.

### choices[].finish_reason == "network_error"

- **面**：流式分片全字段
- **他们**：openclaw openai-stop-reason.ts:32-33 (stopReason 'error', message 'Provider finish_reason: network_error')
- **我们**：nothing reads it — falls into the catch-all Protocol error at openai_compatible.rs:428-435
- **后果**：We do produce an error, so nothing is silently lost, but it is classified Protocol/non-retryable. A network_error from the provider is exactly the retryable case, and we mark it not-retryable — the failover path in failover.rs never gets a chance.
- **同不同意**：同意 —— The classification matters more than the message. openclaw distinguishes it from an unknown reason; we collapse both into non-retryable Protocol.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:428-435 — map "network_error" to a retryable ModelErrorKind::Unavailable rather than the non-retryable catch-all.

### choices[].message (non-streaming message object arriving on a stream)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:672-674 — `choice.delta ?? (choice as ...).message`
- **我们**：nothing reads it — openai_compatible.rs:385 reads `choice["delta"]` only
- **后果**：Endpoints that answer a stream:true request with the non-streaming choice shape yield an empty run: we find no `delta`, emit nothing, and either fail at [DONE] for want of a finish_reason (:192-199) or complete with an empty turn. openclaw pairs this with a JSON-to-SSE synthesizer (src/agents/provider-transport-fetch.ts:160-215, which wraps a plain JSON body as one data frame plus a synthetic [DONE]), so the two together handle providers that ignore stream:true entirely.
- **同不同意**：同意 —— A one-line fallback that turns a total failure into a correct single-shot response. openclaw building a whole synthesizer around it shows how common the shape is among self-hosted and gateway endpoints.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:385 — fall back to `choice["message"]` when `choice["delta"]` is not an object.

### choices[].usage (per-choice usage object)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:655-658 — used only when the top-level chunk.usage is absent
- **我们**：nothing reads it — openai_compatible.rs:439 reads only `chunk["usage"]`
- **后果**：On providers that nest usage under the choice, we emit no Usage event at all. The worker's budget accounting (runtime/apps/worker/src/lib.rs:6231-6245) then sees zero tokens and zero cost for the whole call, so budget limits do not bind and the run is billed as free.
- **同不同意**：同意 —— Silent zero-cost accounting is a worse failure than a wrong number, because nothing surfaces it. openclaw's guard is exactly the right shape: top-level first, per-choice as fallback.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:439 — when `chunk["usage"]` is absent, fall back to `choice["usage"]`.

### usage.total_tokens

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:120 (schema), :402 (ProviderShared.totalTokens(prompt, completion, total) — provider value preferred over the derived sum); codex's Responses analogue reads total_tokens at codex-api/src/sse/responses.rs:128,143
- **我们**：nothing reads it
- **后果**：We never carry a provider-authoritative total. The worker derives its own total by summing input+output (runtime/apps/worker/src/lib.rs:6236-6239), which diverges from the provider's number whenever the provider counts anything else into the total. Reconciling our budget ledger against a provider invoice will not tie out.
- **同不同意**：同意 —— opencode explicitly prefers the provider's total over the derived sum, and codex reads it too. When the provider states a total, it is the billing-authoritative number and a derived sum is a guess.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:439-457 — read total_tokens and carry it, preferring it over the derived sum.

### usage.prompt_tokens_details.cache_write_tokens

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:1934,1941 and providers/openai-completions.ts:1072,1080-1090 (kept as a separate cacheWrite bucket, deliberately NOT subtracted from cached_tokens — the comment cites OpenRouter's provider tests); codex reads the Responses equivalent at codex-api/src/sse/responses.rs:137,152
- **我们**：nothing reads it
- **后果**：On OpenRouter-family endpoints (notably Anthropic models proxied through OpenRouter) cache writes are priced above normal input, and we bill them as ordinary input — under-charging in our ledger, the opposite direction from the cached_tokens error, so the two do not cancel predictably.
- **同不同意**：同意 —— openclaw carries an explicit comment and two upstream links justifying the disjoint bucketing, which means they got this wrong once and fixed it. Cheap to adopt alongside cached_tokens.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:440-456 — read it into a third bucket alongside cached_tokens.

### usage.prompt_cache_hit_tokens (DeepSeek's spelling of cached_tokens)

- **面**：流式分片全字段
- **他们**：openclaw providers/openai-completions.ts:1071,1079-1080 — `rawUsage.prompt_tokens_details?.cached_tokens ?? rawUsage.prompt_cache_hit_tokens ?? 0`
- **我们**：nothing reads it
- **后果**：DeepSeek reports cache hits at the top level of usage under this name, not nested under prompt_tokens_details. Even after we add cached_tokens, DeepSeek cache discounts would still be missed entirely — and DeepSeek is one of the providers where cache pricing differs most sharply.
- **同不同意**：同意 —— A single `??` fallback in openclaw covers a whole provider family. It only matters once cached_tokens is read at all, so it should land in the same change.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:440-456 — fall back to usage.prompt_cache_hit_tokens when prompt_tokens_details.cached_tokens is absent.

### usage.completion_tokens_details.reasoning_tokens

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:126-130 (schema), :394,:400 (reasoningTokens on Usage); openclaw .../openai-completions-transport.ts:1946,1952-1954 and again at :1964-1970 (hasOpenAICompletionsReasoningUsageActivity — used to keep the idle watchdog alive during hidden reasoning); codex reads the Responses equivalent at codex-api/src/sse/responses.rs:139-142,157
- **我们**：nothing reads it
- **后果**：Two losses. First, reporting: we cannot tell a person how much of their spend went to invisible thinking. Second and sharper: openclaw uses a rising reasoning_tokens count as proof of liveness (:640-651, 'Hidden reasoning is still provider progress'). Our stream_idle_timeout (openai_compatible.rs:166-173) counts only SSE arrival, so a reasoning model that sends usage-only chunks while thinking is at least kept alive by those chunks — but we have no way to distinguish real reasoning progress from a stalled provider dribbling keepalives, and no way to report it.
- **同不同意**：同意 —— All three references read it. The liveness use is the non-obvious one and is worth stealing: it is the difference between timing out a thinking model and timing out a hung one.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:440-456 — read completion_tokens_details.reasoning_tokens; carry it on the Usage event and use a rising value as an explicit progress signal for the idle timeout at :166-173.

### usage.cost (OpenRouter provider-reported cost)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:1934,1959 and providers/openai-completions.ts:1074,1103 — applyProviderReportedUsageCost (model-utils.ts:23) overrides the locally computed cost with the provider's own figure
- **我们**：nothing reads it — openai_compatible.rs:453 always uses our own calculate_cost
- **后果**：On OpenRouter, the provider states the actual charge and we ignore it, substituting a price table (ProviderPricing at :16-19) that cannot know which upstream model OpenRouter actually routed to. Our cost_micros is a guess where an exact number was on the wire, and that guess drives budget enforcement.
- **同不同意**：同意 —— A provider-stated cost is authoritative over any local table by definition. openclaw computes locally first and then lets the reported cost win, which is the right precedence.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:448-456 — when usage.cost is present, prefer it over calculate_cost.

### usage repeated across multiple chunks (accumulate vs last-wins)

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:410 `const usage = mapUsage(event.usage) ?? state.usage` (last-wins, emitted once via finishEvents at :468); openclaw .../openai-completions-transport.ts:646 `output.usage = parseTransportChunkUsage(...)` (assignment, last-wins)
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:448-456 — we emit a fresh ModelStreamEvent::Usage for every chunk that carries a usage object
- **后果**：Both references overwrite; we emit. The worker sums (runtime/apps/worker/src/lib.rs:6236-6245, saturating_add into budget_usage.tokens and cost_micros) and the runtime-host sums too (runtime/apps/runtime-host/src/lib.rs:5161-5169). Any endpoint that reports cumulative usage on more than one chunk — vLLM's continuous_usage_stats, some LiteLLM and Groq configurations, and the common pattern of usage on both the last content chunk and the final empty chunk — multiplies our recorded token count and cost, and can falsely trip budget exhaustion mid-run.
- **同不同意**：同意 —— Two references independently chose last-wins-then-emit-once, and our downstream is an accumulator, so the mismatch is structural rather than incidental. Emitting once at the terminal is both safer and closer to what usage means.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:439-457 — hold the latest usage in a local instead of emitting, and emit a single Usage event at the [DONE] branch (:191-203) alongside flush_tool_calls.

### [DONE] matching tolerance (case, surrounding whitespace)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:114 — `/^data:[ \t]*\[DONE\][ \t]*$/i` (case-insensitive, tolerant of leading and trailing spaces/tabs), with an explicit anti-truncation guard at :133-142 so a suffix of an oversized data line cannot be mistaken for the terminal; opencode shared.ts:247 uses an exact `!== "[DONE]"` comparison like ours
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:191 — exact `event.data == "[DONE]"`
- **后果**：The SSE spec strips only a single leading space after the colon, so `data: [DONE] ` with a trailing space reaches us as "[DONE] " and misses. We then fall through to serde_json::from_str at :204-211 and fail with 'provider SSE data is not valid JSON' — a complete response destroyed by one trailing byte.
- **同不同意**：同意 —— openclaw's regex exists because they met sloppy emitters. Trimming before comparison costs nothing. This becomes much less severe once the [DONE]-not-required fix above lands, but the misleading JSON error is worth removing regardless.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:191 — compare `event.data.trim()` case-insensitively against "[DONE]".

### SSE `event:` field name (e.g. `event: error`)

- **面**：流式分片全字段
- **他们**：openclaw inherits the openai-node SDK's branch on sse.event, which raises an APIError when the event name is 'error'. codex reads the event *type* as the primary dispatch key, but from the JSON body's `type` field (codex-api/src/sse/responses.rs:161-163, :341+ match on event.kind), not the SSE event name. opencode ignores the SSE event name entirely (shared.ts:242-248 maps to event.data only).
- **我们**：nothing reads it — openai_compatible.rs:191-211 uses only `event.data`
- **后果**：A provider signalling failure via a named `event: error` frame is indistinguishable to us from any other chunk, and gets silently dropped by the same path as the top-level error object. Same downstream symptom: truncated answer, misleading protocol error, no retry.
- **同不同意**：同意 —— Largely subsumed by fixing the top-level `error` object read, since such frames carry an error body anyway. Worth handling in the same change rather than separately.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:191 — inspect `event.event` alongside the data payload when classifying error frames.

### malformed / non-JSON SSE data frame

- **面**：流式分片全字段
- **他们**：codex-api/src/sse/responses.rs:533-537 — logs and `continue`s, deliberately not fatal; any stored response_error is surfaced only at stream close (:515-521). opencode fails the stream (openai-chat.test.ts:619-625, 'Invalid openai/openai-chat stream event'). openclaw pre-filters structurally malformed blocks before its parser sees them (src/agents/provider-transport-fetch.ts:245-252 — drops event-only and blank-data blocks with the comment that the SDK would otherwise try to JSON.parse them).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:204-211 — hard Protocol error, non-retryable
- **后果**：We match opencode's strictness and diverge from codex's tolerance. One stray non-JSON frame — a vendor keepalive that is not a comment, a stray heartbeat token — destroys a run that was otherwise complete. codex survives it.
- **同不同意**：同意 —— codex's posture is the right one for an adapter that must speak to arbitrary endpoints: skip what you cannot parse, and fail only if the stream never reaches a terminal. Note the empty-data keepalive case is already safe for us — see the next row.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:204-211 — skip unparseable frames (with a counter/log) instead of returning, and rely on the terminal-reason check to fail the run if nothing valid ever arrives.

### inline reasoning tags inside delta.content (<think>, <thinking>, <thought>, <reasoning>, <antthinking>, antml:think/thinking/thought)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:389 (createReasoningTagTextPartitioner) driving :688-705 and :718-731; tag vocabulary at packages/markdown-core/src/reasoning-tag-parser.ts:10-18; the partitioner is markdown-aware (holds back partial tags across chunk boundaries and refuses to split inside code spans)
- **我们**：nothing reads it
- **后果**：Providers that emit thinking inline in the content stream rather than in a separate field — many self-hosted and distilled reasoning models do exactly this — have their raw <think>...</think> markup emitted to the person as answer text. The thinking is not merely unlabelled, it is rendered as the answer.
- **同不同意**：同意 —— Real and visible. But openclaw's implementation is substantial (incremental, markdown-ownership-aware, code-span-safe) precisely because a naive strip mangles legitimate content — a model discussing HTML, or a code block containing the literal tag. Worth adopting, but not worth a quick regex.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:386-401 — route content through a tag-aware partitioner before emitting TextDelta; port the boundary-holding behaviour from packages/markdown-core/src/reasoning-tag-parser.ts rather than pattern-matching naively.

### DeepSeek DSML tool markup inside delta.content (<|DSML|tool_calls>, <|DSML|tool_call>, <|DSML|function_calls>, <|DSML|tool_use_error>, both ASCII | and fullwidth ｜)

- **面**：流式分片全字段
- **他们**：openclaw transports/deepseek-text-filter.ts:1-30+ (strips the markup from visible text, buffering split tag prefixes across chunks) and .../openai-completions-transport.ts:869-900+ (createDeepSeekDsmlToolCallRecoverer — actually *recovers tool calls* out of the markup); gated by compat.thinkingFormat === 'deepseek' at :856-858; flushed at :806-807
- **我们**：nothing reads it
- **后果**：Two losses against DeepSeek-family endpoints that emit DSML rather than native tool_calls. The person sees raw <|DSML|tool_calls> markup in the answer, and the tool call it encodes is never executed — the agent appears to narrate a tool use it never performs.
- **同不同意**：同意 —— This is the most provider-specific item in the sweep and only openclaw handles it, gated behind an explicit compat flag. Worth adopting only if DeepSeek-family endpoints are in scope; if so, the recoverer matters more than the filter, since a stripped-but-unexecuted tool call is still a broken agent.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:386-401 — if DeepSeek-family endpoints are in scope, port both the text filter and the tool-call recoverer, gated on a provider-compat flag rather than applied unconditionally.

### assistant-message reasoning replay on the REQUEST side (round-trip of delta.reasoning_content)

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:77 (reasoning_content on the assistant message schema) and :257-260 (lowerAssistantMessage joins reasoning parts back into reasoning_content); openclaw openai-completions-compat.ts:159,263-265 (requiresReasoningContentOnAssistantMessages, set for DeepSeek and Xiaomi) with sanitization at .../openai-completions-transport.ts:1527-1583
- **我们**：nothing reads it — openai_compatible.rs:341 discards ContentPart::Reasoning with an empty match arm
- **后果**：Even once we read reasoning off the wire, we cannot send it back. DeepSeek and Xiaomi require reasoning_content on prior assistant messages; without it the model loses its chain of thought across every turn of a tool loop, and openclaw flags some endpoints as outright rejecting the replay when the field is malformed or empty. Multi-step reasoning quality degrades turn over turn.
- **同不同意**：同意 —— The read side is pointless without the write side — this is the same round trip openai_responses.rs already handles via ProviderPrivateState. openclaw's sanitization rules (drop empty reasoning artifacts, normalize reasoning_text into reasoning) show the replay is finicky enough to be worth copying rather than improvising.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:341 — emit reasoning_content on the assistant message payload instead of dropping ContentPart::Reasoning, gated on a provider-compat flag.

### HTTP 403 vs 401 distinction

- **面**：错误与重试分类
- **他们**：openclaw src/agents/embedded-agent-helpers/errors.ts:677-688 — 401/403 both consult the body first: `isAuthPermanentErrorMessage` → auth_permanent (no probe, no rotation), billing text on 401/403 → billing (model fallback, not auth). codex-rs/codex-api/src/api_bridge.rs:177-186 special-cases 403+Cloudflare into a user-readable "blocked by Cloudflare" message.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:563 — one arm for both
- **后果**：A Cloudflare region block, a deactivated workspace, and a stale token all read as the same `authentication` kind. The desktop prints one sentence for all three (desktop/shell/src/surfaces/model.ts PROVIDER_FAILURE.authentication = "密钥不对，或者没有权限"), which sends a person to the settings page for a problem that is not in the settings page.
- **同不同意**：同意 —— The remediation differs per case and the body already says which it is; collapsing them loses the only actionable signal.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:562-571 — add body inspection ahead of the status match, as both references do.
- **复核更正**：Two refinements. (1) The billing half of the openclaw rationale does not transfer as motivation: in our tree `Billing` is not a fallback kind either (failover.rs:32-38), so reclassifying a billing-worded 401/403 changes only the sentence shown, not routing — the routing win is specifically Cloudflare/region-block -> `Unavailable`. (2) `status` is not lost: openai_compatible.rs:573 keeps `Some(stat

### HTTP 404 (model not found)

- **面**：错误与重试分类
- **他们**：openclaw src/agents/failover-error.ts:200-201 (`model_not_found` ↔ 404) and src/agents/embedded-agent-helpers/errors.ts:702-715 — 404 → model_not_found unless the body says session_expired/billing/auth/format/context_overflow; model_not_found then takes the non-transient path (failover-policy.ts:42-47, preserves the transient probe budget).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:571 — falls to `_ => (Protocol, false)`
- **后果**：A misspelled model id, or a model the endpoint has retired, is reported to the person as `protocol` → desktop renders "回复的格式不对" ("the reply's format is wrong"). That is a false statement about where the fault is, and it points at the provider instead of at the model policy the operator wrote.
- **同不同意**：同意 —— 404 is the single most operator-fixable provider failure and it is currently indistinguishable from a wire bug. `ModelErrorKind` has no member for it, so this is a protocol change, not a one-line fix.
- **改哪里**：runtime/crates/protocol/src/lib.rs:2982-2991 — `ModelErrorKind` needs a `ModelNotFound` (or reuse `CapabilityMismatch`); then runtime/apps/model-gateway/src/openai_compatible.rs:562-571.
- **复核更正**：Three refinements to the gap as written, none of which rescue the current code:

1. The failover half of the reference behaviour has no analogue here, so do not carry it over as a justification. Our failover falls back only on RateLimited/Timeout/Unavailable (failover.rs:32-37) and actively rejects any policy whose `fallback_on` contains anything else (failover.rs:74-84). A new `ModelNotFound` wou

### HTTP 409 (conflict)

- **面**：错误与重试分类
- **他们**：openclaw src/agents/provider-transport-fetch.ts:527 — 409 is in the SDK-retryable set (`status === 408 || 409 || 429 || >= 500`) and is retried by the transport unless Retry-After exceeds the wait cap. codex has no 409 arm; it falls into `UnexpectedStatus` (codex-rs/codex-api/src/api_bridge.rs:131-144), which `is_retryable()` returns true for (codex-rs/protocol/src/error.rs:384-395), so codex retries it too.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:571 — `_ => (Protocol, false)`
- **后果**：Both references retry 409; we make it terminal. 409 from a gateway/proxy under contention (a common shape for shared inference endpoints) kills the Run with no retry and no fallback.
- **同不同意**：同意 —— Two independent references retry it and neither treats it as a client bug. Ours is the outlier.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:562-571 — add `StatusCode::CONFLICT => (Unavailable, true)`.
- **复核更正**：The gap stands; two refinements to the CHANGE. (1) `StatusCode::CONFLICT => (ModelErrorKind::Unavailable, true)` at openai_compatible.rs:562-571 is the right and in fact the only workable form: RuntimeExecutionPolicySnapshot::is_bounded_and_safe (runtime/crates/protocol/src/lib.rs:344-350) admits only RateLimited | Timeout | Unavailable in fallback_on, so introducing a new ModelErrorKind for confl

### HTTP 413 (payload too large) vs TPM rate limit

- **面**：错误与重试分类
- **他们**：openclaw src/agents/embedded-agent-helpers/context-overflow.ts:20-23 + 32-35 — explicitly refuses to call 413 a context overflow when the body carries a TPM hint ("Groq uses 413 for TPM limits, which is a rate limit, not context overflow"), and matches `(413 && "too large")` → context_overflow otherwise (line 68). codex: 413 falls into `UnexpectedStatus` → retryable (protocol/src/error.rs:384).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:570-571 — 413 reaches `looks_like_context_overflow` only if the body literally contains "context length"/"context window"/"too many tokens"; otherwise `Protocol, false`
- **后果**：A 413 that says "request too large" (the usual wording) is neither ContextOverflow nor retryable — it is `protocol`. The Run dies without the one signal (`context_overflow`) that the compaction machinery downstream would key on, and without the TPM guard openclaw wrote, a Groq-style 413 would be miscalled a context problem if the wording ever did match.
- **同不同意**：同意 —— 413's two meanings need separating exactly as openclaw separates them; our substring list catches neither reliably.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:627-632 — `looks_like_context_overflow` needs "too large"/"request_too_large"/"prompt is too long" and a TPM negative guard.

### 429 — short-window vs long-window rate limit

- **面**：错误与重试分类
- **他们**：openclaw src/llm/utils/rate-limit-window.ts:45-74 — classifies the limit's *window*: RPM/TPM/per-minute or Retry-After ≤ 60s → "short" (same-model retry is allowed); daily/weekly/monthly/subscription or Retry-After > 60s → "long" (no same-model retry, rotate the profile or fall back). Consumed at src/agents/embedded-agent-runner/run/assistant-failover.ts:44-56, 196-203.
- **我们**：nothing reads it — runtime/apps/model-gateway/src/openai_compatible.rs:565 emits one RateLimited for both
- **后果**：A per-minute throttle and a monthly cap get the same treatment: our retry delay is capped at `max_retry_backoff_ms` 2s / `max_retry_after_ms` 60s (runtime/apps/runtime-host/src/lib.rs:844-850), so for a daily cap we retry twice, open a 30s cooldown, and fail — while the useful answer ("this resets tomorrow") was in the message we hashed away.
- **同不同意**：同意 —— The window decides whether waiting is a strategy at all. openclaw's 60-second boundary is a clean, cheap rule.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:547-580 — carry a window hint out of `classify_http_error`; consume it in runtime/apps/runtime-host/src/lib.rs:3976-3990.

### HTTP 5xx — 500/502/504 vs 503/529 (overloaded)

- **面**：错误与重试分类
- **他们**：openclaw src/agents/embedded-agent-helpers/errors.ts:716-737 — 500/502/504 → timeout, 503 → timeout unless the body says overloaded, 529 → overloaded (its own reason, with a dedicated pre-failover backoff at src/agents/embedded-agent-runner/run/failover-retry-controller.ts:139-147). codex-rs/codex-api/src/api_bridge.rs:59-70 — 503 with body code `server_is_overloaded`/`slow_down` → CodexErr::ServerOverloaded, and `is_retryable()` is FALSE for it (protocol/src/error.rs:380) — codex deliberately stops retrying an overloaded model and tells the person to pick another.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:569 — `status.is_server_error() => (Unavailable, true)` for every 5xx
- **后果**：We hammer an overloaded model with retries that codex has decided are counterproductive, and we cannot tell an operator "this model is at capacity, choose another" — the one sentence codex surfaces verbatim. 529 (Anthropic's overload code) is not a server error in reqwest's `is_server_error()` sense? It is (5xx), so it lands as Unavailable — retryable, which is defensible, but undifferentiated.
- **同不同意**：同意 —— Collapsing all 5xx is defensible as a floor, but the overloaded case has a different right answer (switch model, not wait) and both references model it separately.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:569 — inspect the body for `server_is_overloaded`/`slow_down`/`overloaded_error` before the blanket 5xx arm.

### `Retry-After` header — fractional and delta forms

- **面**：错误与重试分类
- **他们**：openclaw src/agents/provider-transport-fetch.ts:464-492 — reads `retry-after-ms` FIRST (fractional ms, `^\d+(\.\d+)?$`), then `retry-after` as integer seconds, then as an HTTP-date via `parseRetryAfterHttpDateMs`. codex does NOT read the header on the model path at all (grep over codex-rs/model-provider, codex-api, http-client, codex-client returns only `retry_after_unauthorized`, an unrelated bool).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:552-556 → 618-625 `parse_retry_after_ms` — integer seconds, else `DateTime::parse_from_rfc2822`
- **后果**：We beat codex here (codex ignores the header entirely) but miss two live forms: `Retry-After: 1.5` (fractional) parses as None, and `retry-after-ms` (Anthropic and OpenAI both emit it) is never looked at. When parsing fails we fall back to the 2s exponential backoff (lib.rs:3976-3990) and, worse, `opened` at lib.rs:2354-2359 is keyed on `retry_after.is_some()` — so a header we failed to parse also fails to open the cooldown circuit that a parsed one would.
- **同不同意**：同意 —— Two extra branches in a function that already exists, and the failure mode is silent (None looks identical to header-absent).
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:618-625 — add `retry-after-ms` at the call site (line 552-556) and accept `f64` seconds.

### `x-ratelimit-*` / provider rate-limit headers

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/rate_limits.rs:23-101 — parses a whole header family per limit id (`x-{limit}-primary-used-percent`, `-window-minutes`, `-reset-at`, secondary window, credits, limit-name) into RateLimitSnapshot, and codex-rs/codex-api/src/sse/responses.rs:39,74-76 emits them as `ResponseEvent::RateLimits` on EVERY successful stream — not only on failure. codex-rs/codex-api/src/api_bridge.rs:97-109 also attaches the snapshot to a 429 so the user sees which limit was hit and when it resets.
- **我们**：nothing reads it — grep for `x-ratelimit`/`ratelimit` over runtime/ returns only `max_retry_after_ms`
- **后果**：We learn a provider is rate-limited only by being refused. Codex shows headroom continuously, so a person sees the wall approaching; we have no field on `ModelStreamEvent` that could carry it even if the adapter parsed it. This is the largest structural difference in this sweep.
- **同不同意**：同意 —— Pre-emptive headroom is what lets a scheduler slow down instead of failing, and the data is free on every response. It needs a new `ModelStreamEvent` variant, so it is a protocol decision, not an adapter fix.
- **改哪里**：runtime/crates/protocol/src/lib.rs:3000-3080 (`ModelStreamEvent`) — a `RateLimits` variant; then runtime/apps/model-gateway/src/openai_compatible.rs:156-160 to read the headers off the successful response.

### `[DONE]` arrives with no finish_reason anywhere in the stream

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:516-520 — end-of-stream without `response.completed` yields `ApiError::Stream("stream closed before response.completed")` → CodexErr::Stream → `is_retryable() == true` (protocol/src/error.rs:385) → the turn is re-sent, up to `stream_max_retries` (default 5, model-provider-info/src/lib.rs:27).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:191-199 — `Protocol, false`, terminal
- **后果**：Same physical event, opposite verdict: codex retries the turn, we end the Run. A truncated stream is the classic transient — a dropped upstream, a proxy timeout — and calling it non-retryable removes the one response that fixes it.
- **同不同意**：同意 —— Retryability here is about whether re-asking could succeed, and it plainly could. Our `retryable: false` is a statement about our parser's confidence, not about the world.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:191-199 — classify as `Unavailable, true` (the only kind in `fallback_on` that fits a truncated stream).

### Idle / stall timeout while streaming

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:505,522-527 — `timeout(idle_timeout, stream.next())` → `ApiError::Stream("idle timeout waiting for SSE")` → retryable, whole turn re-sent; default idle timeout 300_000 ms (codex-rs/model-provider-info/src/lib.rs:26). openclaw src/agents/embedded-agent-runner/run/idle-timeout-breaker.ts:73-89 — an idle timeout with no completed progress increments a breaker; with progress it resets, and `outputTokens` deliberately does not count as progress; on trip it stops retrying the same model (assistant-failover.ts:241,287 same-model idle retry).
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:164-174 — `Timeout, true`; default 60_000 ms (runtime/apps/runtime-host/src/lib.rs:898), scaffold config ships 15_000 ms (runtime/apps/runtime-host/src/main.rs:684)
- **后果**：Our classification is right and matches codex. Our *value* is the problem: 15s in the shipped scaffold against codex's 300s. A reasoning model that thinks silently, or a server that does not send SSE comments as keepalive, trips it routinely. And because the kernel maps Timeout to `RunStatus::TimedOut` (runtime/crates/kernel/src/lib.rs:565-568) the person sees 已超时 rather than a retry.
- **同不同意**：同意 —— A 20x tighter idle bound than the reference, applied to models that legitimately go quiet for a minute, converts healthy calls into timeouts. The breaker openclaw wrote (progress ≠ billed tokens) is also worth copying if we ever add same-provider retry by default.
- **改哪里**：runtime/apps/runtime-host/src/main.rs:684,699 — raise `stream_idle_timeout_ms` off 15000; runtime/apps/runtime-host/src/lib.rs:898 (60000) is closer but still well under both references.

### Malformed JSON in a single `data:` line

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:530-537 — `serde_json::from_str` failure is logged at debug and `continue`s to the next frame; one bad line never ends a stream.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:204-211 — hard `Protocol, false`, whole call fails
- **后果**：One truncated or vendor-extension frame from a proxy kills a turn that was otherwise fine, and (deltas already sent) blocks fallback too. Codex's choice is deliberate: SSE frames are independent, so a bad one is skippable.
- **同不同意**：**不同意** —— Not clear-cut. Codex's skip is right for a chatty stream where a dropped frame is invisible; ours is right for a protocol we intend to hold providers to. But if we keep failing, it should not be `retryable: false` — a truncated frame is the signature of a transport problem, and re-asking is a real remedy.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:204-211 — at minimum flip to a retryable kind; skipping is a judgement call worth taking deliberately.

### `finish_reason: "length"` (output truncated by max_tokens)

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/sse/responses.rs:422-432 — `response.incomplete` becomes `ApiError::Stream("Incomplete response returned, reason: {reason}")`, which is RETRYABLE, so the turn is re-sent; the reason string is carried verbatim.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:450 → runtime/crates/kernel/src/lib.rs:549-559 — `Completed{Length}` → run.failed with `kind: ModelErrorKind::ContextOverflow`
- **后果**：Hitting `max_output_tokens` is reported as a context overflow. Those are different problems with different fixes (raise max_tokens vs. shorten the input), and the desktop prints the input-side sentence — "这轮的上文超过它能接受的长度" — for an output-side truncation. Our openai_responses.rs:266-274 maps `response.incomplete` the same way, so the mis-mapping is consistent across adapters.
- **同不同意**：同意 —— Two distinct causes collapsed into one kind, and the surfaced sentence names the wrong one.
- **改哪里**：runtime/crates/kernel/src/lib.rs:549-559 — `Length` should not become `ContextOverflow`.

### Retry backoff shape — jitter

- **面**：错误与重试分类
- **他们**：codex-rs/codex-client/src/retry.rs:38-47 `backoff` — base × 2^(n-1) × uniform(0.9,1.1); codex-rs/core/src/util.rs:86-91 — 200ms × 2^(n-1) × uniform(0.9,1.1). Jitter in both layers. openclaw src/agents/embedded-agent-runner/run/helpers.ts:66-82 — deliberately UNjittered linear 10s/20s/30s capped at 60s, documented as "linear and deterministic (no jitter) so RPM windows clear predictably".
- **我们**：runtime/apps/runtime-host/src/lib.rs:3976-3990 `same_provider_retry_delay_ms` — `initial × 2^(attempts-1)` capped at `max_retry_backoff_ms`, no jitter
- **后果**：Our backoff is exponential like codex but unjittered like openclaw — the combination neither reference chose. With many Runs sharing one provider, synchronised retries at 100ms/200ms/400ms produce a thundering herd that the jitter exists to break. The defaults also make this mostly moot: `max_same_provider_attempts` is 1 (lib.rs:844), so by default we never retry the same provider at all.
- **同不同意**：同意 —— Jitter is three lines and the failure it prevents (correlated retry storms) is exactly the one a shared model gateway will see. openclaw's no-jitter choice is defensible only because its steps are 10s+.
- **改哪里**：runtime/apps/runtime-host/src/lib.rs:3976-3990 — add jitter, or raise `max_same_provider_attempts` (lib.rs:844) off 1 so the backoff is reachable at all.

### Diagnostic identifiers (x-request-id, cf-ray) on a failure

- **面**：错误与重试分类
- **他们**：codex-rs/codex-api/src/api_bridge.rs:157-162,173-199 — extracts `x-request-id`, `x-oai-request-id`, `cf-ray`, `x-openai-authorization-error`, and a base64 `x-error-json` code, and attaches them to the surfaced error so a person can quote them to the provider.
- **我们**：nothing reads it — runtime/apps/model-gateway/src/openai_compatible.rs:547-580 reads only `Retry-After` off the response headers
- **后果**：When a provider fails for a reason only they can explain, we have nothing to give their support. Codex treats the request id as part of the error, not as logging.
- **同不同意**：同意 —— Free to collect, and it is the difference between a reproducible support ticket and a shrug.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:552-561 — capture `x-request-id`/`cf-ray` alongside Retry-After.

### Number of same-provider attempts before advancing

- **面**：错误与重试分类
- **他们**：codex-rs/model-provider-info/src/lib.rs:26-28 — DEFAULT_STREAM_MAX_RETRIES 5 (whole-turn stream reconnects) and DEFAULT_REQUEST_MAX_RETRIES 4 (transport-level), i.e. up to 4 transport retries inside each of 5 stream attempts. openclaw src/agents/embedded-agent-runner/run/helpers.ts:48 MAX_SAME_MODEL_RATE_LIMIT_RETRIES 3, plus MAX_EMPTY_ERROR_RETRIES 3 (assistant-failure.ts:34) and profile rotation before model fallback.
- **我们**：runtime/apps/runtime-host/src/lib.rs:844 `max_same_provider_attempts: 1` (bounded 1..=4 at lib.rs:857); protocol default `max_provider_attempts: 8` across the chain (runtime/crates/protocol/src/lib.rs:308)
- **后果**：We do not retry the same provider at all by default — a single transient blip advances to the next candidate, or ends the Run if there is none. Both references retry the same endpoint several times before moving, because the overwhelmingly common transient (one bad connection, one 503) clears on the second try against the same host.
- **同不同意**：同意 —— Advancing on the first blip spends the fallback chain on noise and, for a single-provider policy, converts every transient into a failed Run. The knob exists and is capped at 4; the default is the problem.
- **改哪里**：runtime/apps/runtime-host/src/lib.rs:844 — raise `max_same_provider_attempts` off 1.

### tool_choice (only "auto", and only when tools are non-empty)

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:83-89 (union of auto|none|required|{type:function,function:{name}}) and :188-194,358; openclaw openai-completions-transport.ts:1806-1820 (reconciles caller tool_choice, defaults "auto" only for proxy-like endpoints)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:256
- **后果**：A caller can never force a tool call, forbid tool calls, or name a specific tool. Any orchestration that depends on 'must call exactly this tool now' (structured extraction, forced handoff, retry-with-required) is unexpressible through this gateway, and ModelRequest has no field to carry the intent either.
- **同不同意**：同意 —— Both references treat tool_choice as caller-controlled; hardcoding "auto" bakes one policy into the transport.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:256 plus a tool_choice field on ModelRequest (crates/protocol/src/lib.rs:2972-2978).

### temperature

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:101,362; openclaw openai-completions-transport.ts:1764-1766
- **我们**：not sent — no field exists on ModelRequest (/Users/cola/Documents/Code/agent-runtime-platform/runtime/crates/protocol/src/lib.rs:2972-2978)
- **后果**：Every request runs at the provider default (1.0 on OpenAI). Deterministic-ish work (classification, patch generation, judging) cannot be asked for, and a caller who needs low variance has no way to say so.
- **同不同意**：同意 —— Both references thread it through as an optional caller param and omit the key when unset, which is the safe shape — reasoning models that reject temperature != 1 stay unaffected as long as it is omitted by default.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/crates/protocol/src/lib.rs:2972-2978 (add optional sampling fields) then /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253.

### top_p

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:102,363; openclaw openai-completions-transport.ts:1767-1769
- **我们**：not sent — no field on ModelRequest (crates/protocol/src/lib.rs:2972-2978)
- **后果**：Same as temperature: no nucleus-sampling control. Providers whose recommended knob is top_p rather than temperature (several open-weight serving stacks) cannot be tuned at all.
- **同不同意**：同意 —— Optional, omitted-when-unset in both references; costs nothing to support and is the second half of the sampling pair.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253.

### stop

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:106,367; openclaw openai-completions-transport.ts:1792-1794 (normalized to a non-empty string array, extra-params.ts:455-464)
- **我们**：not sent
- **后果**：No way to terminate generation on a delimiter. Anything that streams into a structured envelope (fenced blocks, sentinel-terminated formats) must instead burn tokens to max_tokens and be truncated client-side.
- **同不同意**：同意 —— Cheap, universally supported by chat-completions servers, and both references carry it.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253.

### parallel_tool_calls

- **面**：请求体全字段
- **他们**：codex /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/codex-api/src/common.rs:260 (always serialized) and core/src/client.rs:920; openclaw agents/embedded-agent-runner/extra-params.ts:418-422 (defaults true for gpt-5*) and :686-690 (patched into the payload for api=openai-completions, allowlist at :63-67). opencode does not send it on chat.
- **我们**：not sent — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:254-257
- **后果**：We accept the server default (true on OpenAI). A tool surface with non-idempotent or ordering-sensitive tools cannot be forced into one-call-at-a-time, and our BTreeMap tool-call accumulator (openai_compatible.rs:161,402-421) will happily emit several calls the Runtime may not want to run concurrently.
- **同不同意**：同意 —— Two of three references send it explicitly rather than inheriting the default, and it is the only wire-level lever for serializing tool execution.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:254-257 (set alongside tool_choice when tools are present).

### stream_options.include_usage (sent unconditionally)

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1747-1748, gated on compat.supportsUsageInStreaming (openai-completions-compat.ts:136-141 — off for Cerebras/Chutes/DeepSeek/Mistral/xAI/Zai-native and for configured non-OpenAI endpoints); opencode openai-chat.ts:97,360 sends it always
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:251
- **后果**：On strict OpenAI-compatible servers that do not implement stream_options, the unknown key is a 400 and the whole Run fails — for a field whose only purpose is cost accounting. openclaw maintains a per-endpoint allowlist precisely because this fails in the field.
- **同不同意**：同意 —— We are right to want usage, but the field must be droppable per provider; the reference with the widest endpoint coverage found it is not universally accepted.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:251 — gate on a config capability flag (OpenAiCompatibleConfig, :21-28).

### prompt_cache_key

- **面**：请求体全字段
- **他们**：codex core/src/client.rs:910 + codex-api/src/common.rs:270 (always set, derived per session); openclaw openai-completions-transport.ts:1752-1755 (gated on compat.supportsPromptCacheKey, key clamped to 64 chars at transports/openai-transport-shared.ts:101-108); opencode openrouter.ts:64
- **我们**：not sent
- **后果**：Prompt-prefix caching is left to the provider's implicit hashing. On a multi-turn Run with a large stable system prompt and tool surface, that means avoidable cache misses — measurably more input cost and higher time-to-first-token on every turn after the first.
- **同不同意**：同意 —— Codex sets it on every single request; the Runtime already has a stable session identity to derive a key from, so the cost of not sending it is pure waste.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253 (needs a session/cache-key input on ModelRequest or on the adapter call).

### tools[].function.parameters — raw schema, no normalization and no strict flag

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:179-186 with protocols/utils/tool-schema.ts:48-64 (forces type:"object", flattens anyOf, strips null variants, sets additionalProperties:false); openclaw openai-completions-transport.ts:1372-1402 (sorted tools, per-tool strict flag, normalizeOpenAIStrictToolParameters at providers/openai-tool-schema.ts:160-170)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:236-246
- **后果**：Whatever ToolSpec.input_schema contains goes on the wire untouched. A tool schema with anyOf/oneOf, a nullable union type, tuple prefixItems, or a missing top-level "type" is accepted by some servers and 400-rejected by others (Gemini-compat and Moonshot endpoints in particular); we cannot offer strict tool calling at all because we never emit the strict flag or close the objects.
- **同不同意**：同意 —— Both references treat schema projection as the transport's job, because the same tool definition has to reach servers with different JSON-Schema subsets.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:236-246.

### tools: [] when the transcript contains tool history but the request declares no tools

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1800-1804 and :1821-1823 (sends an empty tools array so tool-role messages stay legal), with :1824-1830 deleting tools/tool_choice for proxy-like endpoints that reject an empty array
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:254-257 (tools key omitted whenever the list is empty)
- **后果**：Replaying a transcript that contains assistant tool_calls and tool-role results while passing no tools produces a body with tool messages and no tools field. Strict servers reject that combination ('messages with role tool must be a response to a preceding tool_calls'), which is exactly the shape a compaction or a tools-disabled follow-up turn produces.
- **同不同意**：同意 —— openclaw carries the empty array deliberately and has a second rule for endpoints that hate it — evidence both behaviours occur in the wild and the choice must be per-endpoint, not absent.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:254-257.

### system vs developer role for reasoning models

- **面**：请求体全字段
- **他们**：openclaw packages/ai/src/openai-completions-messages.ts:69-76 (role "developer" when model.reasoning && compat.supportsDeveloperRole, else "system"; capability at openai-completions-compat.ts:129)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:270 (Role::System always maps to "system")
- **后果**：Reasoning models on OpenAI take instructions via the developer role; the o1 family rejects role "system" outright, and newer reasoning models treat developer as the higher-authority channel. Our System authority lands in the weaker slot or is refused.
- **同不同意**：同意 —— The Runtime's System authority is meant to be the strongest instruction in the request; silently posting it under a role the model deprioritizes undermines exactly the guarantee the role exists for.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:269-274.

### message content as an array of parts for system and assistant roles

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:294-295 (system content is always a joined string) and :253-255 (assistant content joined to one string or null); openclaw packages/ai/src/openai-completions-messages.ts:75 (system string), :158-171 (assistant string) plus transports/openai-completions-string-content.ts:25-40 flattening any text-only array for requiresStringContent endpoints
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:352-358 (any message with 2+ parts, or one non-text part, serializes as a content array)
- **后果**：A two-paragraph System message (two Text parts) goes out as [{type:text},{type:text}]. Strict and older compatible servers accept only a string for system and assistant content and 400; others accept it but break their prompt-cache prefix. Our single-part fast path hides this until a caller sends a multi-part message.
- **同不同意**：同意 —— Both references normalize to a string on exactly these two roles and keep arrays for user content only — the shape that is portable across servers.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:352-358 (join text parts for system and assistant).

### tool_call_id normalization (charset and 40-char cap)

- **面**：请求体全字段
- **他们**：openclaw packages/ai/src/openai-completions-messages.ts:51-63 (splits Responses-style 'callid|itemid', strips to [A-Za-z0-9_-], truncates to 40 for OpenAI)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:292 and :328 (ids passed through verbatim on both the tool message and the assistant tool_calls)
- **后果**：Ids minted by one provider are replayed to another unchanged. Our gateway explicitly does cross-provider failover (apps/model-gateway/src/failover.rs), so a Responses-shaped or Anthropic-shaped id reaches chat-completions and OpenAI 400s on the length/charset — the failover attempt fails for a reason unrelated to the original failure.
- **同不同意**：同意 —— The only reference that supports cross-provider replay is also the only one that normalizes ids, and we have the same replay problem by design.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:290-294 and :327-334.

### tool-result images (hoisting image parts out of tool messages into a following user message)

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:264-285 + :296-300,326-329 (tool text stays in the tool message, files become a trailing user message); openclaw packages/ai/src/openai-completions-messages.ts:245-267 (same hoist, with an 'Attached image(s) from tool result:' preamble)
- **我们**：not sent — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:275-297 requires exactly one ToolResult part and stringifies it
- **后果**：A tool that returns a screenshot or a rendered chart can only deliver it as JSON text. The chat-completions wire format forbids images inside a tool message, and we implement neither the hoist nor an error that explains it — the image is flattened into content.to_string() (openai_compatible.rs:286-289) and the model sees base64 noise instead of a picture.
- **同不同意**：同意 —— Both references independently invented the same hoist, which is the only way to express this on chat-completions; without it a whole class of tools is unusable on this adapter.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:275-297.

### max_tokens clamping to model / remaining-context budget

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1832-1876 (clamps to the model's max output tokens, then to contextTokens minus estimated input, with debug traces)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:252 (request.max_output_tokens forwarded raw)
- **后果**：A caller asking for more output tokens than the model allows gets HTTP 400 ('max_tokens is too large' / 'exceeds context length') instead of a clamped request. Our error classifier catches only the phrase-matched context-overflow case (openai_compatible.rs:627-633), so most of these land as non-retryable Protocol errors.
- **同不同意**：同意 —— The provider knows its own ceiling and the caller does not; clamping turns a hard failure into a slightly shorter answer.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:252 (needs the model's output ceiling on OpenAiCompatibleConfig, :21-28).

### provider-fixed identity headers (User-Agent, Editor-Version, Copilot-Integration-Id, X-Github-Api-Version, x-initiator, Copilot-Vision-Request, api-key instead of Authorization, api-version query)

- **面**：请求体全字段
- **他们**：openclaw src/agents/copilot-dynamic-headers.ts:18-29 and :68-77; opencode providers/azure.ts:29-44,63-70 (removes authorization, sends api-key header plus api-version query)
- **我们**：not sent — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:139-144 hardcodes bearer auth with no header or query extension point
- **后果**：Azure OpenAI (api-key header, api-version query) and GitHub Copilot (required editor identity headers) cannot be reached through this adapter at all — not a field we get wrong, a provider class we cannot address.
- **同不同意**：同意 —— Both references model auth as a pluggable strategy rather than 'bearer, always'; our config already carries an endpoint URL, so the missing piece is just an extra-headers map.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:21-28 (OpenAiCompatibleConfig) and :139-144.

### delta.tool_calls[].function.name repeated on every fragment for the same index

- **面**：工具调用组装边界
- **他们**：opencode /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/utils/tool-stream.ts:126 `const name = delta.name ?? current?.name`; openclaw transport /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:776-778 `if (toolCall.function?.name) { block.name = toolCall.function.name; }`; openclaw provider /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-completions.ts:500-502 `if (!block.name && toolCall.function?.name) { block.name = toolCall.function.name; }`
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:415-417 — `if let Some(name) = fragment["function"]["name"].as_str() { partial.name.push_str(name); }`. Our test /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/tests/openai_compatible.rs:383-386 deliberately pins this: it feeds `"name":"read_"` then `"name":"file"` and asserts `read_file`.
- **后果**：Same shape as the id: a server that repeats the full name each fragment produces `read_fileread_fileread_file`, which fails the worker's allowed-tool lookup and, before that, is a name no tool has. Unlike the id case there is a real tension — we have a test asserting split-name reassembly, and no reference supports it. But the two behaviours are mutually exclusive, and every reference chose the other one. Worth noting the openclaw pair disagrees internally (last-wins in the transport, first-wins in the provider), which tells you neither direction is load-bearing there; what matters is that neither concatenates.
- **同不同意**：同意 —— Split names across fragments are not a shape any of the three references defends against, while repeated names are the shape they all defend against. Our test encodes a choice that was never compared against the alternative. If the split-name case was observed against a real provider it should stay, but then it needs a guard (e.g. only append while the accumulated name is not itself an allowed tool name) rather than unconditional push_str.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:415 and the test at tests/openai_compatible.rs:383-386 — decide which shape is real; if repeats are, switch to first-non-empty-wins and rewrite the test.
- **复核更正**：Three corrections to the gap as written.

1. Line numbers are off by 24. The name concatenation is at `openai_compatible.rs:439-441`, not 415-417 — 414-416 is the content-delta block-index computation. The test's two SSE lines are 382-383 and the assertion is at 411-415, so "tests/openai_compatible.rs:383-386" should read 381-421 for the whole test.

2. The gap says "no reference supports [concate

### delta.tool_calls[] fragment carrying an index but no id (id never arrives at all)

- **面**：工具调用组装边界
- **他们**：openclaw transport /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:752-759 creates the block with `id: toolCall.id || ""` and carries on; openclaw repairs it later at /Users/cola/Documents/Code/agent-source-research/openclaw/src/agents/embedded-agent-runner/run/attempt.tool-call-normalization.ts:668-675 by minting `call_${randomUUID()...}` (`createStandaloneTextToolCallId`, line 617-619); opencode /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/utils/tool-stream.ts:127 `if (!id || !name) return eventError(route, missingToolMessage)` — fails the stream, wired at /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:429-437
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:473-480 — `if tool_call.id.is_empty() || tool_call.name.is_empty() { return Err(provider_error(ModelErrorKind::Protocol, false, None, "streamed tool call is missing id or function name")) }`
- **后果**：We error the turn, non-retryably, at `[DONE]` — after every other event for the turn has already been emitted. That matches opencode's disposition (error) but not its timing (opencode errors at the *first* delta, so the failure is adjacent to the bad fragment and the wasted tokens are bounded). openclaw is strictly more forgiving: it synthesizes an id, because the id is only ever used to correlate the tool result back, and a locally-minted id correlates just as well as a provider-minted one. Our downstream would have caught it anyway — runtime/apps/worker/src/lib.rs:6128 rejects `id.trim().is_empty()` with `InvalidToolCall` — so the adapter check is defence in depth, not the only line.
- **同不同意**：**不同意** —— Erroring is a defensible choice and one reference (opencode) makes it. But the argument for openclaw's repair is strong: an id we mint is functionally identical for our own correlation, and the alternative is discarding a turn's worth of work over a field the model never chose. I would not call our current behaviour wrong; I would call it the strictest of the three without having decided to be strictest.
- **改哪里**：none — behaviour is coherent as written. If you want openclaw's leniency, /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:473 is where an id would be minted instead of erroring.
- **复核更正**：Line citation for our tree is stale by ~24 lines: the guard is at runtime/apps/model-gateway/src/openai_compatible.rs:496-504, with the condition `if tool_call.id.is_empty() || tool_call.name.is_empty()` on line 497. Lines 473-480 are the ModelStreamEvent::Usage emit, unrelated. The CHANGE pointer should read openai_compatible.rs:497, not :473. (473-480 does match the stale copies under .claude/wo

### delta.tool_calls[].index absent from the fragment entirely

- **面**：工具调用组装边界
- **他们**：openclaw transport /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:740-744 — `const streamIndex = typeof toolCall.index === "number" ? toolCall.index : undefined;` then falls back to `toolCallBlocksById.get(toolCall.id)`, so an index-less stream that carries ids still assembles; same fallback at /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-completions.ts:325-329; opencode /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:138-142 declares `index: Schema.Number` (required), so a missing index fails schema decode at /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/route/client.ts:231-241 and fails the stream
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:403-410 — `let index = fragment["index"].as_u64().ok_or_else(|| provider_error(ModelErrorKind::Protocol, false, None, "streamed tool call is missing its index"))?;`
- **后果**：We match opencode (hard failure) and are stricter than openclaw (id fallback). We also fail earlier than opencode's practical path, which is good. The residual exposure is a provider that emits a single tool call per chunk with `id` and no `index` — openclaw explicitly supports that shape and we reject the turn. Whether that provider exists in your fleet is the question; the code as written assumes it does not.
- **同不同意**：**不同意** —— Two of three references would keep going here, and the fallback is cheap: if index is absent and id is present, key on the id. But requiring index is spec-faithful and one reference agrees, so this is a leniency choice rather than a bug.
- **改哪里**：none — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:403 is where an id-keyed fallback would go if you want openclaw's tolerance.

### tool call whose function.name never arrives

- **面**：工具调用组装边界
- **他们**：openclaw creates the block with `name: toolCall.function?.name || ""` (/Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:755), then at /Users/cola/Documents/Code/agent-source-research/openclaw/src/agents/embedded-agent-runner/run/attempt.tool-call-normalization.ts:697-700 tries `inferToolNameFromToolCallId(rawId, allowedToolNames)` (definition lines 149-204: tokenizes the call id, strips `functions.`/`tools.` prefixes and trailing counters, accepts only an unambiguous single match), and if that fails classifies the message as `{ kind: "malformed", toolName: "blank tool name" }` (lines 740-746, 776-777) and rewrites the assistant turn into plain text — `rewriteUnknownToolLoopMessage`, lines 785-795, wired with `rewriteMalformedBlankToolName: true` at line 1016 — so the model reads its own failure and continues. opencode /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/utils/tool-stream.ts:127 errors the stream on a first delta with no name. codex: `name: String` is a required field of `ResponseItem::FunctionCall` (/Users/cola/Documents/Code/agent-source-research/codex/codex-rs/protocol/src/models.rs:873-892), so a nameless item fails deserialization and is silently dropped with a `debug!` at /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/codex-api/src/sse/responses.rs:331-337.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:473-480 — the same `id.is_empty() || name.is_empty()` guard that covers the missing-id case, returning a non-retryable Protocol error.
- **后果**：We error the turn. openclaw recovers twice over (infer from id, then tell the model); opencode errors; codex silently drops the item and lets the turn continue with whatever else arrived. Ours is at the strict end but not alone there. The duplicated concern is that our single guard conflates "no id" (repairable, openclaw mints one) with "no name" (genuinely unusable — there is no tool to call), so the more repairable of the two is held to the stricter standard.
- **同不同意**：**不同意** —— Erroring on a nameless call is defensible — unlike an id, a name cannot be synthesized. What is worth separating is the two halves of line 473: they are different failures with different repair options, and folding them into one message also makes the error text ambiguous when it fires.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:473-480 — split the id and name conditions into two checks with distinct messages, even if both keep erroring.

### duplicate tool_call ids across chunks (two distinct indices carrying the same id)

- **面**：工具调用组装边界
- **他们**：openclaw detects and repairs: /Users/cola/Documents/Code/agent-source-research/openclaw/src/agents/embedded-agent-runner/run/attempt.tool-call-normalization.ts:646-676 walks the blocks tracking `assignedIds`, and at line 658 `if (!assignedIds.has(trimmedId))` — an id already claimed by an earlier block falls through to lines 668-675, which mint a fresh unique id (looping until it collides with neither `usedIds` nor `assignedIds`). During streaming, lookup is index-first (transport line 741) so the two calls stay distinct blocks and only the `toolCallBlocksById` map entry is clobbered at line 773. opencode keys purely by index (/Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:433) and has no dedup — it emits two `tool-call` events with the same id.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:411-414 keyed by index, so the two calls stay separate; no uniqueness check anywhere in `flush_tool_calls` (lines 468-500). The duplicate is caught downstream at /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/worker/src/lib.rs:6128-6136 — `execution.pending_tool_calls.iter().any(|call| call.id == *id) || execution.outstanding_tool_calls.contains_key(id)` → `WorkerAssignmentError::InvalidToolCall`.
- **后果**：The duplicate does not corrupt state — `outstanding_tool_calls` is a `HashMap<String, ToolExecutionRequest>` (runtime/apps/worker/src/lib.rs:692) and a silent collision there would mismatch results to calls, which the guard prevents. But the failure lands in the worker as an attempt-level error rather than in the adapter as a protocol error, so the diagnostic points at the worker, not at the provider that emitted the duplicate. openclaw repairs and proceeds; we stop.
- **同不同意**：同意 —— The guard is correct and important — silently accepting duplicate ids would be far worse. What is missing is that the adapter, which is the only component that can see the provider emitted the duplicate, does not say so; the run fails one layer up with a message that names none of that.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:472 — track seen ids across the flush loop and either error with a provider-attributed message or mint a fresh id the way attempt.tool-call-normalization.ts:668 does.

### stream reaches [DONE] with no finish_reason on any chunk

- **面**：工具调用组装边界
- **他们**：openclaw's provider path agrees with us and throws: /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-completions.ts:552-554 `if (!hasFinishReason) { throw new Error("Stream ended without finish_reason"); }`. openclaw's transport path disagrees with itself — it never tracks `hasFinishReason`, defaults `stopReason: "stop"` at /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:274, and relies on the `[DONE]`-gated promotion at lines 826-833 to rescue tool-call-only turns. opencode does not require one at all: `mapFinishReason` is only consulted when `choice.finish_reason` is truthy (/Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:412) and `finishEvents` line 468 `if (reason)` simply emits no finish event — with the side effect that accumulated tool calls are never finalized either.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:191-199 — `let reason = finish_reason.ok_or_else(|| provider_error(ModelErrorKind::Protocol, false, None, "provider stream completed without finish_reason"))?;`. Pinned by /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/tests/openai_compatible.rs:306-346.
- **后果**：Fail-closed, with one reference agreeing outright. The cost is that it composes badly with the previous entry: a provider that omits finish_reason *and* emits tool_calls is exactly openclaw's Evolink case, and we reject the whole turn where openclaw promotes to toolUse. The `retryable: false` on line 194 also means the platform will not re-roll, though re-rolling a deterministic provider quirk would not help anyway.
- **同不同意**：**不同意** —— Requiring finish_reason is the right default and openclaw's provider path backs it. But the fix for the previous entry should be applied at this same site: if the map has tool calls and `[DONE]` was clean, `ToolCalls` is a sound inference and is strictly better than failing.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:192 — before erroring, infer `ModelFinishReason::ToolCalls` when `!tool_calls.is_empty()`; keep the error for the genuinely empty case.

### stream EOF before [DONE] (connection dropped mid-turn)

- **面**：工具调用组装边界
- **他们**：codex treats it as retryable: /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/codex-api/src/sse/responses.rs:515-520 sends `ApiError::Stream("stream closed before response.completed")`, surfaced again at /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/core/src/session/turn.rs:2259-2267, and `CodexErrorDetails::Stream(..)` is on the retryable side of /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/protocol/src/error.rs:383-389, driving the retry loop at core/src/session/turn.rs:1392-1406. There is a dedicated regression test for it: /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/core/tests/suite/stream_no_completed.rs:23-40 (`retries_on_early_close`). openclaw's transport comment at openai-completions-transport.ts:823-825 makes the same distinction the other way — EOF without `[DONE]` "remains fail-closed" for promotion purposes. opencode has no such requirement: `[DONE]` is dropped as a keep-alive (/Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/shared.ts:236-247) and EOF is a clean halt into `onHalt`.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:175-182 — `let Some(event) = next else { return Err(provider_error(ModelErrorKind::Protocol, false, None, "provider stream ended without [DONE]")) };`
- **后果**：We detect it correctly — requiring `[DONE]` is right and matches codex's requirement of `response.completed` — but we mark it `retryable: false`. codex marks the identical condition retryable and has a test proving the retry. A dropped connection is the canonical transient failure; classifying it as a non-retryable protocol error means a TCP reset mid-turn burns the attempt permanently, when re-rolling would have succeeded.
- **同不同意**：同意 —— The detection is right, the classification is not. `ModelErrorKind::Protocol, retryable: false` says "this provider speaks the protocol wrong and will do so again" — but an early close says nothing about the provider's correctness. Compare our own line 148-152, where a header timeout is correctly `Timeout, retryable: true`.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:176-181 — set `retryable: true` (and consider `ModelErrorKind::Unavailable`), matching codex protocol/src/error.rs:383.

### SSE data frame that is not valid JSON

- **面**：工具调用组装边界
- **他们**：codex skips the frame and keeps reading: /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/codex-api/src/sse/responses.rs:533-537 — `Err(e) => { debug!("Failed to parse SSE event: {e}, data: {}", &sse.data); continue; }`. opencode fails the stream: /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/route/client.ts:231-241 maps any decode failure to `eventError(... "Invalid ${route} stream event")`. openclaw skips non-objects at /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:636-639 (`if (!rawChunk || typeof rawChunk !== "object") continue`), the SDK having already dropped unparseable frames.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:204-211 — `let chunk: Value = serde_json::from_str(&event.data).map_err(|error| provider_error(ModelErrorKind::Protocol, false, None, format!("provider SSE data is not valid JSON: {error}")))?;`
- **后果**：We error the turn on any unparseable frame, matching opencode and diverging from codex/openclaw. In practice this bites on provider keep-alive and comment frames that are not strictly `[DONE]` — some gateways emit `data: ping` or vendor-prefixed frames. One such frame anywhere in a long turn discards the whole turn. Two of three references keep going.
- **同不同意**：**不同意** —— Strictness here is a real choice — silently skipping frames can hide a genuine protocol break — but the asymmetry is worth noticing: we skip nothing while codex, which is otherwise the strictest reference about stream termination, skips freely. codex's position is coherent: end-of-stream is load-bearing, individual frames are not.
- **改哪里**：none — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:204 is the site if you want codex's skip-and-continue.

### choice.finish_reason carrying an unrecognized value

- **面**：工具调用组装边界
- **他们**：opencode degrades gracefully: /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:378-384 `mapFinishReason` returns `"unknown"` for anything unmatched and the turn completes. openclaw converts it to an error stop reason carrying the original text: /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-stop-reason.ts:36-39 `{ stopReason: "error", errorMessage: "Provider finish_reason: ${reason}" }`, which the provider path then throws at /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-completions.ts:549-551. openclaw also maps two values we do not: `"end"` (line 18) and `"network_error"` (line 32).
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:428-435 — `value => { return Err(provider_error(ModelErrorKind::Protocol, false, None, format!("unsupported provider finish_reason {value}"))) }`
- **后果**：We error mid-stream, before `flush_tool_calls` ever runs, so any tool calls already assembled are discarded along with the turn. We are between the two references in strictness: openclaw errors too (with the same message shape), opencode does not. The concrete exposure is the vocabulary — `"end"` and the singular `"tool_call"` are both real spellings openclaw accepts and we reject outright.
- **同不同意**：同意 —— Erroring on an unknown terminal is defensible; rejecting known-real synonyms is not. The two extra spellings openclaw carries are exactly the kind of thing that only shows up against a non-OpenAI server, which is the entire point of an "openai-compatible" adapter.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:424-427 — add `"end"` to the `Stop` arm and `"tool_call"` to the `ToolCalls` arm, per openai-stop-reason.ts:17-29.

### unbounded growth of accumulated tool-call argument text

- **面**：工具调用组装边界
- **他们**：openclaw caps it and throws: /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:380 `MAX_TOOL_CALL_ARGUMENT_BUFFER_BYTES = 256_000`, enforced per-block at lines 784-789 by measuring UTF-8 bytes against a `WeakMap` running total and throwing `"Exceeded tool-call argument buffer limit"`. It carries a matching 256KB cap on buffered post-tool-call deltas (lines 379, 420-424).
- **我们**：Nothing reads it. /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:418-420 — `if let Some(arguments) = fragment["function"]["arguments"].as_str() { partial.arguments.push_str(arguments); }` — unbounded, with unbounded `BTreeMap` entries behind it (line 411 `entry(index).or_default()` will create a bucket for any u64 index the provider sends).
- **后果**：A provider that streams unbounded argument text — a looping model, or a hostile/compromised endpoint — grows the gateway's heap without limit for the duration of the turn. Our only backstop is `stream_idle_timeout` (line 166), which measures the gap *between* chunks, not total volume: a server emitting steadily never trips it. Same for index cardinality: a chunk stream with monotonically increasing indices allocates a `PartialToolCall` per index. This is a resilience gap in a component that terminates untrusted network input on behalf of every tenant, which is a different weight class than the parsing edges above.
- **同不同意**：同意 —— openclaw is the only reference that bounds this and it bounds it in two places, which suggests they hit it. For a multi-tenant gateway the argument is stronger than for a local CLI — the endpoint is tenant-configured, so "the provider is trusted" is not an assumption this file gets to make.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:418-420 — track accumulated bytes per `PartialToolCall` and across the map, erroring past a cap, per openai-completions-transport.ts:784-789.

## cosmetic（24 条）

### delta.reasoning_text

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:1145-1152 (third in the precedence chain)
- **我们**：nothing reads it
- **后果**：Third naming variant for the same content; dropped. Lowest-frequency of the three but free to support once the chain exists.
- **同不同意**：同意 —— Same chain, no extra cost. openclaw also normalizes it back to `reasoning` on replay (:1563-1574), so it is a real wire shape they observed.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:386 — include in the same ordered chain.
- **复核更正**：Two refinements, neither of which undermines the gap. (1) The CHANGE anchor is a few lines high: openai_compatible.rs:386 is the start of the "Two spellings, one fact" comment block; the ordered chain itself is at lines 393-396, so the added `.or_else(|| delta["reasoning_text"].as_str())` goes after line 395 (and the comment's "Two spellings" wording needs updating to three). (2) Precedence order 

### delta.tool_calls[].function.arguments

- **面**：流式分片全字段
- **他们**：opencode .../openai-chat.ts:135 (schema), :434 -> tool-stream.ts:132 `input: ${current?.input ?? ""}${delta.text}` (concatenate); openclaw .../openai-completions-transport.ts:783-798 `block.partialArgs += toolCall.function.arguments`
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:418-420 — push_str (concatenate); parsed once at :481-488
- **后果**：Nothing lost on accumulation. One difference: openclaw caps the accumulated bytes (MAX_TOOL_CALL_ARGUMENT_BUFFER_BYTES, :786-788, throws 'Exceeded tool-call argument buffer limit') and we do not, so a runaway provider can grow an unbounded String in our process.
- **同不同意**：同意 —— Concatenation is correct and matches both. The missing byte cap is a robustness gap, not a protocol gap — worth adding since we already bound other inputs.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:418-420 — bound `partial.arguments` growth and fail with a Protocol error past the cap.

### choices[].index

- **面**：流式分片全字段
- **他们**：nobody. opencode's OpenAIChatChoice (openai-chat.ts:151-154) declares only delta and finish_reason — index is not in the schema. openclaw reads chunk.choices[0] (:649) and never looks at its index.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:390-392 — mapped into ModelStreamEvent::TextDelta.block
- **后果**：Nothing is lost; this is a field we read that neither reference does. Because we never send `n`, index is always 0, so every TextDelta carries block: Some(0). Harmless, but it conflates two different meanings of `block`: in anthropic_messages.rs:467 block is a content-block index (two blocks are two things the model said) and in openai_responses.rs:162 it is output_index, whereas here it is the completion index for n>1. The code comment at :387-389 acknowledges the reinterpretation.
- **同不同意**：**不同意** —— It is defensible — the comment argues the choice *is* the block for this protocol — but no reference reaches for it, and a constant Some(0) buys a client nothing it could not infer. Flagged as invented, not as broken.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:390-392 — optional: drop it, or keep it and leave the comment as the justification.

### choices[] beyond index 0 (multiple completions)

- **面**：流式分片全字段
- **他们**：nobody. opencode .../openai-chat.ts:411 `const choice = event.choices[0]`; openclaw .../openai-completions-transport.ts:649 `chunk.choices[0]`. Both discard every other choice.
- **我们**：runtime/apps/model-gateway/src/openai_compatible.rs:384 — we iterate the full choices array
- **后果**：Nothing lost; another thing we read that no reference does. But it is unreachable: we never send `n` (request_payload at :247-253), so the array is always length 1. If a provider ever returned more, we would interleave two completions' text into one stream and let the last choice's finish_reason overwrite the first (:422-437) — worse than taking [0].
- **同不同意**：**不同意** —— Iterating is only safe if downstream can separate the streams, and `block` (always 0 in practice) is not a reliable separator. Both references chose [0] deliberately. Not urgent because the branch is dead under our own request shape.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:384 — either take only the first choice, or reject a chunk carrying more than one.

### chunk.id (response / completion id)

- **面**：流式分片全字段
- **他们**：openclaw .../openai-completions-transport.ts:643 — `output.responseId ||= chunk.id` (first non-empty wins)
- **我们**：nothing reads it
- **后果**：We keep no provider-side identifier for the completion. When a provider reports a problem after the fact, or a person asks why a turn behaved oddly, we have nothing to correlate against the provider's logs. Note we do capture the HTTP-level request id path elsewhere, but not the completion id.
- **同不同意**：同意 —— One assignment, and it is the only handle a provider support ticket can be built on. codex does the equivalent for Responses (upstream_request_id, codex-api/src/sse/responses.rs:54-58,95).
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:377-459 — capture chunk["id"] once and attach it to the terminal Completed event or an audit record.

### chunk.model (server-reported effective model)

- **面**：流式分片全字段
- **他们**：codex-api/src/sse/responses.rs:46-50 and :177-197 (response_model(), preferring response.headers then top-level headers) — emitted as ResponseEvent::ServerModel at :71-73 and re-emitted whenever it changes mid-stream (:543-553). No chat-completions reference reads the chunk-level `model` field.
- **我们**：nothing reads it
- **后果**：When a gateway silently routes to a different model than requested — OpenRouter substituting an upstream, a proxy falling back — we neither notice nor record it, and we price the result with the pricing table for the model we asked for. codex treats a changed server model as an event worth telling the caller about.
- **同不同意**：同意 —— codex clearly considers this worth surfacing, and it compounds with the usage.cost gap: if we neither read the reported cost nor notice the substituted model, our cost figure is doubly unmoored. Low severity because no chat-completions reference bothers.
- **改哪里**：runtime/apps/model-gateway/src/openai_compatible.rs:377-459 — read chunk["model"] and record a mismatch against self.model.

### Retry visibility while the wait is happening

- **面**：错误与重试分类
- **他们**：codex-rs/core/src/responses_retry.rs:56-68 — emits `Reconnecting... {n}/{max}` to the UI BEFORE sleeping, with an explicit comment that this exists "so the user understands what is happening instead of staring at a seemingly frozen screen"; the first websocket retry is suppressed in release to cut noise. openclaw logs the same at failover-retry-controller.ts:158-160.
- **我们**：runtime/crates/kernel/src/lib.rs:614-634 `record_model_provider_retry_scheduled` writes `model.provider.retry_scheduled` with delay_ms/provider_attempt, and desktop/shell/src/surfaces/model.ts `providerRetry` renders it as "N 秒后再试・第 M 次（原因）"
- **后果**：We match codex here, and the desktop wording is arguably better (it names the delay and the cause). The gap is elsewhere: the retry event is only produced by the runtime-host path, and `max_same_provider_attempts` defaults to 1 (runtime/apps/runtime-host/src/lib.rs:844), so in the default configuration this event is never emitted and the person sees nothing during the one fallback hop.
- **同不同意**：**不同意** —— The mechanism is right and better-worded than the reference; only the default makes it dead code.
- **改哪里**：runtime/apps/runtime-host/src/lib.rs:844 — `max_same_provider_attempts: 1` means the retry path never runs by default.

### seed

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:105,366; openclaw openai-completions-transport.ts:1789-1791
- **我们**：not sent
- **后果**：No reproducibility knob. Replaying a Run against the same provider cannot be made near-deterministic, which makes regression-diffing a Run's output guesswork.
- **同不同意**：同意 —— Both references pass it straight through when set; it is optional everywhere and ignored by servers that do not implement it.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253.

### frequency_penalty

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:103,364; openclaw openai-completions-transport.ts:1783-1785
- **我们**：not sent
- **后果**：No repetition control. Matters mainly for smaller open-weight models that loop; on frontier models the default is fine.
- **同不同意**：同意 —— Both references carry it as an optional passthrough; adding it is free once the sampling struct exists.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253.

### presence_penalty

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:104,365; openclaw openai-completions-transport.ts:1786-1788
- **我们**：not sent
- **后果**：Same as frequency_penalty — no topic-repetition control for models that need it.
- **同不同意**：同意 —— Optional passthrough in both references; note openclaw explicitly filters these out on the Responses API path, so they are chat-completions-only.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253.

### store

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1749-1751 (store:false when the endpoint supports it; compat default at openai-completions-compat.ts:127-128) with an extra strip wrapper at extra-params.ts:706-720; codex codex-api/src/common.rs:262 + core/src/client.rs:921 (true only for Azure); opencode openai-chat.ts:98,334,339
- **我们**：not sent
- **后果**：We inherit the provider's retention default instead of stating our intent. On accounts where chat-completions logging is on, prompts and completions are retained provider-side without the Runtime ever asking for it — a data-handling posture we neither chose nor recorded.
- **同不同意**：同意 —— All three references make retention explicit; openclaw's default is store:false, which is the conservative choice for an agent runtime.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253 (send "store": false, behind the same capability gate as stream_options — non-OpenAI endpoints 400 on it, which is why openclaw strips it).

### prompt_cache_retention ("24h")

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1756-1763 (sent alongside prompt_cache_key when the caller asked for long retention and the endpoint supports it)
- **我们**：not sent
- **后果**：Without it, long-lived sessions fall back to the short default prefix-cache lifetime, so a Run resumed after a gap re-pays for its whole prefix.
- **同不同意**：同意 —— Only meaningful once prompt_cache_key exists; a follow-on to that gap, not an independent one.
- **改哪里**：none (do this with the prompt_cache_key change at /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:247-253).

### image_url — media_type is ignored and the source is forwarded blind

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:205-208 (validateMedia rejects anything outside ProviderShared.IMAGE_MIMES and always emits a data: URL); openclaw packages/ai/src/openai-completions-messages.ts:113-116 (builds data:${mimeType};base64,${data} from the typed block)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:304-306 (ContentPart::Image { source, .. } — media_type discarded; is_provider_safe_image_source at :366-375 only checks the scheme)
- **后果**：A ContentPart::Image whose media_type is application/pdf or video/mp4 passes our check and is posted as image_url; the server 400s on the content type. We also never send image_url.detail, so we cannot ask for low-detail (cheaper) vision.
- **同不同意**：同意 —— Both references validate the mime before choosing the image_url shape; we hold the media_type in hand and discard it, which is a check we could make for free.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:304-306.

### tool message content — empty-string results

- **面**：请求体全字段
- **他们**：openclaw packages/ai/src/openai-completions-messages.ts:20,34-37,231-234 (empty or whitespace-only tool output is replaced with '(no output)' or a media placeholder)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:286-294 (a JSON string "" is forwarded as content: "")
- **后果**：A tool that legitimately returns nothing produces a tool message with empty content. Some compatible servers reject empty tool content, and models that accept it often treat the call as failed and retry it.
- **同不同意**：同意 —— A one-line substitution that removes an ambiguous signal; openclaw added the sentinel for exactly this reason.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:286-294.

### tool message name field

- **面**：请求体全字段
- **他们**：openclaw packages/ai/src/openai-completions-messages.ts:240-242 (adds name when compat.requiresToolResultName)
- **我们**：not sent — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:290-294
- **后果**：A handful of compatible servers still require the legacy name on tool-role messages and error without it. On OpenAI it is ignored, so this only bites on specific self-hosted stacks.
- **同不同意**：**不同意** —— openclaw sends it only behind an explicit per-model compat flag and no other reference sends it at all; adding it unconditionally would be worse than omitting it, and we have no provider that needs it yet.

### assistant content: null vs "" on tool-call-only messages

- **面**：请求体全字段
- **他们**：opencode openai-chat.ts:255 (null when there is no text); openclaw packages/ai/src/openai-completions-messages.ts:129-132 ("" instead of null when compat.requiresAssistantAfterToolResult)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:352-353 (Value::Null)
- **后果**：null is the canonical OpenAI shape and matches opencode exactly. A minority of compatible servers reject null and want an empty string; those would fail on tool-calling turns.
- **同不同意**：**不同意** —— We already match the reference that speaks stock chat-completions; the empty-string variant is a per-endpoint quirk openclaw only applies behind a flag.

### provider-specific reasoning switches (reasoning:{effort} for OpenRouter, reasoning:{enabled} for Together, enable_thinking / chat_template_kwargs.enable_thinking for Qwen)

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1915-1919 (OpenRouter), :1352-1359 (Together), :1318-1346 (Qwen and Qwen chat-template)
- **我们**：not sent
- **后果**：On those endpoints, reasoning cannot be turned on or off at all — reasoning_effort is not the knob they read. A Qwen served through vLLM will keep thinking regardless of a Minimal policy, and an OpenRouter reasoning model ignores our (currently absent) effort entirely.
- **同不同意**：**不同意** —— These are per-endpoint dialects; they belong behind the same provider-capability table as max_tokens field selection, and are only worth adding once that table exists and we actually route to those providers.

### service_tier

- **面**：请求体全字段
- **他们**：codex codex-api/src/common.rs:268 + core/src/client.rs:922; openclaw src/llm/providers/stream-wrappers/openai.ts:370-377 (service_tier "priority" in fast mode, gated to OpenAI-native endpoints)
- **我们**：not sent
- **后果**：No latency/priority tier selection; requests take the account default tier. Only meaningful on first-party OpenAI with a priority contract.
- **同不同意**：**不同意** —— Both references gate it strictly to OpenAI-native endpoints and it 400s elsewhere; nothing in our Runtime currently expresses a latency tier.

### tool ordering stability (sorting tools by name for cache-prefix stability)

- **面**：请求体全字段
- **他们**：openclaw openai-completions-transport.ts:1381 (sortTransportToolsByName, exported from transports/openai-transport-shared.ts:8 as the prompt-cache-stability helper)
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:233-246 (caller order preserved)
- **后果**：If the tool list order varies between turns (map iteration, dynamic MCP tool discovery), the serialized tools block changes and the provider's prompt-cache prefix breaks — silent extra input cost with no visible symptom.
- **同不同意**：同意 —— A stable sort is free and turns an invisible cost regression into a non-issue; it only matters once prompt caching is being pursued at all.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:233-246.

### Accept: text/event-stream request header

- **面**：请求体全字段
- **他们**：codex codex-api/src/endpoint/responses.rs:147-152 (explicit Accept on the streaming POST); openclaw packages/ai/src/transports/openai-transport-params.ts:312-315 (Accept: text/event-stream for the native streaming backend)
- **我们**：not sent — /Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:139-144 sets only bearer auth and reqwest's Content-Type
- **后果**：We ask for a stream in the body and never in the headers. OpenAI does not care, but proxies and gateways that content-negotiate can answer with a buffered JSON body, which our SSE parser (openai_compatible.rs:160) then fails as 'invalid provider SSE stream'.
- **同不同意**：同意 —— One header, no downside, and it makes the intent explicit to anything sitting between us and the model.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:139-144.

### session / correlation headers (session_id, x-session-id, x-client-request-id, x-session-affinity, thread-id, originator, x-openai-subagent, x-codex-installation-id)

- **面**：请求体全字段
- **他们**：codex codex-api/src/endpoint/responses.rs:87-94 and requests/headers.rs:5-14 (session-id, thread-id, x-client-request-id, x-openai-subagent), core/src/client.rs:611-624 (originator, installation id); openclaw packages/ai/src/providers/openai-completions.ts:653-661 (x-session-id for OpenRouter; session_id + x-client-request-id + x-session-affinity otherwise) and transports/openai-transport-params.ts:275-286
- **我们**：not sent
- **后果**：No request correlation with the provider (a support escalation cannot be tied to our Run), and on OpenRouter no session affinity, so consecutive turns of one Run can land on different upstream deployments and lose the prefix cache.
- **同不同意**：同意 —— Both Rust and TypeScript references send session identity on every request; we already have a session and run id to put there, and the headers are ignored by servers that do not know them.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:139-144.

### usage: { include: true } (OpenRouter's usage-accounting field)

- **面**：请求体全字段
- **他们**：opencode packages/llm/src/providers/openrouter.ts:55-66 (extends the OpenAI chat body with usage:{include:true}, reasoning:{...}, prompt_cache_key)
- **我们**：not sent — we rely solely on stream_options.include_usage (/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:251)
- **后果**：On OpenRouter, usage accounting is requested through the usage object; if it is absent the usage chunk our cost path depends on (openai_compatible.rs:439-457, calculate_cost at :512-521) may never arrive and the Run reports zero cost.
- **同不同意**：同意 —— Provider-specific, but it is the one place where our billing telemetry silently reports a wrong number rather than failing loudly — worth having when OpenRouter is a configured provider.
- **改哪里**：none (belongs with the per-provider capability table implied by the stream_options and max_tokens fixes).

### chunk.choices containing more than one choice

- **面**：工具调用组装边界
- **他们**：Both TypeScript references read only the first choice and ignore the rest: opencode /Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/openai-chat.ts:411 `const choice = event.choices[0]`; openclaw /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/transports/openai-completions-transport.ts:649 and /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/providers/openai-completions.ts:410, both `chunk.choices[0]`, each with an explicit `if (!choice) continue`.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:384 — `for choice in chunk["choices"].as_array().into_iter().flatten() {` — we iterate every choice, but `tool_calls` (line 161) is keyed on the tool-call index alone, with no choice dimension.
- **后果**：If a server ever returns n>1 completions, tool calls from different choices sharing an index number are merged into one `PartialToolCall` — ids, names and argument text from two independent completions concatenated together. `finish_reason` is likewise last-choice-wins (line 423). We never send `n` in the request payload (lines 247-257), so this needs a misbehaving or defaulted-n server to fire, which makes it unlikely rather than impossible. The text path is already choice-aware (line 390 threads `choice["index"]` into the block field) — so the file half-acknowledges multiple choices and then merges their tool calls.
- **同不同意**：同意 —— Low likelihood, but it is an internal inconsistency rather than a considered position: we thread the choice index into TextDelta blocks specifically because more than one choice is conceivable, then key the tool-call map as though it is not. Either take both choices seriously or, like both references, take the first and skip the rest.
- **改哪里**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:384 — key the map on `(choice_index, tool_index)`, or read only the first choice as both references do.

### assembled arguments that parse as valid JSON but not as an object (string, array, number, null)

- **面**：工具调用组装边界
- **他们**：openclaw coerces to `{}`: /Users/cola/Documents/Code/agent-source-research/openclaw/packages/ai/src/utils/json-parse.ts:116-120 `asStreamingJsonRecord` returns the value only when it is a non-null, non-array object, otherwise `{}`; every call site is typed `Record<string, unknown>` (line 129). opencode does not constrain it — `parseToolInput` (/Users/cola/Documents/Code/agent-source-research/opencode/packages/llm/src/protocols/shared.ts:155-156) returns whatever JSON decodes. codex requires an object where it matters: /Users/cola/Documents/Code/agent-source-research/codex/codex-rs/core/src/tools/handlers/mod.rs:106-113 `let Value::Object(arguments) = &mut arguments else { return Err(FunctionCallError::RespondToModel(format!("{tool_name} arguments must be an object"))) }` — again handed back to the model, not raised as an error.
- **我们**：/Users/cola/Documents/Code/agent-runtime-platform/runtime/apps/model-gateway/src/openai_compatible.rs:481-495 — `serde_json::from_str` into the `Value` inferred by `ModelStreamEvent::ToolCall.arguments` (/Users/cola/Documents/Code/agent-runtime-platform/runtime/crates/protocol/src/lib.rs:3023-3027, `arguments: Value`), with no object check.
- **后果**：`arguments: "\"hello\""` or `"[1,2]"` passes our flush intact and reaches the worker as a non-object `Value`. It then fails at each tool's own deserialization — e.g. runtime/apps/worker/src/lib.rs:7067-7069 `serde_json::from_value::<SubagentHistoryArguments>(...).map_err(|_| WorkerAssignmentError::InvalidToolCall)` — so it is caught, but as an opaque `InvalidToolCall` at dispatch rather than as a shape error at the boundary that could name what arrived.
- **同不同意**：**不同意** —— Caught either way, and codex's message ("arguments must be an object") is only better because codex can hand it back to the model — a channel we do not currently have. Not worth a change on its own; worth folding in if the invalid-JSON entry above is reworked to a respond-to-model shape.

