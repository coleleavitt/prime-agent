// The shared harness of the golden generators beside it (pi's own provider
// entry point against an in-process fetch): see `generate_requests.ts` for
// how to run them. Nothing here leaves the process.
import { mkdirSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

export const repo = process.env.ANTHROPIC_AUTH_REPO
if (!repo) throw new Error('ANTHROPIC_AUTH_REPO is required')
export const scratch = homedir()
export const settingsFile = join(scratch, 'pi-agent', 'anthropic-auth.json')
mkdirSync(join(scratch, 'pi-agent'), { recursive: true })
process.env.PI_ANTHROPIC_AUTH_FILE = settingsFile
process.env.PI_ANTHROPIC_AUTH_CACHEKEEP_REGISTRY_DIR = join(scratch, 'cachekeep')

export const ACCOUNT_UUID = '00000000-0000-4000-8000-000000000001'
export const DEVICE_ID = 'a'.repeat(64)

export const OK_STREAM = [
  { type: 'message_start', message: { id: 'msg_1', usage: { input_tokens: 1, output_tokens: 1 } } },
  { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
  { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'hello' } },
  { type: 'content_block_stop', index: 0 },
  { type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 2 } },
  { type: 'message_stop' },
]

export function sse(events: unknown[]) {
  return events
    .map((event) => `event: ${(event as { type: string }).type}\ndata: ${JSON.stringify(event)}\n\n`)
    .join('')
}

export type Recorded = { url: string; headers: Record<string, string>; body: string }
// Mutable through `mock`: ES module bindings are read-only to importers.
export const mock: {
  recorded: Recorded[]
  bootstrapUuid: string | null
  replies: Array<{ status: number; body: string }>
} = { recorded: [], bootstrapUuid: ACCOUNT_UUID, replies: [] }

globalThis.fetch = (async (input: unknown, init?: RequestInit) => {
  const url = String(input instanceof Request ? input.url : input)
  if (url.includes('/api/claude_cli/bootstrap')) {
    return mock.bootstrapUuid
      ? new Response(JSON.stringify({ oauth_account: { account_uuid: mock.bootstrapUuid } }), { status: 200 })
      : new Response('{}', { status: 403 })
  }
  if (url.includes('/v1/messages')) {
    const headers: Record<string, string> = {}
    new Headers(init?.headers).forEach((value, name) => {
      headers[name] = value
    })
    mock.recorded.push({ url, headers, body: String(init?.body) })
    const reply = mock.replies.shift() ?? { status: 200, body: sse(OK_STREAM) }
    return new Response(reply.body, {
      status: reply.status,
      headers: { 'content-type': reply.status === 200 ? 'text/event-stream' : 'application/json' },
    })
  }
  return new Response('{}', { status: 404 })
}) as typeof fetch

export const { streamCortexKitAnthropic } = await import(join(repo, 'packages/pi/src/stream.ts'))

export function model(id: string) {
  return {
    id,
    name: id,
    api: 'cortexkit-anthropic-messages',
    provider: 'anthropic',
    baseUrl: 'https://api.anthropic.com',
    reasoning: true,
    input: ['text', 'image'],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: 1_000_000,
    maxTokens: 128_000,
  }
}

const usage = {
  input: 0,
  output: 0,
  cacheRead: 0,
  cacheWrite: 0,
  totalTokens: 0,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
}
export function assistant(content: unknown[], api = 'anthropic-messages') {
  return { role: 'assistant', content, api, provider: 'anthropic', model: 'claude-x', usage, stopReason: 'stop', timestamp: 1 }
}
export function user(content: unknown) {
  return { role: 'user', content, timestamp: 1 }
}
export function toolResult(toolCallId: string, content: unknown[], isError = false) {
  return { role: 'toolResult', toolCallId, toolName: 't', content, isError, timestamp: 1 }
}
export const IMAGE = { type: 'image', data: 'iVBORw0KGgo=', mimeType: 'image/png' }
export const PI_PROMPT = [
  'You are an expert coding assistant operating inside pi.',
  'Available tools:\n- read: Read file contents',
  'Pi documentation (read only when the user asks about pi itself):\n- Main documentation: /docs/README.md',
  'Guidelines:\n- Be concise',
].join('\n\n')

export type Case = {
  name: string
  model: string
  context: Record<string, unknown>
  options?: Record<string, unknown>
  settings?: Record<string, unknown>
  accountUuid?: boolean
}

const outcomesFile = join(process.env.REFUSAL_LOG_DIR ?? join(scratch, 'refusal'), 'content-filter-outcomes.jsonl')

/** Run `cases` through pi's provider; each recorded request, and the
 * content-filter outcome pi logged for it. */
export async function runRequestCases(cases: Case[], tokenPrefix: string) {
  const { existsSync, readFileSync, rmSync } = await import('node:fs')
  const results = []
  for (const [index, testCase] of cases.entries()) {
    writeFileSync(settingsFile, `${JSON.stringify(testCase.settings ?? {}, null, 2)}\n`)
    mock.bootstrapUuid = testCase.accountUuid === false ? null : ACCOUNT_UUID
    mock.recorded = []
    rmSync(outcomesFile, { force: true })
    // One token per case: the plugin keeps one identity per token.
    const token = `sk-ant-oat01-${tokenPrefix}-${String(index).padStart(2, '0')}-000000000000`
    const stream = streamCortexKitAnthropic(model(testCase.model), testCase.context, {
      apiKey: token,
      sessionId: `ses-${testCase.name}`,
      ...(testCase.options ?? {}),
    })
    const message = await stream.result()
    if (message.stopReason === 'error') throw new Error(`${testCase.name}: ${message.errorMessage}`)
    const request = mock.recorded.at(-1)
    if (!request) throw new Error(`${testCase.name}: no Messages request`)
    const { 'x-client-request-id': _requestId, ...headers } = request.headers
    const outcome = existsSync(outcomesFile)
      ? JSON.parse(readFileSync(outcomesFile, 'utf8').trim().split('\n').at(-1) ?? 'null')
      : null
    results.push({
      name: testCase.name,
      model: testCase.model,
      context: testCase.context,
      options: testCase.options ?? {},
      settings: testCase.settings ?? {},
      token,
      identity: {
        deviceId: DEVICE_ID,
        accountUuid: testCase.accountUuid === false ? null : ACCOUNT_UUID,
        sessionId: headers['x-claude-code-session-id'],
      },
      url: request.url,
      headers,
      bodyText: request.body,
      filter: outcome?.filter ?? null,
    })
  }
  return results
}
