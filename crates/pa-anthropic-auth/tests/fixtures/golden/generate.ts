// The generator of `request_shape.json` beside this file: what the pi plugin
// (anthropic-auth `packages/pi`, and the `packages/core` it builds requests
// with) sends on an OAuth Messages request, for `src/shape/tests.rs` to
// compare with the request shape pa-anthropic-auth applies.
//
// Every value comes from the plugin's own code: pi's `buildAnthropicRequest`
// (the billing system block, `metadata.user_id`, Claude Code's body key
// order) and core's `applyClaudeCodeHeaders` (the header set pi's
// `sendAnthropicRequestUnrecorded` builds on a fresh `Headers`),
// `selectClaudeCodeBetas`, `buildBillingHeaderValue`,
// `getClaudeCodeUserAgent` and `buildClaudeCodeMetadataUserId`. Nothing here
// touches a store, a credential file or the network: the identity is passed
// in (pi resolves it from the device file and the bootstrap endpoint), and
// the version check is off (the compiled floor is the version).
//
// To regenerate (bun; a sandbox HOME; the plugin repo at
// ANTHROPIC_AUTH_REPO, default ~/WebstormProjects/forks/anthropic-auth under
// the real HOME, with `packages/core` built to `dist/`):
//   env -i HOME=<empty dir> PATH="$PATH" ANTHROPIC_AUTH_REPO=<the plugin repo> \
//     OPENCODE_ANTHROPIC_AUTH_DISABLE_VERSION_CHECK=1 bun generate.ts > request_shape.json
// Generated from anthropic-auth 7f5d88a ("pi: replay thinking signatures only
// from Anthropic-origin messages") on linux x64 (the stainless os/arch the
// Rust tests replace with the host's).
import { homedir } from 'node:os'
import { join } from 'node:path'

const repo =
  process.env.ANTHROPIC_AUTH_REPO ??
  join(homedir(), 'WebstormProjects/forks/anthropic-auth')
// The plugin's own module graph: pi's converter imports the core package
// (its `dist/`), so the helpers come from the same module instance.
const core = await import(join(repo, 'packages/core/dist/index.js'))
const pi = await import(join(repo, 'packages/pi/src/convert.ts'))

const TOKEN = 'sk-ant-oat01-golden-golden-golden-golden-00'
const identity = {
  deviceId: 'a'.repeat(64),
  accountUuid: '00000000-0000-4000-8000-000000000001',
  sessionId: '11111111-2222-4333-8444-555555555555',
}
const anonymous = { deviceId: 'b'.repeat(64), sessionId: 'ses-anonymous' }

/** The headers pi sends (fresh `Headers` + the request's own betas). */
function headersFor(
  body: Record<string, unknown>,
  id: typeof identity | typeof anonymous,
  incomingBetas?: string,
) {
  const headers = new Headers()
  if (incomingBetas) headers.set('anthropic-beta', incomingBetas)
  core.applyClaudeCodeHeaders(headers, TOKEN, { body, identity: id })
  const out: Record<string, string> = {}
  for (const [name, value] of headers) {
    // Minted per request.
    if (name !== 'x-client-request-id') out[name] = value
  }
  return out
}

const fullAgent = {
  model: 'claude-sonnet-5',
  tools: [{ name: 't' }],
  system: [],
  thinking: { type: 'adaptive' },
  context_management: {},
  output_config: { effort: 'high' },
  diagnostics: {},
}
const betaBodies = [
  { model: 'claude-opus-5-5', stream: true },
  { model: 'claude-haiku-4-5' },
  { model: 'claude-haiku-4-5[1m]' },
  { model: 'claude-opus-4-8-20260101', speed: 'fast' },
  fullAgent,
  { model: 'm', output_config: { format: { type: 'json_schema' } } },
  { model: 'claude-sonnet-4-5', output_config: { effort: 'max' } },
]
const extras = [[], ['claude-code-20250219', 'oauth-2025-04-20', 'x-extra']]

const billingInputs = [
  [{ role: 'user', content: 'Say hello to the world, please.' }],
  [
    { role: 'user', isMeta: true, content: 'meta text first' },
    {
      role: 'user',
      content: [
        { type: 'image', source: {} },
        { type: 'text', text: 'Read the file at path one' },
        { type: 'text', text: 'second block' },
      ],
    },
  ],
  [{ role: 'user', content: 'héllo wörld \u{1F600} and more text here' }],
  [{ role: 'assistant', content: 'no user' }],
  [{ role: 'user', content: 'abc' }],
]

const userAgentEnvs = [
  {},
  { CLAUDE_CODE_ENTRYPOINT: 'sdk-ts' },
  {
    CLAUDE_CODE_ENTRYPOINT: ' ',
    CLAUDE_AGENT_SDK_VERSION: '0.2.1',
    CLAUDE_AGENT_SDK_CLIENT_APP: 'app',
  },
]
const userAgent = userAgentEnvs.map((env) => {
  const saved = { ...process.env }
  Object.assign(process.env, env)
  const ua = core.getClaudeCodeUserAgent('2.1.280')
  for (const key of Object.keys(env)) {
    if (saved[key] === undefined) delete process.env[key]
    else process.env[key] = saved[key]
  }
  return { env, userAgent: ua }
})

// One request through pi's converter: a system prompt and one user turn.
const context = {
  systemPrompt: 'You help.',
  messages: [
    { role: 'user', content: 'Say hello to the world, please.', timestamp: 0 },
  ],
}
const built = await pi.buildAnthropicRequest(
  'claude-opus-5-5',
  context,
  { maxTokens: 32000 },
  { enabled: false, mode: 'hybrid' },
  false,
  identity,
)
const piBody = JSON.parse(built.bodyText)

process.stdout.write(
  `${JSON.stringify(
    {
      version: core.getCachedClaudeCodeVersion(),
      token: TOKEN,
      identity,
      headers: [
        {
          body: { model: 'claude-opus-5-5', stream: true },
          identity,
          incomingBetas: 'claude-code-20250219,oauth-2025-04-20',
          headers: headersFor(
            { model: 'claude-opus-5-5', stream: true },
            identity,
            'claude-code-20250219,oauth-2025-04-20',
          ),
        },
        {
          body: piBody,
          identity,
          incomingBetas: '',
          headers: headersFor(piBody, identity),
        },
      ],
      betas: betaBodies.flatMap((body) =>
        extras.map((extra) => ({
          body,
          extra,
          betas: core.selectClaudeCodeBetas(body, extra),
        })),
      ),
      billing: billingInputs.flatMap((messages) =>
        ['2.1.280', '2.1.300'].map((version) => ({
          messages,
          version,
          text: core.buildBillingHeaderValue(messages, version, 'cli'),
        })),
      ),
      userAgent,
      metadata: [
        { identity, userId: core.buildClaudeCodeMetadataUserId(identity) },
        {
          identity: anonymous,
          userId: core.buildClaudeCodeMetadataUserId(anonymous),
        },
      ],
      piRequest: {
        model: 'claude-opus-5-5',
        context,
        bodyText: built.bodyText,
        keys: Object.keys(piBody),
        system0: piBody.system[0],
        metadata: piBody.metadata,
      },
    },
    null,
    2,
  )}\n`,
)
