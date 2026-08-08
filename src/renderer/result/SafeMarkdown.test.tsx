import { fireEvent, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'

import { SafeMarkdown } from './SafeMarkdown'

describe('SafeMarkdown', () => {
  it('does not create executable DOM nodes from raw HTML', () => {
    const { container } = render(
      <SafeMarkdown
        content={'正常文本\n\n<script>window.pwned = true</script>\n<img src="x" onerror="window.pwned = true">'}
        onOpenExternal={() => undefined}
      />
    )

    expect(screen.getByText('正常文本')).toBeInTheDocument()
    expect(container.querySelector('script')).toBeNull()
    expect(container.querySelector('img')).toBeNull()
    expect(container.querySelector('[onerror]')).toBeNull()
  })

  it('renders dangerous links as inert text', () => {
    const onOpenExternal = vi.fn()
    const { container } = render(
      <SafeMarkdown
        content={'[脚本](javascript:alert(1)) [数据](data:text/html,pwned)'}
        onOpenExternal={onOpenExternal}
      />
    )

    expect(screen.getByText('脚本')).toBeInTheDocument()
    expect(screen.getByText('数据')).toBeInTheDocument()
    expect(container.querySelector('a')).toBeNull()
    expect(onOpenExternal).not.toHaveBeenCalled()
  })

  it('routes an HTTP(S) link through the provided opener', () => {
    const onOpenExternal = vi.fn()
    render(
      <SafeMarkdown content={'[官网](https://example.com/path?q=1)'} onOpenExternal={onOpenExternal} />
    )

    const link = screen.getByRole('link', { name: '官网' })
    fireEvent.click(link)

    expect(onOpenExternal).toHaveBeenCalledOnce()
    expect(onOpenExternal).toHaveBeenCalledWith('https://example.com/path?q=1')
  })

  it('ignores middle-click and does not open external links', () => {
    const onOpenExternal = vi.fn()
    render(
      <SafeMarkdown content={'[官网](https://example.com/path?q=1)'} onOpenExternal={onOpenExternal} />
    )

    const link = screen.getByRole('link', { name: '官网' })
    const clickEvent = fireEvent.click(link, { button: 1 })
    const auxEvent = fireEvent(
      link,
      new MouseEvent('auxclick', { bubbles: true, cancelable: true, button: 1 })
    )

    expect(clickEvent).toBe(false)
    expect(auxEvent).toBe(false)
    expect(onOpenExternal).not.toHaveBeenCalled()
  })

  it('ignores primary clicks with modifier keys', () => {
    const onOpenExternal = vi.fn()
    render(
      <SafeMarkdown content={'[官网](https://example.com/path?q=1)'} onOpenExternal={onOpenExternal} />
    )

    const link = screen.getByRole('link', { name: '官网' })

    for (const modifiers of [
      { metaKey: true },
      { ctrlKey: true },
      { shiftKey: true },
      { altKey: true }
    ]) {
      const prevented = fireEvent.click(link, { button: 0, ...modifiers })
      expect(prevented).toBe(false)
    }

    expect(onOpenExternal).not.toHaveBeenCalled()
  })

  it('does not load remote Markdown images', () => {
    const { container } = render(
      <SafeMarkdown content={'![追踪像素](https://example.com/pixel.gif)'} onOpenExternal={() => undefined} />
    )

    expect(container.querySelector('img')).toBeNull()
    expect(screen.getByText('[图片：追踪像素]')).toBeInTheDocument()
  })

  it('renders GitHub-flavored Markdown tables and task lists', () => {
    const { container } = render(
      <SafeMarkdown
        content={'| 项目 | 状态 |\n| --- | --- |\n| 翻译 | 完成 |\n\n- [x] 支持 Markdown'}
        onOpenExternal={() => undefined}
      />
    )

    expect(screen.getByRole('table')).toBeInTheDocument()
    expect(screen.getByRole('columnheader', { name: '项目' })).toBeInTheDocument()
    expect(screen.getByRole('cell', { name: '完成' })).toBeInTheDocument()
    expect(container.querySelector('input[type="checkbox"]')).toBeDisabled()
  })

  it('renders pipe-less GFM tables as real tables', () => {
    render(
      <SafeMarkdown
        content={'项目 | 状态\n--- | ---\n翻译 | 完成'}
        onOpenExternal={() => undefined}
      />
    )

    expect(screen.getByRole('table')).toBeInTheDocument()
    expect(screen.getByRole('columnheader', { name: '项目' })).toBeInTheDocument()
    expect(screen.getByRole('cell', { name: '完成' })).toBeInTheDocument()
  })

  it('renders headings, ordered and unordered lists, blockquotes and fenced code', () => {
    render(
      <SafeMarkdown
        content={[
          '# 一级标题',
          '',
          '1. 有序项目',
          '',
          '- 无序项目',
          '',
          '> 引用段落',
          '',
          '行内 `const value = 1`',
          '',
          '```ts',
          'const answer = 42',
          '```'
        ].join('\n')}
        onOpenExternal={() => undefined}
      />
    )

    expect(screen.getByRole('heading', { name: '一级标题', level: 1 })).toBeInTheDocument()
    expect(screen.getByText('有序项目').closest('ol')).toBeInTheDocument()
    expect(screen.getByText('无序项目').closest('ul')).toBeInTheDocument()
    expect(screen.getByText('引用段落').closest('blockquote')).toBeInTheDocument()
    expect(screen.getByText('const value = 1').tagName).toBe('CODE')
    expect(screen.getByText('const answer = 42').closest('pre')).toBeInTheDocument()
  })

  it('preserves translation paragraphs separated by a single newline', () => {
    const { container } = render(
      <SafeMarkdown
        content={'第一段完整内容。\n第二段完整内容。\n\n**强调** 后仍分段。'}
        onOpenExternal={() => undefined}
      />
    )

    const paragraphs = container.querySelectorAll('p')
    expect(paragraphs.length).toBeGreaterThanOrEqual(2)
    expect(paragraphs[0]?.textContent).toContain('第一段完整内容。')
    expect(paragraphs[1]?.textContent).toContain('第二段完整内容。')
  })

  it('renders Chinese-style list markers as real lists', () => {
    const { container } = render(
      <SafeMarkdown
        content={'概要：\n1、准备材料\n2、开始翻译\n• 补充说明'}
        onOpenExternal={() => undefined}
      />
    )

    expect(screen.getByText('准备材料').closest('ol')).toBeInTheDocument()
    expect(screen.getByText('开始翻译').closest('ol')).toBeInTheDocument()
    expect(screen.getByText('补充说明').closest('ul')).toBeInTheDocument()
    // Prose before the list stays its own block, not glued into the first item.
    expect(container.querySelector('p')?.textContent).toContain('概要')
  })

  it('renders non-math GFM without KaTeX nodes', () => {
    const { container } = render(
      <SafeMarkdown
        content={[
          '# 总结',
          '',
          '- 要点一',
          '- 要点二',
          '',
          '```ts',
          'const ok = true',
          '```'
        ].join('\n')}
        onOpenExternal={() => undefined}
      />
    )

    expect(screen.getByRole('heading', { name: '总结', level: 1 })).toBeInTheDocument()
    expect(screen.getByText('要点一').closest('ul')).toBeInTheDocument()
    expect(screen.getByText('const ok = true').closest('pre')).toBeInTheDocument()
    expect(container.querySelector('.katex')).toBeNull()
    expect(container.querySelector('math')).toBeNull()
  })

  it('renders inline and display Markdown formulas with accessible KaTeX output', () => {
    const { container } = render(
      <SafeMarkdown
        content={'欧拉公式 $e^{i\\pi}+1=0$。\n\n$$\n\\int_0^1 x^2\\,dx=\\frac{1}{3}\n$$'}
        onOpenExternal={() => undefined}
      />
    )

    expect(container.querySelectorAll('.katex')).toHaveLength(2)
    expect(container.querySelector('.katex-display')).toBeInTheDocument()
    expect(container.querySelectorAll('math[xmlns="http://www.w3.org/1998/Math/MathML"]'))
      .toHaveLength(2)
  })

  it('keeps untrusted formula commands from creating links or remote resources', () => {
    const { container } = render(
      <SafeMarkdown
        content={'$\\href{javascript:alert(1)}{危险链接}$\n\n$$\\includegraphics{https://example.com/a.png}$$'}
        onOpenExternal={() => undefined}
      />
    )

    expect(container.querySelector('a')).toBeNull()
    expect(container.querySelector('img')).toBeNull()
    expect(container.querySelector('.katex')).toBeInTheDocument()
  })
})
