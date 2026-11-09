import thinkingFixtures from './__tests__/fixtures/thinking-protocols.json'
import type { ProviderProtocol } from './constants'
// src/shared/thinking.test.ts
import { describe, expect, it } from 'vitest'
import {
  clampThinkingMode,
  detectModelThinking,
  defaultThinkingLevel,
  effectiveThinkingLevels,
  inferredThinkingCapability,
  inferThinkingLevels,
  modelSupportsThinkingOff,
  protocolSupportsThinkingOff,
  THINKING_LEVELS
} from './thinking'

describe('inferThinkingLevels', () => {
  it('excludes every non-reasoning name variant across protocols', () => {
    for (const marker of ['non-reasoning', 'non reasoning', 'nonreasoning', 'non-thinking', 'non thinking', 'nonthinking']) {
      for (const [protocol, family] of [
        ['openAiChat', 'qwen3'], ['openAiResponses', 'qwen3'],
        ['anthropicMessages', 'claude-sonnet-4'], ['gemini', 'gemini-3-pro']
      ] as const) {
        expect(detectModelThinking(`${family}-${marker}`, protocol)).toEqual({ levels: [], capability: null })
      }
    }
  })

  it('returns empty for ordinary chat models', () => {
    expect(inferThinkingLevels('gpt-4o-mini')).toEqual([])
    expect(inferThinkingLevels('qwen-plus')).toEqual([])
  })

  it('detects OpenAI o-series and versioned GPT reasoning models', () => {
    expect(inferThinkingLevels('o3-mini')).toEqual(['low', 'medium', 'high'])
    expect(inferThinkingLevels('o1')).toEqual(['low', 'medium', 'high'])
    expect(inferThinkingLevels('gpt-5')).toContain('medium')
    expect(inferThinkingLevels('gpt-5.1-codex')).toEqual([
      'low',
      'medium',
      'high'
    ])
    expect(inferThinkingLevels('gpt6-luna')).toEqual(['low', 'medium', 'high', 'xhigh', 'max'])
    expect(inferThinkingLevels('openai/gpt-6-luna')).toEqual(['low', 'medium', 'high', 'xhigh', 'max'])
    expect(inferThinkingLevels('gpt-oss-120b')).toEqual(['low', 'medium', 'high'])
  })

  it('detects qwen3 thinking and Claude extended patterns', () => {
    expect(inferThinkingLevels('deepseek-r1')).toEqual([])
    expect(inferThinkingLevels('qwen3-235b-a22b')).toEqual(['medium'])
    expect(inferThinkingLevels('claude-opus-4-thinking')).toContain('high')
  })

  it('detects DeepSeek V4 low/high/max gears', () => {
    expect(inferThinkingLevels('deepseek-ai/deepseek-v4.1-flash-1017')).toEqual([
      'low',
      'high',
      'xhigh'
    ])
    expect(inferredThinkingCapability('deepseek-ai/deepseek-v4.1-flash-1017')).toMatchObject({
      source: 'heuristic',
      dialect: 'reasoningEffortMax',
      supportsOff: true
    })
  })

  it('detects Anthropic and Gemini families under native and gateway protocols', () => {
    expect(inferThinkingLevels('claude-sonnet-4-5', 'anthropicMessages')).toEqual([
      'low',
      'medium',
      'high'
    ])
    expect(inferThinkingLevels('claude-3-5-sonnet', 'anthropicMessages')).toEqual([])
    expect(inferThinkingLevels('gemini-2.5-pro', 'gemini')).toEqual(['low', 'medium', 'high'])
    expect(inferThinkingLevels('gemini-2.5-pro', 'openAiChat')).toEqual([
      'low',
      'medium',
      'high'
    ])
    expect(inferThinkingLevels('anthropic/claude-sonnet-4.5', 'openAiChat')).toEqual([
      'low',
      'medium',
      'high'
    ])
  })

  it('detects other vendor reasoning families by version', () => {
    expect(inferThinkingLevels('glm-4.6')).toEqual(['medium'])
    expect(inferThinkingLevels('glm-z1-air')).toEqual(['medium'])
    expect(inferThinkingLevels('kimi-k3')).toEqual(['medium'])
    expect(inferThinkingLevels('grok-4-fast')).toEqual(['low', 'medium', 'high'])
    expect(inferThinkingLevels('magistral-medium-2506')).toEqual([])
  })

  it('does not mistake dates or older versions for thinking models', () => {
    expect(inferThinkingLevels('qwen-plus-2025')).toEqual([])
    expect(inferThinkingLevels('qwen2.5-72b-instruct')).toEqual([])
    expect(inferThinkingLevels('glm-4-plus')).toEqual([])
    expect(inferThinkingLevels('grok-3')).toEqual([])
    expect(inferThinkingLevels('deepseek-v3')).toEqual(['medium'])
    expect(inferThinkingLevels('gpt-4.1')).toEqual([])
    expect(inferThinkingLevels('chatgpt-4o-latest')).toEqual([])
  })

  it('ignores non-text and older Gemini models', () => {
    expect(inferThinkingLevels('gemini-embedding-001', 'gemini')).toEqual([])
    expect(inferThinkingLevels('gemini-2.5-flash-image', 'gemini')).toEqual([])
    expect(inferThinkingLevels('gemini-2.5-flash-preview-tts', 'gemini')).toEqual([])
    expect(inferThinkingLevels('gemini-2.0-flash', 'gemini')).toEqual([])
  })
})

describe('thinking defaults and protocol capability', () => {
  it('prefers medium, then high, as the default gear', () => {
    expect(defaultThinkingLevel(['low', 'medium', 'high'])).toBe('medium')
    expect(defaultThinkingLevel(['low', 'high', 'xhigh'])).toBe('high')
    expect(defaultThinkingLevel(['low'])).toBe('low')
    expect(defaultThinkingLevel([])).toBeNull()
  })

  it('marks Gemini Pro as unable to fully disable thinking', () => {
    expect(protocolSupportsThinkingOff('gemini-2.5-pro', 'gemini')).toBe(false)
    expect(protocolSupportsThinkingOff('gemini-2.5-flash', 'gemini')).toBe(true)
    expect(protocolSupportsThinkingOff('o3-mini', 'openAiChat')).toBe(false)
  })

  it('builds heuristic capability with the right dialect', () => {
    expect(inferredThinkingCapability('qwen3-235b-a22b', 'openAiChat')).toMatchObject({
      source: 'heuristic',
      dialect: 'chatTemplateKwargs'
    })
    expect(inferredThinkingCapability('o3-mini', 'openAiChat')).toMatchObject({
      source: 'heuristic',
      dialect: 'reasoningEffort'
    })
    expect(inferredThinkingCapability('gpt6-luna', 'openAiChat')).toMatchObject({
      source: 'heuristic',
      dialect: 'reasoningEffort',
      supportsOff: true
    })
    expect(inferredThinkingCapability('glm-4.6', 'openAiChat')).toMatchObject({
      source: 'heuristic',
      dialect: 'thinkingToggle'
    })
    expect(inferredThinkingCapability('claude-sonnet-4-5', 'openAiChat')).toMatchObject({
      source: 'heuristic',
      dialect: 'reasoningEffort'
    })
    expect(inferredThinkingCapability('claude-sonnet-4-5', 'anthropicMessages')).toMatchObject({
      source: 'heuristic',
      dialect: null
    })
    expect(inferredThinkingCapability('deepseek-r1', 'openAiChat')).toEqual({
      source: 'heuristic',
      dialect: null,
      supportsOff: false
    })
  })
})

describe('clampThinkingMode', () => {
  it('keeps off always', () => {
    expect(clampThinkingMode('off', ['low', 'high'])).toBe('off')
    expect(clampThinkingMode('off', [])).toBe('off')
  })

  it('falls back to off when model has no levels or mode unsupported', () => {
    expect(clampThinkingMode('high', [])).toBe('off')
    expect(clampThinkingMode('xhigh', ['low', 'medium'])).toBe('off')
  })

  it('keeps supported mode', () => {
    expect(clampThinkingMode('medium', ['low', 'medium', 'high'])).toBe('medium')
  })

  it('allows auto only when the model exposes gears', () => {
    expect(clampThinkingMode('auto', ['low', 'medium'])).toBe('auto')
    expect(clampThinkingMode('auto', [])).toBe('off')
  })
})

describe('effectiveThinkingLevels', () => {
  it('prefers stored metadata over inference', () => {
    expect(effectiveThinkingLevels('gpt-4o', ['low', 'high'])).toEqual(['low', 'high'])
    expect(effectiveThinkingLevels('deepseek-r1', ['medium'])).toEqual(['medium'])
  })

  it('falls back to model-id heuristics when metadata is empty', () => {
    expect(effectiveThinkingLevels('deepseek-r1', [])).toEqual([])
    expect(effectiveThinkingLevels('gpt-4o-mini', [])).toEqual([])
  })
})

describe('modelSupportsThinkingOff', () => {
  it('is true whenever thinking levels exist', () => {
    expect(modelSupportsThinkingOff(['low', 'medium'])).toBe(true)
    expect(modelSupportsThinkingOff([])).toBe(false)
  })
})

// Shared with Rust: the picker must offer exactly the gears the request layer sends.
describe('thinking protocol parity', () => {
  it.each(thinkingFixtures)('$protocol $modelId $mode', (fixture) => {
    expect(detectModelThinking(fixture.modelId, fixture.protocol as ProviderProtocol, fixture.baseUrl)).toEqual({
      levels: fixture.levels,
      capability: { source: 'heuristic', dialect: fixture.dialect, supportsOff: fixture.supportsOff }
    })
  })
})
