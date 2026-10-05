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
  (the opencode plugin keeps its own copy, `~/.config/opencode/anthropic-auth.json`); the pi plugin's settings
  file (`pi/settings.rs`) is the same file, resolved the same way. Re-read when the
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
    `overloaded_error`), outside a sticky session: the row cools down (`retry-after`, else the reset of the window
    the server named binding, else the later window reset, else a minute) and is unpinned (napi `markRateLimited`),
    the reading is recorded and confirmed by a usage poll, and the request moves to the first other login that
    passes the quota policy (pi's fallback pass: polled first when due; unknown quota fails closed by default), as
    pa-ai re-sends it while the hooks name a login the request has not used. No such login: the 429 is reported,
    and the login keeps serving its live token (a cooling-down login is still a login: `status` lists every
    enabled OAuth inference row, so nothing falls through to auth.json or reads as "no API key"). In a sticky
    session see routing.
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
  - `sticky-balanced` (the plugins' `StickySessionRouter`, via the SDK's port): the session (the top-level session
    this process serves, from `on_session_start`; a child agent rides its parent's login) is assigned the login
    with the most spendable quota per hour until reset (per-window reserve: the larger of `minimumRemaining` and,
    when armed, the killswitch threshold) over the prompt bytes already assigned to it (the context size pa-ai
    reports), persisted by SHA-256 of the session id in `anthropic-auth-routing-state.json` beside the sidecar
    (`PI_ANTHROPIC_AUTH_ROUTING_STATE_FILE`; shared with pi, cross-process locked), and kept across processes and
    restarts. Candidates not fresh are polled first (waited for). It moves only on a fresh reading showing the 7d
    or the model's scoped window spent, or the 5h window spent more than 15 minutes before its reset (within 15
    minutes the request is refused locally with a 429 and a session-jittered `retry-after`, keeping the session),
    a killswitch block, the login leaving the pool, or a model change. A 429 (or `rate_limit_error` opening) on
    it is confirmed by a poll: only that move takes the request to another login; otherwise the 429 is reported
    and the session stays (no cooldown). A complete pool with no eligible login is refused locally with the
    plugins' 429 (`No OAuth account currently satisfies sticky-balanced quota policy. Retry in …`, the scoped
    model's weekly message, or a 401 naming logins that need a re-login); an incomplete pool (a reading still
    unknown or stale after its poll) is routed by the ordered pass instead of pi's retryable connection error.
  - Killswitch (`killswitch.enabled`, the opencode plugin's): a login whose remaining 5h/7d percent is below its
    threshold (`killswitch.accounts[<store id>]`, else `killswitch.main`, else 5%/10%), or whose scoped window for
    the request model is at or below its scoped threshold (default 0%), never serves; unknown quota blocks under
    `failClosedOnUnknownQuota`. Readings are polled first when due (its eager refresh). When no login can serve,
    the request is refused locally with a 429: `Killswitch: no routable accounts. Retry in Xm Ys.` (the earliest
    future reset plus a minute, else 300 s), or `<Model> weekly limit reached, no routable accounts. …` when the
    model's scoped window drove the block. pi applies the killswitch only in its sticky pass; prime-agent applies
    it in every mode, as opencode does. Its thresholds apply to every login by store id (no main account here).
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
    `claudeCache.enabled`/`mode`, `claudeFast.enabled`. A file that does not parse reads as defaults (pi fails the
    request instead; a warning is logged).
  - fast mode (core `fast.ts`, pi `commands.ts`): a persisted setting, not a model or a tier. On, a request to
    Opus 4.6, 4.7, 4.8 or Opus 5 (any point release) carries `speed: "fast"` and `fast-mode-2026-02-01`; other models
    are untouched. Toggled with `/claude-fast [on|off]` and the cache with `/claude-cache [on|off|mode
    explicit|automatic|hybrid]`, session slash commands (`SessionFeature::slash_commands`) with pi's arguments and
    texts, written to the settings file as pi's setters write it: under its `<file>.config-write.lock` (created
    exclusively, `{"ownerId","expiresAt"}`, 10 s, waited for up to 12 s; an expired one is taken over), the changed
    section merged, the fields pi normalizes on a rewrite (`version`, `main`, `refresh`/`quota`'s known fields,
    `accounts`) and every other key kept in order, new keys in pi's order, atomically, owner-only,
    `JSON.stringify(config, null, 2)` and a newline; a missing file is created as pi creates it. pi's runtime state
    file is not written. A corrupt file is refused (pi's message), never overwritten.
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
- `AnthropicAuthFeature`: a `SessionFeature` that reports adoption once per process and serves the plugins'
  commands (below).
- Cache keep-alive (`cachekeep.rs`, core `cachekeep.ts` / `cachekeep-registry.ts` as pi's `stream.ts` runs them, the
  plugin's timing): with `/claude-cache mode hybrid` on and `/claude-cachekeep always` (or a local `HH-HH` window,
  overnight allowed), each store-served request with a session id is remembered as sent (`prepare`, in memory, no
  I/O). Five minutes before its one-hour cache expires the same request goes again as a prewarm (`max_tokens: 0`,
  no `stream`, budgeted thinking / structured output format / forced tool choice dropped, Claude Code's key order,
  the billing `cch` placeholder kept) with pi's headers rebuilt for it (the tuple chosen by the prewarm body plus
  `extended-cache-ttl`, and `fast-mode` for a fast body), then an hour after each success. A failure retries with
  the plugin's jittered backoff (1 min doubling to 15) while the last success's cache lives; past it the session is
  dropped (a cold write would be paid), as is a body with no breakpoints. At most 32 sessions and 16 Mi UTF-16 units
  of bodies (oldest out); a session belongs to the day its window opened. Unlike the plugin:
  - the prewarm authenticates with the store's current token for the login the session's request was served with
    (`get_access_token` for that row, refreshed under the store's claim when expired), never a remembered one;
  - the scheduler runs on the crate's own thread (`anthropic-cachekeep`, started by the first tracked request,
    ticking every minute and parked while nothing is tracked), never on a request, paint or startup path;
  - a sticky session's new assignment prefers the login its cache is kept warm on (the opencode plugin's
    `trackedOAuthRoute`, `preferred_account_id`; the quota branch's "no CacheKeep pre-assignment" gap).
  The registry (`<tmp>/opencode-anthropic-auth/cachekeep-sessions/pi`, `PI_ANTHROPIC_AUTH_CACHEKEEP_REGISTRY_DIR`,
  shared with pi): one `<pid>-<uuid>.json` per process (`{"version":1,"updatedAt","sessions":[...]}`, atomic, `0600`
  in a `0700` directory), rewritten after each change and tick, removed when nothing is tracked; records older
  than three minutes are ignored. `/claude-cachekeep [always|off|HH-HH|subagents on|off]` writes `cacheKeep` as
  pi's setters do and prints pi's status over every live process's sessions (`Next prewarm` in local time, en-US).
  Golden (`pi_extras.json`): the prewarm bodies, the whole prewarm request pi's own scheduler sends 55 minutes
  after a hybrid request, its registry record, and the command's texts and file writes. Not ported: the plugin's
  per-prewarm request dumps and cache diagnostics, a record removed at process exit (the lease expires it), and
  subagent tracking (pi only persists the flag).
- Account commands (`pi/account_commands.rs`; session slash commands, the plugins' arguments and texts, golden:
  `tests/fixtures/golden/pi_extras.json`, recorded by `generate_extras.ts` from the plugins' own handlers):
  - `/claude-routing [main-first|fallback-first|sticky-balanced|mode <m>|reset]` (pi's): the mode written to the
    sidecar's `routing.mode` (the routing reads it on the next request); `reset` forgets this session's sticky
    assignment (the top-level session's key, hashed, in the routing state), so its next request is assigned again.
  - `/claude-killswitch [on|off|set <login>:<5h>,<1w>[,<scoped>] ...]` (the opencode plugin's; pi has none): the
    killswitch section written as opencode writes it; its table lists `main` and the store's logins by id (the
    thresholds the routing applies per store id; `set all:` sets `main` and every login).
  - `/claude-quota` (pi's text, `buildClaudeQuotaSummary`): every OAuth login of the store (the one the store
    serves first as `main`, the others `fallback`, disabled ones marked), named by label else id, with this
    process's readings (headers and polls) else what the store recorded; the last token refresh and the row's
    current error. Not shown: the plan tier (the store keeps no profile).
  - Every write goes through the plugins' setters' path (`PluginSettings::update`: the `.config-write.lock`, the
    normalized fields, key order, atomic `0600`, `JSON.stringify(config, null, 2)`).

## Non-goals (here)

- Account management beyond logout (enable, disable, reorder, pin, remote revoke): the plugins' account commands
  own it; prime-agent has no account command surface.
- The plugins' other commands (`/claude-account`, `/claude-dump`, `/claude-logging`, `/claude-prime`). The
  sidecar's other sections (`fallbackOn`, `refresh`, relay, prime, dump, logging) and its fallback `accounts`
  (API-key routes included) are not read: the store's logins are the pool.
- The rest of pi's request (the opt-in relay transport, content filtering). A `--api-key` `sk-ant-oat` token, or
  any token the store did not serve, keeps pa-ai's native Claude Code mode.

## Public API

`install`, `shared_source`, `PROVIDER_ID`, `QUOTA_RESERVE_ENV`, `SharedStoreSource` (`new`, `store_path`, `usage`, `store_login`),
`SharedStoreConfig` (`from_env`, `isolated`; `background` runs the keep-alive thread, `version_url` the version lookup, `quota_reserve` the reserve, `pi` the plugin's request-path files, `config_path` the plugins' sidecar, `routing_state_path` the sticky routing state, `cachekeep_registry_dir` the keep-alive's session registry), `PiConfig` (`from_env`, `under`), `NewLogin`, `StoredLogin` (`claude_code_notice`), `SourceUsage`, `STORE_LABEL`, `AnthropicAuthFeature`, `TELEMETRY_EVENT`.

## Seams

- `pa_core::auth::install_credential_source` (the provider credential source seam: credential, custody of
  auth.json's login, logout).
- `pa_ai::request_hooks::install_request_hooks` (the provider request hooks; `admit`, the generic admission seam:
  another credential, or a local refusal; `prepare` reads the caller's request, `RequestSource`, and sends exact
  bytes, `OutgoingRequest::body`; `response_event` rewrites the streamed events).
- `pa_core::features::SessionFeature::on_session_start` (the sticky routing key).
- `pa_core::features::SessionFeature::on_agent_end` (the adoption event) and `slash_commands` /
  `execute_slash_command` (`/claude-fast`, `/claude-cache`, `/claude-cachekeep`, `/claude-routing`,
  `/claude-killswitch`, `/claude-quota`).

## Files

Reads the plugins' sidecar configuration (`~/.pi/agent/anthropic-auth.json`, `PI_AGENT_DIR` / `PI_ANTHROPIC_AUTH_FILE`;
written only by the commands, as the plugins' setters write it; missing, unreadable or malformed: the plugins'
defaults; re-read when it changes) and reads and writes the sticky routing state beside it (`anthropic-auth-routing-state.json` and its
`.flock`, the plugins' format through the SDK's router: hashed session ids, written atomically `0600`, only in
`sticky-balanced` mode). Reads and writes `~/.anthropic-accounts/accounts.json` (and its lock) only through the SDK, under the SDK's rules,
and `~/.anthropic-accounts/device.json` (the installation's device id, the plugins' format; created when missing,
never overwritten); reads the pi plugin's settings file (`~/.pi/agent/anthropic-auth.json`, above) and writes it
for `/claude-fast`, `/claude-cache`, `/claude-cachekeep`, `/claude-routing` and `/claude-killswitch` (with its `.config-write.lock`);
`/claude-routing reset` removes one assignment from the sticky routing state; writes this process's record in
the cache keep-alive's session registry (above) and reads the others';
through the Claude Code link, reads Claude Code's `.claude.json` / `.credentials.json` (or the macOS Keychain) and
publishes a rotation of the linked account to it, as the plugins do. It owns no file under `~/.prime/agent/`.

## Telemetry

`anthropic_shared_auth` (schema v4), once per process at the first agent end after the store answered a request:
`source` (how the first credential was obtained: `store`, `refreshed`, `adopted`, `claude_code`, or `failed`),
`refreshed` and `failed` (the process's counts so far), and (additive, optional in the catalogue) `migrated`
(auth.json logins moved into the store), `recovered` (401s re-sent with a recovered token), `rotated` (429s moved
to another login), `polled` (usage polls sent), `poll_failed` (of those, the ones that failed), `quota_routed`
(requests sent past the first login by quota policy or killswitch), `blocked` (requests refused locally),
`sticky_assigned` and `sticky_migrated` (sticky sessions assigned a login, and moved). Never an account id, email,
label, session id or token.
