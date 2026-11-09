import {
  RICH_MARKDOWN_SCALAR_LIMIT,
  contentLooksLikeMath,
  hasLikelyMarkdownSyntax,
  shouldRenderRichMarkdown
} from './markdownPolicy'

describe('rich Markdown policy', () => {
  it.each([
    'ordinary plain text',
    'a sentence with a hyphen - in the middle',
    'price is $5 without a closing math delimiter'
  ])('keeps obvious plain text lightweight: %s', (content) => {
    expect(hasLikelyMarkdownSyntax(content)).toBe(false)
  })

  it.each([
    '# heading',
    '- list item',
    '• Chinese bullet',
    '第一项。 • 第二项。 • 第三项。',
    '1、中文有序',
    '（1）括号序号',
    '[link](https://example.com)',
    '```ts\nconst value = 1\n```',
    '$x^2$',
    '| A | B |\n| - | - |'
  ])('recognizes representative Markdown: %s', (content) => {
    expect(hasLikelyMarkdownSyntax(content)).toBe(true)
  })

  it('uses Unicode scalars at the exact rich-render boundary', () => {
    const atLimit = `# ${'😀'.repeat(RICH_MARKDOWN_SCALAR_LIMIT - 2)}`
    expect(shouldRenderRichMarkdown(atLimit, RICH_MARKDOWN_SCALAR_LIMIT)).toBe(true)
    expect(shouldRenderRichMarkdown(`${atLimit}😀`, RICH_MARKDOWN_SCALAR_LIMIT + 1)).toBe(false)
  })
})

describe('contentLooksLikeMath', () => {
  it.each([
    '# heading only',
    '- list item',
    '| A | B |\n| - | - |',
    'price is $5 without a closing math delimiter',
    'cost $12 alone'
  ])('returns false without paired math delimiters: %s', (content) => {
    expect(contentLooksLikeMath(content)).toBe(false)
  })

  it.each([
    '$x^2$',
    'inline $e^{i\\pi}+1=0$ formula',
    '$$\n\\int_0^1 x^2\\,dx\n$$',
    '$$a+b$$',
    // conservative false positive: paired dollars that are not real math
    'costs $5 and $10 today'
  ])('returns true when delimiters look like math: %s', (content) => {
    expect(contentLooksLikeMath(content)).toBe(true)
  })
})
