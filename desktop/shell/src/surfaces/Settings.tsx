import { useState } from "react";
import { register } from "./registry";

const SECTIONS = [
  "Connection", "Tools & access", "Models", "MCP servers", "Appearance", "Advanced",
] as const;

/// Only one of these reaches the runtime.
///
/// Saying so on the page matters: a person who changes an appearance setting
/// and a person who changes an approval policy are doing very different
/// things, and nothing in a settings list otherwise distinguishes them.
const TOOLS = [
  { name: "workspace.read_text", effect: "pure", ask: false, contained: true },
  { name: "workspace.write_text", effect: "idempotent", ask: true, contained: true },
  { name: "shell.exec", effect: "unknown", ask: true, contained: true },
  { name: "process.start", effect: "non-idempotent", ask: true, contained: false },
];

function SettingsView() {
  const [section, setSection] = useState<string>("Tools & access");
  return (
    <div className="pane split">
      <nav className="idx">
        {SECTIONS.map((name) => (
          <button
            key={name}
            type="button"
            className={name === section ? "on" : ""}
            onClick={() => setSection(name)}
          >
            {name}
            {name === "Tools & access" && <span className="live">runtime</span>}
          </button>
        ))}
      </nav>
      <div className="body">
        {section === "Tools & access" ? (
          <>
            <table className="rows">
              <thead><tr><th>Tool</th><th>Effect</th><th>Ask first</th><th>Containment</th></tr></thead>
              <tbody>
                {TOOLS.map((tool) => (
                  <tr key={tool.name}>
                    <td className="p">{tool.name}</td>
                    <td>{tool.effect}</td>
                    <td>
                      <span className="seg">
                        <span className={tool.ask ? "on" : ""}>always</span>
                        <span className={tool.ask ? "" : "on"}>never</span>
                      </span>
                    </td>
                    <td className={tool.contained ? "" : "warn"}>
                      {tool.contained ? "enforced" : "partial — no memory limit"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="note">
              These write the runtime's own per-tool policy and are frozen into every run
              started afterwards. A run already in flight keeps the policy it was admitted
              with, so recovery replays the same decisions.
            </p>
          </>
        ) : (
          <div className="empty">
            {section} — not wired yet. See <code>docs/desktop-ui-gap.md</code>.
          </div>
        )}
      </div>
    </div>
  );
}

register({
  id: "settings",
  label: "Settings",
  group: "setup",
  view: SettingsView,
  toolbar: () => (<><b>Settings</b><span className="tb-r">esc back to chat</span></>),
  commands: [
    { id: "settings:permissions", title: "/permissions", hint: "per-tool approval policy" },
    { id: "settings:models", title: "/models", hint: "routing and failover" },
    { id: "settings:mcp", title: "/mcp", hint: "servers and credentials" },
  ],
});
