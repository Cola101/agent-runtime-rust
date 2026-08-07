# ADR-0040: MCP tool federation

## Status

Proposed. No implementation yet; this decides the shape before any is written.

## Context

The platform ships three Tools: `workspace.read_text`, `workspace.write_text`,
`shell.exec`. Codex ships roughly eighteen model-facing ones. That gap is the
single largest reason an Agent here can do less than one there, and it is not a
gap that closes by writing tools one at a time — each new tool is linear work
and a release.

MCP is the lever. It is one interface; once it exists the tool surface is
supplied by an ecosystem rather than by us. That is the whole argument for
doing it before anything else on the capability side.

It is also the first thing this platform has been asked to run that it did not
build. Every Tool so far is a binary we registered, pinned by digest, and
re-validated before every spawn (ADR-0025). An MCP server is third-party code
chosen by a tenant. None of the existing trust machinery applies to it, so the
question is not "how do we call MCP" but "what is the trust boundary, and where
does it sit".

### What the references do, and why neither answer transfers

**Codex** treats MCP servers as user-chosen local configuration and does not
contain them. It tells the server about the sandbox rather than putting the
server inside one (`codex-mcp/src/rmcp_client.rs`, the
`codex/sandbox-state-meta` capability). For a single-user local tool where the
user picked the server and the server runs as that user, that is coherent. It
does not survive multi-tenancy: a server process is shared mutable state, and
"the user chose it" is not a statement anyone can make on behalf of a tenant.

**OpenClaw**'s `src/mcp` is the *serving* side — exposing its own tools as an
MCP server. Its consuming side lives in a skill (`skills/mcporter`), outside the
runtime's own trust decisions.

So neither reference answers the question this platform has to answer, and the
one thing worth taking from Codex is smaller and specific: it re-validates the
tool catalog before dispatching a call, and refuses when the catalog changed
after the call was prepared (`codex-mcp/src/binding.rs:271-275`). That is the
same invariant Checkpoint binding already enforces here, arrived at
independently.

## Decision

1. **v1 federates MCP servers over HTTP only. No local process is spawned.**

   This is the decision everything else follows from. Spawning a tenant-supplied
   MCP server means executing arbitrary third-party code on the Worker, and the
   containment this platform has does not fit it: the Seatbelt profile emits no
   network rule at all (ADR-0036 decision 5), and network access is the entire
   point of most MCP servers. Making that work needs a different containment
   story, and doing it badly is worse than not doing it.

   Over HTTP the third-party code runs on the third party's machine. What
   crosses our boundary is a request and a response, which is a boundary this
   platform already knows how to reason about.

2. **An MCP server is registered like a model Provider, not like a Tool.**
   Tenant-scoped row, endpoint, sealed credential, and the same BYOK envelope
   the Provider registry uses (ADR-0028). The Worker never sees the credential
   in plaintext; the Model Gateway pattern of holding the seal applies unchanged.

   This is the reuse that makes v1 small: the shape already exists, it is
   already multi-tenant, and it already keeps credentials away from the Worker.

3. **Tools are namespaced by server and never collide with native Tools.**
   `mcp:<server>/<tool>`. A federated tool that could be named
   `workspace.write_text` would let a registration silently replace a Tool whose
   safety this platform vouches for.

4. **The catalog is discovered once per Run, frozen, and bound.**
   Discovery happens at Run start. The resulting tool set enters the effective
   Tool catalog digest exactly as native Tools do, so a Checkpoint restore
   recomputes it and refuses on mismatch (ADR-0029). A server whose catalog
   changes mid-Run does not get to change what the Run may do; the call is
   refused rather than dispatched against a catalog nobody approved. Codex
   enforces the same rule.

5. **The three-way intersection is unchanged.**
   Effective Tools stay `Skill declared ∩ Worker trusted ∩ delegated scopes`.
   A federated tool is only reachable if the Skill declares it by qualified
   name, the AgentVersion delegates `tool:mcp:<server>`, and the server is
   registered for that tenant. A Skill still cannot widen anything.

6. **Every federated tool is `ask` and `non_idempotent`.** Its effects are
   unknown by construction — that is what third-party means — so it is
   approval-gated on every call and never auto-retried after an ambiguous
   failure. `AutoApproval` stays `Never` and ADR-0039's exemption does not
   extend here; that exemption rests on knowing the command cannot write, and
   nothing is known about a federated tool.

7. **Server responses are bounded and untrusted input.** Size caps on tool
   results as for native Tools, and the content is passed to the model as data.
   A federated tool result is not permitted to alter instructions, tool
   definitions, or approval state.

8. **Egress is explicit.** A registered server's endpoint is the only host the
   federation client may reach for that server. This keeps the outbound surface
   enumerable per tenant, which is what makes it auditable.

## Consequences

### Positive

- The tool surface stops being a function of how much we write.
- Credentials, tenancy and isolation reuse a path that already exists and is
  already tested, rather than inventing a second one.
- The Worker still executes nothing it did not build.

### Negative

- **Local stdio MCP servers — the most common kind today — are not supported.**
  Most published MCP servers are npm or Python processes meant to run locally.
  Excluding them excludes most of the ecosystem, which is a real cost against
  the reason for doing this at all.
- A federated tool call is a network round trip inside a Run, so a slow or
  unavailable server becomes a slow or failing Run. Timeouts and failure
  classification need to be explicit, not inherited.
- Approving every federated call is the same fatigue ADR-0039 was written to
  fix, and no exemption is available here.

### Neutral

- Discovery-at-start means a server added mid-Run is not visible until the next
  Run. That is the same rule Skills already follow.

## Limitations

- **This ADR does not solve untrusted local code.** It routes around it. Local
  stdio servers need their own decision, and that decision needs a containment
  story that permits network — which is a different profile from ADR-0036's, not
  a parameter of it.
- No decision yet on: OAuth flows for servers that require them (Codex has a
  whole discovery path for this), server health and circuit breaking, per-tool
  rather than per-server scopes, or how a tenant reviews what a server's tools
  actually do before delegating them.
- Cost and rate limiting for federated calls are unaddressed.

## Alternatives Considered

- **Spawn stdio servers under Seatbelt with network allowed:** rejected for v1,
  not forever. It is the honest way to support the real ecosystem, but it means
  designing a second containment profile whose whole purpose is to permit the
  thing the current one forbids, and doing that carelessly undoes ADR-0036.
- **Spawn stdio servers uncontained, as Codex does:** rejected. Coherent for a
  single-user tool, incoherent for a platform where nobody can consent on a
  tenant's behalf.
- **Register each federated tool individually as a trusted Tool:** rejected. It
  reintroduces the per-tool linear work MCP exists to remove.
- **Proxy MCP through the Model Gateway:** considered and deferred. It would put
  the credential in the process that already holds provider seals, which is
  attractive, but the Gateway's job is protocol conversion for models and
  widening it needs its own decision.

## References

- ADR-0025 trusted native development Tools
- ADR-0028 tenant provider registry and safe failover
- ADR-0029 signed SkillVersion and trusted Tool activation
- ADR-0036 Seatbelt-contained trusted Tools
- ADR-0039 read-only shell command auto-approval
- Codex `codex-rs/codex-mcp/src/binding.rs`, `codex-rs/codex-mcp/src/rmcp_client.rs`
- OpenClaw `src/mcp/` (serving side), `skills/mcporter` (consuming side)
