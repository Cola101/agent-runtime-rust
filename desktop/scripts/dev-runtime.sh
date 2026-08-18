#!/usr/bin/env bash
# Bring up a real runtime-host on this machine for the desktop client to talk to.
#
# Real runtime, real durable event log, real approvals. The model is a loopback
# stub — there is no vendor account and no API key here, and the shell says so
# on screen rather than letting scripted text pass for a model's.
#
# Everything it creates lives under desktop/.dev/, which is gitignored.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
dev="$repo/desktop/.dev"
host="$repo/runtime/target/debug/agent-runtime-host"

if [[ ! -x "$host" ]]; then
  echo "runtime-host is not built. Run:" >&2
  echo "  cargo build --manifest-path $repo/runtime/Cargo.toml -p agent-runtime-host --bin agent-runtime-host" >&2
  exit 1
fi

mkdir -p "$dev/state" "$dev/workspace"
: > "$dev/workspace/notes.txt"
printf 'the retention sweep stats every run directory each pass\n' > "$dev/workspace/notes.txt"

# The stub prints the port it bound, so nothing here has to pick one and hope.
node "$here/stub-provider.mjs" 0 > "$dev/provider.port" 2> "$dev/provider.log" &
provider_pid=$!
trap 'kill "$provider_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
  [[ -s "$dev/provider.port" ]] && break
  sleep 0.1
done
port="$(tr -d '[:space:]' < "$dev/provider.port")"
if [[ -z "$port" ]]; then
  echo "stub provider did not report a port; see $dev/provider.log" >&2
  exit 1
fi
echo "stub provider on 127.0.0.1:$port (scripted replies, no vendor)"

# Recorded for the shell, so the status line can say the model is a stub. A
# client that cannot tell a scripted answer from a model's is a client that
# will eventually show one as the other.
cat > "$dev/session.json" <<JSON
{
  "state_root": "$dev/state",
  "workspace_root": "$dev/workspace",
  "provider": { "kind": "stub", "endpoint": "http://127.0.0.1:$port/v1/chat/completions" }
}
JSON

export AGENT_RUNTIME_LOCAL_STATE_ROOT="$dev/state"
export AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT="$dev/workspace"
export AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT="http://127.0.0.1:$port/v1/chat/completions"
export AGENT_RUNTIME_LOCAL_PROVIDER_MODEL="stub"
# Not a credential. The stub ignores it; it exists because the config requires
# the field, and leaving it empty would look like a redacted real one.
export AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY="not-a-credential-stub-provider"
export AGENT_RUNTIME_LOCAL_TOOL_CONSENT="ask"
export AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN="$repo/runtime/target/debug/agent-runtime-host"
export AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES="tool:workspace.read,tool:workspace.write,tool:shell.exec"

echo "state root: $dev/state"
exec "$host" serve
