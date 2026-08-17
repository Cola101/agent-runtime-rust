# Codex MCP 2026 compatibility fixture

This package does not vendor Codex source. The compatibility runner verifies a
clean Codex checkout at one reviewed commit and SHA-256, then the build script
includes that exact upstream fixture from the local checkout into `OUT_DIR`.

Without an explicit, verified source path the package builds a stub that exits
with status 2. Ordinary workspace builds therefore remain self-contained and
cannot be mistaken for cross-project compatibility evidence.

Run from the repository root:

```text
runtime/scripts/test-codex-mcp-2026-compat.sh
```

Set `CODEX_REFERENCE_ROOT` only when the Codex checkout is not available at the
default sibling `agent-source-research/codex` path. Updating the pinned commit
or source digest requires a new source review and evidence update.
