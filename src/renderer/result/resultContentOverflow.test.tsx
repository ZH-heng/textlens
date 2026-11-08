import { act, renderHook } from '@testing-library/react'
import { createRef } from 'react'

import { hasVerticalOverflow, useResultContentOverflow } from './resultContentOverflow'

let resizeCallback: ResizeObserverCallback | null = null

function setGeometry(element: HTMLElement, clientHeight: number, scrollHeight: number): void {
  Object.defineProperties(element, {
    clientHeight: { configurable: true, value: clientHeight },
    scrollHeight: { configurable: true, value: scrollHeight },
    scrollTop: { configurable: true, writable: true, value: 0 }
  })
}

beforeEach(() => {
  resizeCallback = null
  Object.defineProperty(window, 'ResizeObserver', {
    configurable: true,
    value: class ResizeObserverMock {
      constructor(callback: ResizeObserverCallback) {
        resizeCallback = callback
      }

      observe(): void {}
      disconnect(): void {}
    }
  })
})

afterEach(() => {
  Reflect.deleteProperty(window, 'ResizeObserver')
})

describe('result content overflow', () => {
  it('does not enable scrolling while waiting for the first model content', () => {
    const scroller = document.createElement('div')
    const inner = document.createElement('div')
    setGeometry(scroller, 300, 900)
    const scrollRef = createRef<HTMLDivElement>()
    const observedRef = createRef<HTMLDivElement>()
    scrollRef.current = scroller
    observedRef.current = inner

    const { result } = renderHook(() => (
      useResultContentOverflow(scrollRef, observedRef, false)
    ))

    expect(result.current).toEqual({ isOverflowing: false, trackMarginPx: 0 })
  })

  it('keeps scrolling disabled for short content', () => {
    const scroller = document.createElement('div')
    const inner = document.createElement('div')
    setGeometry(scroller, 300, 300)
    expect(hasVerticalOverflow(scroller)).toBe(false)
    const scrollRef = createRef<HTMLDivElement>()
    const observedRef = createRef<HTMLDivElement>()
    scrollRef.current = scroller
    observedRef.current = inner

    const { result } = renderHook(() => (
      useResultContentOverflow(scrollRef, observedRef, true)
    ))

    expect(result.current).toEqual({ isOverflowing: false, trackMarginPx: 0 })
  })

  it('defers the first geometry read to a scheduled frame', async () => {
    let geometryReads = 0
    const scroller = document.createElement('div')
    Object.defineProperties(scroller, {
      clientHeight: { configurable: true, value: 300 },
      scrollHeight: {
        configurable: true,
        get: () => {
          geometryReads += 1
          return 900
        }
      },
      scrollTop: { configurable: true, writable: true, value: 0 }
    })
    const scrollRef = createRef<HTMLDivElement>()
    const observedRef = createRef<HTMLDivElement>()
    scrollRef.current = scroller
    observedRef.current = document.createElement('div')

    renderHook(() => useResultContentOverflow(scrollRef, observedRef, true))
    expect(geometryReads).toBe(0)

    await act(async () => {
      await new Promise<void>((resolve) => window.requestAnimationFrame(() => resolve()))
    })
    expect(geometryReads).toBe(1)
  })

  it('enables native scrolling for overflow and limits its track to the middle half', async () => {
    const scroller = document.createElement('div')
    const inner = document.createElement('div')
    setGeometry(scroller, 320, 321)
    expect(hasVerticalOverflow(scroller)).toBe(false)
    const scrollRef = createRef<HTMLDivElement>()
    const observedRef = createRef<HTMLDivElement>()
    scrollRef.current = scroller
    observedRef.current = inner

    const { result } = renderHook(() => (
      useResultContentOverflow(scrollRef, observedRef, true)
    ))
    expect(result.current.isOverflowing).toBe(false)

    setGeometry(scroller, 320, 700)
    await act(async () => {
      resizeCallback?.([], {} as ResizeObserver)
      await new Promise((resolve) => window.requestAnimationFrame(resolve))
    })

    expect(result.current).toEqual({ isOverflowing: true, trackMarginPx: 80 })
  })

  it('never leaves more than half of an odd-height viewport for the track', async () => {
    const scroller = document.createElement('div')
    const inner = document.createElement('div')
    setGeometry(scroller, 485, 900)
    const scrollRef = createRef<HTMLDivElement>()
    const observedRef = createRef<HTMLDivElement>()
    scrollRef.current = scroller
    observedRef.current = inner

    const { result } = renderHook(() => (
      useResultContentOverflow(scrollRef, observedRef, true)
    ))

    await act(async () => {
      await new Promise<void>((resolve) => window.requestAnimationFrame(() => resolve()))
    })

    expect(result.current.trackMarginPx).toBe(122)
    expect(485 - result.current.trackMarginPx * 2).toBeLessThanOrEqual(485 / 2)
  })
})
