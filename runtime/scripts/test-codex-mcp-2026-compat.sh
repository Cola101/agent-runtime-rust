#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUNTIME_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
REPOSITORY_ROOT=$(CDPATH= cd -- "$RUNTIME_ROOT/.." && pwd)
CODEX_ROOT=${CODEX_REFERENCE_ROOT:-"$REPOSITORY_ROOT/../agent-source-research/codex"}
FIXTURE_RELATIVE_PATH="codex-rs/rmcp-client/src/bin/test_mcp_2026_stdio_server.rs"
EXPECTED_CODEX_REVISION="ff352fab6209dc0f9d13fc0036ed3f9404682b2c"
EXPECTED_FIXTURE_SHA256="02224a4a998359a1e35c15ab489bcb3463dbdd0a0cec23428e8d15f06ec6b3d8"
FIXTURE_SOURCE="$CODEX_ROOT/$FIXTURE_RELATIVE_PATH"

if ! git -C "$CODEX_ROOT" rev-parse --git-dir >/dev/null 2>&1 || [ ! -f "$FIXTURE_SOURCE" ]; then
  echo "Codex reference checkout not found; set CODEX_REFERENCE_ROOT" >&2
  exit 2
fi

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    echo "no SHA-256 utility found (expected shasum or sha256sum)" >&2
    exit 2
  fi
}

verify_reference() {
  ACTUAL_REVISION=$(git -C "$CODEX_ROOT" rev-parse HEAD)
  if [ "$ACTUAL_REVISION" != "$EXPECTED_CODEX_REVISION" ]; then
    echo "Codex reference revision changed: expected $EXPECTED_CODEX_REVISION, got $ACTUAL_REVISION" >&2
    exit 2
  fi

  if [ -n "$(git -C "$CODEX_ROOT" status --porcelain -- "$FIXTURE_RELATIVE_PATH")" ]; then
    echo "Codex MCP fixture source has uncommitted changes" >&2
    exit 2
  fi

  ACTUAL_FIXTURE_SHA256=$(sha256_file "$FIXTURE_SOURCE")
  if [ "$ACTUAL_FIXTURE_SHA256" != "$EXPECTED_FIXTURE_SHA256" ]; then
    echo "Codex MCP fixture digest changed: expected $EXPECTED_FIXTURE_SHA256, got $ACTUAL_FIXTURE_SHA256" >&2
    exit 2
  fi
}

verify_reference

export CARGO_TARGET_DIR="$RUNTIME_ROOT/target"
export CODEX_MCP_2026_FIXTURE_SOURCE="$FIXTURE_SOURCE"
cargo build \
  --manifest-path "$RUNTIME_ROOT/Cargo.toml" \
  --locked \
  -p agent-codex-mcp-2026-compat-fixture
verify_reference

TEST_NAME="codex_mcp_2026_stdio_server_completes_a_recoverable_agent_loop"
if ! TEST_OUTPUT=$(
  CODEX_MCP_2026_STDIO_SERVER="$CARGO_TARGET_DIR/debug/agent-codex-mcp-2026-compat-fixture" \
    CARGO_TERM_COLOR=never \
    cargo test \
      --manifest-path "$RUNTIME_ROOT/Cargo.toml" \
      --locked \
      -p agent-runtime-host \
      --test standalone_run \
      "$TEST_NAME" \
      -- --ignored --exact 2>&1
); then
  printf '%s\n' "$TEST_OUTPUT" >&2
  exit 1
fi
printf '%s\n' "$TEST_OUTPUT"
if ! printf '%s\n' "$TEST_OUTPUT" | grep -F "test $TEST_NAME ... ok" >/dev/null; then
  echo "exact compatibility test did not execute successfully: $TEST_NAME" >&2
  exit 1
fi
if ! printf '%s\n' "$TEST_OUTPUT" | grep -F \
  "test result: ok. 1 passed; 0 failed; 0 ignored;" >/dev/null; then
  echo "unexpected compatibility test summary: $TEST_NAME" >&2
  exit 1
fi
