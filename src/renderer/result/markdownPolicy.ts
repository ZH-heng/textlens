export const RICH_MARKDOWN_SCALAR_LIMIT = 16_384

// Includes Chinese-style list markers (1、 / （1） / •) so translation output
// that only uses those markers still enters the rich Markdown path.
const BLOCK_MARKDOWN =
  /(?:^|\n)[ \t]{0,3}(?:#{1,6}[ \t]+|>[ \t]+|[-+*•·●○][ \t]+|\d+[.)、．][ \t]*|[（(]\d+[）)][ \t]*|```|~~~|\$\$|\|[^\n]*\|)/u
const INLINE_MARKDOWN = /(?:\[[^\]\n]+\]\([^\n)]+\)|`[^`\n]+`|\*\*[^*\n]+\*\*|__[^_\n]+__|\$[^$\n]+\$)/u

/** One GFM table delimiter cell: dashes with optional alignment colons. */
const TABLE_DELIMITER_CELL = /^:?-{1,}:?$/u

/**
 * Conservative KaTeX eligibility: prefer false positives over dropping real
 * formulas. Matches display `$$...$$` (any `$$`) and inline `$...$` with
 * non-empty same-line content between dollars.
 */
const MATH_DISPLAY = /\$\$/u
const MATH_INLINE = /\$[^$\n]+\$/u

/**
 * True when a line looks like a GFM table delimiter row, with or without
 * leading/trailing pipes (e.g. `--- | ---`, `- | -`, `| --- | --- |`).
 */
export function isTableDelimiterRow(line: string): boolean {
  const trimmed = line.trim()
  if (!trimmed.includes('|')) return false
  const inner = trimmed.replace(/^\|/u, '').replace(/\|$/u, '')
  const cells = inner
    .split('|')
    .map((cell) => cell.trim())
  return cells.length > 0 && cells.every((cell) => TABLE_DELIMITER_CELL.test(cell))
}

/**
 * GFM tables do not require a leading pipe, so a header line such as
 * `功能 | 说明` followed by a delimiter row is table evidence even when the
 * `BLOCK_MARKDOWN` leading-pipe pattern never matches.
 */
function hasLikelyTableSyntax(content: string): boolean {
  const lines = content.split('\n')
  for (let i = 0; i + 1 < lines.length; i++) {
    if (!lines[i]!.includes('|')) continue
    if (isTableDelimiterRow(lines[i + 1]!)) return true
  }
  return false
}

export function hasLikelyMarkdownSyntax(content: string): boolean {
  return (
    BLOCK_MARKDOWN.test(content) ||
    INLINE_MARKDOWN.test(content) ||
    hasLikelyTableSyntax(content)
  )
}

export function contentLooksLikeMath(content: string): boolean {
  return MATH_DISPLAY.test(content) || MATH_INLINE.test(content)
}

export function shouldRenderRichMarkdown(
  content: string,
  contentScalarCount: number
): boolean {
  return content.length > 0 &&
    contentScalarCount <= RICH_MARKDOWN_SCALAR_LIMIT &&
    hasLikelyMarkdownSyntax(content)
}
