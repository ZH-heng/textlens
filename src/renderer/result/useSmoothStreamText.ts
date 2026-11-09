import { useCallback, useLayoutEffect, useRef, useState } from 'react'

import {
  SmoothStreamController
} from './streamPlayback'

export type SmoothStreamFrameScheduler = (
  callback: FrameRequestCallback
) => number

export type SmoothStreamFrameCanceller = (handle: number) => void

export type SmoothStreamClock = () => number

// Stable module-level identities: inline default arrows would change every
// render and re-run the pump effect below on each frame.
const defaultRequestFrame: SmoothStreamFrameScheduler = (callback) =>
  window.requestAnimationFrame(callback)
const defaultCancelFrame: SmoothStreamFrameCanceller = (handle) =>
  window.cancelAnimationFrame(handle)
const defaultNow: SmoothStreamClock = () => performance.now()

export interface SmoothStreamSnapshot {
  text: string
  /** Scalar count of `text`, maintained incrementally by the controller.
      Only meaningful while `streaming` is true; terminal states must fall
      back to the store's authoritative `contentScalarCount`. */
  scalarCount: number
}

const EMPTY_STREAM_SNAPSHOT: SmoothStreamSnapshot = { text: '', scalarCount: 0 }

/**
 * Display-layer adaptive typewriter. `target` stays the full logical stream
 * text from the store; returned text lags slightly so bursts paint smoothly.
 *
 * - Non-streaming / terminal: always returns `target` immediately (no lag).
 * - Streaming: rAF pump only while backlog remains; stops when caught up and
 *   restarts on the next target growth (no forever-rAF between tokens).
 * - First non-empty target: complete first chunk is displayed immediately.
 * - useLayoutEffect seeds before paint so first-burst is not a post-paint flash.
 */
export function useSmoothStreamText(
  target: string,
  streaming: boolean,
  resetKey: string,
  _config: undefined = undefined,
  requestFrame: SmoothStreamFrameScheduler = defaultRequestFrame,
  cancelFrame: SmoothStreamFrameCanceller = defaultCancelFrame,
  now: SmoothStreamClock = defaultNow
): SmoothStreamSnapshot {
  const [reducedMotion, setReducedMotion] = useState(() =>
    window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false
  )
  useLayoutEffect(() => {
    // Own the media query for this subscription lifecycle, including a
    // StrictMode setup/cleanup/setup, rather than retaining a render-time MQL.
    const motionPreference = window.matchMedia?.('(prefers-reduced-motion: reduce)')
    if (!motionPreference) return
    const update = (): void => setReducedMotion(motionPreference.matches)
    if (motionPreference.addEventListener) {
      motionPreference.addEventListener('change', update)
      update()
      return () => motionPreference.removeEventListener('change', update)
    }
    motionPreference.addListener?.(update)
    update()
    return () => motionPreference.removeListener?.(update)
  }, [])

  const controllerRef = useRef<SmoothStreamController | null>(null)
  if (controllerRef.current === null) {
    controllerRef.current = new SmoothStreamController()
  }
  const controller = controllerRef.current

  const frameRef = useRef<number | null>(null)
  const timerRef = useRef<number | null>(null)
  const animationRef = useRef(streaming)
  const [displayed, setDisplayed] = useState<SmoothStreamSnapshot>(EMPTY_STREAM_SNAPSHOT)
  const resetKeyRef = useRef(resetKey)

  const publish = useCallback(() => {
    const text = controller.getDisplayed()
    const scalarCount = controller.getDisplayedScalarCount()
    setDisplayed((current) => current.text === text && current.scalarCount === scalarCount
      ? current : { text, scalarCount })
  }, [controller])

  const cancelScheduled = useCallback(() => {
    if (frameRef.current !== null) cancelFrame(frameRef.current)
    if (timerRef.current !== null) window.clearTimeout(timerRef.current)
    frameRef.current = timerRef.current = null
  }, [cancelFrame])

  const schedule = useCallback(function schedule(): void {
    if (frameRef.current !== null || timerRef.current !== null) return
    const pump = (): void => {
      cancelScheduled()
      if (!animationRef.current) return
      controller.tick(now(), document.visibilityState === 'hidden')
      publish()
      if (controller.needsTick()) schedule()
    }
    frameRef.current = requestFrame(pump)
    timerRef.current = window.setTimeout(pump, Math.max(0, Math.min(100, controller.getDeadline() - now())))
  }, [cancelScheduled, controller, now, publish, requestFrame])

  useLayoutEffect(() => {
    if (resetKeyRef.current !== resetKey) {
      resetKeyRef.current = resetKey
      controller.reset()
    }
    animationRef.current = streaming && !reducedMotion
    controller.setTarget(target, animationRef.current, now())
    publish()
    if (controller.needsTick()) {
      // Target growth must not cancel an already queued frame. Otherwise dense
      // IPC updates can repeatedly postpone playback without ever painting.
      schedule()
    } else {
      cancelScheduled()
    }
  }, [cancelScheduled, controller, now, publish, reducedMotion, resetKey, schedule, streaming, target])

  useLayoutEffect(() => () => {
    cancelScheduled()
  }, [cancelScheduled])

  return streaming
    ? displayed
    : { text: target, scalarCount: displayed.scalarCount }
}
