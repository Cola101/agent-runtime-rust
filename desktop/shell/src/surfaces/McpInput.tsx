/// Answering `mcp.input.required`.
///
/// Every widget here is one property the MCP server declared in its
/// `requested_schema`, every answer is one of the three actions the protocol
/// defines, and the identity that goes back — input id, response-binding
/// version, binding digest — is echoed from the event. There is no generic
/// "MCP form": a mode this build does not know, or a field type it has no
/// widget for, is said on screen and left unacceptable rather than guessed at.
///
/// One submission answers every request at once because that is the contract:
/// the runtime refuses a resolution that does not answer the exact pending set.
import { useState } from "react";
import { MCP_ACTIONS, mcpModeLabel } from "./model";
import { useDesk } from "../desk";
import type { McpField, McpRequest, McpResponse, RunView } from "../store";

/// What the person has typed into one request's fields, before it is a value.
type Draft = Record<string, string | boolean>;

type Action = (typeof MCP_ACTIONS)[number]["action"];

/// What one field may be sent as.
///
/// The runtime compares each value's JSON type against the type the schema
/// declared and refuses the whole resolution when they disagree, so the
/// conversion happens here — where the person can still fix it — rather than
/// arriving later as a rejected answer with no field named.
type Filled =
  | { state: "empty" }
  | { state: "value"; value: unknown }
  | { state: "bad"; why: string }
  /// A declared type this client has no widget for. Named on screen: a form
  /// that quietly asks for less than the server did is the same mistake as one
  /// that asks for more.
  | { state: "unsupported" };

function filled(field: McpField, raw: string | boolean | undefined): Filled {
  if (field.type === "boolean") {
    return typeof raw === "boolean" ? { state: "value", value: raw } : { state: "empty" };
  }
  if (field.type !== "string" && field.type !== "number" && field.type !== "integer") {
    return { state: "unsupported" };
  }
  const text = typeof raw === "string" ? raw : "";
  if (text.trim() === "") return { state: "empty" };
  if (field.type === "string") return { state: "value", value: text };
  const value = Number(text);
  if (!Number.isFinite(value)) return { state: "bad", why: "要一个数字" };
  if (field.type === "integer" && !Number.isInteger(value)) return { state: "bad", why: "要一个整数" };
  return { state: "value", value };
}

/// Why this request cannot be accepted right now, or null when it can.
function refusal(request: McpRequest, draft: Draft): string | null {
  if (request.mode === "url") return null;
  if (request.mode !== "form") return "本版本不认识这种请求方式，只能拒绝或取消";
  for (const field of request.fields) {
    const value = filled(field, draft[field.name]);
    if (value.state === "bad") return `${field.name}：${value.why}`;
    if (!field.required) continue;
    if (value.state === "empty") return `${field.name} 是必填的`;
    if (value.state === "unsupported") {
      return `${field.name} 是 ${field.type}，本客户端没有这种字段的输入方式`;
    }
  }
  return null;
}

/// Only what the person actually put there. An optional field left alone is
/// left out rather than sent as an empty string or as a false nobody chose.
function content(request: McpRequest, draft: Draft): Record<string, unknown> {
  const answered: Record<string, unknown> = {};
  for (const field of request.fields) {
    const value = filled(field, draft[field.name]);
    if (value.state === "value") answered[field.name] = value.value;
  }
  return answered;
}

function Field({
  field, group, value, onChange,
}: {
  field: McpField;
  group: string;
  value: string | boolean | undefined;
  onChange(next: string | boolean): void;
}) {
  const id = `${group}-${field.name}`;
  // The schema's own words for what it is asking for: the property name, its
  // JSON type, and whether it is in `required`. All three decide what the
  // runtime will accept, so all three are on screen.
  const spec = (
    <span className="fspec mono dim">
      {field.title ? `${field.name} ・ ` : ""}{field.type}{field.required ? " ・ 必填" : ""}
    </span>
  );
  const state = filled(field, value);

  if (field.type === "boolean") {
    return (
      <fieldset className="field">
        <legend>{field.title ?? field.name} {spec}</legend>
        {field.description && <p className="fdesc">{field.description}</p>}
        {[{ shown: "是", wire: true }, { shown: "否", wire: false }].map((choice) => (
          <label className="opt" key={String(choice.wire)}>
            <input
              type="radio"
              name={id}
              checked={value === choice.wire}
              onChange={() => onChange(choice.wire)}
            />
            <span>{choice.shown}</span>
            <span className="mono dim">{String(choice.wire)}</span>
          </label>
        ))}
      </fieldset>
    );
  }

  if (state.state === "unsupported") {
    return (
      <div className="field">
        <span className="fname">{field.title ?? field.name}</span> {spec}
        {field.description && <p className="fdesc">{field.description}</p>}
        <p className="fdesc">
          本客户端没有 {field.type} 字段的输入方式。这一项填不了 ——
          原样的 requested_schema 在原始事件里。
        </p>
      </div>
    );
  }

  return (
    <div className="field">
      <label htmlFor={id}>
        <span className="fname">{field.title ?? field.name}</span> {spec}
      </label>
      {field.description && <p className="fdesc">{field.description}</p>}
      {field.choices ? (
        <select
          id={id}
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onChange(event.target.value)}
        >
          <option value="">（还没选）</option>
          {field.choices.map((choice) => (
            <option key={choice} value={choice}>{choice}</option>
          ))}
        </select>
      ) : (
        <input
          id={id}
          type="text"
          inputMode={field.type === "string" ? undefined : "decimal"}
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onChange(event.target.value)}
        />
      )}
      {state.state === "bad" && <div className="err">{state.why}</div>}
    </div>
  );
}

function Request({
  request, group, draft, action, onDraft, onAction,
}: {
  request: McpRequest;
  group: string;
  draft: Draft;
  action: Action | undefined;
  onDraft(field: string, value: string | boolean): void;
  onAction(action: Action): void;
}) {
  const mode = mcpModeLabel(request.mode);
  const why = refusal(request, draft);
  return (
    <li className="req">
      <div className="req-h">
        <span className="mono">{request.key}</span>
        <span className="mono dim">mode {request.mode}</span>
        {mode && <span className="dim">{mode}</span>}
      </div>
      <p className="q">{request.message}</p>

      {request.mode === "form" && request.fields.map((field) => (
        <Field
          key={field.name}
          field={field}
          group={group}
          value={draft[field.name]}
          onChange={(next) => onDraft(field.name, next)}
        />
      ))}

      {request.mode === "url" && request.url && (
        <div className="field">
          {/* The address itself, not a word standing in for it. This link
              leaves the app — the shell hands any new window to the real
              browser — and a person is entitled to see where they are going. */}
          <a className="mono" href={request.url} target="_blank" rel="noreferrer">
            {request.url}
          </a>
          <p className="fdesc">
            在浏览器里完成之后再选接受。这台客户端看不到那边发生了什么，
            接受只是把这个答复交回给 Run。
          </p>
          {request.elicitationId && (
            <div className="bind mono">elicitation_id {request.elicitationId}</div>
          )}
        </div>
      )}

      {request.meta != null && (
        <div className="bind mono">meta {JSON.stringify(request.meta)}</div>
      )}

      <fieldset className="opts">
        <legend>怎么回答 {request.key}</legend>
        {MCP_ACTIONS.map((option) => (
          <label className="opt" key={option.action}>
            <input
              type="radio"
              name={`${group}-action`}
              checked={action === option.action}
              disabled={option.action === "accept" && why !== null}
              onChange={() => onAction(option.action)}
            />
            <span>{option.label}</span>
            <span className="mono dim">{option.action}</span>
          </label>
        ))}
      </fieldset>
      {why && <div className="err">现在不能接受：{why}</div>}
    </li>
  );
}

/// The whole answer to one run's pending input request.
///
/// Rendered inside whatever card the surface already drew, so the transcript
/// and the queue can each keep their own heading.
export function McpInputForm({ run }: { run: RunView }) {
  const desk = useDesk();
  const input = run.mcpInput;
  const [actions, setActions] = useState<Record<string, Action>>({});
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  if (!input) return null;

  const ready = input.requests.every((request) => {
    const action = actions[request.key];
    if (!action) return false;
    return action !== "accept" || refusal(request, drafts[request.key] ?? {}) === null;
  });

  const send = async () => {
    if (!ready || sending) return;
    setSending(true);
    setError(null);
    const responses: Record<string, McpResponse> = {};
    for (const request of input.requests) {
      const action = actions[request.key];
      responses[request.key] =
        action === "accept" && request.mode === "form"
          ? { action, content: content(request, drafts[request.key] ?? {}) }
          : { action };
    }
    setError(await desk.answerMcpInput(run.id, responses));
    setSending(false);
  };

  return (
    <div className="ask-input">
      <ol className="reqs">
        {input.requests.map((request) => (
          <Request
            key={request.key}
            request={request}
            group={`${run.id}-${request.key}`}
            draft={drafts[request.key] ?? {}}
            action={actions[request.key]}
            onDraft={(field, value) =>
              setDrafts((was) => ({
                ...was,
                [request.key]: { ...(was[request.key] ?? {}), [field]: value },
              }))}
            onAction={(action) => setActions((was) => ({ ...was, [request.key]: action }))}
          />
        ))}
      </ol>

      <button type="button" className="answer" disabled={!ready || sending} onClick={() => void send()}>
        {sending ? "提交中" : "提交回答"}
      </button>
      {/* Said rather than implied: the button hands the answer to the Run, and
          what the tool call then does is its own event in the log. */}
      <div className="bind">
        {input.requests.length > 1
          ? `${input.requests.length} 个请求要一次答完 —— Runtime 只接受答完整套的回复`
          : "提交之后这个 Run 才会继续，结果看日志里的 mcp.input.resolved"}
      </div>
      <div className="bind mono">
        input {input.inputId.slice(0, 8)}… ・ v{input.inputVersion} ・
        绑定 {input.bindingDigest.slice(0, 16)}… ・ 第 {input.round} 轮 ・
        工具调用 {input.toolCallId}
      </div>
      {error && <div className="err">{error}</div>}
    </div>
  );
}
