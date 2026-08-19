#!/usr/bin/env bash
# Build the distributable macOS app.
#
# Assembled by hand rather than with `@electron/packager`, and that is a trade
# rather than a preference. On the machine this was built for, the npm registry
# answers metadata but serves tarballs at roughly 800 bytes a second -- a 12 KB
# package took fifteen seconds -- so installing a packager's dependency tree is
# not something that finishes. Everything a packager does for an unsigned local
# build is here: copy the Electron bundle, put the app inside it, rewrite a few
# Info.plist keys, and drop the runtime beside it.
#
# What that costs, stated rather than hidden: the helper processes keep the name
# "Electron Helper" in Activity Monitor, and there is no icon. Both are cosmetic
# and both are what a packager would have handled.
#
# The runtime binary and the proto go in `Contents/Resources`, beside the app
# directory rather than inside it. A file inside an asar archive is not
# executable, and a runtime that cannot be spawned surfaces as one that never
# listened.
#
# Unsigned. This is an Alpha for the machine that built it; signing and
# notarisation are a separate decision with an Apple account attached, and an
# ad-hoc signature would make the bundle look distributable when it is not.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
shell="$repo/desktop/shell"
out="$repo/desktop/dist-app"
app="$out/Runtime Desk.app"
runtime="$repo/runtime/target/release/agent-runtime-host"
trusted_tool="$repo/runtime/target/release/agent-trusted-workspace-tool"
proto="$repo/contracts/proto/runtime.proto"

if [[ "$(uname -sm)" != "Darwin arm64" ]]; then
  echo "this packages an Apple Silicon build and is being run on $(uname -sm)" >&2
  exit 1
fi

echo "==> runtime (release)"
cargo build --manifest-path "$repo/runtime/Cargo.toml" --release -p agent-runtime-host --bin agent-runtime-host
cargo build --manifest-path "$repo/runtime/Cargo.toml" --release -p agent-trusted-workspace-tool
[[ -x "$runtime" ]] || { echo "the release runtime is missing at $runtime" >&2; exit 1; }

echo "==> renderer"
pnpm --filter @agent-runtime/desktop-shell build

echo "==> electron"
electron_app="$(node -p "
  const path = require('node:path');
  path.join(path.dirname(require.resolve('electron', { paths: ['$shell'] })), 'dist', 'Electron.app');
")"
[[ -d "$electron_app" ]] || { echo "no Electron bundle at $electron_app" >&2; exit 1; }

rm -rf "$out"
mkdir -p "$out"
cp -R "$electron_app" "$app"

# The bundle's executable is renamed so the running process is called what the
# app is called; CFBundleExecutable has to move with it or the bundle will not
# launch at all.
mv "$app/Contents/MacOS/Electron" "$app/Contents/MacOS/Runtime Desk"

plist="$app/Contents/Info.plist"
version="$(node -p "require('$shell/package.json').version")"
/usr/libexec/PlistBuddy -c "Set :CFBundleExecutable 'Runtime Desk'" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleName 'Runtime Desk'" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName 'Runtime Desk'" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier dev.agentruntime.desk" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$plist"

echo "==> app"
target="$app/Contents/Resources/app"
mkdir -p "$target"
cp "$shell/package.json" "$target/"
cp -R "$shell/electron" "$target/"
cp -R "$shell/dist" "$target/"

# Only what the host process needs, and all of it. The renderer's dependencies
# are already inside the Vite bundle, and copying the whole tree would ship the
# test runner and the TypeScript compiler; copying only the direct ones ships a
# bundle that fails to load, which is how `@js-sdsl/ordered-map` was found.
node "$here/bundle-deps.mjs" "$shell" "$target/node_modules"

echo "==> resources"
cp "$runtime" "$app/Contents/Resources/agent-runtime-host"
chmod +x "$app/Contents/Resources/agent-runtime-host"
# The trusted workspace tool is its own program, and the bundle shipped without
# it: the app pointed AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN at the host, which
# answers "unsupported command --stdio", so every workspace read and write in
# an installed build failed. It goes beside the host because that is where the
# app looks for it.
cp "$trusted_tool" "$app/Contents/Resources/agent-trusted-workspace-tool"
chmod +x "$app/Contents/Resources/agent-trusted-workspace-tool"
cp "$proto" "$app/Contents/Resources/runtime.proto"

# The three things the app cannot start without. Checked here rather than
# discovered on someone's machine: a bundle missing any one of them launches,
# opens a window, and then reports it has no runtime.
for required in \
  "Contents/Resources/agent-runtime-host" \
  "Contents/Resources/runtime.proto" \
  "Contents/Resources/app/dist/index.html"; do
  [[ -e "$app/$required" ]] || { echo "the bundle is missing $required" >&2; exit 1; }
done
[[ -x "$app/Contents/Resources/agent-trusted-workspace-tool" ]] \
  || { echo "the trusted workspace tool is missing from the bundle" >&2; exit 1; }
[[ -x "$app/Contents/Resources/agent-runtime-host" ]] \
  || { echo "the bundled runtime is not executable" >&2; exit 1; }

# A require that resolves only through the checkout would pass every check above
# and fail on a machine that has no checkout. This asks the bundle to load its
# own host modules from inside itself.
#
# Every module rather than a list. The list was written when `electron/` held
# four files; six more arrived since, and each one that a hand-maintained check
# does not name is a module whose missing dependency this gate would let ship.
# The two exclusions are not omissions: `main.cjs` and `preload.cjs` require
# `electron` itself, which only resolves inside the Electron process, so bare
# node cannot load them and their absence here says nothing about the bundle.
# The two that cannot be loaded are still parsed. Excluding them from the load
# check is right -- `electron` resolves only inside the Electron process -- but
# it left the app's entry point and its only bridge as the two files in the
# bundle nothing looked at. A syntax error in either ships a bundle that opens
# no window at all, and `node --check` finds that without running anything.
for parse_only in "electron/main.cjs" "electron/preload.cjs"; do
  if ! node --check "$app/Contents/Resources/app/$parse_only"; then
    echo "the bundled $parse_only does not parse" >&2
    exit 1
  fi
done

if ! (cd "$app/Contents/Resources/app" && node -e "
  const fs = require('node:fs');
  const inside = fs.readdirSync('./electron')
    .filter((name) => name.endsWith('.cjs'))
    .filter((name) => name !== 'main.cjs' && name !== 'preload.cjs');
  if (inside.length === 0) throw new Error('no host modules were bundled at all');
  for (const name of inside) require('./electron/' + name);
  console.log('host modules loaded: ' + inside.join(', '));
"); then
  echo "the bundled host process cannot resolve its own dependencies" >&2
  exit 1
fi

echo
echo "built: $app"
du -sh "$app" | awk '{print "size: "$1}'
