#!/usr/bin/env bash
#
# Build the `code` CLI binary AND the standalone Claude proxy JS bundle.
#
# 1. Compiles the Rust CLI (cli/ crate, ~10s debug / ~30-60s release).
# 2. Bundles the TypeScript proxy into a single portable JS file
#    (out/vs/platform/agentHost/node/claudeProxyBundle.js, ~15ms via esbuild).
#
# The bundle embeds all VS Code module dependencies and only externalises
# native addons. It does NOT require a full `npm run transpile-client`.
#
# One-time prereqs:
#   Linux:   sudo apt-get install -y build-essential pkg-config libssl-dev
#   macOS:   brew install openssl pkg-config
#   All:     npm ci --ignore-scripts   # installs node_modules
#            cd build && npm ci && cd ..  # installs esbuild etc.
#
# Usage:
#   ./build-code.sh                  # debug build + bundle
#   ./build-code.sh --release        # release build + bundle
#   ./build-code.sh 8123             # debug build + run proxy on TCP port 8123
#   ./build-code.sh --release 8123   # release build + run
#   ./build-code.sh /tmp/p           # ... or on a unix socket path
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"   # repo root (parent of cli/)

command -v cargo >/dev/null 2>&1 || export PATH="$HOME/.cargo/bin:$PATH"

release_flag=""
profile="debug"
run_target=""
for arg in "$@"; do
    if [[ "$arg" == "--release" ]]; then
        release_flag="--release"
        profile="release"
    else
        run_target="$arg"
    fi
done

# --- 1. Rust CLI ---------------------------------------------------------------
( cd "$here" && cargo build --bin code $release_flag )
exe="$here/target/$profile/code"
echo "Built: $exe"

# --- 2. JS bundle --------------------------------------------------------------
esbuild="$root/build/node_modules/.bin/esbuild"
entry="$root/src/vs/platform/agentHost/node/claudeProxyMain.ts"
outfile="$root/out/vs/platform/agentHost/node/claudeProxyBundle.js"

if [[ ! -x "$esbuild" ]]; then
    echo "Warning: esbuild not found at $esbuild. Run: cd build && npm ci" >&2
else
    "$esbuild" "$entry" \
        --bundle \
        --platform=node \
        --format=esm \
        "--outfile=$outfile" \
        --external:@vscode/deviceid \
        --external:@vscode/windows-registry
    echo "Bundle: $outfile"
fi

# --- 3. dist/ ------------------------------------------------------------------
# Copy binary + bundle into dist/ so `code` finds `claudeProxyBundle.js`
# next to itself without any env vars.
dist="$here/dist"
mkdir -p "$dist"
cp -f "$exe"    "$dist/code"
[[ -f "$outfile" ]] && cp -f "$outfile" "$dist/claudeProxyBundle.js"
echo "dist/: $dist"

# --- 4. Optional run -----------------------------------------------------------
if [[ -n "${run_target:-}" ]]; then
    echo "Running: code agent proxy claude $run_target"
    "$dist/code" agent proxy claude "$run_target"
fi
