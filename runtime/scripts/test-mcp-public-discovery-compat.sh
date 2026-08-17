#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUNTIME_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
TEST_NAME="discovery_works_against_every_configured_server"
CONTEXT7_ENDPOINT="https://mcp.context7.com/mcp"
MICROSOFT_LEARN_ENDPOINT="https://learn.microsoft.com/api/mcp"
COMPAT_ENDPOINTS="$CONTEXT7_ENDPOINT,$MICROSOFT_LEARN_ENDPOINT"

for command_name in cargo grep; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "required command is unavailable: $command_name" >&2
    exit 2
  fi
done

# This gate is discovery-only. Remove every credential variable understood by
# the external compatibility test so a local login cannot make it pass.
unset AGENT_RUNTIME_MCP_COMPAT_ENDPOINT
unset AGENT_RUNTIME_MCP_COMPAT_AUTH_ENDPOINT
unset AGENT_RUNTIME_MCP_COMPAT_BEARER

if ! TEST_OUTPUT=$(
  AGENT_RUNTIME_MCP_COMPAT_ENDPOINTS="$COMPAT_ENDPOINTS" \
    CARGO_TERM_COLOR=never \
    cargo test \
      --manifest-path "$RUNTIME_ROOT/Cargo.toml" \
      --locked \
      -p agent-model-gateway \
      --test mcp_real_server_compat \
      "$TEST_NAME" \
      -- --ignored --exact --nocapture 2>&1
); then
  printf '%s\n' "$TEST_OUTPUT" >&2
  exit 1
fi
printf '%s\n' "$TEST_OUTPUT"

if ! printf '%s\n' "$TEST_OUTPUT" | grep -F "test $TEST_NAME ... ok" >/dev/null; then
  echo "exact public compatibility test did not execute successfully" >&2
  exit 1
fi
if ! printf '%s\n' "$TEST_OUTPUT" | grep -F \
  "test result: ok. 1 passed; 0 failed; 0 ignored;" >/dev/null; then
  echo "unexpected public compatibility test summary" >&2
  exit 1
fi
for endpoint in "$CONTEXT7_ENDPOINT" "$MICROSOFT_LEARN_ENDPOINT"; do
  if ! printf '%s\n' "$TEST_OUTPUT" | grep -F "compat: $endpoint ->" >/dev/null; then
    echo "public MCP endpoint did not produce a non-empty catalog: $endpoint" >&2
    exit 1
  fi
done
