#!/usr/bin/env node
// Regenerates the fork's Mermaid parity goldens (crates/pa-mermaid) from the TS fork's own
// dependencies (perf/session-catalog-resume, a358fd19e):
//
//   node scripts/lovely-mermaid-goldens.mjs <lovely-mermaid-0.3.3 package dir> <marked-18 package dir>
//
// (`npm pack lovely-mermaid@0.3.3 marked@18.0.7` and untar each.)
//
// - crates/pa-mermaid/src/render/goldens.json: lovely-mermaid's `render()` for each case's
//   `src`: rows as [role, text] runs (adjacent spans of one role merged: the TS product styles
//   by role only, so author classes and links, which only split runs, are not carried), width,
//   warnings.
// - crates/pa-mermaid/src/policy_goldens.json: the fork's `layoutMermaid` (components/mermaid.ts,
//   verbatim) for each case's `src`, `width` and `streaming`. A case with `widthRule` derives
//   its width from the flowchart turned a quarter: `rotated` is the rotated art's width,
//   `rotated-1` one column short of it.
// - crates/pa-tui/src/markdown/diagram_transform_goldens.json: the fork's
//   `createMermaidMarkdownTransform` (verbatim, themeless) over each case's text, the layout
//   each fence got, and the block structure marked lexes from the rewritten markdown.
// - crates/pa-tui/src/custom_message/diagram_text_goldens.json: the fork's
//   `createMermaidTextRenderer` (verbatim, themeless) over each case's plain text: the
//   segments, and the layout each fence got.
//
// Inputs are read back from the files themselves; edit a case's input there and rerun.
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const [lovelyDir, markedDir] = process.argv.slice(2)
if (!lovelyDir || !markedDir) {
  console.error('usage: node scripts/lovely-mermaid-goldens.mjs <lovely-mermaid dir> <marked dir>')
  process.exit(2)
}
const { render, diagramKind } = await import(pathToFileURL(join(lovelyDir, 'dist/index.js')).href)
const { Marked } = await import(pathToFileURL(join(markedDir, 'lib/marked.esm.js')).href)
const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const writeCases = (path, cases) =>
  writeFileSync(path, `[\n${cases.map((c) => JSON.stringify(c)).join(',\n')}\n]\n`)
const readCases = (path) => JSON.parse(readFileSync(path, 'utf8'))
/** Regenerate one golden file in place; a file not in this tree is skipped. */
const regenerate = (path, generate) => {
  if (existsSync(path)) writeCases(path, readCases(path).map(generate))
}

/** Styled rows as [role, text] runs, adjacent runs of one role merged. */
const roleRows = (art) =>
  art.styled.map((row) => {
    const out = []
    for (const span of row) {
      const last = out.at(-1)
      if (last && last[0] === span.role) last[1] += span.text
      else out.push([span.role, span.text])
    }
    return out
  })
const artOf = (src) => {
  const art = render(src)
  return art === null
    ? null
    : { width: art.width, warnings: art.warnings, rows: roleRows(art), plain: art.plain }
}
const rendererPath = join(root, 'crates/pa-mermaid/src/render/goldens.json')
regenerate(rendererPath, ({ name, src }) => ({ name, src, art: artOf(src) }))

// ---- the fork's components/mermaid.ts (a358fd19e), verbatim but for the theme
const markdownParser = new Marked()
const isMermaid = (token) =>
  token.type === 'code' && token.lang?.trim().split(/\s+/, 1)[0]?.toLowerCase() === 'mermaid'
const FLOWCHART_HEADER = /^(\s*(?:flowchart|graph))(?:([ \t]+)(TB|TD|BT|LR|RL))?(?=[\s;]|%%|$)/i
function rotateFlowchart(source) {
  const lines = source.split('\n')
  let index = 0
  while (index < lines.length && lines[index].trim() === '') index++
  if (lines[index]?.trim() === '---') {
    index++
    while (index < lines.length && lines[index].trim() !== '---') index++
    index++
  }
  while (index < lines.length && (lines[index].trim() === '' || lines[index].trim().startsWith('%%'))) index++
  const header = lines[index]
  const match = header === undefined ? null : FLOWCHART_HEADER.exec(header)
  if (!header || !match) return undefined
  const current = match[3]?.toUpperCase() ?? 'TB'
  const direction = current === 'LR' || current === 'RL' ? 'TD' : 'LR'
  lines[index] = `${match[1]} ${direction}${header.slice(match[0].length)}`
  return { source: lines.join('\n'), direction }
}
function describeWarnings(warnings) {
  const suffix = warnings.length > 1 ? ` (+${warnings.length - 1} more)` : ''
  return `Mermaid diagram incomplete: ${warnings[0]}${suffix}`
}
function layoutMermaid(source, availableWidth, isStreaming) {
  const notices = []
  const art = render(source)
  let chosen = art && art.width <= availableWidth ? art : undefined
  let neededWidth = art?.width
  if (!chosen && art) {
    const rotated = rotateFlowchart(source)
    const rotatedArt = rotated ? render(rotated.source) : null
    if (rotated && rotatedArt) {
      neededWidth = Math.min(art.width, rotatedArt.width)
      if (rotatedArt.width <= availableWidth) {
        chosen = rotatedArt
        if (!isStreaming) {
          const axis = rotated.direction === 'LR' ? 'left to right' : 'top to bottom'
          notices.push({ level: 'info', text: `Mermaid diagram drawn ${axis} to fit ${availableWidth} columns` })
        }
      }
    }
  }
  if (chosen) {
    if (!isStreaming && chosen.warnings.length > 0) {
      notices.push({ level: 'warning', text: describeWarnings(chosen.warnings) })
    }
    return { kind: 'art', art: chosen, notices }
  }
  if (!isStreaming) {
    if (neededWidth !== undefined) {
      notices.push({
        level: 'warning',
        text: `Mermaid diagram not drawn: needs ${neededWidth} columns, ${availableWidth} available`,
      })
    } else if (diagramKind(source) !== null) {
      notices.push({ level: 'warning', text: 'Mermaid diagram not drawn: no statement could be parsed' })
    } else {
      const header = source.trim().split(/\s+/, 1)[0]
      if (header) {
        notices.push({ level: 'info', text: `Mermaid diagram not drawn: ${header} is not supported in the terminal` })
      }
    }
  }
  return { kind: 'source', notices }
}
const artRows = (art) => art.plain
const noticeText = (notice) => notice.text
function codeSpan(line) {
  const content = line || ' '
  const longestBacktickRun = Math.max(0, ...Array.from(content.matchAll(/`+/g), (m) => m[0].length))
  const fence = '`'.repeat(longestBacktickRun + 1)
  const padding = content.startsWith('`') || content.endsWith('`') ? ' ' : ''
  return `${fence}${padding}${content}${padding}${fence}`
}
const isActive = (mode, isStreaming) => mode !== 'off' && (!isStreaming || mode === 'streaming')
function transform(markdown, availableWidth, isStreaming, mode, record) {
  if (!isActive(mode, isStreaming)) return markdown
  const tokens = markdownParser.lexer(markdown)
  return tokens
    .map((token, index) => {
      if (!isMermaid(token)) return token.raw
      const layout = layoutMermaid(token.text, availableWidth, isStreaming)
      record(token.text, availableWidth, isStreaming, layout)
      const notices = layout.notices.map((notice) => codeSpan(noticeText(notice)))
      const next = tokens[index + 1]
      const end = next && next.type !== 'space' ? '\n\n' : '\n'
      if (layout.kind === 'source') {
        if (notices.length === 0) return token.raw
        return `${token.raw.replace(/\n*$/, '\n')}${notices.join('  \n')}${end}`
      }
      return `${[...artRows(layout.art).map(codeSpan), ...notices].join('  \n')}${end}`
    })
    .join('')
}
function textRender(text, availableWidth, mode, record) {
  if (!isActive(mode, false) || !text.includes('mermaid')) return undefined
  const segments = []
  let pending = ''
  let changed = false
  for (const token of markdownParser.lexer(text)) {
    if (!isMermaid(token)) {
      pending += token.raw
      continue
    }
    const layout = layoutMermaid(token.text, availableWidth, false)
    record(token.text, availableWidth, false, layout)
    const notices = layout.notices.map((notice) => `${noticeText(notice)}\n`).join('')
    changed ||= layout.kind === 'art' || notices !== ''
    if (layout.kind === 'source') {
      pending += `${token.raw.replace(/\n*$/, '\n')}${notices}`
      continue
    }
    if (pending) segments.push({ kind: 'text', text: pending.replace(/\n$/, '') })
    segments.push({ kind: 'rows', rows: artRows(layout.art) })
    pending = notices
  }
  if (pending) segments.push({ kind: 'text', text: pending.replace(/\n+$/, '') })
  return changed ? segments : undefined
}

/** A layout as the goldens carry it: the art's role runs, or the source; the notices. */
const layoutJson = (layout) =>
  layout.kind === 'art'
    ? { kind: 'art', rows: roleRows(layout.art), width: layout.art.width, notices: layout.notices }
    : { kind: 'source', notices: layout.notices }

const policyPath = join(root, 'crates/pa-mermaid/src/policy_goldens.json')
regenerate(policyPath, ({ name, src, width, widthRule, streaming }) => {
    if (widthRule !== undefined) {
      const rotated = render(rotateFlowchart(src).source).width
      width = widthRule === 'rotated' ? rotated : rotated - 1
    }
    const layout = layoutJson(layoutMermaid(src, width, streaming))
    return { name, src, width, ...(widthRule === undefined ? {} : { widthRule }), streaming, layout }
})

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

const recorder = () => {
  const fences = []
  const record = (src, width, streaming, layout) =>
    fences.push({ src, width, streaming, layout: layoutJson(layout) })
  return { fences, record }
}

const transformPath = join(root, 'crates/pa-tui/src/markdown/diagram_transform_goldens.json')
regenerate(transformPath, ({ name, text, width, streaming, mode }) => {
  const { fences, record } = recorder()
  const transformed = transform(text, width, streaming, mode, record)
  return { name, text, width, streaming, mode, fences, transformed, blocks: structure(transformed) }
})

const textPath = join(root, 'crates/pa-tui/src/custom_message/diagram_text_goldens.json')
regenerate(textPath, ({ name, text, width, mode }) => {
  const { fences, record } = recorder()
  const segments = textRender(text, width, mode, record) ?? null
  return { name, text, width, mode, fences, segments }
})
