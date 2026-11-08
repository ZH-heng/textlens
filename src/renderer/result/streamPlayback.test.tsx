import { describe, expect, it, vi } from 'vitest'

import {
  advanceByUnicodeScalars,
  flushGraphemeTail,
  MAX_GRAPHEME_CARRY_SCALARS,
  SmoothStreamController,
  splitGraphemes,
  splitStableGraphemeTail
} from './streamPlayback'

describe('bounded grapheme tail', () => {
  it.each([
    ['combining mark', 'e', '\u0301', 'e\u0301'],
    ['ZWJ family', '👨', '\u200d👩', '👨\u200d👩'],
    ['variation selector', '❤', '\ufe0f', '❤️'],
    ['skin tone', '👍', '🏽', '👍🏽'],
    ['regional flag', '🇨', '🇳', '🇨🇳'],
    ['Hangul Jamo', 'ᄀ', 'ᅡ', '가']
  ])('keeps the %s tail bounded across deltas', (_name, first, second, combined) => {
    const held = splitStableGraphemeTail('', first)
    expect(held.carry).toBe(first)
    const extended = splitStableGraphemeTail(held.carry, second)
    expect(`${extended.released}${extended.carry}`).toBe(combined)
  })

  it('segments only a bounded suffix and bounds the carry in Unicode scalars', () => {
    const segment = vi.fn(splitGraphemes)
    const prefix = 'a'.repeat(1_000_000)
    const update = splitStableGraphemeTail('', `${prefix}😀`, segment)

    expect(segment).toHaveBeenCalledTimes(1)
    expect(Array.from(segment.mock.calls[0]![0]).length).toBeLessThanOrEqual(512)
    expect(Array.from(update.carry).length).toBeLessThanOrEqual(MAX_GRAPHEME_CARRY_SCALARS)
    expect(`${update.released}${update.carry}`).toBe(`${prefix}😀`)
  })

  it('flushes the terminal grapheme tail', () => {
    const held = splitStableGraphemeTail('', 'e')
    expect(flushGraphemeTail(held.carry)).toBe('e')
  })
})

describe('smooth stream typewriter', () => {
  it('shows a small first-burst peek then drains backlog smoothly over time', () => {
    const stream = new SmoothStreamController()
    // First chunks are always immediate.
    expect(stream.setTarget('你好', true, 0)).toBe('你好')
    expect(stream.needsTick()).toBe(false)

    const long = `你好${'字'.repeat(80)}`
    expect(stream.setTarget(long, true, 1000)).toBe('你好')
    expect(stream.needsTick()).toBe(true)

    const afterOne = stream.tick(1016)
    expect(afterOne.startsWith('你好')).toBe(true)
    expect(afterOne.length).toBeGreaterThan('你好'.length)
    expect(afterOne.length).toBeLessThan(long.length)

    let guard = 0
    let t = 1016
    while (stream.needsTick() && guard < 400) {
      t += 16
      stream.tick(t)
      guard += 1
    }
    expect(stream.getDisplayed()).toBe(long)
  })

  it('shows the complete opening chunk immediately, including large chunks', () => {
    const stream = new SmoothStreamController()
    const opening = '字'.repeat(40)
    const shown = stream.setTarget(opening, true, 0)
    expect(shown).toBe(opening)
    expect(shown.length).toBeGreaterThan(0)
    expect(stream.needsTick()).toBe(false)
  })

  it('snaps to full text when streaming ends', () => {
    const stream = new SmoothStreamController()
    stream.setTarget('ab', true, 0)
    stream.setTarget(`ab${'c'.repeat(50)}`, true, 16)
    expect(stream.getDisplayed().length).toBeLessThan(52)
    expect(stream.setTarget(`ab${'c'.repeat(50)}`, false, 32)).toBe(`ab${'c'.repeat(50)}`)
    expect(stream.needsTick()).toBe(false)
  })


  it('bounds display lag even for huge bursts and continuous target growth', () => {
    const stream = new SmoothStreamController()
    stream.setTarget('My', true, 0)
    stream.setTarget('My' + '😀'.repeat(2000), true, 1000)
    const firstFrame = stream.tick(1016)
    expect(firstFrame.length).toBeGreaterThan(2)
    expect(firstFrame.length).toBeLessThan(stream.getTarget().length)
    for (let t = 1032; t <= 1112; t += 16) {
      stream.setTarget(stream.getTarget() + '中文', true, t)
      stream.tick(t)
    }
    stream.tick(1176)
    expect(stream.getDisplayed()).toBe(stream.getTarget())
    expect(stream.getDisplayedScalarCount()).toBe(Array.from(stream.getTarget()).length)
    expect(stream.needsTick()).toBe(false)
    // A new burst after an idle period gets its own playback budget.
    const shown = stream.getDisplayed()
    stream.setTarget(shown + 'x'.repeat(100), true, 5000)
    expect(stream.tick(5016).length - shown.length).toBeLessThan(100)
    stream.tick(5120)
    expect(stream.getDisplayed()).toBe(stream.getTarget())
  })

  it('releases expired chunks during sustained arrivals at different rates', () => {
    for (const charsPerSecond of [20, 80, 240, 1200, 6000]) {
      const stream = new SmoothStreamController()
      let target = '首'
      stream.setTarget(target, true, 0)
      const arrivals: Array<{ at: number; end: number }> = []
      let emitted = 0
      for (let time = 16; time <= 1000; time += 16) {
        const next = Math.floor(charsPerSecond * time / 1000)
        const delta = '字'.repeat(next - emitted)
        emitted = next
        target += delta
        if (delta) {
          stream.setTarget(target, true, time)
          arrivals.push({ at: time, end: target.length })
        }
        stream.tick(time)
        for (const arrival of arrivals) {
          if (time - arrival.at >= 64) expect(stream.getDisplayed().length).toBeGreaterThanOrEqual(arrival.end)
        }
      }
      stream.tick(1100)
      expect(stream.getDisplayed()).toBe(target)
    }
  })

  it('rebuilds safely for replaced and shortened snapshots', () => {
    const stream = new SmoothStreamController()
    stream.setTarget('first', true, 0)
    stream.setTarget('first pending', true, 10)
    expect(stream.setTarget('replacement😀', true, 20)).toBe('replacement😀')
    expect(stream.setTarget('replace', true, 30)).toBe('replace')
    expect(stream.needsTick()).toBe(false)
  })

  it('does not split surrogate pairs when advancing', () => {
    const emoji = '😀'
    const value = `${emoji}${emoji}${emoji}`
    const mid = advanceByUnicodeScalars(value, 0, 1)
    expect(value.slice(0, mid)).toBe(emoji)
    expect(advanceByUnicodeScalars(value, 0, 3)).toBe(value.length)
  })
})
