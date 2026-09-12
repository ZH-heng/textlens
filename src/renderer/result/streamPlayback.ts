import { countUnicodeScalars } from '../../shared'

export const MAX_GRAPHEME_CARRY_SCALARS = 256

export interface GraphemeTailUpdate {
  released: string
  carry: string
}

let segmenter: Intl.Segmenter | null | undefined

export function splitGraphemes(value: string): string[] {
  if (typeof Intl.Segmenter === 'function') {
    segmenter ??= new Intl.Segmenter(undefined, { granularity: 'grapheme' })
    return Array.from(segmenter.segment(value), (part) => part.segment)
  }
  return Array.from(value)
}

function boundedTailStart(value: string, limit: number): number {
  let index = value.length
  let scalarCount = 0
  while (index > 0 && scalarCount < limit) {
    index -= 1
    const codeUnit = value.charCodeAt(index)
    if (
      codeUnit >= 0xdc00 &&
      codeUnit <= 0xdfff &&
      index > 0
    ) {
      const preceding = value.charCodeAt(index - 1)
      if (preceding >= 0xd800 && preceding <= 0xdbff) index -= 1
    }
    scalarCount += 1
  }
  return index
}

export function splitStableGraphemeTail(
  previousCarry: string,
  delta: string,
  segment: (value: string) => readonly string[] = splitGraphemes
): GraphemeTailUpdate {
  const previousCarryStart = boundedTailStart(previousCarry, MAX_GRAPHEME_CARRY_SCALARS)
  const deltaTailStart = boundedTailStart(delta, MAX_GRAPHEME_CARRY_SCALARS)
  const stablePrefix = previousCarry.slice(0, previousCarryStart) + delta.slice(0, deltaTailStart)
  const tail = previousCarry.slice(previousCarryStart) + delta.slice(deltaTailStart)
  const graphemes = segment(tail)
  const lastGrapheme = graphemes.at(-1) ?? ''
  let released = stablePrefix + graphemes.slice(0, -1).join('')
  const carryStart = boundedTailStart(lastGrapheme, MAX_GRAPHEME_CARRY_SCALARS)
  if (carryStart > 0) released += lastGrapheme.slice(0, carryStart)

  return { released, carry: lastGrapheme.slice(carryStart) }
}

export function flushGraphemeTail(carry: string): string {
  return carry
}

/**
 * Time-based adaptive typewriter for stream display.
 * Store still holds full logical text immediately; the UI reveals it at a
 * continuous velocity so network bursts feel like smooth typing, not chunk dumps.
 */
export interface SmoothStreamConfig {
  /** Calm reveal rate (Unicode scalars / second) when backlog is thin. */
  baseCharsPerSec: number
  /** Catch-up rate when backlog is large. */
  maxCharsPerSec: number
  /** Backlog size (scalars) at which rate reaches maxCharsPerSec. */
  catchUpBacklog: number
  /**
   * On first non-empty paint while streaming, reveal at most this many scalars
   * immediately for TTFB; the rest drains at the typewriter rate.
   * `0` means show the entire first chunk at once.
   */
  firstBurstMax: number
  /** Hard cap of scalars released in a single tick (safety). */
  maxStep: number
}

export const DEFAULT_SMOOTH_STREAM_CONFIG: SmoothStreamConfig = {
  // ~4–5 chars/frame at 60fps — snappy Chinese / mixed text without chunk dumps.
  baseCharsPerSec: 78,
  // Drain multi-token bursts fast so display lag stays under ~0.3s for large IPC.
  maxCharsPerSec: 360,
  // Begin ramping earlier so English dumps and multi-token CN bursts do not stall.
  catchUpBacklog: 36,
  // Small immediate peek for TTFB; remainder typewrites so the first SSE chunk
  // never dumps a whole paragraph in one paint.
  firstBurstMax: 8,
  maxStep: 56
}

/**
 * Scalars / second given remaining backlog. Smoothstep-ish lerp so rate does
 * not jump discretely between "slow" and "fast".
 */
export function smoothStreamRate(
  backlogScalars: number,
  config: SmoothStreamConfig = DEFAULT_SMOOTH_STREAM_CONFIG
): number {
  if (backlogScalars <= 0) return 0
  const t = Math.min(1, backlogScalars / Math.max(1, config.catchUpBacklog))
  // Ease-in: stay near base for small backlog, ramp to max as it grows.
  const eased = t * t * (3 - 2 * t)
  return (
    config.baseCharsPerSec +
    (config.maxCharsPerSec - config.baseCharsPerSec) * eased
  )
}

/**
 * How many whole scalars to release for `elapsedMs` at the adaptive rate,
 * carrying a fractional residual across frames for continuous motion.
 */
export function smoothStreamStep(
  backlogScalars: number,
  elapsedMs: number,
  residual: number,
  config: SmoothStreamConfig = DEFAULT_SMOOTH_STREAM_CONFIG
): { step: number; residual: number } {
  if (backlogScalars <= 0 || elapsedMs <= 0) {
    return { step: 0, residual }
  }
  const rate = smoothStreamRate(backlogScalars, config)
  const budget = residual + (rate * elapsedMs) / 1000
  const raw = Math.floor(budget)
  const step = Math.min(backlogScalars, Math.max(0, Math.min(config.maxStep, raw)))
  // Keep fractional leftover only when we did not clamp to backlog/maxStep.
  const nextResidual =
    step >= backlogScalars || step >= config.maxStep
      ? 0
      : budget - step
  return { step, residual: nextResidual }
}

/** Advance `from` by up to `scalarCount` Unicode scalars without splitting surrogates. */
export function advanceByUnicodeScalars(
  value: string,
  from: number,
  scalarCount: number
): number {
  let index = Math.max(0, Math.min(from, value.length))
  let remaining = Math.max(0, scalarCount)
  while (index < value.length && remaining > 0) {
    const codeUnit = value.charCodeAt(index)
    if (codeUnit >= 0xd800 && codeUnit <= 0xdbff && index + 1 < value.length) {
      const next = value.charCodeAt(index + 1)
      if (next >= 0xdc00 && next <= 0xdfff) {
        index += 2
        remaining -= 1
        continue
      }
    }
    index += 1
    remaining -= 1
  }
  return index
}

/** Count Unicode scalars from `from` to end of string, capped for cheap probes. */
function countBacklogScalars(value: string, from: number, cap: number): number {
  let index = Math.max(0, Math.min(from, value.length))
  let count = 0
  while (index < value.length && count < cap) {
    const next = advanceByUnicodeScalars(value, index, 1)
    if (next === index) break
    index = next
    count += 1
  }
  return count
}

export class SmoothStreamController {
  private target = ''
  private displayed = ''
  /** Scalar count of `displayed`, maintained incrementally so per-frame
      consumers never rescan the full revealed text. */
  private displayedScalarCount = 0
  private streaming = false
  private residual = 0
  private lastTickAt: number | null = null
  private readonly config: SmoothStreamConfig

  constructor(config: SmoothStreamConfig = DEFAULT_SMOOTH_STREAM_CONFIG) {
    this.config = config
  }

  getDisplayed(): string {
    return this.displayed
  }

  getDisplayedScalarCount(): number {
    return this.displayedScalarCount
  }

  getTarget(): string {
    return this.target
  }

  needsTick(): boolean {
    return this.streaming && this.displayed !== this.target
  }

  reset(): void {
    this.target = ''
    this.displayed = ''
    this.displayedScalarCount = 0
    this.streaming = false
    this.residual = 0
    this.lastTickAt = null
  }

  /**
   * Update logical target. First non-empty paint while streaming reveals up to
   * `firstBurstMax` scalars immediately (TTFB). Terminal / non-streaming snaps
   * to full target.
   */
  setTarget(target: string, streaming: boolean, now = 0): string {
    this.target = target
    this.streaming = streaming

    if (!streaming) {
      this.displayed = target
      this.displayedScalarCount = countUnicodeScalars(target)
      this.residual = 0
      this.lastTickAt = null
      return this.displayed
    }

    // Request reset / content replace (not a pure append).
    if (
      this.displayed.length > 0 &&
      !target.startsWith(this.displayed) &&
      !this.displayed.startsWith(target)
    ) {
      this.displayed = target
      this.displayedScalarCount = countUnicodeScalars(target)
      this.residual = 0
      this.lastTickAt = now || null
      return this.displayed
    }

    if (this.displayed.length > target.length) {
      this.displayed = target
      this.displayedScalarCount = countUnicodeScalars(target)
      this.residual = 0
      this.lastTickAt = now || null
      return this.displayed
    }

    // First visible answer chunk: small immediate peek for TTFB, rest typewrites.
    if (this.displayed.length === 0 && target.length > 0) {
      const burst =
        this.config.firstBurstMax <= 0
          ? target.length
          : advanceByUnicodeScalars(target, 0, this.config.firstBurstMax)
      this.displayed = target.slice(0, burst)
      this.displayedScalarCount = countUnicodeScalars(this.displayed)
      this.residual = 0
      // Anchor the clock so the next tick uses a real frame delta, not a huge gap.
      this.lastTickAt = now || null
      return this.displayed
    }

    return this.displayed
  }

  /**
   * One animation-frame of time-based catch-up.
   * Pass `performance.now()` (or a test clock) so release rate is frame-rate stable.
   */
  tick(now: number): string {
    if (!this.streaming || this.displayed === this.target) {
      this.lastTickAt = now
      return this.displayed
    }
    if (!this.target.startsWith(this.displayed)) {
      this.displayed = this.target
      this.displayedScalarCount = countUnicodeScalars(this.displayed)
      this.residual = 0
      this.lastTickAt = now
      return this.displayed
    }

    const previous = this.lastTickAt
    this.lastTickAt = now
    // First tick after idle: assume one 60fps frame so we still advance.
    const elapsedMs =
      previous === null ? 1000 / 60 : Math.min(100, Math.max(0, now - previous))

    const cap = this.config.maxStep * 4
    const backlog = countBacklogScalars(
      this.target,
      this.displayed.length,
      Math.max(cap, this.config.catchUpBacklog)
    )
    if (backlog <= 0) return this.displayed

    const { step, residual } = smoothStreamStep(
      backlog,
      elapsedMs,
      this.residual,
      this.config
    )
    this.residual = residual
    if (step <= 0) return this.displayed

    const end = advanceByUnicodeScalars(this.target, this.displayed.length, step)
    this.displayedScalarCount += countUnicodeScalars(
      this.target.slice(this.displayed.length, end)
    )
    this.displayed = this.target.slice(0, end)
    return this.displayed
  }
}
