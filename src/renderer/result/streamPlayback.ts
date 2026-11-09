import { AdaptiveTextBuffer } from './adaptiveTextBuffer'
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

/** Snapshot adapter: first content is immediate, subsequent content uses Myna's playout buffer. */
export class SmoothStreamController {
  private buffer = new AdaptiveTextBuffer()
  private target = ''
  private displayed = ''
  private scalarCount = 0
  private streaming = false
  getDisplayed(): string { return this.displayed }
  getDisplayedScalarCount(): number { return this.scalarCount }
  getTarget(): string { return this.target }
  needsTick(): boolean { return this.streaming && this.buffer.length > 0 }
  getDeadline(): number { return this.buffer.deadline }
  reset(): void {
    this.buffer = new AdaptiveTextBuffer()
    this.target = this.displayed = ''
    this.scalarCount = 0
    this.streaming = false
  }
  setTarget(target: string, streaming: boolean, now = 0): string {
    const previous = this.target
    if (!streaming || !target.startsWith(previous)) {
      this.reset()
      this.target = this.displayed = target
      this.scalarCount = countUnicodeScalars(target)
      this.streaming = streaming
      return this.displayed
    }
    this.streaming = streaming
    this.target = target
    this.buffer.push(target.slice(previous.length), now)
    if (!previous && target) this.release(now, true)
    return this.displayed
  }
  private release(now: number, all = false): void {
    const text = this.buffer.take(now, all)
    if (text) {
      this.displayed += text
      this.scalarCount += countUnicodeScalars(text)
    }
  }
  tick(now: number, all = false): string {
    if (this.streaming) this.release(now, all)
    return this.displayed
  }
}
