import { useCallback, useEffect, useRef, type RefObject, type UIEvent } from 'react'

const BOTTOM_THRESHOLD_PX = 64

export function isNearScrollBottom(element: HTMLElement): boolean {
  return element.scrollHeight - element.scrollTop - element.clientHeight < BOTTOM_THRESHOLD_PX
}

export function useAutoFollowOutput(
  scrollRef: RefObject<HTMLDivElement | null>,
  observedRef: RefObject<HTMLDivElement | null>,
  resetKey: string | null
): (event: UIEvent<HTMLDivElement>) => void {
  const followingRef = useRef(true)
  const frameRef = useRef<number | null>(null)

  const scrollToBottom = useCallback((): void => {
    if (frameRef.current !== null) return
    frameRef.current = window.requestAnimationFrame(() => {
      frameRef.current = null
      const element = scrollRef.current
      if (element && followingRef.current) {
        const bottom = Math.max(0, element.scrollHeight - element.clientHeight)
        // Wrapped lines can notify repeatedly without changing scroll position.
        // Avoid redundant scroll writes and the resulting scroll events/layout.
        if (Math.abs(element.scrollTop - bottom) > 1) element.scrollTop = bottom
      }
    })
  }, [scrollRef])

  useEffect(() => {
    followingRef.current = true
    scrollToBottom()
  }, [resetKey, scrollToBottom])

  useEffect(() => {
    const observed = observedRef.current
    if (!observed || typeof ResizeObserver !== 'function') return
    const observer = new ResizeObserver(scrollToBottom)
    observer.observe(observed)
    return () => observer.disconnect()
  }, [observedRef, scrollToBottom])

  useEffect(() => () => {
    if (frameRef.current !== null) window.cancelAnimationFrame(frameRef.current)
  }, [])

  return useCallback((event: UIEvent<HTMLDivElement>): void => {
    followingRef.current = isNearScrollBottom(event.currentTarget)
  }, [])
}
