import {
  RICH_MARKDOWN_SCALAR_LIMIT,
  contentLooksLikeMath,
  hasLikelyMarkdownSyntax,
  isTableDelimiterRow,
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
    '1、中文有序',
    '（1）括号序号',
    '[link](https://example.com)',
    '```ts\nconst value = 1\n```',
    '$x^2$',
    '| A | B |\n| - | - |',
    'A | B\n--- | ---',
    '功能 | 说明\n- | -\n划词 | 选中即查'
  ])('recognizes representative Markdown: %s', (content) => {
    expect(hasLikelyMarkdownSyntax(content)).toBe(true)
  })

  it('recognizes pipe-less GFM tables as rich Markdown', () => {
    const table = ['方案 | 耗时', '--- | ---', 'A | 1 天', 'B | 2 天'].join('\n')
    expect(hasLikelyMarkdownSyntax(table)).toBe(true)
    expect(shouldRenderRichMarkdown(table, table.length)).toBe(true)
  })

  it.each([
    ['--- | ---', true],
    ['- | -', true],
    ['| --- | --- |', true],
    ['| - |', true],
    ['A | B', false],
    ['---', false],
    ['- item | other', false]
  ])('isTableDelimiterRow(%s) === %s', (line, expected) => {
    expect(isTableDelimiterRow(line)).toBe(expected)
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
