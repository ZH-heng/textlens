import { describe, expect, it } from 'vitest'

import { convertListMarkers, normalizeMarkdownSource } from './markdownNormalize'

describe('convertListMarkers', () => {
  it('maps Chinese ordered markers to GFM ordered lists', () => {
    expect(convertListMarkers('1、第一点')).toBe('1. 第一点')
    expect(convertListMarkers('2．第二点')).toBe('2. 第二点')
    expect(convertListMarkers('（3）第三点')).toBe('3. 第三点')
    expect(convertListMarkers('(4) fourth')).toBe('4. fourth')
  })

  it('maps decorative bullets to GFM unordered lists', () => {
    expect(convertListMarkers('• 条目')).toBe('- 条目')
    expect(convertListMarkers('· 条目')).toBe('- 条目')
    expect(convertListMarkers('● 条目')).toBe('- 条目')
  })

  it('leaves already-valid GFM list markers alone', () => {
    expect(convertListMarkers('- item')).toBe('- item')
    expect(convertListMarkers('1. item')).toBe('1. item')
    expect(convertListMarkers('  2) item')).toBe('  2) item')
  })
})

describe('normalizeMarkdownSource', () => {
  it('splits sentence-ended single newlines into paragraphs', () => {
    const input = '第一段完整内容。\n第二段完整内容。'
    expect(normalizeMarkdownSource(input)).toBe('第一段完整内容。\n\n第二段完整内容。')
  })

  it('keeps soft-wrapped mid-sentence lines as a single paragraph', () => {
    const input = 'This is a long English sentence that\ncontinues without terminal punctuation'
    expect(normalizeMarkdownSource(input)).toBe(input)
  })

  it('keeps tight sibling list items without blank lines between them', () => {
    const input = '前言说明。\n- 条目一\n- 条目二\n结尾说明。'
    expect(normalizeMarkdownSource(input)).toBe(
      '前言说明。\n\n- 条目一\n- 条目二\n\n结尾说明。'
    )
  })

  it('converts Chinese list markers and separates surrounding prose', () => {
    const input = '概要：\n1、准备材料\n2、开始翻译\n完成。'
    expect(normalizeMarkdownSource(input)).toBe(
      '概要：\n\n1. 准备材料\n2. 开始翻译\n\n完成。'
    )
  })

  it('does not rewrite content inside fenced code blocks', () => {
    const input = ['说明。', '```', '1、不要改', '第二行。', '```', '结束。'].join('\n')
    expect(normalizeMarkdownSource(input)).toBe(
      ['说明。', '', '```', '1、不要改', '第二行。', '```', '', '结束。'].join('\n')
    )
  })

  it.each([
    '```python\nprint("a\\nb\\nc\\nd\\ne")\n```',
    '`a\\nb\\nc` and `\\t`',
    '{"pattern":"\\n\\t","path":"C:\\new\\tools\\notes"}',
    '/\\n\\t\\w+/gu',
    '第一段。\\n\\n第二段。\\n- 条目'
  ])('preserves literal escapes in already decoded content: %s', (input) => {
    expect(normalizeMarkdownSource(input)).toBe(input)
  })

  it.each(['A title\n---', 'A title.\n===', 'A multi\nline title\n---'])('preserves Setext headings: %s', (input) => {
    expect(normalizeMarkdownSource(input)).toBe(input)
  })

  it('does not let a shorter fence close an enclosing code sample', () => {
    const input = ['````markdown', '```', '1、不要改', '正文。', '下一行。', '```', '````'].join('\n')
    expect(normalizeMarkdownSource(input)).toBe(input)
  })

  it('preserves nested lists and indented continuations', () => {
    const input = '- parent\n  continuation\n  - child\n    nested continuation\n- sibling'
    expect(normalizeMarkdownSource(input)).toBe(input)
  })

  it('separates headings and blockquotes from preceding prose', () => {
    const input = '上文。\n## 标题\n> 引用一行\n后文。'
    expect(normalizeMarkdownSource(input)).toBe(
      '上文。\n\n## 标题\n\n> 引用一行\n\n后文。'
    )
  })

  it('is a pure no-op for already-clean Markdown', () => {
    const input = ['# 标题', '', '段落一。', '', '- a', '- b', '', '段落二。'].join('\n')
    expect(normalizeMarkdownSource(input)).toBe(input)
  })
})
