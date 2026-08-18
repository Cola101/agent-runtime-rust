#!/usr/bin/env bash
# The desktop client mirrors the daemon's built-in local invocation identity,
# because EventCursor takes that identity as an argument and the socket does
# not serve it. A mirrored constant is a constant that can drift, so this reads
# the real one out of the runtime source and fails if the copy no longer
# matches. It compiles nothing and touches nothing under runtime/.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

python3 - "$repo" <<'PY'
import re, sys, pathlib

repo = pathlib.Path(sys.argv[1])
host = (repo / "runtime/apps/runtime-host/src/lib.rs").read_text()
proto = (repo / "runtime/crates/protocol/src/lib.rs").read_text()
embedded = (repo / "runtime/apps/runtime-host/src/embedded.rs").read_text()
node = (repo / "desktop/shell/electron/localRuntime.cjs").read_text()

def as_uuid(hexdigits):
    raw = hexdigits.replace("_", "").zfill(32)
    return f"{raw[0:8]}-{raw[8:12]}-{raw[12:16]}-{raw[16:20]}-{raw[20:32]}"

consts = {
    name: as_uuid(value)
    for name, value in re.findall(
        r"const\s+(LOCAL_[A-Z_]+)\s*:\s*Uuid\s*=\s*Uuid::from_u128\(0x([0-9a-fA-F_]+)\)", host
    )
}

body = re.search(r"pub const fn local_invocation_context\(\)[^{]*\{(.*?)\n\}", host, re.S)
if not body:
    sys.exit("could not find local_invocation_context() in runtime-host")

expected = {}
for field, value in re.findall(r"(\w+):\s*([^,\n]+),", body.group(1)):
    value = value.strip()
    if value in consts:
        expected[field] = consts[value]
    elif m := re.match(r"Uuid::from_u128\(0x([0-9a-fA-F_]+)\)", value):
        expected[field] = as_uuid(m.group(1))
    elif "RUNTIME_INVOCATION_SCHEMA_VERSION" in value:
        v = re.search(r"RUNTIME_INVOCATION_SCHEMA_VERSION\s*:\s*u32\s*=\s*(\d+)", proto)
        expected[field] = int(v.group(1)) if v else None

block = re.search(r"const LOCAL_INVOCATION = Object\.freeze\(\{(.*?)\}\)", node, re.S)
if not block:
    sys.exit("could not find LOCAL_INVOCATION in the desktop client")
mirrored = {}
for field, value in re.findall(r'(\w+):\s*"?([^",\n]+)"?,', block.group(1)):
    mirrored[field] = int(value) if value.isdigit() else value

# The page ceiling is mirrored too. Asking past it is rejected outright, so a
# drifted copy does not truncate a transcript — it empties it.
cap = re.search(r"RUNTIME_EVENT_CURSOR_MAX_EVENTS\s*:\s*usize\s*=\s*(\d+)", embedded)
expected["__event_cursor_max__"] = int(cap.group(1)) if cap else None
mc = re.search(r"const EVENT_CURSOR_MAX_EVENTS = (\d+)", node)
mirrored["__event_cursor_max__"] = int(mc.group(1)) if mc else None

cursor = re.search(r"RUNTIME_EVENT_CURSOR_SCHEMA_VERSION\s*:\s*u32\s*=\s*(\d+)", embedded)
expected["__event_cursor_schema__"] = int(cursor.group(1)) if cursor else None
mv = re.search(r"const EVENT_CURSOR_SCHEMA_VERSION = (\d+)", node)
mirrored["__event_cursor_schema__"] = int(mv.group(1)) if mv else None

drift = [
    (key, expected[key], mirrored.get(key))
    for key in sorted(expected)
    if mirrored.get(key) != expected[key]
]
if drift:
    for key, want, got in drift:
        print(f"drift: {key}\n  runtime: {want}\n  desktop: {got}", file=sys.stderr)
    sys.exit(f"{len(drift)} mirrored constant(s) no longer match the runtime")
print(f"local invocation identity matches the runtime ({len(expected)} fields)")
PY
