import {
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
  type RefObject
} from 'react'

const OVERFLOW_TOLERANCE_PX = 1

export interface ResultContentOverflowState {
  isOverflowing: boolean
  trackMarginPx: number
}

const INITIAL_STATE: ResultContentOverflowState = {
  isOverflowing: false,
  trackMarginPx: 0
}

export function hasVerticalOverflow(element: HTMLElement): boolean {
  return element.scrollHeight > element.clientHeight + OVERFLOW_TOLERANCE_PX
}

/**
 * Enables the native scrollbar only when the result really exceeds its
 * viewport. The measured track inset keeps the draggable scrollbar within
 * the middle half of the result area on older WKWebView releases as well.
 */
export function useResultContentOverflow(
  scrollRef: RefObject<HTMLDivElement | null>,
  observedRef: RefObject<HTMLDivElement | null>,
  enabled: boolean
): ResultContentOverflowState {
  const [state, setState] = useState<ResultContentOverflowState>(INITIAL_STATE)
  const frameRef = useRef<number | null>(null)

  const measure = useCallback((): void => {
    const element = scrollRef.current
    const isOverflowing = Boolean(element && enabled && hasVerticalOverflow(element))
    const trackMarginPx = isOverflowing && element
      ? Math.max(0, Math.ceil(element.clientHeight / 4))
      : 0

    setState((current) => (
      current.isOverflowing === isOverflowing && current.trackMarginPx === trackMarginPx
        ? current
        : { isOverflowing, trackMarginPx }
    ))

    if (!isOverflowing && element && element.scrollTop !== 0) element.scrollTop = 0
  }, [enabled, scrollRef])

  const scheduleMeasure = useCallback((): void => {
    if (frameRef.current !== null) return
    frameRef.current = window.requestAnimationFrame(() => {
      frameRef.current = null
      measure()
    })
  }, [measure])

  useLayoutEffect(() => {
    // Coalesce geometry reads into a frame rather than forcing layout during
    // the first token's React commit.
    scheduleMeasure()

    const scrollElement = scrollRef.current
    const observedElement = observedRef.current
    const observer = typeof ResizeObserver === 'function'
      ? new ResizeObserver(scheduleMeasure)
      : null

    if (scrollElement) observer?.observe(scrollElement)
    if (observedElement && observedElement !== scrollElement) observer?.observe(observedElement)
    window.addEventListener('resize', scheduleMeasure)

    return () => {
      observer?.disconnect()
      window.removeEventListener('resize', scheduleMeasure)
      if (frameRef.current !== null) {
        window.cancelAnimationFrame(frameRef.current)
        frameRef.current = null
      }
    }
  }, [observedRef, scheduleMeasure, scrollRef])

  return state
}
