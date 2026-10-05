// The generator of `pi_requests.json` beside this file: the whole request
// the pi plugin sends for representative conversations, for
// `src/pi/convert/tests.rs` (and the modules after it) to compare with the
// request pa-anthropic-auth builds from the same conversation, byte for
// byte.
//
// Nothing is reimplemented here: each case runs pi's own provider entry
// point (`streamCortexKitAnthropic`, packages/pi/src/stream.ts) with the
// network replaced by an in-process `fetch` that records the Messages
// request and answers with a scripted stream. The settings file
// (`PI_ANTHROPIC_AUTH_FILE`) is written per case; the account store and
// the plugin's logs live under a scratch HOME; no request leaves the
// process. The Claude Code identity's account uuid comes from the mocked
// bootstrap endpoint, the device id from the scratch HOME's device.json;
// the per-token session id is the plugin's random one, recorded from the
// request's `x-claude-code-session-id`.
//
// To regenerate (bun; a scratch HOME; a checkout of anthropic-auth at the
// commit below with `packages/core` built to `dist/` and node_modules
// installed):
//   mkdir -p <scratch>/.anthropic-accounts <scratch>/tmp
//   printf '{"version":1,"device_id":"%s"}\n' "$(printf 'a%.0s' $(seq 64))" \
//     > <scratch>/.anthropic-accounts/device.json
//   chmod 700 <scratch>/.anthropic-accounts; chmod 600 <scratch>/.anthropic-accounts/device.json
//   env -i HOME=<scratch> TMPDIR=<scratch>/tmp PATH="$PATH" \
//     REFUSAL_LOG_DIR=<scratch>/refusal ANTHROPIC_AUTH_REPO=<the checkout> \
//     OPENCODE_ANTHROPIC_AUTH_DISABLE_VERSION_CHECK=1 \
//     bun generate_requests.ts > pi_requests.json
// Generated from anthropic-auth 7f5d88a ("pi: replay thinking signatures
// only from Anthropic-origin messages") on linux x64.
import { rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import {
  ACCOUNT_UUID,
  assistant,
  type Case,
  IMAGE,
  mock,
  model,
  OK_STREAM,
  PI_PROMPT,
  repo,
  runRequestCases,
  scratch,
  settingsFile,
  sse,
  streamCortexKitAnthropic,
  toolResult,
  user,
} from './golden_harness.ts'


export const CASES: Case[] = [
  {
    name: 'text-with-pi-prompt',
    model: 'claude-opus-4-8',
    context: {
      systemPrompt: PI_PROMPT,
      messages: [
        user('Say hello to the world, please.'),
        assistant([{ type: 'text', text: 'Hello, world.' }]),
        user([{ type: 'text', text: 'And once more.' }]),
      ],
    },
    options: { reasoning: 'high' },
  },
  {
    name: 'tool-use-and-results',
    model: 'claude-sonnet-4-5',
    context: {
      systemPrompt: 'You help with files.',
      tools: [
        { name: 'read', description: 'Read a file', parameters: { type: 'object', properties: { path: { type: 'string' } }, required: ['path'] } },
        { name: 'deep_research', description: 'Research', parameters: { type: 'object', properties: { q: { type: 'string' } } } },
        { name: 'my_tool', description: 'Custom', parameters: { type: 'object' } },
      ],
      messages: [
        user('Read the config file.'),
        assistant([
          { type: 'text', text: 'Reading it.' },
          { type: 'toolCall', id: 'call_1|fc 1', name: 'read', arguments: { path: 'config.toml' } },
          { type: 'toolCall', id: 'call_2', name: 'deep_research', arguments: { q: 'x' } },
        ]),
        toolResult('call_1|fc 1', [{ type: 'text', text: 'key = 1' }]),
        toolResult('call_unknown', [{ type: 'text', text: 'stray' }]),
        toolResult('call_2', [], true),
        user('Now run something.'),
        assistant([{ type: 'toolCall', id: 'orphan', name: 'my_tool', arguments: {} }]),
        user('Never mind; summarize.'),
      ],
    },
    options: { reasoning: 'medium', thinkingBudgets: { medium: 3000 }, maxTokens: 2048 },
  },
  {
    name: 'images',
    model: 'claude-haiku-4-5',
    context: {
      messages: [
        user([IMAGE]),
        assistant([{ type: 'text', text: 'A picture.' }]),
        user([{ type: 'text', text: 'Compare with this:' }, IMAGE]),
        assistant([{ type: 'toolCall', id: 'shot', name: 'screenshot', arguments: {} }]),
        toolResult('shot', [IMAGE]),
      ],
    },
  },
  {
    name: 'thinking-blocks',
    model: 'claude-opus-4-8',
    context: {
      systemPrompt: 'You think.',
      messages: [
        user('First question.'),
        assistant([
          { type: 'thinking', thinking: 'Signed reasoning.', thinkingSignature: 'EqQBCkgIBRABGAIiQ' },
          { type: 'thinking', thinking: 'Unsigned reasoning.' },
          { type: 'thinking', thinking: '   ' },
          { type: 'text', text: 'Answer one.' },
        ]),
        user('Second question.'),
        assistant(
          [
            { type: 'thinking', thinking: 'Foreign reasoning.', thinkingSignature: 'reasoning_content' },
            { type: 'thinking', thinking: 'Encrypted.', thinkingSignature: 'gAAAAABencrypted' },
            { type: 'text', text: 'Answer two.' },
          ],
          'openai-completions',
        ),
        user('Third question.'),
        assistant([{ type: 'text', text: 'A trailing assistant turn.' }]),
      ],
    },
    options: { reasoning: 'xhigh' },
  },
  {
    name: 'cache-hybrid',
    model: 'claude-sonnet-5',
    context: {
      systemPrompt: 'You cache.',
      tools: [{ name: 'bash', description: 'Run', parameters: { type: 'object', properties: { cmd: { type: 'string' } }, required: ['cmd'] } }],
      messages: [user('Cache this, please.')],
    },
    options: { reasoning: 'off' },
    settings: { claudeCache: { enabled: true, mode: 'hybrid' } },
  },
  {
    name: 'cache-automatic',
    model: 'claude-opus-4-6',
    context: { messages: [user('Automatic caching.')] },
    options: { reasoning: 'max' },
    settings: { claudeCache: { enabled: true, mode: 'automatic' } },
  },
  {
    name: 'cache-explicit-fast',
    model: 'claude-opus-4-8',
    context: { systemPrompt: 'Fast.', messages: [user('Go fast.')] },
    settings: { claudeCache: { enabled: true }, claudeFast: { enabled: true } },
  },
  {
    name: 'fast-on-an-unsupported-model',
    model: 'claude-sonnet-4-6',
    context: { messages: [user('Fast where it is not.')] },
    options: { reasoning: 'xhigh' },
    settings: { claudeFast: { enabled: true }, claudeCache: { enabled: false, mode: 'hybrid' } },
  },
  {
    name: 'no-account-uuid',
    model: 'claude-opus-4-5',
    context: { messages: [user('No identity.')] },
    options: { reasoning: 'off' },
    accountUuid: false,
  },
  {
    name: 'server-fallback-opus-5-5',
    model: 'claude-opus-5-5',
    context: { systemPrompt: 'You help.', messages: [user('Say hello to the world, please.')] },
    options: { reasoning: 'off' },
  },
  {
    name: 'server-fallback-fable-5-snapshot',
    model: 'claude-fable-5-20260601',
    context: { messages: [user('Fable.')] },
    options: { reasoning: 'high' },
    settings: { claudeFast: { enabled: true } },
  },
  {
    name: 'fallback-markers-replayed',
    model: 'claude-opus-5',
    context: {
      messages: [
        user('First.'),
        assistant([
          { type: 'thinking', thinking: '\u2060', thinkingSignature: 'cortexkit-server-fallback-v1:claude-opus-5|claude-opus-4-8' },
          { type: 'thinking', thinking: '\u2060', thinkingSignature: 'cortexkit-server-fallback-v1:not a model|claude-opus-4-8' },
          { type: 'text', text: 'Served by the fallback.' },
        ]),
        user('Second.'),
      ],
    },
  },
  {
    name: 'fallback-markers-dropped-off-a-fallback-model',
    model: 'claude-sonnet-4-6',
    context: {
      messages: [
        user('First.'),
        assistant([
          { type: 'thinking', thinking: '\u2060', thinkingSignature: 'cortexkit-server-fallback-v1:claude-opus-5|claude-opus-4-8' },
          { type: 'text', text: 'Served by the fallback.' },
        ]),
        assistant([
          { type: 'thinking', thinking: '\u2060', thinkingSignature: 'cortexkit-server-fallback-v1:claude-opus-5|claude-opus-4-8' },
          { type: 'text', text: 'From another provider.' },
        ], 'openai-responses'),
        user('Second.'),
      ],
    },
  },
  {
    name: 'blank-turns-and-docs-only-prompt',
    model: 'claude-opus-4-8',
    context: {
      systemPrompt: 'Pi documentation: see /docs.',
      messages: [user('   '), user([]), user('Real question.')],
    },
    options: { maxTokens: 1 , reasoning: 'low'},
  },
]

const results = await runRequestCases(CASES, 'golden-case')

// What pi keeps of a streamed response (the assistant message content).
type ResponseCase = {
  name: string
  model: string
  context: Record<string, unknown>
  events: unknown[]
}
const RESPONSES: ResponseCase[] = [
  {
    name: 'a-server-fallback-block',
    model: 'claude-opus-5-5',
    context: { messages: [user('Hi.')] },
    events: [
      OK_STREAM[0],
      { type: 'content_block_start', index: 0, content_block: { type: 'fallback', from: { model: 'claude-opus-5-5' }, to: { model: 'claude-opus-4-8' } } },
      { type: 'content_block_stop', index: 0 },
      { type: 'content_block_start', index: 1, content_block: { type: 'text', text: '' } },
      { type: 'content_block_delta', index: 1, delta: { type: 'text_delta', text: 'hello' } },
      { type: 'content_block_stop', index: 1 },
      ...OK_STREAM.slice(4),
    ],
  },
  {
    name: 'an-unsafe-fallback-block-is-skipped',
    model: 'claude-opus-5-5',
    context: { messages: [user('Hi.')] },
    events: [
      OK_STREAM[0],
      { type: 'content_block_start', index: 0, content_block: { type: 'fallback', from: { model: 'claude-opus-5-5' }, to: { model: 'gpt-5' } } },
      { type: 'content_block_stop', index: 0 },
      ...OK_STREAM.slice(1),
    ],
  },
  {
    name: 'the-research-tool-alias-is-restored',
    model: 'claude-opus-4-8',
    context: {
      messages: [user('Research.')],
      tools: [{ name: 'deep_research', description: 'Research', parameters: { type: 'object', properties: {} } }],
    },
    events: [
      OK_STREAM[0],
      { type: 'content_block_start', index: 0, content_block: { type: 'tool_use', id: 'toolu_1', name: 'prime_deep_research', input: {} } },
      { type: 'content_block_delta', index: 0, delta: { type: 'input_json_delta', partial_json: '{"q":"x"}' } },
      { type: 'content_block_stop', index: 0 },
      { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 2 } },
      { type: 'message_stop' },
    ],
  },
]
const responses = []
for (const [index, testCase] of RESPONSES.entries()) {
  writeFileSync(settingsFile, '{}\n')
  mock.bootstrapUuid = ACCOUNT_UUID
  mock.replies = [{ status: 200, body: sse(testCase.events) }]
  const stream = streamCortexKitAnthropic(model(testCase.model), testCase.context, {
    apiKey: `sk-ant-oat01-golden-response-${String(index).padStart(2, '0')}-0000000000`,
    sessionId: `ses-${testCase.name}`,
  })
  const message = await stream.result()
  if (message.stopReason === 'error') throw new Error(`${testCase.name}: ${message.errorMessage}`)
  responses.push({
    name: testCase.name,
    model: testCase.model,
    context: testCase.context,
    sse: sse(testCase.events),
    content: message.content,
    stopReason: message.stopReason,
  })
}

// pi's settings commands (packages/pi/src/commands.ts) against the
// settings file: what each prints and what the file holds after it.
const { registerCommands } = await import(join(repo, 'packages/pi/src/commands.ts'))
const handlers = new Map<string, (args: string, ctx: unknown) => Promise<void>>()
registerCommands({
  registerCommand: (name: string, def: { handler: (args: string, ctx: unknown) => Promise<void> }) => {
    handlers.set(name, def.handler)
  },
})
const { existsSync, readFileSync } = await import('node:fs')
const stateFile = join(scratch, 'pi-agent', 'anthropic-auth-state.json')
type CommandRun = { command: string; args: string }
const COMMAND_SEQUENCES: Array<{ name: string; initial: string | null; runs: CommandRun[] }> = [
  {
    name: 'fast-from-no-file',
    initial: null,
    runs: [
      { command: 'claude-fast', args: '' },
      { command: 'claude-fast', args: 'on' },
      { command: 'claude-fast', args: ' on ' },
      { command: 'claude-fast', args: 'off' },
      { command: 'claude-fast', args: 'on now' },
    ],
  },
  {
    name: 'fast-over-an-existing-file',
    initial: `${JSON.stringify({ routing: { mode: 'sticky' }, claudeFast: { enabled: false, note: 'kept' }, accounts: [], custom: [1, 2] }, null, 2)}\n`,
    runs: [{ command: 'claude-fast', args: 'on' }],
  },
  {
    name: 'cache-from-no-file',
    initial: null,
    runs: [
      { command: 'claude-cache', args: '' },
      { command: 'claude-cache', args: 'mode hybrid' },
      { command: 'claude-cache', args: 'on' },
      { command: 'claude-cache', args: 'mode automatic' },
      { command: 'claude-cache', args: 'mode bogus' },
      { command: 'claude-cache', args: 'off' },
    ],
  },
]
const commands = []
for (const sequence of COMMAND_SEQUENCES) {
  rmSync(settingsFile, { force: true })
  rmSync(stateFile, { force: true })
  if (sequence.initial !== null) writeFileSync(settingsFile, sequence.initial)
  const steps = []
  for (const run of sequence.runs) {
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

process.stdout.write(`${JSON.stringify({ version: '2.1.280', cases: results, responses, commands }, null, 2)}\n`)
