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
  - `admit`: each request carries the token of the login the routing picks for it now (below; `get_access_token`
    for that login, under the in-process flight lock), so a token another process rotated since the session
    resolved it is replaced before the send; a request no login may serve is refused locally (never sent) with the
    plugins' 429 (or 401) answer.
  - `rejected` after a 401 (once per request, pa-ai's rule): anthropic-napi's `handleUnauthorized`
    (`recover_unauthorized`: one claimed refresh of the row owning the rejected token; a retry only with a new
    version of the same login), and, when the store no longer holds the rejected token (another process rotated
    it), the store's current token re-read under its lock if it differs. Otherwise the 401 is reported.
- The plugins' sidecar configuration (`config.rs`), read as the pi plugin reads it, so a setting made for pi applies
  here too: `PI_ANTHROPIC_AUTH_FILE`, else `$PI_AGENT_DIR/anthropic-auth.json`, else `~/.pi/agent/anthropic-auth.json`
  (the opencode plugin keeps its own copy, `~/.config/opencode/anthropic-auth.json`). Read only, re-read when the
  file's size or mtime changes; a missing, unreadable or non-object file is the defaults, a value of the wrong type
  its default. Read: `quota.enabled` (only `false` disables), `quota.checkIntervalMinutes` (5, floored at 1),
  `quota.refreshEveryNRequests` (off), `quota.minimumRemaining.{five_hour|5h, seven_day|1w}` (0, remaining percent),
  `quota.failClosedOnUnknownQuota` (true), `routing.mode` and `killswitch` (see routing).
- Quota and rate limits (`quota.rs`), as the plugins keep them in the store:
  - `observe`: every response to a store-served request is read for the `anthropic-ratelimit-unified-*` windows
    (`normalize_quota_headers`); a changed reading is recorded on the row holding the token (napi
    `recordQuotaHeaders`) and a served request marks its row used (`markUsed`, at most every five minutes). The
    writes run on the keep-alive thread (inline only when no thread runs).
  - The usage poll (`GET /api/oauth/usage`, the plugins' `QuotaManager`, through the SDK's `OAuthClient::usage` and
    sans-I/O `QuotaManager`): the 5h/7d windows with their resets, the model-scoped weekly windows, extra-usage credits
    and the binding window of one login. Cadence: a login's reading is due one `quota.checkIntervalMinutes` (sidecar,
    default 5, at least 1) after it was taken, a minute after the reset of a window below its minimum, once per
    interval while only headers were seen (they carry no scoped windows), and on every
    `quota.refreshEveryNRequests`-th request (default off). Triggers: a request whose login is due queues a poll of
    that login (it is not waited for); a 429 is confirmed by a poll of the limited login, waited for (bounded, 30 s)
    before the request moves on. Backoff (the plugins'): one minute doubling to fifteen for a 429/5xx/network
    failure, five minutes for any other, none for a 401/403; one poll in flight per login. Polls run on the
    keep-alive thread (at most one a second, the plugins' quota API gate), never on a request, paint or startup
    path (inline only when no thread runs). The access token polled with is the row's live one, else the store's
    claimed refresh of that row. The result merges with the header readings (a newer header window wins, the
    poll's scoped windows and credits stay), and its percentages are recorded on the row holding that token
    (`record_quota_snapshot_for_access_token`, as the pi plugin's `recordQuota`), so every tool's selection sees
    them. Not ported: the plugins' cross-process poll lock (`opencode-*-quota-refresh` file locks) and persisting
    the full snapshot in a state file; the SDK's request carries `anthropic-version` and no `claude-code/<version>`
    user agent.
  - Display: at each agent end of an Anthropic session the agents view gets the line `Claude quota: 5h 48% / 7d 55%
    used` (`publish_feature_status`, feature `anthropic-auth`; status `{fiveHourPercent, sevenDayPercent, checkedAt,
    source: "headers"|"poll"|"store"}`), from the last served login's latest reading (headers and polls merged; the
    source names the newest producer), else what the store recorded for it; published again only when it changes.
    prime-agent has no other usage/limits surface.
  - 429 switching (`rejected(RateLimited)`, also a 200 whose stream opens with `rate_limit_error` /
    `overloaded_error`): the row cools down (`retry-after`, else the reset of the window
    the server named binding, else the later window reset, else a minute) and is unpinned (napi `markRateLimited`),
    the reading is recorded and confirmed by a usage poll, and the request moves to the first other login that
    passes the quota policy (pi's fallback pass: polled first when due; unknown quota fails closed by default), as
    pa-ai re-sends it while the hooks name a login the request has not used. No such login: the 429 is reported,
    and the login keeps serving its live token (a cooling-down login is still a login: `status` lists every
    enabled OAuth inference row, so nothing falls through to auth.json or reads as "no API key").
  - Quota reserve: `ANTHROPIC_QUOTA_RESERVE_PCT` (0-100; the napi `reservePct`) prefers logins whose fresh
    recorded usage is below it in both windows; when every login is at it, the store's plain pick serves.
- Routing (`routing.rs`, for store-served tokens; replaces the plain store pick per request). Candidates: the
  store's logins in its routing order (`current` first; cooling-down, exhausted and dead-refresh rows out; under
  the quota reserve when any is); the first plays the plugins' main account, the rest their fallbacks; their quota
  is this process's readings over what the store recorded. By the sidecar's `routing.mode`:
  - `main-first` (default; pi's ordered pass): the first serves unless a fresh reading has it spent (a 5h/7d
    window, or the request model's scoped window, at 0% left; a stale spent reading is re-polled first) or the
    killswitch blocks it; then the first other login passing the quota policy (`quota.minimumRemaining` per
    window; unknown quota fails closed unless `failClosedOnUnknownQuota: false`), the model's scoped window and
    the killswitch, each polled first when due; none: the first serves anyway, unless the killswitch blocks it.
  - `fallback-first`: those other logins first, then the first.
  - `sticky-balanced`: routed by the ordered pass (the sticky router is not wired yet).
  - Killswitch (`killswitch.enabled`, the opencode plugin's): a login whose remaining 5h/7d percent is below its
    threshold (`killswitch.accounts[<store id>]`, else `killswitch.main`, else 5%/10%), or whose scoped window for
    the request model is at or below its scoped threshold (default 0%), never serves; unknown quota blocks under
    `failClosedOnUnknownQuota`. Readings are polled first when due (its eager refresh). When no login can serve,
    the request is refused locally with a 429: `Killswitch: no routable accounts. Retry in Xm Ys.` (the earliest
    future reset plus a minute, else 300 s), or `<Model> weekly limit reached, no routable accounts. …` when the
    model's scoped window drove the block. pi applies the killswitch only in its sticky pass; prime-agent applies
    it in every mode, as opencode does. Its thresholds apply to every login by store id (no main account here).
- Request shape (`prepare`, `shape.rs`): a request the store's token authenticates goes out as the pi plugin sends
  it (anthropic-auth core `applyClaudeCodeHeaders` on a fresh request, pi `buildAnthropicRequest`):
  - headers: the plugin's Claude Code beta tuple by body shape (base; full-agent; structured-output), then
    `fast-mode` for `speed:"fast"`, `context-1m` for a 1M-capable model, then the request's own betas; the
    `claude-cli/<version> (external, <entrypoint>[, agent-sdk/..][, client-app/..])` user agent; the Claude Code
    `x-stainless-*` set (`js`, `node`, package 0.112.1, runtime v26.3.0, host os/arch, `helper-method: stream`);
    `x-claude-code-session-id` (this process's per-account session) and a fresh `x-client-request-id`; the
    environment-forwarded headers (`CLAUDE_CODE_CONTAINER_ID`, `CLAUDE_CODE_REMOTE_SESSION_ID`,
    `CLAUDE_AGENT_SDK_CLIENT_APP`, `CLAUDE_CODE_ADDITIONAL_PROTECTION`); headers the shape does not set (a
    provider's configured headers, prime-agent's request ids) follow, `x-api-key` never;
  - body: the billing block `x-anthropic-billing-header: cc_version=<version>.<suffix>; cc_entrypoint=cli;
    cch=00000;` first among the system blocks (the suffix samples UTF-16 positions 4, 7, 20 of the first user
    text), `metadata.user_id` (`{"device_id","account_uuid","session_id"}`; the device id from `device.json` beside
    the store, created like the plugins' when missing; the account uuid the store holds; omitted without one), and
    Claude Code's key order;
  - the version: the npm registry's latest Claude Code (the SDK's `claude_version`), read on the keep-alive thread
    at its start and hourly, never below the verified floor (2.1.280);
    `OPENCODE_ANTHROPIC_AUTH_DISABLE_VERSION_CHECK=1` keeps the floor.

  Native or fork: upstream TS v0.9.8's own OAuth path sent `claude-code-20250219,oauth-2025-04-20` (+ its own betas),
  `claude-cli/2.1.281` and `x-app: cli` (pa-ai sends exactly these natively), a pinned version (no lookup), no
  session id, no billing block and no `metadata.user_id` of its own. Its `x-stainless-*` headers were the JS SDK's
  transport artefacts (the SDK's own package version and the Node runtime's version, on every Anthropic request,
  OAuth or not), which the Rust port does not emulate for any provider. So every piece above is fork behaviour and
  comes through the request hooks, only for the store's tokens; `--no-default-features` sends what it always
  sent. Golden: `tests/fixtures/golden/request_shape.json`, generated from the plugin's own code by `generate.ts`.
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
- Sticky-balanced session routing (not wired yet).
- Writing the sidecar or the plugins' commands (`/claude-routing`, `/claude-killswitch`, `/claude-quota`; a
  sticky session's `reset`): prime-agent reads the settings the plugins write. The sidecar's other sections
  (`fallbackOn`, `refresh`, relay, cache, fast mode, prime, dump, logging) and its fallback `accounts` (API-key
  routes included) are not read: the store's logins are the pool.
- The rest of pi's request (its own message conversion and system-prompt split, server-side fallback with its
  `fallbacks` body field and betas, the 1M-context credits latch, fast mode, the cache-keep relay, content
  filtering): pa-ai's Claude Code mode builds the request; the shape above is applied on top. A `--api-key`
  `sk-ant-oat` token, or any token the store did not serve, keeps pa-ai's native Claude Code mode.

## Public API

`install`, `shared_source`, `PROVIDER_ID`, `QUOTA_RESERVE_ENV`, `SharedStoreSource` (`new`, `store_path`, `usage`, `store_login`),
`SharedStoreConfig` (`from_env`, `isolated`; `background` runs the keep-alive thread, `version_url` the version lookup, `quota_reserve` the reserve, `config_path` the plugins' sidecar), `NewLogin`, `StoredLogin` (`claude_code_notice`), `SourceUsage`, `STORE_LABEL`, `AnthropicAuthFeature`, `TELEMETRY_EVENT`.

## Seams

- `pa_core::auth::install_credential_source` (the provider credential source seam: credential, custody of
  auth.json's login, logout).
- `pa_ai::request_hooks::install_request_hooks` (the provider request hooks; `admit`, the generic admission seam:
  another credential, or a local refusal).
- `pa_core::features::SessionFeature::on_agent_end` (the adoption event).

## Files

Reads the plugins' sidecar configuration (`~/.pi/agent/anthropic-auth.json`, `PI_AGENT_DIR` / `PI_ANTHROPIC_AUTH_FILE`;
never written: the plugins' commands own it; missing, unreadable or malformed: the plugins' defaults; re-read when it
changes). Reads and writes `~/.anthropic-accounts/accounts.json` (and its lock) only through the SDK, under the SDK's rules,
and `~/.anthropic-accounts/device.json` (the installation's device id, the plugins' format; created when missing,
never overwritten);
through the Claude Code link, reads Claude Code's `.claude.json` / `.credentials.json` (or the macOS Keychain) and
publishes a rotation of the linked account to it, as the plugins do. It owns no file under `~/.prime/agent/`.

## Telemetry

`anthropic_shared_auth` (schema v4), once per process at the first agent end after the store answered a request:
`source` (how the first credential was obtained: `store`, `refreshed`, `adopted`, `claude_code`, or `failed`),
`refreshed` and `failed` (the process's counts so far), and (additive, optional in the catalogue) `migrated`
(auth.json logins moved into the store), `recovered` (401s re-sent with a recovered token), `rotated` (429s moved
to another login), `polled` (usage polls sent), `poll_failed` (of those, the ones that failed), `quota_routed`
(requests sent past the first login by quota policy or killswitch) and `blocked` (requests refused locally). Never
an account id, email, label or token.
