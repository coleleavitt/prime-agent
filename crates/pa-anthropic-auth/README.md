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
- Quota and rate limits (`quota.rs`), as the plugins keep them in the store:
  - `observe`: every response to a store-served request is read for the `anthropic-ratelimit-unified-*` windows
    (`normalize_quota_headers`); a changed reading is recorded on the row holding the token (napi
    `recordQuotaHeaders`) and a served request marks its row used (`markUsed`, at most every five minutes). The
    writes run on the keep-alive thread (inline only when no thread runs).
  - Display: at each agent end of an Anthropic session the agents view gets the line `Claude quota: 5h 48% / 7d 55%
    used` (`publish_feature_status`, feature `anthropic-auth`; status `{fiveHourPercent, sevenDayPercent, checkedAt,
    source: "headers"|"store"}`), from the last served login's latest reading, else what the store recorded for it;
    published again only when it changes. prime-agent has no other usage/limits surface.
  - 429 switching (`rejected(RateLimited)`, also a 200 whose stream opens with `rate_limit_error` /
    `overloaded_error`): the row cools down (`retry-after`, else the reset of the window the server named binding,
    else the later window reset, else a minute) and is unpinned (napi `markRateLimited`), the reading is recorded,
    and the request moves to the next login in the store's order (pa-ai re-sends it while the hooks name a login
    the request has not used). No other login: the 429 is reported, and the login keeps serving its live token
    (a cooling-down login is still a login: `status` lists every enabled OAuth inference row, so nothing falls
    through to auth.json or reads as "no API key").
  - Quota reserve: `ANTHROPIC_QUOTA_RESERVE_PCT` (0-100; the napi `reservePct`) prefers logins whose fresh
    recorded usage is below it in both windows; when every login is at it, the store's plain pick serves.
- Request shape (`prepare`; `pi/`, `shape.rs`): a request the store's token authenticates goes out as the pi plugin
  sends it (anthropic-auth pi `streamCortexKitAnthropic` → `sendAnthropicRequestUnrecorded`):
  - body (`pi/convert.rs`, pi's `buildAnthropicRequest`), rebuilt from the caller's conversation and options that
    pa-ai hands the hook (`RequestSource`; pa-ai's own conversion is discarded): pi's message converter (orphaned tool
    calls and stray results dropped, tool ids sanitized, Claude Code tool casing and the `deep_research` wire alias,
    text/image user turns, signed thinking replayed only from Anthropic-origin messages, other thinking as text,
    OpenAI reasoning dropped, trailing assistant turns stripped), the system-prompt split (pi's documentation
    paragraph moves to a cached block ahead of the first user turn; an unknown prompt moves whole), the billing block
    (`x-anthropic-billing-header: cc_version=<version>.<suffix>; cc_entrypoint=cli; cch=00000;`, the suffix sampling
    UTF-16 positions 4, 7, 20 of the first user text) and Claude Code's identity block, pi's thinking shape (the
    5-series' summarized adaptive thinking, effort for adaptive models, a budget below `max_tokens` for the older
    families; the caller's level as given, `off` included, as pi tests it), `max_tokens` 16384 unless the caller set
    one, pi's cache breakpoints and its cache mode (`claudeCache`), `metadata.user_id` (`{"device_id","account_uuid",
    "session_id"}`; the device id from `device.json` beside the store, created like the plugins' when missing; the
    account uuid the store holds; omitted without one). Sent as `JSON.stringify` bytes in Claude Code's key order
    (`OutgoingRequest::body`). Golden: `tests/fixtures/golden/pi_requests.json`, recorded by
    `generate_requests.ts` from pi's own provider entry point against an in-process fetch.
  - server-side fallback (`pi/fallback.rs`, pi `stream.ts`): a request to Opus 5 (any point release) or Fable 5
    carries `fallbacks: "default"` and both server-side-fallback betas (`server-side-fallback-2026-06-01`, then
    `-07-01`) after the tuple. The `fallback` block a served fallback streams is kept as pi's marker (a thinking
    block holding a word joiner, signed `cortexkit-server-fallback-v1:<from>|<to>`; through pa-ai's
    `response_event`); a later request replays a marker as the `fallback` block when it goes to a fallback model
    again and drops it otherwise. A body the fallback or a marker changed goes out in pi's own key order (as pi
    serializes it then), not Claude Code's.
  - the 1M-context credits latch (`pi/context1m.rs`, pi `stream.ts` after Claude Code 2.1.260's
    `longContext1mCreditsBlocked`): a 1M-capable model's request carries `context-1m` until Anthropic answers one
    with HTTP 429 "extra usage / usage credits are required for long context" (read from the rejection's body); from
    then on that token's requests leave without `context-1m` (the 200k window). Keyed by the token's fingerprint
    (SHA-256, 16 hex) in memory for the life of the process, as the plugin keeps it: never written to the store or a
    file, so a rotated token or a new process starts unlatched. The 429 that latches is reported (or moved to
    another login) as before; nothing is re-sent for it.
  - the streamed `prime_deep_research` tool name is restored to `deep_research` (pi `fromClaudeCodeToolName`; pi
    restores it only when the caller declared `deep_research`, which the alias implies).
  - settings (`pi/settings.rs`): the plugin's settings file, `anthropic-auth.json` in pi's agent directory
    (`PI_ANTHROPIC_AUTH_FILE`, else `$PI_AGENT_DIR` or `~/.pi/agent`; prime-agent's TS build loaded the plugin
    without an agent dir of its own, so both tools share it), read per request (memoized on size and mtime);
    `claudeCache.enabled`/`mode`, `claudeFast.enabled`.
  - headers (`shape.rs`, core `applyClaudeCodeHeaders` on a fresh request, so none of pa-ai's own betas): the Claude
    Code beta tuple by body shape (base; full-agent; structured-output), then `fast-mode` for `speed:"fast"`,
    `context-1m` for a 1M-capable model; the `claude-cli/<version> (external, <entrypoint>[, agent-sdk/..][,
    client-app/..])` user agent; the Claude Code `x-stainless-*` set (`js`, `node`, package 0.112.1, runtime v26.3.0,
    host os/arch, `helper-method: stream`); `x-claude-code-session-id` (this process's per-account session) and a
    fresh `x-client-request-id`; the environment-forwarded headers (`CLAUDE_CODE_CONTAINER_ID`,
    `CLAUDE_CODE_REMOTE_SESSION_ID`, `CLAUDE_AGENT_SDK_CLIENT_APP`, `CLAUDE_CODE_ADDITIONAL_PROTECTION`); headers the
    shape does not set (a provider's configured headers) follow, `x-api-key` never;
  - the version: the npm registry's latest Claude Code (the SDK's `claude_version`), read on the keep-alive thread
    at its start and hourly, never below the verified floor (2.1.280);
    `OPENCODE_ANTHROPIC_AUTH_DISABLE_VERSION_CHECK=1` keeps the floor.

  Native or fork: upstream TS v0.9.8's own OAuth path sent `claude-code-20250219,oauth-2025-04-20` (+ its own betas),
  `claude-cli/2.1.281` and `x-app: cli` (pa-ai sends exactly these natively), a pinned version (no lookup), no
  session id, no billing block and no `metadata.user_id` of its own. Its `x-stainless-*` headers were the JS SDK's
  transport artefacts (the SDK's own package version and the Node runtime's version, on every Anthropic request,
  OAuth or not), which the Rust port does not emulate for any provider. So every piece above is fork behaviour and
  comes through the request hooks, only for the store's tokens; `--no-default-features` sends what it always
  sent. Goldens: `tests/fixtures/golden/request_shape.json` (headers, betas, billing, user agent; `generate.ts`) and
  `pi_requests.json` (whole requests; `generate_requests.ts`).
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
- The usage endpoint poll (`/api/oauth/usage`), sticky-balanced routing, the killswitch and per-window minimum
  thresholds of the plugins' sidecar configuration: readings come from response headers only.
- The rest of pi's request (fast mode's command, the cache keep-alive, content filtering). A `--api-key` `sk-ant-oat` token, or any
  token the store did not serve, keeps pa-ai's native Claude Code mode.

## Public API

`install`, `shared_source`, `PROVIDER_ID`, `QUOTA_RESERVE_ENV`, `SharedStoreSource` (`new`, `store_path`, `usage`, `store_login`),
`SharedStoreConfig` (`from_env`, `isolated`; `background` runs the keep-alive thread, `version_url` the version lookup, `quota_reserve` the reserve, `pi` the plugin's request-path files), `PiConfig` (`from_env`, `under`), `NewLogin`, `StoredLogin` (`claude_code_notice`), `SourceUsage`, `STORE_LABEL`, `AnthropicAuthFeature`, `TELEMETRY_EVENT`.

## Seams

- `pa_core::auth::install_credential_source` (the provider credential source seam: credential, custody of
  auth.json's login, logout).
- `pa_ai::request_hooks::install_request_hooks` (the provider request hooks; `prepare` reads the caller's request,
  `RequestSource`, and sends exact bytes, `OutgoingRequest::body`; `response_event` rewrites the streamed events).
- `pa_core::features::SessionFeature::on_agent_end` (the adoption event).

## Files

Reads and writes `~/.anthropic-accounts/accounts.json` (and its lock) only through the SDK, under the SDK's rules,
and `~/.anthropic-accounts/device.json` (the installation's device id, the plugins' format; created when missing,
never overwritten); reads the pi plugin's settings file (`~/.pi/agent/anthropic-auth.json`, above);
through the Claude Code link, reads Claude Code's `.claude.json` / `.credentials.json` (or the macOS Keychain) and
publishes a rotation of the linked account to it, as the plugins do. It owns no file under `~/.prime/agent/`.

## Telemetry

`anthropic_shared_auth` (schema v4), once per process at the first agent end after the store answered a request:
`source` (how the first credential was obtained: `store`, `refreshed`, `adopted`, `claude_code`, or `failed`),
`refreshed` and `failed` (the process's counts so far), and (additive, optional in the catalogue) `migrated`
(auth.json logins moved into the store), `recovered` (401s re-sent with a recovered token) and `rotated` (429s moved
to another login). Never an account id, email, label or token.
