import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { memo, useState, type JSX } from 'react'

import type { SafeMarkdownProps } from './SafeMarkdown'
import { createRetryableMarkdownLoader, type SafeMarkdownImporter } from './LazySafeMarkdown'
import {
  ResultOutput,
  type MarkdownTaskScheduler,
  type ResultOutputMilestone
} from './ResultOutput'

function deferred<T>(): {
  promise: Promise<T>
  resolve(value: T): void
  reject(error: unknown): void
} {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((resolver, rejecter) => {
    resolve = resolver
    reject = rejecter
  })
  return { promise, resolve, reject }
}

class DeterministicScheduler implements MarkdownTaskScheduler {
  private nextHandle = 1
  readonly frames = new Map<number, FrameRequestCallback>()
  readonly idleTasks = new Map<number, () => void>()

  requestFrame = (callback: FrameRequestCallback): number => {
    const handle = this.nextHandle++
    this.frames.set(handle, callback)
    return handle
  }

  cancelFrame = (handle: number): void => {
    this.frames.delete(handle)
  }

  requestIdle = (callback: () => void): number => {
    const handle = this.nextHandle++
    this.idleTasks.set(handle, callback)
    return handle
  }

  cancelIdle = (handle: number): void => {
    this.idleTasks.delete(handle)
  }

  runNextFrame(): void {
    const next = this.frames.entries().next().value as [number, FrameRequestCallback] | undefined
    if (!next) throw new Error('No queued frame')
    this.frames.delete(next[0])
    next[1](performance.now())
  }

  runNextIdle(): void {
    const next = this.idleTasks.entries().next().value as [number, () => void] | undefined
    if (!next) throw new Error('No queued idle task')
    this.idleTasks.delete(next[0])
    next[1]()
  }
}

const onOpenExternal = (): void => undefined

function outputProps(overrides: Partial<React.ComponentProps<typeof ResultOutput>> = {}) {
  const content = '# Deferred heading'
  return {
    requestKey: 'request-1',
    status: 'completed' as const,
    content,
    contentScalarCount: [...content].length,
    contentRevision: 1,
    revealCommitted: false,
    onOpenExternal,
    ...overrides
  }
}

describe('ResultOutput', () => {
  it('commits plain content before presentation and a later Markdown task', async () => {
    const scheduler = new DeterministicScheduler()
    const markdownImport = deferred<{ SafeMarkdown: (props: SafeMarkdownProps) => JSX.Element }>()
    const importer = vi.fn(() => markdownImport.promise) as unknown as SafeMarkdownImporter
    const loader = createRetryableMarkdownLoader(importer)
    const milestones: ResultOutputMilestone[] = []
    const view = render(
      <ResultOutput
        {...outputProps()}
        loader={loader}
        scheduler={scheduler}
        onMilestone={(milestone) => milestones.push(milestone)}
      />
    )

    expect(screen.getByText('# Deferred heading')).toHaveClass('stream-plain-text')
    expect(screen.queryByRole('heading', { name: 'Deferred heading' })).not.toBeInTheDocument()
    expect(milestones).toEqual(['plain-layout'])

    view.rerender(
      <ResultOutput
        {...outputProps({ revealCommitted: true })}
        loader={loader}
        scheduler={scheduler}
        onMilestone={(milestone) => milestones.push(milestone)}
      />
    )
    expect(screen.getByText('# Deferred heading')).toHaveClass('stream-plain-text')
    expect(scheduler.frames.size).toBe(1)

    act(() => scheduler.runNextFrame())
    expect(screen.getByText('# Deferred heading')).toHaveClass('stream-plain-text')
    expect(importer).not.toHaveBeenCalled()
    expect(scheduler.idleTasks.size).toBe(1)

    act(() => scheduler.runNextIdle())
    expect(importer).toHaveBeenCalledOnce()
    expect(screen.getByText('# Deferred heading')).toHaveClass('stream-plain-text')

    await act(async () => {
      markdownImport.resolve({
        SafeMarkdown: ({ content }) => <h1>{content.replace(/^# /u, '')}</h1>
      })
      await markdownImport.promise
    })

    expect(await screen.findByRole('heading', { name: 'Deferred heading' })).toBeInTheDocument()
    expect(milestones).toEqual([
      'plain-layout',
      'presentation-opportunity',
      'markdown-task',
      'rich-commit'
    ])
  })

  it.each(['cancelled', 'error'] as const)(
    'keeps %s content plain after all scheduler work',
    (status) => {
      const scheduler = new DeterministicScheduler()
      const importer = vi.fn().mockResolvedValue({
        SafeMarkdown: ({ content }: SafeMarkdownProps) => <h1>{content}</h1>
      })
      render(
        <ResultOutput
          {...outputProps({ status, revealCommitted: true, content: '# Never rich' })}
          loader={createRetryableMarkdownLoader(importer)}
          scheduler={scheduler}
        />
      )

      expect(screen.getByText('# Never rich')).toHaveClass('stream-plain-text')
      expect(scheduler.frames.size).toBe(0)
      expect(scheduler.idleTasks.size).toBe(0)
      expect(importer).not.toHaveBeenCalled()
    }
  )

  it('renders Markdown content live while streaming', async () => {
    const scheduler = new DeterministicScheduler()
    const importer = vi.fn().mockResolvedValue({
      SafeMarkdown: ({ content }: SafeMarkdownProps) => <h1>{content.replace(/^# /u, '')}</h1>
    })
    render(
      <ResultOutput
        {...outputProps({ status: 'streaming', revealCommitted: true })}
        loader={createRetryableMarkdownLoader(importer)}
        scheduler={scheduler}
      />
    )

    expect(screen.getByText('# Deferred heading')).toHaveClass('stream-plain-text')

    act(() => scheduler.runNextFrame())
    act(() => scheduler.runNextIdle())
    expect(importer).toHaveBeenCalledOnce()

    expect(await screen.findByRole('heading', { name: 'Deferred heading' })).toBeInTheDocument()
  })

  it('keeps streaming plain text lightweight', () => {
    const scheduler = new DeterministicScheduler()
    const importer = vi.fn()
    render(
      <ResultOutput
        {...outputProps({
          status: 'streaming',
          content: 'ordinary plain text',
          contentScalarCount: 19,
          revealCommitted: true
        })}
        loader={createRetryableMarkdownLoader(importer)}
        scheduler={scheduler}
      />
    )

    expect(screen.getByText('ordinary plain text')).toHaveClass('stream-plain-text')
    expect(scheduler.frames.size).toBe(0)
    expect(importer).not.toHaveBeenCalled()
  })

  it.each([
    { name: 'plain text', content: 'ordinary plain text', contentScalarCount: 19 },
    { name: 'over-limit Markdown', content: '# too large', contentScalarCount: 16_385 }
  ])('does not import completed $name', ({ content, contentScalarCount }) => {
    const scheduler = new DeterministicScheduler()
    const importer = vi.fn()
    render(
      <ResultOutput
        {...outputProps({ content, contentScalarCount, revealCommitted: true })}
        loader={createRetryableMarkdownLoader(importer)}
        scheduler={scheduler}
      />
    )

    expect(screen.getByText(content)).toHaveClass('stream-plain-text')
    expect(scheduler.frames.size).toBe(0)
    expect(importer).not.toHaveBeenCalled()
  })

  it('keeps complete content visible and retries a rejected import with a new generation', async () => {
    const scheduler = new DeterministicScheduler()
    const importer = vi.fn()
      .mockRejectedValueOnce(new Error('first import failed'))
      .mockResolvedValueOnce({
        SafeMarkdown: ({ content }: SafeMarkdownProps) => <h1>{content.replace(/^# /u, '')}</h1>
      })
    const loader = createRetryableMarkdownLoader(importer)
    const firstGeneration = loader.nextGeneration()
    render(
      <ResultOutput
        {...outputProps({ revealCommitted: true })}
        loader={loader}
        scheduler={scheduler}
      />
    )

    act(() => scheduler.runNextFrame())
    await act(async () => {
      scheduler.runNextIdle()
      await Promise.resolve()
    })

    expect(screen.getByText('# Deferred heading')).toHaveClass('stream-plain-text')
    const retry = await screen.findByRole('button', { name: '重试富文本渲染' })
    fireEvent.click(retry)
    expect(scheduler.frames.size).toBe(1)
    act(() => scheduler.runNextFrame())
    await act(async () => {
      scheduler.runNextIdle()
      await Promise.resolve()
    })

    expect(await screen.findByRole('heading', { name: 'Deferred heading' })).toBeInTheDocument()
    expect(importer).toHaveBeenCalledTimes(2)
    expect(loader.nextGeneration().id).toBeGreaterThan(firstGeneration.id + 1)
  })

  it('memoizes rich output across unrelated parent control changes', async () => {
    const scheduler = new DeterministicScheduler()
    const parser = vi.fn(({ content }: SafeMarkdownProps) => (
      <h1>{content.replace(/^# /u, '')}</h1>
    ))
    const loader = createRetryableMarkdownLoader(async () => ({ SafeMarkdown: memo(parser) }))

    function Parent(): JSX.Element {
      const [controls, setControls] = useState(0)
      return (
        <>
          <button type="button" onClick={() => setControls((value) => value + 1)}>
            Controls {controls}
          </button>
          <ResultOutput
            {...outputProps({ revealCommitted: true })}
            loader={loader}
            scheduler={scheduler}
          />
        </>
      )
    }

    render(<Parent />)
    act(() => scheduler.runNextFrame())
    await act(async () => {
      scheduler.runNextIdle()
      await Promise.resolve()
    })
    expect(await screen.findByRole('heading', { name: 'Deferred heading' })).toBeInTheDocument()
    expect(parser).toHaveBeenCalledOnce()

    fireEvent.click(screen.getByRole('button', { name: 'Controls 0' }))
    await waitFor(() => expect(screen.getByRole('button', { name: 'Controls 1' })).toBeInTheDocument())
    expect(parser).toHaveBeenCalledOnce()
  })
})
