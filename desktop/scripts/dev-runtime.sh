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
# Still "ask" by default: approvals are half of what this client is for.
#
# Worth knowing before you try to walk a process session from the window: the
# local adapter derives one control-command id per (run, "approve"), so the
# second Approve on the same Run replays the first receipt instead of deciding
# the new question. Five of the eight process.* Tools are Ask, so a session
# parks again after every one of them and only the first can be granted from
# here. Set AGENT_RUNTIME_LOCAL_TOOL_CONSENT=allow-once to see a whole session.
export AGENT_RUNTIME_LOCAL_TOOL_CONSENT="${AGENT_RUNTIME_LOCAL_TOOL_CONSENT:-ask}"
export AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN="$repo/runtime/target/debug/agent-runtime-host"
scopes="tool:workspace.read,tool:workspace.write,tool:shell.exec"

# The eight process.* Tools — durable process sessions, PTY included — exist
# only when the host is told which program a session runs. Nothing has a
# default here: `process.start` takes no command, so this one line *is* the
# program, and picking one silently would mean the runtime spawned something
# nobody chose. `__pty-session-supervisor` is wired by the host itself from its
# own executable, so a PTY needs nothing further here.
#
# The registered executable must be a regular file with an execute bit and not
# a symlink (`TrustedNativeExecutor::new`), which is why `/bin/sh` is checked
# rather than assumed: it is a real file on macOS and usually a symlink to dash
# on Linux. A host that cannot satisfy that still starts — the client's 进程会话
# surface then says no process.* call was ever recorded, which is true.
session_program="${AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE:-/bin/sh}"
if [[ -f "$session_program" && ! -L "$session_program" && -x "$session_program" ]]; then
  export AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE="$session_program"
  scopes="$scopes,tool:process.session"
  echo "process sessions: $session_program"
else
  unset AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE
  echo "process sessions off: $session_program is not a regular executable file" >&2
  echo "  (the runtime refuses symlinks; set AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE to one)" >&2
fi

export AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES="$scopes"

echo "state root: $dev/state"
exec "$host" serve
