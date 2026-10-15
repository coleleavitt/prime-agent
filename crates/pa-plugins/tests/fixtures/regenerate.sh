#!/bin/sh
# Rebuild the test plugin component from its hand-written core module:
# embed the plugin world's type information, wrap it as a component, and print
# it as text so the fixture stays reviewable (the tests parse the text).
# Needs `wasm-tools` (https://github.com/bytecodealliance/wasm-tools).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
wit="$here/../../wit/plugin.wit"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
wasm-tools parse "$here/fixture-plugin.core.wat" -o "$tmp/core.wasm"
wasm-tools component embed "$wit" --world plugin "$tmp/core.wasm" -o "$tmp/embedded.wasm"
wasm-tools component new "$tmp/embedded.wasm" -o "$tmp/component.wasm"
wasm-tools validate --features component-model "$tmp/component.wasm"
wasm-tools print "$tmp/component.wasm" > "$here/fixture-plugin.wat"
