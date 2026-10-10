# anthropic-rs, vendored

Pinned source of `anthropic` 0.1.0 (MIT OR Apache-2.0), the shared Anthropic auth SDK from
`git@github.com:coleleavitt/anthropic-rs.git` at commit `50cb3606950de7a1fd36048e9e9aead7f0c8e26f`
(2026-10-10, "keepalive: a pass with nothing due takes no store lock", on the `refresh-commit-recovery` branch,
after 449f30a). Only the crate is
vendored (`Cargo.toml`, `README.md`, `PORTING.md`, `docs/`, `src/`, from `git archive`); the `anthropic-napi`
workspace member and the upstream `Cargo.lock` are not. `crates/pa-anthropic-auth` is its only user.

## Why vendored

`deny.toml` admits crates.io and vendored path dependencies only (`allow-git = []`). The repository is not
anonymously reachable (`git ls-remote https://github.com/coleleavitt/anthropic-rs` asks for credentials), so a
pinned git dependency would break CI and every other clone, and a path outside this repository would too.
Publishing to crates.io is the owner's call. When it is published, replace this directory with the registry
dependency and drop the `exclude` entry in the workspace manifest.

## Source changes

Formatting only: nine files (`access.rs`, `credentials/link.rs`, `credentials/mod.rs`, `device/mod.rs`, `legacy.rs`,
`lib.rs`, `quota_manager.rs`, `shaping.rs`, `store.rs`) are reformatted with rustfmt's defaults, because the
workspace's `cargo fmt --all --check` covers local path dependencies and upstream is formatted with a
`HorizontalVertical` import layout. The changes are import-list layout; no token changes.

The manifest differs in two places:

1. `[workspace]` is empty (upstream lists `anthropic-napi`, which is not vendored). The prime-agent workspace
   excludes this directory, so the workspace lint gates do not apply to third-party code.
2. `reqwest` is `0.12` with `rustls-tls` (ring, webpki roots) instead of `0.13` with `rustls`. reqwest 0.13's
   `rustls` feature selects aws-lc-rs, whose `aws-lc-sys` carries the OpenSSL licence `deny.toml` does not allow
   and adds a C build to every target; 0.12 + `rustls-tls` is the stack the rest of prime-agent already ships.
   The crate uses only the API both versions share (`Client`, `ClientBuilder`, `header`, JSON and streaming
   bodies), and it builds unchanged against it.

## Updating

```sh
git -C <anthropic-rs checkout> archive <commit> -- Cargo.toml README.md PORTING.md docs src \
  | tar -x -C vendor/anthropic
```

then reapply the two manifest changes above, run
`rustfmt --edition 2024 --config-path <an empty rustfmt.toml>` over `vendor/anthropic/src/**/*.rs`, update the
commit in this file, and run the pa-anthropic-auth tests
and `cargo deny --all-features --workspace check advisories licenses`.
