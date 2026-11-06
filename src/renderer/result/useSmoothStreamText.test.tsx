import { act, render, screen } from '@testing-library/react'
import { StrictMode, useState, type ReactElement } from 'react'
import { describe, expect, it, vi } from 'vitest'

import { useSmoothStreamText, type SmoothStreamFrameScheduler } from './useSmoothStreamText'

function Harness({
  target,
  streaming,
  resetKey,
  requestFrame,
  cancelFrame,
  now
}: {
  target: string
  streaming: boolean
  resetKey: string
  requestFrame: SmoothStreamFrameScheduler
  cancelFrame: (id: number) => void
  now: () => number
}): ReactElement {
  const stream = useSmoothStreamText(
    target,
    streaming,
    resetKey,
    undefined,
    requestFrame,
    cancelFrame,
    now
  )
  return (
    <div data-testid="out" data-scalar-count={stream.scalarCount}>
      {stream.text}
    </div>
  )
}

describe('useSmoothStreamText', () => {
  it('shows a whole streaming burst without a typewriter when reduced motion is enabled', () => {
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: true })))
    const requestFrame = vi.fn<SmoothStreamFrameScheduler>()
    const target = `立即显示😀${'内容'.repeat(20)}`
    render(<Harness target={target} streaming resetKey="reduced" requestFrame={requestFrame} cancelFrame={vi.fn()} now={() => 0} />)
    expect(screen.getByTestId('out').textContent).toBe(target)
    expect(screen.getByTestId('out').dataset.scalarCount).toBe(String(Array.from(target).length))
    expect(requestFrame).not.toHaveBeenCalled()
    vi.unstubAllGlobals()
  })

  it('cancels an active typewriter and flushes its backlog when the system enables reduced motion', () => {
    let onChange: (() => void) | null = null
    const preference = {
      matches: false,
      addEventListener: vi.fn((_type: string, listener: () => void) => { onChange = listener }),
      removeEventListener: vi.fn()
    }
    vi.stubGlobal('matchMedia', vi.fn(() => preference))
    const requestFrame = vi.fn<SmoothStreamFrameScheduler>().mockReturnValue(42)
    const cancelFrame = vi.fn()
    const target = `完整输出😀${'字'.repeat(40)}`
    const view = render(<Harness target={target} streaming resetKey="active" requestFrame={requestFrame} cancelFrame={cancelFrame} now={() => 0} />)
    expect(screen.getByTestId('out').textContent).not.toBe(target)
    expect(requestFrame).toHaveBeenCalledOnce()
    act(() => { preference.matches = true; onChange?.() })
    expect(cancelFrame).toHaveBeenCalledWith(42)
    expect(screen.getByTestId('out').textContent).toBe(target)
    view.rerender(<Harness target={`${target}新增`} streaming resetKey="active" requestFrame={requestFrame} cancelFrame={cancelFrame} now={() => 0} />)
    expect(screen.getByTestId('out').textContent).toBe(`${target}新增`)
    expect(requestFrame).toHaveBeenCalledOnce()
    view.unmount()
    expect(preference.removeEventListener).toHaveBeenCalledWith('change', onChange)
    vi.unstubAllGlobals()
  })

  it('keeps one live motion subscription after StrictMode replay and disposes it on unmount', () => {
    const preferences: Array<{ matches: boolean; listeners: Set<() => void> }> = []
    vi.stubGlobal('matchMedia', vi.fn(() => {
      const preference = { matches: false, listeners: new Set<() => void>() }
      preferences.push(preference)
      return {
        get matches() { return preference.matches },
        addEventListener: (_type: string, listener: () => void) => preference.listeners.add(listener),
        removeEventListener: (_type: string, listener: () => void) => preference.listeners.delete(listener)
      }
    }))
    const requestFrame = vi.fn<SmoothStreamFrameScheduler>().mockReturnValue(42)
    const cancelFrame = vi.fn()
    const target = `即时输出😀${'内容'.repeat(40)}`
    const view = render(
      <StrictMode>
        <Harness target={target} streaming resetKey="strict-motion" requestFrame={requestFrame} cancelFrame={cancelFrame} now={() => 0} />
      </StrictMode>
    )
    const active = preferences.filter((preference) => preference.listeners.size > 0)
    expect(active).toHaveLength(1)
    const preference = active[0]
    if (!preference) throw new Error('Missing live motion preference subscription')
    expect(preference.listeners.size).toBe(1)
    expect(screen.getByTestId('out').textContent).not.toBe(target)
    const frameCount = requestFrame.mock.calls.length
    act(() => {
      preference.matches = true
      preference.listeners.forEach((listener) => listener())
    })
    expect(screen.getByTestId('out').textContent).toBe(target)
    expect(requestFrame).toHaveBeenCalledTimes(frameCount)
    expect(cancelFrame).toHaveBeenCalledWith(42)
    view.unmount()
    expect(preferences.every((preference) => preference.listeners.size === 0)).toBe(true)
    vi.unstubAllGlobals()
  })

  it('returns full target immediately when not streaming', () => {
    const requestFrame = vi.fn<SmoothStreamFrameScheduler>()
    const cancelFrame = vi.fn()
    render(
      <Harness
        target="完整结果"
        streaming={false}
        resetKey="r1"
        requestFrame={requestFrame}
        cancelFrame={cancelFrame}
        now={() => 0}
      />
    )
    expect(screen.getByTestId('out')).toHaveTextContent('完整结果')
    expect(requestFrame).not.toHaveBeenCalled()
  })

  it('shows first-burst immediately then drains backlog on rAF ticks', () => {
    let clock = 0
    const frames: FrameRequestCallback[] = []
    const requestFrame: SmoothStreamFrameScheduler = (cb) => {
      frames.push(cb)
      return frames.length
    }
    const cancelFrame = vi.fn()

    const long = `你好${'字'.repeat(40)}`
    const { rerender } = render(
      <Harness
        target="你好"
        streaming
        resetKey="r1"
        requestFrame={requestFrame}
        cancelFrame={cancelFrame}
        now={() => clock}
      />
    )
    expect(screen.getByTestId('out')).toHaveTextContent('你好')

    rerender(
      <Harness
        target={long}
        streaming
        resetKey="r1"
        requestFrame={requestFrame}
        cancelFrame={cancelFrame}
        now={() => clock}
      />
    )
    // Still at first-burst seed until frames advance.
    expect(screen.getByTestId('out').textContent).toBe('你好')
    expect(frames.length).toBeGreaterThan(0)

    act(() => {
      clock += 16
      const cb = frames.shift()
      cb?.(clock)
    })
    const afterOne = screen.getByTestId('out').textContent ?? ''
    expect(afterOne.startsWith('你好')).toBe(true)
    expect(afterOne.length).toBeGreaterThan(2)
    expect(afterOne.length).toBeLessThan(long.length)

    let guard = 0
    while ((screen.getByTestId('out').textContent ?? '') !== long && guard < 200) {
      act(() => {
        clock += 16
        const cb = frames.shift()
        if (cb) cb(clock)
      })
      guard += 1
    }
    expect(screen.getByTestId('out')).toHaveTextContent(long)
    // The incremental counter must land on the full scalar count of `long`
    // (42 code points) without any full-text rescan at the call site.
    expect(screen.getByTestId('out').dataset.scalarCount).toBe('42')
  })

  it('returns the complete result immediately when streaming ends mid-drain', () => {
    let clock = 0
    const frames: FrameRequestCallback[] = []
    const requestFrame: SmoothStreamFrameScheduler = (cb) => {
      frames.push(cb)
      return frames.length
    }

    const long = `ab${'c'.repeat(40)}`
    const { rerender } = render(
      <Harness
        target={long}
        streaming
        resetKey="r1"
        requestFrame={requestFrame}
        cancelFrame={vi.fn()}
        now={() => clock}
      />
    )
    expect((screen.getByTestId('out').textContent ?? '').length).toBeLessThan(long.length)

    rerender(
      <Harness
        target={long}
        streaming={false}
        resetKey="r1"
        requestFrame={requestFrame}
        cancelFrame={vi.fn()}
        now={() => clock}
      />
    )
    expect(screen.getByTestId('out')).toHaveTextContent(long)
  })

  it('resets displayed text when resetKey changes', () => {
    let clock = 0
    const frames: FrameRequestCallback[] = []
    const requestFrame: SmoothStreamFrameScheduler = (cb) => {
      frames.push(cb)
      return frames.length
    }

    function Driver(): ReactElement {
      const [key, setKey] = useState('req-a')
      const [target, setTarget] = useState('第一段内容较多一些文字')
      return (
        <>
          <Harness
            target={target}
            streaming
            resetKey={key}
            requestFrame={requestFrame}
            cancelFrame={vi.fn()}
            now={() => clock}
          />
          <button
            type="button"
            onClick={() => {
              setKey('req-b')
              setTarget('第二')
            }}
          >
            next
          </button>
        </>
      )
    }

    render(<Driver />)
    expect(screen.getByTestId('out').textContent?.length).toBeGreaterThan(0)

    act(() => {
      screen.getByRole('button', { name: 'next' }).click()
    })
    // New request first-burst for short "第二"
    expect(screen.getByTestId('out')).toHaveTextContent('第二')
  })

  it('stops the pump when backlog is empty (no forever rAF)', () => {
    let clock = 0
    const frames: FrameRequestCallback[] = []
    const requestFrame: SmoothStreamFrameScheduler = (cb) => {
      frames.push(cb)
      return frames.length
    }

    render(
      <Harness
        target="短"
        streaming
        resetKey="r1"
        requestFrame={requestFrame}
        cancelFrame={vi.fn()}
        now={() => clock}
      />
    )
    // Fully shown via first-burst — no backlog to animate.
    expect(screen.getByTestId('out')).toHaveTextContent('短')
    expect(frames).toHaveLength(0)
  })
})
