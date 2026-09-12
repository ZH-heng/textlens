import { useLayoutEffect, useRef, useState } from 'react'

import {
  DEFAULT_SMOOTH_STREAM_CONFIG,
  SmoothStreamController,
  type SmoothStreamConfig
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
 * - First non-empty target: small immediate peek for TTFB, rest typewrites.
 * - useLayoutEffect seeds before paint so first-burst is not a post-paint flash.
 */
export function useSmoothStreamText(
  target: string,
  streaming: boolean,
  resetKey: string,
  config: SmoothStreamConfig = DEFAULT_SMOOTH_STREAM_CONFIG,
  requestFrame: SmoothStreamFrameScheduler = defaultRequestFrame,
  cancelFrame: SmoothStreamFrameCanceller = defaultCancelFrame,
  now: SmoothStreamClock = defaultNow
): SmoothStreamSnapshot {
  const controllerRef = useRef<SmoothStreamController | null>(null)
  if (controllerRef.current === null) {
    controllerRef.current = new SmoothStreamController(config)
  }
  const controller = controllerRef.current

  const targetRef = useRef(target)
  const animationRef = useRef(streaming)
  targetRef.current = target

  const [displayed, setDisplayed] = useState<SmoothStreamSnapshot>(
    EMPTY_STREAM_SNAPSHOT
  )
  const resetKeyRef = useRef(resetKey)

  useLayoutEffect(() => {
    if (resetKeyRef.current !== resetKey) {
      resetKeyRef.current = resetKey
      controller.reset()
    }

    animationRef.current = streaming
    if (!streaming) {
      const snapped = controller.setTarget(target, false, now())
      setDisplayed((current) =>
        current.text === snapped ? current : { text: snapped, scalarCount: controller.getDisplayedScalarCount() }
      )
      return
    }

    const seeded = controller.setTarget(target, true, now())
    setDisplayed((current) =>
      current.text === seeded ? current : { text: seeded, scalarCount: controller.getDisplayedScalarCount() }
    )

    if (!controller.needsTick()) return

    let handle: number | null = null
    let cancelled = false

    const pump = (frameTime: number): void => {
      handle = null
      if (cancelled || !animationRef.current) return

      // Pull latest target so mid-pump IPC growth is absorbed without waiting
      // for a React commit (store updates still re-run this effect too).
      controller.setTarget(targetRef.current, true, frameTime)
      if (!controller.needsTick()) {
        return
      }

      const advanced = controller.tick(frameTime)
      setDisplayed((current) =>
        current.text === advanced ? current : { text: advanced, scalarCount: controller.getDisplayedScalarCount() }
      )
      if (controller.needsTick()) {
        handle = requestFrame(pump)
      }
    }

    handle = requestFrame(pump)
    return () => {
      cancelled = true
      if (handle !== null) cancelFrame(handle)
    }
  }, [cancelFrame, controller, now, requestFrame, resetKey, streaming, target])

  return streaming
    ? displayed
    : { text: target, scalarCount: displayed.scalarCount }
}
