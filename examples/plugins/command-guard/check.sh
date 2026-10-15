#!/bin/sh
# Build the example plugin for wasm32-wasip2 and run the host test that loads
# it (crates/pa-plugins/tests/example_plugin.rs). Needs the target:
#   rustup target add wasm32-wasip2
set -eu
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
cargo test --locked
cargo build --locked --release --target wasm32-wasip2
target=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
wasm="$target/wasm32-wasip2/release/command_guard.wasm"
cd "$here/../../.."
PA_PLUGINS_EXAMPLE_WASM="$wasm" cargo test --locked -p pa-plugins --test example_plugin -- --nocapture
echo "built $wasm"
