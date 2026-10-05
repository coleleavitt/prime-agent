# pa-anthropic-auth

Anthropic subscription auth through the shared account store: prime-agent's `anthropic` provider takes its OAuth
credential from `~/.anthropic-accounts/accounts.json`, the store the fork's opencode and pi plugins use (through
anthropic-napi), by way of the same Rust SDK (`vendor/anthropic`, anthropic-rs pinned; see its
`PRIME_AGENT_PATCH.md`). One login serves every tool on the machine, and one refresh protocol rotates it.

Wired by `pa-cli` behind `feature = "anthropic-auth"` (default on). `--no-default-features` installs nothing:
auth.json resolves the `anthropic` provider exactly as before.

## Scope

- `SharedStoreSource`: `pa_core::auth::ProviderCredentialSource` over the store.
  - `status()` (cheap, offline, memoized on the file's size and mtime): a login exists when the store has an enabled
    OAuth row with an inference scope. The label is `shared account store`; the revision is a hash of the
    candidates' ids and access tokens, so a rotation clears a stale mark.
  - `credential()`: `anthropic::access::get_access_token` (the plugins' `getAccessToken`): routing order (`current`
    first), the Claude Code login link (its live token borrowed, its newer login adopted, a rotation published to
    it), a refresh claimed through the store (OS file lock, a lease the TS consumers honour, a compare-and-swap that
    never overwrites a newer rotation), rotation past a dead login. Requests in this process take one flight lock,
    so a concurrent request reads the first one's rotation from the store.
  - No login in the store: the lookup falls through to auth.json. A login that cannot produce a token (refresh
    failed, revoked, store unreadable, network down): the provider's OAuth authentication failure
    (`oauth_refresh_failed`, "Run /login"), never auth.json's login in its place.
- Request hooks (`pa_ai::request_hooks`, for the `anthropic` provider id): only for an access token this source
  served (remembered, the latest 64, as the pi plugin remembers them), never a runtime key.
  - `current_credential`: each request carries the store's token for it now (`get_access_token`, under the
    in-process flight lock), so a token another process rotated since the session resolved it is replaced before
    the send.
  - `rejected` after a 401 (once per request, pa-ai's rule): anthropic-napi's `handleUnauthorized`
    (`recover_unauthorized`: one claimed refresh of the row owning the rejected token; a retry only with a new
    version of the same login), and, when the store no longer holds the rejected token (another process rotated
    it), the store's current token re-read under its lock if it differs. Otherwise the 401 is reported.
- Keep-alive, on the crate's own thread (`anthropic-keepalive`), started by the first served credential (never at
  install, on a paint path or during startup); its first pass a minute later, then every ten minutes plus up to a
  minute of jitter (the opencode plugin's tick): the SDK's machine-wide `keep_alive_once` (idle logins whose
  refresh token nears its expiry, one process per machine behind the store's lease), then every login this process
  served within the hour whose access token expires within 20 minutes is refreshed ahead (`refresh_shared`,
  claimed), so no request waits on that refresh. Off in `SharedStoreConfig::isolated` (`background: false`).
- Migration (`adopt_stored_login`, the custody half of anthropic-napi's `importOAuthAccount`, as the pi plugin
  moves its host's refresh token): an Anthropic OAuth login `auth.json` still holds is moved into the store on the
  first lookup. A live login is identified at the profile endpoint (an expired one is never refreshed to find out);
  a row already holding the token, or a login of the same account (account and organization), wins and the import
  is discarded; otherwise it becomes a new row named like napi's (email, org-qualified on collision; else the account
  uuid; else `account-<8 hex>`), `current` when nothing is pinned. Either way pa-core then removes `auth.json`'s
  entry (only while it still holds that token), so the store is the login's only custodian. A malformed login or
  an unusable store leaves it in `auth.json`.
- Logout (`remove_login`, the plugins' account removal, napi `removeAccount`): `/logout anthropic` removes the
  row the provider is served from now (the routing order's first candidate; while every login cools down, the
  pinned one or the first) under the store lock. Nothing is revoked at Anthropic and Claude Code's own login is
  left alone; the notice names the store and how many logins still serve the provider.
- `install()`: installs the process's source (`shared_source()`, configured from the environment: the store path
  overrides `ANTHROPIC_ACCOUNTS_FILE` / `ANTHROPIC_ACCOUNTS_DIR`, the `ANTHROPIC_OAUTH_*` endpoint overrides,
  `ANTHROPIC_NATIVE_PUBLISH`) for the `anthropic` provider id. No I/O.
- `SharedStoreSource::store_login(NewLogin)`: the custody half of anthropic-napi's `completeLogin`. pa-cli's
  `/login anthropic` runs the native browser flow (callback server raced against the paste, unchanged UX) and hands
  the tokens here instead of auth.json: the account is identified at the profile endpoint (best effort), merged into
  the row holding the same login (else a new row named after the email), made `current`, and, when Claude Code is
  logged into the same account (whose login this one revokes), published to Claude Code.
- `AnthropicAuthFeature`: a `SessionFeature` that reports adoption once per process.

## Non-goals (here)

- Account management beyond logout (enable, disable, reorder, pin, remote revoke): the plugins' account commands
  own it; prime-agent has no account command surface.
- Quota reads, quota-reserve routing, rotation on 429.
- The request shape (headers, betas, system prompt, tool names): pa-ai's Claude Code mode owns it for every
  `sk-ant-oat` token, whatever its source. The source adds no headers.

## Public API

`install`, `shared_source`, `PROVIDER_ID`, `SharedStoreSource` (`new`, `store_path`, `usage`, `store_login`),
`SharedStoreConfig` (`from_env`, `isolated`; its `background` field runs the keep-alive thread), `NewLogin`, `StoredLogin` (`claude_code_notice`), `SourceUsage`, `STORE_LABEL`, `AnthropicAuthFeature`, `TELEMETRY_EVENT`.

## Seams

- `pa_core::auth::install_credential_source` (the provider credential source seam: credential, custody of
  auth.json's login, logout).
- `pa_ai::request_hooks::install_request_hooks` (the provider request hooks).
- `pa_core::features::SessionFeature::on_agent_end` (the adoption event).

## Files

Reads and writes `~/.anthropic-accounts/accounts.json` (and its lock) only through the SDK, under the SDK's rules;
through the Claude Code link, reads Claude Code's `.claude.json` / `.credentials.json` (or the macOS Keychain) and
publishes a rotation of the linked account to it, as the plugins do. It owns no file under `~/.prime/agent/`.

## Telemetry

`anthropic_shared_auth` (schema v4), once per process at the first agent end after the store answered a request:
`source` (how the first credential was obtained: `store`, `refreshed`, `adopted`, `claude_code`, or `failed`),
`refreshed` and `failed` (the process's counts so far). Never an account id, email, label or token.
