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

/** Advance by Unicode scalars without splitting a surrogate pair. */
export function advanceByUnicodeScalars(value: string, from: number, scalarCount: number): number {
  let index = Math.max(0, Math.min(from, value.length))
  while (index < value.length && scalarCount-- > 0) {
    const first = value.charCodeAt(index++)
    if (first >= 0xd800 && first <= 0xdbff && index < value.length) {
      const next = value.charCodeAt(index)
      if (next >= 0xdc00 && next <= 0xdfff) index++
    }
  }
  return index
}

// Time budgets, not output speed limits. The first chunk bypasses playback.
const MIN_SMOOTH_MS = 32
const MAX_SMOOTH_MS = 48
const MAX_QUEUED_MS = 64
const RATE_RESET_MS = 250

interface Chunk { text: string; offset: number; arrivedAt: number }

export class SmoothStreamController {
  private target = ''
  private displayed = ''
  private scalarCount = 0
  private streaming = false
  private chunks: Chunk[] = []
  private size = 0
  private lastArrival: number | null = null
  private lastFrame = 0
  private interval = 0
  private rate = 0
  private credit = 0

  getDisplayed(): string { return this.displayed }
  getDisplayedScalarCount(): number { return this.scalarCount }
  getTarget(): string { return this.target }
  needsTick(): boolean { return this.streaming && this.size > 0 }
  getDeadline(): number { return this.chunks.length ? this.chunks[0]!.arrivedAt + MAX_QUEUED_MS : Infinity }

  reset(): void {
    this.target = ''
    this.displayed = ''
    this.scalarCount = 0
    this.streaming = false
    this.clearQueue()
    this.lastArrival = null
    this.interval = this.rate = 0
  }

  private clearQueue(): void {
    this.chunks = []
    this.size = 0
    this.credit = 0
  }

  setTarget(target: string, streaming: boolean, now = 0): string {
    const previous = this.target
    this.streaming = streaming
    this.target = target
    if (!streaming || !target.startsWith(previous)) {
      this.clearQueue()
      this.displayed = target
      this.scalarCount = countUnicodeScalars(target)
      this.lastArrival = streaming && target ? now : null
      this.interval = this.rate = 0
      return this.displayed
    }
    const delta = target.slice(previous.length)
    if (!delta) return this.displayed
    if (this.lastArrival !== null) {
      const gap = now - this.lastArrival
      if (gap > 0) {
        const sample = delta.length * 1000 / Math.max(16, gap)
        if (!this.interval || gap >= RATE_RESET_MS) {
          this.interval = Math.min(gap, MAX_SMOOTH_MS * 2)
          this.rate = sample
        } else {
          const weight = 1 - Math.exp(-gap / 120)
          this.interval += weight * (Math.min(gap, MAX_SMOOTH_MS * 2) - this.interval)
          this.rate += weight * (sample - this.rate)
        }
      }
    }
    this.lastArrival = now
    if (!previous) {
      this.displayed = delta
      this.scalarCount = countUnicodeScalars(delta)
      return this.displayed
    }
    if (!this.size) { this.lastFrame = now; this.credit = 0 }
    this.chunks.push({ text: delta, offset: 0, arrivedAt: now })
    this.size += delta.length
    return this.displayed
  }

  tick(now: number): string {
    if (!this.needsTick()) return this.displayed
    const elapsed = Math.max(0, now - this.lastFrame)
    this.lastFrame = now
    const window = Math.max(MIN_SMOOTH_MS, Math.min(MAX_SMOOTH_MS, this.interval / 2))
    const speed = Math.max(this.rate, this.size * 1000 / window)
    const budget = this.credit + speed * elapsed / 1000
    let count = elapsed >= MAX_SMOOTH_MS || this.size <= 2
      ? this.size : Math.max(1, Math.floor(budget))
    let due = 0
    for (const chunk of this.chunks) {
      if (now - chunk.arrivedAt < MAX_QUEUED_MS) break
      due += chunk.text.length - chunk.offset
    }
    count = Math.min(this.size, Math.max(count, due))
    this.credit = Math.max(0, budget - count) % 1
    const parts: string[] = []
    while (count > 0 && this.chunks.length) {
      const chunk = this.chunks[0]!
      let end = Math.min(chunk.text.length, chunk.offset + count)
      const before = chunk.text.charCodeAt(end - 1)
      const after = chunk.text.charCodeAt(end)
      if (before >= 0xd800 && before <= 0xdbff && after >= 0xdc00 && after <= 0xdfff) end++
      const part = chunk.text.slice(chunk.offset, end)
      parts.push(part)
      const taken = end - chunk.offset
      this.size -= taken
      count -= taken
      chunk.offset = end
      if (end === chunk.text.length) this.chunks.shift()
    }
    const released = parts.join('')
    this.displayed += released
    this.scalarCount += countUnicodeScalars(released)
    return this.displayed
  }
}
