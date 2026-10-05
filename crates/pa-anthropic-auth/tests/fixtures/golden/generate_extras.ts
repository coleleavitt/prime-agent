// The generator of `pi_extras.json` beside this file: what the plugin's
// account commands print and write, its quota summary, and its cache
// keep-alive (the prewarm body, and the whole prewarm request pi sends), for
// `src/pi/commands/tests.rs` and `src/cachekeep/tests.rs` to compare with,
// byte for byte.
//
// Nothing is reimplemented here: the commands are pi's own handlers
// (packages/pi/src/commands.ts) on the settings file; `/claude-killswitch`,
// which only the opencode plugin registers, runs as its handler does
// (`loadAccounts`, `getKillswitchConfig`, `executeKillswitchCommand`,
// `setKillswitchPersistent`) over a fixed list of login ids (prime-agent's
// store logins); the quota summaries are core's `buildClaudeQuotaSummary`;
// the prewarm is pi's own stream entry point with cache keep-alive on, its
// scheduler's tick fired by hand after the clock moved 55 minutes. The
// network is the harness's in-process `fetch`; no request leaves the process.
//
// To regenerate: as `generate_requests.ts` says, plus TZ=UTC (the status
// text prints a local time), then
//   bun generate_extras.ts > pi_extras.json
// Generated from anthropic-auth 7f5d88a on linux x64.
import { existsSync, readdirSync, readFileSync, rmSync, writeFileSync, mkdirSync } from 'node:fs'
import { join } from 'node:path'

// The clock and the keep-alive's scheduler, under the generator's control:
// both are read when the plugin's code runs, so they are replaced before it
// is loaded.
const realNow = Date.now
let fakeNow: number | null = null
Date.now = () => fakeNow ?? realNow()
const realSetInterval = globalThis.setInterval
const ticks: Array<() => void> = []
globalThis.setInterval = ((callback: () => void, ms?: number) => {
  if (ms === 60_000 && fakeNow !== null) {
    ticks.push(callback)
    return { unref() {}, ref() {}, hasRef: () => false } as unknown as ReturnType<typeof setInterval>
  }
  return realSetInterval(callback, ms)
}) as typeof setInterval

const harness = await import('./golden_harness.ts')
const { mock, repo, scratch, settingsFile, streamCortexKitAnthropic, model, user, ACCOUNT_UUID, DEVICE_ID } = harness
const core = await import(join(repo, 'packages/core/dist/index.js'))
const registryDir = process.env.PI_ANTHROPIC_AUTH_CACHEKEEP_REGISTRY_DIR as string

// -- Commands ---------------------------------------------------------------
const { registerCommands } = await import(join(repo, 'packages/pi/src/commands.ts'))
const handlers = new Map<string, (args: string, ctx: unknown) => Promise<void>>()
registerCommands({
  registerCommand: (name: string, def: { handler: (args: string, ctx: unknown) => Promise<void> }) => {
    handlers.set(name, def.handler)
  },
})
const LOGIN_IDS = ['acct-a', 'acct-b']
// The opencode plugin's `/claude-killswitch` handler over prime-agent's logins.
handlers.set('claude-killswitch', async (args: string, ctx: unknown) => {
  const storage = await core.loadAccounts(settingsFile)
  const config = core.getKillswitchConfig(storage)
  const result = core.executeKillswitchCommand({ argumentsText: args, config, accountIds: LOGIN_IDS })
  if (result.updatedConfig) await core.setKillswitchPersistent(result.updatedConfig, settingsFile)
  ;(ctx as { ui: { notify: (message: string) => void } }).ui.notify(result.text)
})

const stateFile = join(scratch, 'pi-agent', 'anthropic-auth-state.json')
// Another instance's live registry record, for the status listing.
const OTHER_SESSIONS = [
  { id: 'ses-other-b', cacheExpiresAt: 1_779_102_000_000, nextPrewarmAt: 1_779_101_700_000 },
  { id: 'ses-other-a', cacheExpiresAt: 1_779_103_000_000, nextPrewarmAt: 1_779_102_700_000 },
]
type CommandRun = { command: string; args: string; registry?: boolean }
const SEQUENCES: Array<{ name: string; initial: string | null; runs: CommandRun[] }> = [
  {
    name: 'routing-from-no-file',
    initial: null,
    runs: [
      { command: 'claude-routing', args: '' },
      { command: 'claude-routing', args: 'sticky-balanced' },
      { command: 'claude-routing', args: 'mode fallback-first' },
      { command: 'claude-routing', args: ' MAIN-FIRST ' },
      { command: 'claude-routing', args: 'reset' },
      { command: 'claude-routing', args: 'mode bogus' },
      { command: 'claude-routing', args: 'reset now' },
    ],
  },
  {
    name: 'routing-over-an-existing-file',
    initial: `${JSON.stringify({ quota: { enabled: true, extra: 1 }, routing: { mode: 'sticky', note: 'kept' }, accounts: [], custom: true }, null, 2)}\n`,
    runs: [
      { command: 'claude-routing', args: '' },
      { command: 'claude-routing', args: 'sticky-balanced' },
    ],
  },
  {
    name: 'killswitch-from-no-file',
    initial: null,
    runs: [
      { command: 'claude-killswitch', args: '' },
      { command: 'claude-killswitch', args: 'on' },
      { command: 'claude-killswitch', args: 'set main:3,8,0 acct-b:5,10' },
      { command: 'claude-killswitch', args: 'set all:7,12' },
      { command: 'claude-killswitch', args: 'off' },
      { command: 'claude-killswitch', args: '' },
      { command: 'claude-killswitch', args: 'set nope' },
      { command: 'claude-killswitch', args: 'on' },
    ],
  },
  {
    name: 'killswitch-over-an-existing-file',
    initial: `${JSON.stringify({ killswitch: { accounts: { 'acct-a': { '5h': 2, '1w': 4 } }, enabled: false }, accounts: [] }, null, 2)}\n`,
    runs: [
      { command: 'claude-killswitch', args: 'on' },
      { command: 'claude-killswitch', args: 'set acct-a:1,2,3' },
    ],
  },
  {
    name: 'cachekeep-from-no-file',
    initial: null,
    runs: [
      { command: 'claude-cachekeep', args: '' },
      { command: 'claude-cachekeep', args: 'always' },
      { command: 'claude-cachekeep', args: '9-17' },
      { command: 'claude-cachekeep', args: '22-6' },
      { command: 'claude-cachekeep', args: 'subagents on' },
      { command: 'claude-cachekeep', args: 'off' },
      { command: 'claude-cachekeep', args: '7-7' },
      { command: 'claude-cachekeep', args: 'sometimes' },
    ],
  },
  {
    name: 'cachekeep-with-hybrid-cache-and-tracked-sessions',
    initial: `${JSON.stringify({ claudeCache: { enabled: true, mode: 'hybrid' }, cacheKeep: { enabled: true, startHour: 1, endHour: 2, note: 'kept' }, accounts: [] }, null, 2)}\n`,
    runs: [
      { command: 'claude-cachekeep', args: '', registry: true },
      { command: 'claude-cachekeep', args: 'always', registry: true },
    ],
  },
]
const commands = []
for (const sequence of SEQUENCES) {
  rmSync(settingsFile, { force: true })
  rmSync(stateFile, { force: true })
  rmSync(registryDir, { recursive: true, force: true })
  if (sequence.initial !== null) writeFileSync(settingsFile, sequence.initial)
  const steps = []
  for (const run of sequence.runs) {
    if (run.registry) {
      mkdirSync(registryDir, { recursive: true })
      writeFileSync(
        join(registryDir, 'other-instance.json'),
        `${JSON.stringify({ version: 1, updatedAt: Date.now(), sessions: OTHER_SESSIONS })}\n`,
      )
    }
    const notified: string[] = []
    const ctx = {
      ui: { notify: (message: string) => notified.push(message) },
      sessionManager: { getSessionId: () => 'pi-session' },
    }
    const handler = handlers.get(run.command)
    if (!handler) throw new Error(`no ${run.command}`)
    await handler(run.args, ctx)
    steps.push({
      ...run,
      text: notified.join('\n'),
      file: existsSync(settingsFile) ? readFileSync(settingsFile, 'utf8') : null,
    })
  }
  commands.push({ name: sequence.name, initial: sequence.initial, steps })
}

// -- Quota summaries ---------------------------------------------------------
const NOW = Date.parse('2026-05-18T10:00:00.000Z')
const window = (used: number, resetsInMs: number | null, checkedAgoMs: number) => ({
  usedPercent: used,
  remainingPercent: Math.round((100 - used) * 1000) / 1000,
  ...(resetsInMs === null ? {} : { resetsAt: new Date(NOW + resetsInMs).toISOString() }),
  checkedAt: NOW - checkedAgoMs,
})
const money = (amountMinor: number, currency: string, exponent = 2) => ({ amountMinor, currency, exponent })
const QUOTA_CASES = [
  { name: 'no-accounts', accounts: [] },
  {
    name: 'one-login-headers-only',
    accounts: [{ name: 'work', role: 'main', enabled: true, quota: { five_hour: window(48, 2 * 3_600_000 + 61_000, 30_000), seven_day: window(55.25, 3 * 86_400_000, 90 * 60_000) } }],
  },
  {
    name: 'polled-logins',
    accounts: [
      {
        name: 'main-login',
        role: 'main',
        enabled: true,
        tierLabel: 'Max 20x',
        lastRefreshedAt: NOW - 61 * 60_000,
        quota: {
          five_hour: window(100, 59_000, 4 * 60_000),
          seven_day: window(33.33, 6 * 86_400_000 + 45 * 60_000, 4 * 60_000),
          scoped: [
            {
              id: 'claude-weekly-scoped-fable',
              title: 'Fable only',
              modelId: 'claude-fable-5',
              modelName: 'Fable',
              usedPercent: 12.06,
              remainingPercent: 87.94,
              resetsAt: new Date(NOW + 30 * 60_000).toISOString(),
              checkedAt: NOW - 120 * 60_000,
            },
          ],
          extraUsage: { used: money(123456, 'USD'), limit: money(500000, 'USD'), exhausted: false },
          bindingWindow: 'five_hour',
          fallbackAdvised: true,
        },
      },
      {
        name: 'spare',
        role: 'fallback',
        enabled: false,
        error: 'refresh token revoked',
        quota: {
          seven_day: window(0, -5_000, 0),
          scoped: [
            {
              id: 'claude-weekly-scoped-opus',
              title: 'Opus only',
              modelName: 'Opus',
              usedPercent: 100,
              remainingPercent: 0,
              checkedAt: NOW - 5 * 60_000,
            },
          ],
          extraUsage: { used: money(9999, 'EUR'), limit: money(9999, 'EUR'), exhausted: true },
          bindingWindow: 'claude-weekly-scoped-opus',
        },
      },
      {
        name: 'credits',
        role: 'fallback',
        enabled: true,
        quota: {
          extraUsage: { used: money(150000, 'JPY', 0), limit: money(25, 'GBP'), exhausted: false },
        },
      },
      {
        name: 'odd-money',
        role: 'fallback',
        quota: { extraUsage: { used: money(1, 'XQZ'), limit: money(1234567, 'NOTACODE'), exhausted: false } },
      },
      { name: 'unknown', role: 'fallback' },
    ],
  },
]
const quota = QUOTA_CASES.map((testCase) => ({
  ...testCase,
  now: NOW,
  text: core.buildClaudeQuotaSummary({ accounts: testCase.accounts, now: NOW }),
}))

// -- Cache keep-alive ---------------------------------------------------------
const PREWARM_BODIES = [
  {
    name: 'rewritten-hybrid-request',
    bodyText: JSON.stringify({
      model: 'claude-opus-4-7',
      max_tokens: 64_000,
      stream: true,
      thinking: { type: 'enabled', budget_tokens: 4096 },
      output_config: { format: { type: 'json_schema' } },
      tool_choice: { type: 'any' },
      system: [
        { type: 'text', text: 'x-anthropic-billing-header: cc_version=2.1.177.3bf; cc_entrypoint=cli; cch=abcde;' },
        { type: 'text', text: 'identity' },
        { type: 'text', text: 'stable', cache_control: { type: 'ephemeral', ttl: '1h' } },
      ],
      messages: [{ role: 'user', content: 'hello' }],
    }),
  },
  {
    name: 'adaptive-thinking-and-auto-tool-choice-kept',
    bodyText: JSON.stringify({
      messages: [{ role: 'user', content: [{ type: 'text', text: 'hi', cache_control: { type: 'ephemeral' } }] }],
      model: 'claude-opus-5',
      thinking: { type: 'adaptive' },
      tool_choice: { type: 'auto' },
      output_config: { effort: 'high' },
      speed: 'fast',
      stream: true,
      max_tokens: 1,
    }),
  },
  { name: 'no-breakpoints', bodyText: JSON.stringify({ model: 'claude-opus-4-7', messages: [{ role: 'user', content: 'hi' }] }) },
  { name: 'not-json', bodyText: '{"model":' },
]
const prewarmBodies = []
for (const testCase of PREWARM_BODIES) {
  prewarmBodies.push({ ...testCase, result: await core.buildCacheKeepPrewarmBody(testCase.bodyText) })
}

// The whole prewarm pi sends: a hybrid-cached request with keep-alive on,
// then the scheduler's tick 55 minutes later.
writeFileSync(
  settingsFile,
  `${JSON.stringify({ claudeCache: { enabled: true, mode: 'hybrid' }, cacheKeep: { enabled: true, always: true }, accounts: [] }, null, 2)}\n`,
)
rmSync(registryDir, { recursive: true, force: true })
mock.bootstrapUuid = ACCOUNT_UUID
mock.recorded = []
const trackedAt = Date.parse('2026-05-18T10:00:00.000Z')
fakeNow = trackedAt
const token = 'sk-ant-oat01-cachekeep-00-000000000000'
const context = {
  systemPrompt: 'You keep caches warm.',
  messages: [user('Remember this long and stable prefix, please.')],
}
const message = await streamCortexKitAnthropic(model('claude-opus-4-8'), context, {
  apiKey: token,
  sessionId: 'ses-cachekeep',
  reasoning: 'high',
}).result()
if (message.stopReason === 'error') throw new Error(`cachekeep request: ${message.errorMessage}`)
const original = mock.recorded.at(-1)
if (!original || ticks.length !== 1) throw new Error(`no tracked request (${ticks.length} ticks)`)
const registryFiles = readdirSync(registryDir).filter((name) => name.endsWith('.json'))
const registryRecord = readFileSync(join(registryDir, registryFiles[0] as string), 'utf8')
fakeNow = trackedAt + 54 * 60_000
ticks[0]?.()
await new Promise((resolve) => realSetInterval === undefined ? resolve(null) : setTimeout(resolve, 200))
const beforeLead = mock.recorded.length
fakeNow = trackedAt + 55 * 60_000
ticks[0]?.()
for (let waited = 0; mock.recorded.length === beforeLead && waited < 100; waited++) {
  await new Promise((resolve) => setTimeout(resolve, 20))
}
const prewarm = mock.recorded.at(-1)
if (!prewarm || mock.recorded.length !== beforeLead + 1) throw new Error('no prewarm request')
const strip = (headers: Record<string, string>) => {
  const { 'x-client-request-id': _requestId, ...rest } = headers
  return rest
}
const cachekeep = {
  token,
  model: 'claude-opus-4-8',
  sessionId: 'ses-cachekeep',
  trackedAt,
  identity: { deviceId: DEVICE_ID, accountUuid: ACCOUNT_UUID, sessionId: original.headers['x-claude-code-session-id'] },
  // Prewarms before the 55th minute: none.
  prewarmsAt54Minutes: beforeLead - 1,
  registryRecord,
  original: { url: original.url, headers: strip(original.headers), bodyText: original.body },
  prewarm: { url: prewarm.url, headers: strip(prewarm.headers), bodyText: prewarm.body },
}
fakeNow = null

process.stdout.write(
  `${JSON.stringify({ version: '2.1.280', commands, quota, prewarmBodies, cachekeep }, null, 2)}\n`,
)
process.exit(0)
