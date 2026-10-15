# command-guard: an example Prime Agent plugin

A WebAssembly component, written in Rust, that guards the shell commands the agent runs in its Python kernel. The
plugin contract is `crates/pa-plugins/wit/plugin.wit`; the design is in `docs/hooks-and-plugins.md`.

What it does:

- Refuses `rm -rf` aimed at `/`, `~` or `$HOME`, and a download piped into a shell.
- Asks you before `git push` and `sudo` (the TUI opens a yes/no prompt; print mode refuses).
- Counts the commands it saw, asked about, refused, and the ones that failed, in the panel under the chat editor.
- Offers the model a `check` tool: `await wasm_plugins.call("guard", "check", command="...")`.
- Offers you `/wasm guard stats`.

## Build and install

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp "$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)/wasm32-wasip2/release/command_guard.wasm" \
   ~/.prime/agent/plugins/guard.wasm
```

The `wasm32-wasip2` target links a component directly; no `cargo component` or `wasm-tools` step is needed. New
sessions load it; `/wasm` lists it.

## Check

`./check.sh` runs the rule tests, builds the component, and runs the agent's host test that loads it
(`crates/pa-plugins/tests/example_plugin.rs`).

## Writing your own

- `wit_bindgen::generate!` over the contract generates the `Guest` trait (one method per export) and the `host`
  imports (`log`, `set-status`); `export!` registers your implementation.
- An instance serves one session. Keep per-session state in a `thread_local!`; a trapped instance is replaced by a
  fresh one, which sees `on-session-start` again.
- Every call runs under a deadline (2 s by default) and a memory cap (64 MiB); WASI grants nothing unless a
  `guard.toml` beside the `.wasm` grants directories or environment variables.
- Dependencies here are pinned to releases at least 7 days old, as the repository requires.
