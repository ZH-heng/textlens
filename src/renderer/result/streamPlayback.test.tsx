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

describe('adaptive snapshot playback', () => {
  it('shows the first block immediately and flushes authoritative terminal content', () => {
    const stream = new SmoothStreamController()
    expect(stream.setTarget('首😀', true, 0)).toBe('首😀')
    stream.setTarget('首😀后续内容', true, 16)
    expect(stream.getDisplayed()).toBe('首😀')
    stream.tick(32)
    expect(stream.setTarget('首😀后续内容', false, 40)).toBe('首😀后续内容')
    expect(stream.needsTick()).toBe(false)
  })
  it('drains without a terminal event and handles replacements', () => {
    const stream = new SmoothStreamController()
    stream.setTarget('首', true, 0)
    stream.setTarget('首' + '😀'.repeat(200), true, 16)
    stream.tick(1100)
    expect(stream.getDisplayed()).toBe(stream.getTarget())
    expect(stream.getDisplayedScalarCount()).toBe(201)
    expect(stream.setTarget('替换', true, 1200)).toBe('替换')
    expect(advanceByUnicodeScalars('😀😀', 0, 1)).toBe(2)
  })
})

it('drains all queued chunks for a hidden window without advancing its clock', () => {
  const stream = new SmoothStreamController()
  stream.setTarget('首', true, 0)
  stream.setTarget('首后续', true, 16)
  stream.setTarget('首后续内容', true, 32)
  expect(stream.tick(33, true)).toBe('首后续内容')
  expect(stream.needsTick()).toBe(false)
  stream.setTarget('首后续内容再次到达', true, 48)
  stream.tick(64)
  expect(stream.getDisplayed().startsWith('首后续内容')).toBe(true)
})
