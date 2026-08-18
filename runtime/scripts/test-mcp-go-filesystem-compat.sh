#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
RUNTIME_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
SOURCE_URL="https://github.com/mark3labs/mcp-filesystem-server.git"
SOURCE_TAG="v0.11.1"
EXPECTED_REVISION="5646396f50ba144b9dd1ca9d088db0ac08cab3f8"
EXPECTED_TREE="8dcf90035679d3f7a9ed509f941efdd36d9abe85"
EXPECTED_GO_MOD_SHA256="f967edd0f15e9cfa53bf7cd2eb5b3fd5290463c65faf3bbdf6bfc944d0453c7c"
EXPECTED_GO_SUM_SHA256="f869f93873eb5bc27309b948581d6c205cca7ca7b7a7f69909c598106676dbe1"
EXPECTED_MCP_GO_VERSION="v0.32.0"
TEST_NAME="mcp_go_filesystem_server_completes_an_agent_loop"
TMP_BASE=${TMPDIR:-/tmp}
TMP_BASE=${TMP_BASE%/}
TEMP_ROOT=$(mktemp -d "$TMP_BASE/agent-runtime-mcp-go.XXXXXX")
SOURCE_ROOT="$TEMP_ROOT/source"
SERVER_BINARY="$TEMP_ROOT/bin/mcp-filesystem-server"

cleanup() {
  cleanup_status=$?
  trap - EXIT INT TERM HUP
  case "$TEMP_ROOT" in
    "$TMP_BASE"/agent-runtime-mcp-go.*)
      # Downloaded Go modules are intentionally read-only. Restore owner write
      # permission, then delete only the guarded mktemp tree; moving it to the
      # macOS Trash would retain the entire build cache on disk.
      chmod -R u+w "$TEMP_ROOT" 2>/dev/null || cleanup_status=1
      find "$TEMP_ROOT" -depth -delete 2>/dev/null || cleanup_status=1
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

for command_name in cargo chmod find git go grep; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "required command is unavailable: $command_name" >&2
    exit 2
  fi
done

git -c http.https://github.com.proxy= clone \
  --quiet \
  --depth 1 \
  --branch "$SOURCE_TAG" \
  "$SOURCE_URL" \
  "$SOURCE_ROOT"

actual_revision=$(git -C "$SOURCE_ROOT" rev-parse HEAD)
if [ "$actual_revision" != "$EXPECTED_REVISION" ]; then
  echo "mcp-go Server revision changed: expected $EXPECTED_REVISION, got $actual_revision" >&2
  exit 2
fi
actual_tree=$(git -C "$SOURCE_ROOT" rev-parse HEAD^{tree})
if [ "$actual_tree" != "$EXPECTED_TREE" ]; then
  echo "mcp-go Server tree changed: expected $EXPECTED_TREE, got $actual_tree" >&2
  exit 2
fi
if [ -n "$(git -C "$SOURCE_ROOT" status --porcelain)" ]; then
  echo "mcp-go Server checkout is not clean" >&2
  exit 2
fi

actual_go_mod_sha256=$(sha256_file "$SOURCE_ROOT/go.mod")
if [ "$actual_go_mod_sha256" != "$EXPECTED_GO_MOD_SHA256" ]; then
  echo "mcp-go Server go.mod changed: expected $EXPECTED_GO_MOD_SHA256, got $actual_go_mod_sha256" >&2
  exit 2
fi
actual_go_sum_sha256=$(sha256_file "$SOURCE_ROOT/go.sum")
if [ "$actual_go_sum_sha256" != "$EXPECTED_GO_SUM_SHA256" ]; then
  echo "mcp-go Server go.sum changed: expected $EXPECTED_GO_SUM_SHA256, got $actual_go_sum_sha256" >&2
  exit 2
fi
if ! grep -F "github.com/mark3labs/mcp-go $EXPECTED_MCP_GO_VERSION" \
  "$SOURCE_ROOT/go.mod" >/dev/null; then
  echo "mcp-go dependency is not pinned to $EXPECTED_MCP_GO_VERSION" >&2
  exit 2
fi

mkdir -p \
  "$TEMP_ROOT/bin" \
  "$TEMP_ROOT/home" \
  "$TEMP_ROOT/gopath" \
  "$TEMP_ROOT/gomodcache" \
  "$TEMP_ROOT/gocache" \
  "$TEMP_ROOT/tmp"
(
  cd "$SOURCE_ROOT"
  env -i \
    PATH="$PATH" \
    HOME="$TEMP_ROOT/home" \
    TMPDIR="$TEMP_ROOT/tmp" \
    GOPATH="$TEMP_ROOT/gopath" \
    GOMODCACHE="$TEMP_ROOT/gomodcache" \
    GOCACHE="$TEMP_ROOT/gocache" \
    GOPROXY="https://proxy.golang.org,direct" \
    GOSUMDB="sum.golang.org" \
    GOTOOLCHAIN="local" \
    CGO_ENABLED=0 \
    go build -mod=readonly -trimpath -o "$SERVER_BINARY" .
)
if [ -n "$(git -C "$SOURCE_ROOT" status --porcelain)" ]; then
  echo "building the mcp-go Server modified the pinned checkout" >&2
  exit 2
fi

if ! test_output=$(
  MCP_GO_FILESYSTEM_SERVER="$SERVER_BINARY" \
    CARGO_TERM_COLOR=never \
    cargo test \
      --manifest-path "$RUNTIME_ROOT/Cargo.toml" \
      --locked \
      -p agent-runtime-host \
      --test standalone_run \
      "$TEST_NAME" \
      -- --ignored --exact 2>&1
); then
  printf '%s\n' "$test_output" >&2
  exit 1
fi
printf '%s\n' "$test_output"
if ! printf '%s\n' "$test_output" | grep -F "test $TEST_NAME ... ok" >/dev/null; then
  echo "exact mcp-go compatibility test did not execute successfully" >&2
  exit 1
fi
if ! printf '%s\n' "$test_output" | grep -F \
  "test result: ok. 1 passed; 0 failed; 0 ignored;" >/dev/null; then
  echo "unexpected mcp-go compatibility test summary" >&2
  exit 1
fi

printf 'verified mark3labs/mcp-filesystem-server@%s (%s) with mcp-go@%s\n' \
  "$SOURCE_TAG" "$EXPECTED_REVISION" "$EXPECTED_MCP_GO_VERSION"
