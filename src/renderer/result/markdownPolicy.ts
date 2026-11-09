import { hasCollapsedBulletList } from './markdownNormalize'

export const RICH_MARKDOWN_SCALAR_LIMIT = 16_384

// Includes Chinese-style list markers (1、 / （1） / •) so translation output
// that only uses those markers still enters the rich Markdown path.
const BLOCK_MARKDOWN =
  /(?:^|\n)[ \t]{0,3}(?:#{1,6}[ \t]+|>[ \t]+|[-+*•·●○][ \t]+|\d+[.)、．][ \t]*|[（(]\d+[）)][ \t]*|```|~~~|\$\$|\|[^\n]*\|)/u
const INLINE_MARKDOWN = /(?:\[[^\]\n]+\]\([^\n)]+\)|`[^`\n]+`|\*\*[^*\n]+\*\*|__[^_\n]+__|\$[^$\n]+\$)/u

/**
 * Conservative KaTeX eligibility: prefer false positives over dropping real
 * formulas. Matches display `$$...$$` (any `$$`) and inline `$...$` with
 * non-empty same-line content between dollars.
 */
const MATH_DISPLAY = /\$\$/u
const MATH_INLINE = /\$[^$\n]+\$/u

export function hasLikelyMarkdownSyntax(content: string): boolean {
  return BLOCK_MARKDOWN.test(content) || INLINE_MARKDOWN.test(content) ||
    (content.includes('•') || content.includes('·') || content.includes('●') || content.includes('○')) &&
    content.split(/\r?\n/u).some(hasCollapsedBulletList)
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
