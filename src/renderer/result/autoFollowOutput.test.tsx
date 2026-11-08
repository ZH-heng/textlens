import { act, renderHook } from '@testing-library/react'
import { createRef } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { isNearScrollBottom, useAutoFollowOutput } from './autoFollowOutput'

let resizeCallback: ResizeObserverCallback | null = null
const disconnect = vi.fn()

beforeEach(() => {
  vi.useFakeTimers()
  resizeCallback = null
  disconnect.mockReset()
  Object.defineProperty(window, 'ResizeObserver', {
    configurable: true,
    value: class {
      constructor(callback: ResizeObserverCallback) {
        resizeCallback = callback
      }
      observe(): void {}
      disconnect = disconnect
    }
  })
})

afterEach(() => {
  vi.useRealTimers()
  Reflect.deleteProperty(window, 'ResizeObserver')
})

describe('result auto-follow scrolling', () => {
  it('recognizes whether the user is near the bottom', () => {
    const element = document.createElement('div')
    Object.defineProperties(element, {
      scrollHeight: { configurable: true, value: 500 },
      clientHeight: { configurable: true, value: 200 },
      scrollTop: { configurable: true, writable: true, value: 250 }
    })
    expect(isNearScrollBottom(element)).toBe(true)
    element.scrollTop = 100
    expect(isNearScrollBottom(element)).toBe(false)
  })

  it('avoids scroll writes while content fits or the viewport is already at the bottom', async () => {
    const scroller = document.createElement('div')
    let height = 100
    let position = 0
    const write = vi.fn((value: number) => { position = value })
    Object.defineProperties(scroller, {
      scrollHeight: { get: () => height },
      clientHeight: { value: 200 },
      scrollTop: { get: () => position, set: write }
    })
    const scrollRef = createRef<HTMLDivElement>()
    const observedRef = createRef<HTMLDivElement>()
    scrollRef.current = scroller
    observedRef.current = document.createElement('div')
    renderHook(() => useAutoFollowOutput(scrollRef, observedRef, 'one'))
    await act(async () => vi.advanceTimersByTimeAsync(16))
    expect(write).not.toHaveBeenCalled()
    height = 500
    act(() => resizeCallback?.([], {} as ResizeObserver))
    await act(async () => vi.advanceTimersByTimeAsync(16))
    expect(write).toHaveBeenCalledExactlyOnceWith(300)
    act(() => resizeCallback?.([], {} as ResizeObserver))
    await act(async () => vi.advanceTimersByTimeAsync(16))
    expect(write).toHaveBeenCalledOnce()
  })

  it('coalesces resize updates and stops following after the user scrolls away', async () => {
    const scroller = document.createElement('div')
    const observed = document.createElement('div')
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 500 },
      clientHeight: { configurable: true, value: 200 },
      scrollTop: { configurable: true, writable: true, value: 0 }
    })
    const scrollRef = createRef<HTMLDivElement>()
    const observedRef = createRef<HTMLDivElement>()
    scrollRef.current = scroller
    observedRef.current = observed
    const { result } = renderHook(() => useAutoFollowOutput(scrollRef, observedRef, 'request-1'))

    await act(async () => vi.advanceTimersByTimeAsync(16))
    expect(scroller.scrollTop).toBe(300)

    scroller.scrollTop = 100
    act(() => result.current({ currentTarget: scroller } as never))
    act(() => {
      resizeCallback?.([], {} as ResizeObserver)
      resizeCallback?.([], {} as ResizeObserver)
    })
    await act(async () => vi.advanceTimersByTimeAsync(16))
    expect(scroller.scrollTop).toBe(100)
  })
})
