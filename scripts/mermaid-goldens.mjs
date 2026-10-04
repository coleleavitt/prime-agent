#!/usr/bin/env node
// Regenerates the Mermaid parity goldens from the TS product's own dependencies:
//
//   node scripts/mermaid-goldens.mjs <grok-mermaid-0.2.3 package dir> <marked-18 package dir>
//
// (`npm pack grok-mermaid@0.2.3 marked@18` and untar each, or point at a node_modules copy.)
//
// - crates/pa-tui/src/mermaid/goldens.json: grok-mermaid's `render()` for each case's `src`
//   (styled rows, width, warnings), the renderer the TS v0.9.8 product shipped.
// - crates/pa-tui/src/markdown/mermaid_transform_goldens.json: the TS v0.9.8
//   `components/mermaid.ts` transform (verbatim, themeless) over each case's assistant text,
//   and the block structure marked lexes from the rewritten markdown.
//
// Inputs are read back from the files themselves; edit a case's input there and rerun.
import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const [grokDir, markedDir] = process.argv.slice(2)
if (!grokDir || !markedDir) {
  console.error('usage: node scripts/mermaid-goldens.mjs <grok-mermaid dir> <marked dir>')
  process.exit(2)
}
const { render } = await import(pathToFileURL(join(grokDir, 'dist/index.js')).href)
const { Marked } = await import(pathToFileURL(join(markedDir, 'lib/marked.esm.js')).href)
const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const writeCases = (path, cases) =>
  writeFileSync(path, `[\n${cases.map((c) => JSON.stringify(c)).join(',\n')}\n]\n`)

const artOf = (src) => {
  const art = render(src)
  return art === null
    ? null
    : {
        width: art.width,
        warnings: art.warnings,
        rows: art.styled.map((row) => row.map((s) => [s.cls, s.text])),
        plain: art.plain,
      }
}
const rendererPath = join(root, 'crates/pa-tui/src/mermaid/goldens.json')
writeCases(
  rendererPath,
  JSON.parse(readFileSync(rendererPath, 'utf8')).map(({ name, src }) => ({ name, src, art: artOf(src) })),
)

// ---- the TS v0.9.8 transform, verbatim but for the theme (components/mermaid.ts)
const markdownParser = new Marked()
const isMermaid = (token) =>
  token.type === 'code' && token.lang?.trim().split(/\s+/, 1)[0]?.toLowerCase() === 'mermaid'
function codeSpan(line) {
  const content = line || ' '
  const longestBacktickRun = Math.max(0, ...Array.from(content.matchAll(/`+/g), (m) => m[0].length))
  const fence = '`'.repeat(longestBacktickRun + 1)
  const padding = content.startsWith('`') || content.endsWith('`') ? ' ' : ''
  return `${fence}${padding}${content}${padding}${fence}`
}
function transform(markdown, availableWidth, isStreaming, mode) {
  if (mode === 'off' || (isStreaming && mode !== 'streaming')) return markdown
  return markdownParser
    .lexer(markdown)
    .map((token) => {
      if (!isMermaid(token)) return token.raw
      const art = render(token.text)
      if (!art || art.width > availableWidth) return token.raw
      if (!isStreaming && art.warnings.length > 0) {
        const suffix = art.warnings.length > 1 ? ` (+${art.warnings.length - 1} more)` : ''
        return `${token.raw}\n${codeSpan(`Mermaid diagram not rendered: ${art.warnings[0]}${suffix}`)}  \n`
      }
      return `${art.plain.map(codeSpan).join('  \n')}\n`
    })
    .join('')
}

// ---- what marked lexes from the rewritten markdown: [type, blank line before, row texts]
const inlineText = (tokens) =>
  tokens
    .map((t) => (t.type === 'codespan' || t.type === 'text' ? t.text : t.type === 'br' ? '\n' : t.raw))
    .join('')
function structure(md) {
  const out = []
  let blank = false
  for (const t of markdownParser.lexer(md)) {
    if (t.type === 'space') {
      blank = true
      continue
    }
    const rows =
      t.type === 'paragraph'
        ? inlineText(t.tokens).split('\n')
        : t.type === 'code'
          ? t.text.split('\n')
          : t.type === 'heading'
            ? [t.text]
            : t.type === 'list'
              ? t.items.map((i) => i.text)
              : [t.raw]
    out.push([t.type, blank, rows])
    blank = false
  }
  return out
}

const transformPath = join(root, 'crates/pa-tui/src/markdown/mermaid_transform_goldens.json')
writeCases(
  transformPath,
  JSON.parse(readFileSync(transformPath, 'utf8')).map(({ name, text, width, streaming, mode }) => {
    const transformed = transform(text, width, streaming, mode)
    return { name, text, width, streaming, mode, transformed, blocks: structure(transformed) }
  }),
)
