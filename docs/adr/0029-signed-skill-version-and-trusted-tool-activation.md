# ADR-0029: Signed SkillVersion and trusted Tool activation

## Status

Accepted

## Context

The runtime needs tenant-scoped Skills that can be selected by immutable AgentVersions and
executed on cloud, edge, and native development workers. The native macOS environment must not
introduce Docker, a virtual machine, Kubernetes, or arbitrary tenant code execution.

Codex separates Skill source authority from package/resource identifiers and performs bounded
reads from the environment that owns a Skill. OpenClaw adds deterministic tree digests, session
snapshots, node-hosted Skill constraints, path-bound reads, and static scanning. Neither design by
itself supplies the immutable tenant/application binding required by this control plane.

## Decision

1. A `SkillVersion` is an immutable, application-scoped artifact. Its canonical manifest contains
   its identity, human-readable instructions, supported runtime/platform constraints, and declared
   Tool names. The control plane signs the lowercase SHA-256 artifact digest with a dedicated
   Ed25519 key and records the signing key identifier.
2. An `AgentVersion` binds an ordered list of `SkillVersion` identifiers. Cross-tenant and
   cross-application references fail as not found. The binding cannot change after publication.
3. The scheduler resolves the binding into signed Skill snapshots in `RunExecutionCommand`.
   Workers verify every artifact digest and signature before accepting the assignment.
4. A Skill declaration grants no capability. The Worker intersects its declared Tool names with
   the preinstalled Tool catalog and the AgentVersion delegated scopes. Missing or untrusted Tools
   fail closed. Approval and side-effect policy remain owned by the Tool registry.
5. Skill instructions are appended deterministically to the immutable Agent instructions only
   after signature verification. Checkpoints bind the resulting effective Tool catalog digest.
6. Native development stores metadata in PostgreSQL and transports the signed snapshot inline.
   Production may replace artifact transport with an OCI-backed provider without changing the
   signed manifest or runtime verification contract.
7. `SKILL.md` and `AGENTS.md` are compatibility import formats only. Importing either never grants
   Shell, network, file, MCP, or native execution permission.

## Consequences

### Positive

- Tenant and application isolation is enforced by composite foreign keys and PostgreSQL RLS.
- A compromised message or stale Worker cannot silently modify Skill instructions or Tool
  declarations.
- Native development remains lightweight and only executes platform-installed trusted binaries.
- The storage backend can move from PostgreSQL/inline snapshots to OCI digest references later.

### Negative

- Dynamic arbitrary Skill scripts are deliberately unsupported in native development.
- Key rotation needs an explicit signing-key trust set and immutable historical verification.
- Full OCI publishing, SBOM validation, malware scanning, and public marketplace review remain a
  production control-plane milestone.

### Neutral

- Skill instructions and Tool capabilities are versioned together, so editing either publishes a
  new SkillVersion.

## Failure Modes

- Invalid digest, signature, duplicate Skill identity, unsupported runtime version, or unavailable
  Tool: reject the assignment before Run acceptance.
- Tool implementation changes after registration: existing trusted-native executable revalidation
  rejects execution; checkpoint recovery also rejects Tool catalog drift.
- Missing signing key: control-plane startup fails rather than publishing unsigned Skills.

## Alternatives Considered

- **Load arbitrary workspace `SKILL.md` files:** rejected because filesystem presence is not a
  tenant authorization boundary and local scripts would violate the trusted-Tool-only objective.
- **Run all Skill packages in containers locally:** rejected because native development explicitly
  excludes Docker and the operational cost is unnecessary for trusted preinstalled Tools.
- **Copy Codex or OpenClaw discovery unchanged:** rejected because host-directory precedence and a
  single Gateway/session cache do not provide immutable multi-tenant application bindings.
- **Require OCI locally:** rejected because it adds a registry and image workflow without improving
  the native trusted-Tool execution boundary.

## References

- Codex `codex-rs/ext/skills/src/catalog.rs`
- Codex `codex-rs/ext/skills/src/provider/executor.rs`
- OpenClaw `src/skills/lifecycle/skill-tree-digest.ts`
- OpenClaw `src/skills/runtime/session-snapshot.ts`
- OpenClaw `src/node-host/skills.ts`
