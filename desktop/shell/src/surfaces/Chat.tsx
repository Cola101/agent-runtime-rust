import { register } from "./registry";

/// A tool call is two lines, not a card.
///
/// Boxing each one puts a border around every third element and the column
/// stops reading as a conversation.
function Act({ verb, target, result }: { verb: string; target: string; result: string }) {
  return (
    <div className="act">
      <b>{verb}</b> {target}
      <span className="out">{result}</span>
    </div>
  );
}

/// Facts the runtime reports about the log itself — retired events, a replaced
/// host, a rollback — drawn where they happened. A hairline and a few words:
/// never hidden, never loud.
function Note({ children }: { children: string }) {
  return (
    <div className="note">
      <span>{children}</span>
    </div>
  );
}

/// The only coloured thing on the screen.
///
/// The decision is bound to one call. There is deliberately no "approve
/// whatever is current" affordance, because that races a transcript that is
/// still moving.
function Gate() {
  return (
    <div className="gate">
      <div className="h">Waiting on you</div>
      <code className="cmd">cargo test -p agent-runtime-host --test embedded_retention</code>
      <ol>
        <li className="pick">
          <span className="k">1</span> Run it
        </li>
        <li>
          <span className="k">2</span> Run it, and stop asking for shell here
        </li>
        <li>
          <span className="k">3</span> No — say what to do instead
        </li>
      </ol>
      <div className="bind">applies to this command only</div>
    </div>
  );
}

function ChatView() {
  return (
    <div className="flow">
      <Note>earlier events retired</Note>

      <div className="ask">
        the retention scan blows its 2s budget under load but passes alone. why?
      </div>

      <div className="rep">
        <p>
          The sweep stats every run directory each pass. Under load that is a thousand
          directories on a contended disk, so the ceiling is I/O rather than logic.
        </p>
      </div>

      <Act verb="Read" target="retention.rs" result="84 lines · maintain_retention at :112" />

      <div className="rep">
        <p>
          It stats every directory even when the manifest already knows the run is
          terminal. Patched to consult the manifest first.
        </p>
      </div>

      <Act verb="Edit" target="retention.rs" result="1 hunk" />

      <Gate />
    </div>
  );
}

register({
  id: "chat",
  label: "Chat",
  group: "work",
  view: ChatView,
  commands: [{ id: "chat:redirect", title: "Redirect the agent" }],
});
