#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUNTIME_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
COMPAT_ROOT="$RUNTIME_ROOT/compat/mcp-server-everything-http"
EXPECTED_SERVER_VERSION="2026.7.4"
EXPECTED_SDK_VERSION="1.30.0"
EXPECTED_LOCK_SHA256="23e4f0ecd182015ac85c35721acaef30828518c50597cd18be3a8df2c6a8e5aa"
TMP_BASE=${TMPDIR:-/tmp}
TMP_BASE=${TMP_BASE%/}
TEMP_ROOT=$(mktemp -d "$TMP_BASE/agent-runtime-mcp-http.XXXXXX")
INSTALL_ROOT="$TEMP_ROOT/server"
SERVER_ENTRY="$INSTALL_ROOT/node_modules/@modelcontextprotocol/server-everything/dist/index.js"
SERVER_PID=

cleanup() {
  cleanup_status=$?
  trap - EXIT INT TERM HUP

  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    server_command=$(ps -p "$SERVER_PID" -o command= 2>/dev/null || true)
    case "$server_command" in
      *"$SERVER_ENTRY"*)
        kill -INT "$SERVER_PID" 2>/dev/null || true
        shutdown_attempt=0
        while kill -0 "$SERVER_PID" 2>/dev/null && [ "$shutdown_attempt" -lt 50 ]; do
          shutdown_attempt=$((shutdown_attempt + 1))
          sleep 0.05
        done
        if kill -0 "$SERVER_PID" 2>/dev/null; then
          kill -KILL "$SERVER_PID" 2>/dev/null || true
        fi
        wait "$SERVER_PID" 2>/dev/null || true
        ;;
      *)
        echo "refusing to stop PID $SERVER_PID because its identity changed" >&2
        cleanup_status=1
        ;;
    esac
  fi

  if [ "$cleanup_status" -ne 0 ] && [ -f "$TEMP_ROOT/server.stderr" ]; then
    echo "official MCP server stderr:" >&2
    tail -80 "$TEMP_ROOT/server.stderr" >&2 || true
  fi

  case "$TEMP_ROOT" in
    "$TMP_BASE"/agent-runtime-mcp-http.*)
      if [ -x /usr/bin/trash ]; then
        /usr/bin/trash "$TEMP_ROOT"
      else
        /bin/rm -rf "$TEMP_ROOT"
      fi
      ;;
    *)
      echo "refusing to remove unexpected temporary path: $TEMP_ROOT" >&2
      cleanup_status=1
      ;;
  esac

  exit "$cleanup_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

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

for command_name in cargo curl node npm; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "required command is unavailable: $command_name" >&2
    exit 2
  fi
done

actual_lock_sha256=$(sha256_file "$COMPAT_ROOT/package-lock.json")
if [ "$actual_lock_sha256" != "$EXPECTED_LOCK_SHA256" ]; then
  echo "MCP compatibility lock changed: expected $EXPECTED_LOCK_SHA256, got $actual_lock_sha256" >&2
  exit 2
fi

mkdir -p "$INSTALL_ROOT" "$TEMP_ROOT/home" "$TEMP_ROOT/tmp"
cp "$COMPAT_ROOT/package.json" "$COMPAT_ROOT/package-lock.json" "$INSTALL_ROOT/"
copied_lock_sha256=$(sha256_file "$INSTALL_ROOT/package-lock.json")
if [ "$copied_lock_sha256" != "$EXPECTED_LOCK_SHA256" ]; then
  echo "copied MCP compatibility lock does not match the reviewed digest" >&2
  exit 2
fi
env -i \
  PATH="$PATH" \
  HOME="$TEMP_ROOT/home" \
  TMPDIR="$TEMP_ROOT/tmp" \
  npm_config_cache="$TEMP_ROOT/npm-cache" \
  npm_config_userconfig=/dev/null \
  npm_config_update_notifier=false \
  npm ci \
    --prefix "$INSTALL_ROOT" \
    --omit=dev \
    --ignore-scripts \
    --no-audit \
    --no-fund \
    --loglevel=error

installed_server_version=$(
  node -p "require('$INSTALL_ROOT/node_modules/@modelcontextprotocol/server-everything/package.json').version"
)
if [ "$installed_server_version" != "$EXPECTED_SERVER_VERSION" ]; then
  echo "official MCP server version changed: expected $EXPECTED_SERVER_VERSION, got $installed_server_version" >&2
  exit 2
fi
installed_sdk_version=$(
  node -p "require('$INSTALL_ROOT/node_modules/@modelcontextprotocol/sdk/package.json').version"
)
if [ "$installed_sdk_version" != "$EXPECTED_SDK_VERSION" ]; then
  echo "official MCP SDK version changed: expected $EXPECTED_SDK_VERSION, got $installed_sdk_version" >&2
  exit 2
fi

COMPAT_PORT=${AGENT_RUNTIME_MCP_COMPAT_PORT:-$(
  node -e 'const s=require("node:net").createServer();s.listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})'
)}
case "$COMPAT_PORT" in
  ''|*[!0-9]*)
    echo "AGENT_RUNTIME_MCP_COMPAT_PORT must be a numeric TCP port" >&2
    exit 2
    ;;
esac
if [ "$COMPAT_PORT" -lt 1 ] || [ "$COMPAT_PORT" -gt 65535 ]; then
  echo "AGENT_RUNTIME_MCP_COMPAT_PORT must be between 1 and 65535" >&2
  exit 2
fi

(
  cd "$INSTALL_ROOT"
  exec env -i \
    PATH="$PATH" \
    HOME="$TEMP_ROOT/home" \
    TMPDIR="$TEMP_ROOT/tmp" \
    PORT="$COMPAT_PORT" \
    node "$SERVER_ENTRY" streamableHttp \
    >"$TEMP_ROOT/server.stdout" 2>"$TEMP_ROOT/server.stderr"
) &
SERVER_PID=$!
COMPAT_ENDPOINT="http://127.0.0.1:$COMPAT_PORT/mcp"

ready=false
attempt=0
while [ "$attempt" -lt 100 ]; do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "official MCP server exited before readiness" >&2
    exit 1
  fi
  if curl --silent --max-time 1 --output /dev/null "$COMPAT_ENDPOINT" 2>/dev/null; then
    ready=true
    break
  fi
  attempt=$((attempt + 1))
  sleep 0.05
done
if [ "$ready" != true ]; then
  echo "official MCP server did not become ready" >&2
  exit 1
fi

run_exact_test() {
  package_name=$1
  test_target=$2
  test_name=$3
  if ! test_output=$(
    AGENT_RUNTIME_MCP_COMPAT_ENDPOINT="$COMPAT_ENDPOINT" \
      CARGO_TERM_COLOR=never \
      cargo test \
        --manifest-path "$RUNTIME_ROOT/Cargo.toml" \
        --locked \
        -p "$package_name" \
        --test "$test_target" \
        "$test_name" \
        -- --ignored --exact 2>&1
  ); then
    printf '%s\n' "$test_output" >&2
    exit 1
  fi
  printf '%s\n' "$test_output"
  if ! printf '%s\n' "$test_output" | grep -F "test $test_name ... ok" >/dev/null; then
    echo "exact compatibility test did not execute successfully: $test_name" >&2
    exit 1
  fi
  if ! printf '%s\n' "$test_output" | grep -F \
    "test result: ok. 1 passed; 0 failed; 0 ignored;" >/dev/null; then
    echo "unexpected compatibility test summary: $test_name" >&2
    exit 1
  fi
}

run_exact_test \
  agent-model-gateway \
  mcp_real_server_compat \
  discovery_works_against_the_reference_server
run_exact_test \
  agent-model-gateway \
  mcp_real_server_compat \
  a_tool_call_round_trips_against_the_reference_server
run_exact_test \
  agent-runtime-host \
  standalone_run \
  official_streamable_http_server_completes_an_agent_loop

printf 'verified @modelcontextprotocol/server-everything@%s with sdk@%s at %s\n' \
  "$installed_server_version" "$installed_sdk_version" "$COMPAT_ENDPOINT"
