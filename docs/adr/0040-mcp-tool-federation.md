# ADR-0040: MCP tool federation

## Status

Accepted. HTTP federation is implemented in the Rust Runtime and credential-free
HTTP is also available in the standalone Host through ADR-0045. Local operator-
trusted stdio MCP is implemented separately by ADR-0046; OAuth onboarding and
pin-aware outbound proxying remain outside this decision.

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

**OpenClaw** now has a first-class consuming Runtime under `src/agents`: stdio,
SSE and Streamable HTTP transports, OAuth, requester-scoped connection
resolution, session Runtime caching and lifecycle disposal. Its mature local
Gateway boundary is useful evidence for transport breadth, but it does not bind
an immutable MCP authority to a cross-Worker Checkpoint.

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
   Discovery happens at Run start. The resulting qualified Tool names and their
   frozen catalog digests enter a dedicated Checkpoint binding set, alongside
   the native Tool catalog digest. Recovery rediscovers and requires an exact
   binding-set match before model work or an old approval can continue. A server whose catalog
   changes mid-Run does not get to change what the Run may do; the call is
   refused rather than dispatched against a catalog nobody approved. Codex
   enforces the same rule.

   Discovery uses at most four in-flight servers per Run. Completion is folded
   back into command order so network timing cannot change model-visible Tool
   order or which duplicate-name conflict wins. The default Worker policy gives
   each server 3 seconds and the whole discovery 10 seconds; a standalone host
   may provide another `McpDiscoveryPolicy`. Completed catalogs survive a total
   deadline while queued and in-flight servers are cancelled and reported.
   RunExecution v10 now decides the effective discovery policy before network
   work (ADR-0041). Checkpoint schema 8 freezes it together with the catalog
   bindings; schema 9 additionally freezes the configured server authority
   (ADR-0045). Recovery under different concurrency or deadline
   semantics is rejected even when the discovered Tool catalog is unchanged;
   a v10 Run cannot resume from a pre-schema-8 Checkpoint that cannot prove the
   complete Runtime policy.

   Every clone of one gateway client also shares a process-local admission
   scheduler (ADR-0042). Its default ceiling is 32 active discovery RPCs across
   Runs. Queued requests rotate by `tenant_id`, so one tenant cannot drain its
   whole queue before another queued tenant receives a slot. Queue wait is
   covered by the Run's total discovery deadline, and cancellation releases the
   slot without waiting for the remote server.

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

- **The multi-tenant cloud Worker does not launch tenant stdio processes.**
  ADR-0046 supports operator-trusted local stdio in the standalone Host, but
  that native trust decision does not transfer to shared Workers without a
  strong sandbox and a signed executable supply chain.
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
- The shared scheduler is implemented in the protocol-neutral discovery path,
  but the current NATS adapter still awaits each Run's discovery inside its
  serial assignment poll. It therefore avoids unbounded concurrency but lets a
  slow discovery delay later assignment polling. An async admission supervisor
  is still required before claiming Worker-level multi-Run throughput.
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
- **Give the Worker plaintext MCP credentials:** rejected. Federation is served
  by the existing Rust Gateway process so sealed credentials stay in the same
  restricted access domain as model-provider credentials; the Worker receives
  only Tool metadata and bounded results.

## References

- ADR-0025 trusted native development Tools
- ADR-0028 tenant provider registry and safe failover
- ADR-0029 signed SkillVersion and trusted Tool activation
- ADR-0036 Seatbelt-contained trusted Tools
- ADR-0039 read-only shell command auto-approval
- ADR-0041 runtime execution policy snapshot
- ADR-0042 shared MCP discovery admission
- ADR-0045 protocol-neutral MCP backend and standalone Host
- ADR-0046 standalone stdio MCP session and process lifecycle
- Codex `codex-rs/codex-mcp/src/binding.rs`, `codex-rs/codex-mcp/src/rmcp_client.rs`
- OpenClaw `src/agents/mcp-transport.ts`, `src/agents/mcp-oauth.ts`,
  `src/agents/agent-bundle-mcp-manager-lifecycle.ts`
