/**
 * Lightweight Markdown source cleanup for model/translation output.
 *
 * CommonMark collapses single newlines inside paragraphs to spaces, so
 * translations that use one `\n` between paragraphs or Chinese-style list
 * markers (1、 / •) often render as a single blob. This pass repairs structure
 * outside fenced code without a full re-parse, keeping O(n) cost.
 */

const FENCE_MARKER = /^[ \t]{0,3}(`{3,}|~{3,})(.*)$/u
const ATX_HEADING = /^[ \t]{0,3}#{1,6}(?:[ \t]|$)/u
const BLOCKQUOTE = /^[ \t]{0,3}>/u
const TABLE_ROW = /^[ \t]{0,3}\|/u
const HORIZONTAL_RULE = /^[ \t]{0,3}(?:-{3,}|\*{3,}|_{3,})[ \t]*$/u
const SETEXT_UNDERLINE = /^[ \t]{0,3}(?:=+|-+)[ \t]*$/u
/** GFM / CommonMark list markers, plus common Chinese translation markers. */
const LIST_LINE =
  /^[ \t]{0,3}(?:[-*+•·●○](?:[ \t]+|$)|(?:\d{1,3}(?:[.)]|、|．)|[（(]\d{1,3}[）)])(?:[ \t]+|$))/u
const ORDERED_CN = /^([ \t]{0,3})(\d{1,3})([、．])([ \t]*)(.*)$/u
const ORDERED_PAREN = /^([ \t]{0,3})[（(](\d{1,3})[）)]([ \t]*)(.*)$/u
const UNORDERED_BULLET = /^([ \t]{0,3})([•·●○])([ \t]+)(.*)$/u
/** Sentence-like endings used to decide when a single newline is a paragraph break. */
const SENTENCE_END = /(?:[.!?…。！？；;]|\u2026)["'”’」』》）)\]]*$/u

type LineKind = 'blank' | 'list' | 'table' | 'heading' | 'quote' | 'hr' | 'fence' | 'prose'

export function normalizeMarkdownSource(source: string): string {
  if (!source) return source

  // The transport has already decoded JSON. Literal escapes are content,
  // especially inside code, regular expressions and Windows paths.
  const text = source.replace(/\r\n?/gu, '\n')
  const lines = text.split('\n')
  const out: string[] = []
  let fenceChar: string | null = null
  let fenceLength = 0
  let previousKind: LineKind = 'blank'
  let previousLine = ''

  const emitBlank = (): void => {
    if (out.length === 0 || previousKind === 'blank') return
    out.push('')
    previousKind = 'blank'
    previousLine = ''
  }

  const emitLine = (line: string, kind: LineKind): void => {
    out.push(line)
    previousKind = kind
    previousLine = line
  }

  for (const raw of lines) {
    if (fenceChar !== null) {
      emitLine(raw, 'fence')
      if (isMatchingFenceClose(raw, fenceChar, fenceLength)) {
        fenceChar = null
      }
      continue
    }

    const fenceMatch = raw.match(FENCE_MARKER)
    if (fenceMatch && !(fenceMatch[1]![0] === '`' && fenceMatch[2]!.includes('`'))) {
      // Outside a fence any fence marker opens (``` / ```ts / ~~~).
      if (previousKind !== 'blank') emitBlank()
      fenceChar = fenceMatch[1]![0]!
      fenceLength = fenceMatch[1]!.length
      emitLine(raw, 'fence')
      continue
    }

    if (raw.trim().length === 0) {
      emitBlank()
      continue
    }

    // A Setext underline belongs to the preceding paragraph. Separating it
    // would turn an h1/h2 into ordinary prose and a horizontal rule.
    const priorKind = previousKind as LineKind
    if (priorKind === 'prose' && SETEXT_UNDERLINE.test(raw)) {
      emitLine(raw, 'heading')
      continue
    }

    const line = convertListMarkers(raw)
    const kind = priorKind === 'list' && /^[ \t]{2,}\S/u.test(line)
      ? 'list'
      : classifyLine(line)

    if (previousKind !== 'blank' && shouldSeparate(previousKind, kind, previousLine, line)) {
      emitBlank()
    }

    emitLine(line, kind)
  }

  while (out.length > 0 && out[0] === '') out.shift()
  while (out.length > 0 && out[out.length - 1] === '') out.pop()
  return out.join('\n')
}

function classifyLine(line: string): LineKind {
  if (LIST_LINE.test(line)) return 'list'
  if (TABLE_ROW.test(line)) return 'table'
  if (ATX_HEADING.test(line)) return 'heading'
  if (BLOCKQUOTE.test(line)) return 'quote'
  if (HORIZONTAL_RULE.test(line)) return 'hr'
  return 'prose'
}

/**
 * Keep tight runs of the same structural kind (list items, table rows, quote
 * lines). Insert a blank line on every other block transition, and between
 * prose lines that look like finished sentences / paragraphs.
 */
function shouldSeparate(
  previous: LineKind,
  next: LineKind,
  previousLine: string,
  nextLine: string
): boolean {
  if (previous === next && (previous === 'list' || previous === 'table' || previous === 'quote')) {
    return false
  }
  if (previous !== 'prose' || next !== 'prose') {
    return true
  }
  return looksLikeParagraphBreak(previousLine, nextLine)
}

function isMatchingFenceClose(line: string, fenceChar: string, fenceLength: number): boolean {
  const match = line.match(/^[ \t]{0,3}(`{3,}|~{3,})[ \t]*$/u)
  if (!match) return false
  const marker = match[1]!
  return marker[0] === fenceChar && marker.length >= fenceLength
}

/**
 * Map Chinese / decorative list markers to GFM so remark-gfm can render them.
 * Leaves already-valid `- ` / `1. ` lines untouched.
 */
export function convertListMarkers(line: string): string {
  const unordered = line.match(UNORDERED_BULLET)
  if (unordered) {
    return `${unordered[1]}- ${unordered[4] ?? ''}`
  }

  const orderedCn = line.match(ORDERED_CN)
  if (orderedCn) {
    const space = orderedCn[4] && orderedCn[4].length > 0 ? orderedCn[4] : ' '
    return `${orderedCn[1]}${orderedCn[2]}.${space}${orderedCn[5] ?? ''}`
  }

  const orderedParen = line.match(ORDERED_PAREN)
  if (orderedParen) {
    const space = orderedParen[3] && orderedParen[3].length > 0 ? orderedParen[3] : ' '
    return `${orderedParen[1]}${orderedParen[2]}.${space}${orderedParen[4] ?? ''}`
  }

  return line
}

function looksLikeParagraphBreak(previousLine: string, nextLine: string): boolean {
  const previous = previousLine.trimEnd()
  const next = nextLine.trimStart()
  if (!previous || !next) return false
  // Soft-wrapped English mid-sentence: keep as a single paragraph.
  if (!SENTENCE_END.test(previous)) return false
  // Do not split before indented continuations (code / nested list).
  if (/^[ \t]{2,}\S/u.test(nextLine)) return false
  return true
}
