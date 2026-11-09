import { z } from 'zod'

import type { ProviderProtocol } from './constants'

export const THINKING_LEVELS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const
export type ThinkingLevel = (typeof THINKING_LEVELS)[number]
export type ThinkingMode = 'off' | 'auto' | ThinkingLevel

export const thinkingLevelSchema = z.enum(THINKING_LEVELS)
export const thinkingModeSchema = z.union([z.literal('off'), z.literal('auto'), thinkingLevelSchema])

export const thinkingCapabilitySourceSchema = z.enum(['explicit', 'heuristic', 'manual'])
export type ThinkingCapabilitySource = z.infer<typeof thinkingCapabilitySourceSchema>

export const thinkingDialectSchema = z.enum([
  'reasoningEffort',
  'reasoningEffortMax',
  'enableThinking',
  'chatTemplateKwargs',
  'thinkingToggle',
  'openRouterReasoning',
  'geminiLevel'
])
export type ThinkingDialect = z.infer<typeof thinkingDialectSchema>

export const thinkingCapabilitySchema = z
  .object({
    source: thinkingCapabilitySourceSchema,
    dialect: thinkingDialectSchema.nullable(),
    supportsOff: z.boolean()
  })
  .strict()
export type ThinkingCapability = z.infer<typeof thinkingCapabilitySchema>

export const THINKING_LEVEL_LABELS: Record<ThinkingLevel, string> = {
  minimal: '最低',
  low: '低',
  medium: '中',
  high: '高',
  xhigh: '极高',
  max: '最高'
}

const STANDARD_THREE: ThinkingLevel[] = ['low', 'medium', 'high']
const OPENAI_FULL: ThinkingLevel[] = ['minimal', 'low', 'medium', 'high', 'xhigh']
// DeepSeek V4 exposes low/high/max rather than the OpenAI middle gear.
const DEEPSEEK_V4_LEVELS: ThinkingLevel[] = ['low', 'high', 'xhigh']

type ThinkingFamily =
  | 'oSeries'
  | 'gptOss'
  | 'gpt'
  | 'claude'
  | 'gemini'
  | 'deepseekMax'
  | 'deepseekReasoner'
  | 'qwen'
  | 'glm'
  | 'kimi'
  | 'grok'
  | 'magistral'
  | 'generic'

const FAMILY_LEVELS: Record<ThinkingFamily, ThinkingLevel[]> = {
  oSeries: STANDARD_THREE,
  gptOss: STANDARD_THREE,
  gpt: OPENAI_FULL,
  claude: STANDARD_THREE,
  gemini: STANDARD_THREE,
  deepseekMax: DEEPSEEK_V4_LEVELS,
  // DeepSeek R1 / reasoner models reason unconditionally and expose no
  // reasoning-effort knob, so offering gears would only produce 400s.
  deepseekReasoner: [],
  qwen: STANDARD_THREE,
  glm: STANDARD_THREE,
  kimi: STANDARD_THREE,
  grok: STANDARD_THREE,
  magistral: STANDARD_THREE,
  generic: STANDARD_THREE
}

function isModelNameChar(character: string | undefined): boolean {
  return character !== undefined && /[a-z0-9]/iu.test(character)
}

/**
 * First 1-2 digit version after a vendor anchor (gpt6, gpt-5.1, claude-3-7,
 * gemini-2.5). Longer numbers such as dates or parameter counts are ignored.
 */
function familyVersion(id: string, anchor: string): readonly [number, number] | null {
  let from = 0
  while (from < id.length) {
    const at = id.indexOf(anchor, from)
    if (at < 0) return null
    from = at + 1
    if (isModelNameChar(id[at - 1]) || /^[a-z]/iu.test(id.slice(at + anchor.length))) {
      continue
    }
    const match = /^[^0-9]{0,16}?(\d{1,2})(?!\d)(?:[.-](\d{1,2})(?!\d))?/u.exec(
      id.slice(at + anchor.length)
    )
    if (match) return [Number(match[1]), Number(match[2] ?? 0)]
  }
  return null
}

function isOSeries(id: string): boolean {
  return /(^|[/:_-])o[1-9]([/:_-]|$)/u.test(id)
}

function isReasoningClaude(id: string): boolean {
  if (!id.includes('claude')) return false
  if (id.includes('thinking')) return true
  const version = familyVersion(id, 'claude')
  return version !== null && (version[0] > 3 || (version[0] === 3 && version[1] >= 7))
}

function isReasoningGemini(id: string): boolean {
  if (!id.includes('gemini')) return false
  if (['embedding', 'image', 'tts', 'audio'].some((kind) => id.includes(kind))) return false
  if (id.includes('thinking')) return true
  const version = familyVersion(id, 'gemini')
  return version !== null && (version[0] > 2 || (version[0] === 2 && version[1] >= 5))
}

function isReasoningQwen(id: string): boolean {
  if (id.includes('qwq')) return true
  const version = familyVersion(id, 'qwen')
  return (
    version !== null &&
    version[0] >= 3 &&
    (id.includes('think') || id.includes('reasoning') || !id.includes('instruct'))
  )
}

function isReasoningGlm(id: string): boolean {
  if (id.includes('glm-z1')) return true
  const version = familyVersion(id, 'glm')
  return version !== null && (version[0] > 4 || (version[0] === 4 && version[1] >= 5))
}

function isReasoningKimi(id: string): boolean {
  const version = familyVersion(id, 'kimi')
  return version !== null && version[0] >= 2
}

function isReasoningGrok(id: string): boolean {
  const version = familyVersion(id, 'grok')
  return version !== null && (version[0] >= 4 || (version[0] === 3 && id.includes('mini')))
}

function detectOpenAiFamily(id: string): ThinkingFamily | null {
  if (isOSeries(id)) return 'oSeries'
  if (id.includes('gpt-oss')) return 'gptOss'
  const gpt = familyVersion(id, 'gpt')
  if (gpt && id.includes('chat')) return null
  if (gpt && gpt[0] >= 5) return 'gpt'
  if (isReasoningClaude(id)) return 'claude'
  if (isReasoningGemini(id)) return 'gemini'
  const deepSeek = familyVersion(id, 'deepseek')
  if (deepSeek && deepSeek[0] >= 4) return 'deepseekMax'
  if (id.includes('deepseek') && (id.includes('r1') || id.includes('reasoner'))) {
    return 'deepseekReasoner'
  }
  if (isReasoningQwen(id)) return 'qwen'
  if (isReasoningGlm(id)) return 'glm'
  if (isReasoningKimi(id)) return 'kimi'
  if (isReasoningGrok(id)) return 'grok'
  if (id.includes('magistral')) return 'magistral'
  if (id.includes('thinking') || id.includes('reasoner') || id.includes('reasoning')) {
    return 'generic'
  }
  return null
}

function detectThinkingFamily(id: string, protocol: ProviderProtocol): ThinkingFamily | null {
  if (protocol === 'anthropicMessages') return isReasoningClaude(id) ? 'claude' : null
  if (protocol === 'gemini') return isReasoningGemini(id) ? 'gemini' : null
  return detectOpenAiFamily(id)
}

/** Keep the detected wire dialect separate from each tool's thinking preference. */
export function detectModelThinking(
  modelId: string,
  protocol: ProviderProtocol = 'openAiChat',
  baseUrl = ''
): { levels: ThinkingLevel[]; capability: ThinkingCapability | null } {
  const id = modelId.trim().toLowerCase()
  let family = detectThinkingFamily(id, protocol)
  const deepseek = familyVersion(id, 'deepseek')
  const openAiProtocol = protocol === 'openAiChat' || protocol === 'openAiResponses'
  if (!family && openAiProtocol && deepseek?.[0] === 3) family = 'generic'
  if (!family || /non[- ]?(?:reasoning|thinking)/u.test(id)) return { levels: [], capability: null }
  let levels = [...FAMILY_LEVELS[family]]
  let dialect: ThinkingDialect | null = protocol === 'anthropicMessages' || protocol === 'gemini'
    ? null
    : family === 'deepseekMax' ? 'reasoningEffortMax' : 'reasoningEffort'
  let supportsOff = true
  if (protocol === 'gemini') {
    if (id.includes('gemini-2.0-flash-thinking') || id.includes('thinking-exp')) {
      return { levels: [], capability: { source: 'heuristic', dialect: null, supportsOff: false } }
    }
    if ((familyVersion(id, 'gemini')?.[0] ?? 0) >= 3) {
      dialect = 'geminiLevel'
      levels = id.includes('flash') ? ['minimal', 'low', 'medium', 'high'] : ['low', 'high']
      supportsOff = false
    } else {
      supportsOff = !(id.includes('pro') && !id.includes('flash'))
    }
  } else if (protocol !== 'anthropicMessages') {
    const gpt = familyVersion(id, 'gpt')
    if (family === 'gpt' && gpt) {
      if (gpt[0] >= 6) {
        levels = ['low', 'medium', 'high', 'xhigh', 'max']
        supportsOff = !(id.includes('astra') || (gpt[1] === 1 && id.includes('sol')))
      } else if (gpt[1] >= 2) {
        levels = id.includes('pro') ? ['medium', 'high', 'xhigh'] : ['low', 'medium', 'high', 'xhigh']
        supportsOff = !id.includes('pro')
      } else {
        levels = gpt[1] === 0 && !id.includes('codex')
          ? ['minimal', 'low', 'medium', 'high']
          : ['low', 'medium', 'high']
        supportsOff = gpt[1] >= 1
      }
    }
    if (family === 'oSeries' || family === 'gptOss' || family === 'grok') supportsOff = false
    if (family === 'oSeries' && /(?:^|[/])o1-(?:preview|mini)(?:-|$)/u.test(id)) {
      levels = []
      dialect = null
    }
    if (family === 'qwen') {
      const qwen = familyVersion(id, 'qwen')
      if (qwen && qwen[0] === 3 && qwen[1] >= 8) {
        levels = ['low', 'medium', 'xhigh']
      } else {
        dialect = /(?:^|\.)dashscope\.aliyuncs\.com$/u.test(endpointHost(baseUrl))
          ? 'enableThinking' : 'chatTemplateKwargs'
        levels = ['medium']
      }
    }
    if (family === 'glm') {
      const glm = familyVersion(id, 'glm')
      if (glm && (glm[0] > 5 || (glm[0] === 5 && glm[1] >= 1))) {
        levels = ['low', 'medium', 'high', 'xhigh']
      } else {
        dialect = 'thinkingToggle'
        levels = ['medium']
      }
    }
    if (family === 'kimi' || deepseek?.[0] === 3) {
      dialect = 'thinkingToggle'
      levels = ['medium']
    }
    if (family === 'deepseekReasoner' || family === 'magistral'
      || (family === 'generic' && deepseek?.[0] !== 3)
      || (family === 'kimi' && id.includes('thinking'))) {
      levels = []
      dialect = null
      supportsOff = false
    }
    if (endpointHost(baseUrl) === 'openrouter.ai' && levels.length > 0) dialect = 'openRouterReasoning'
  }
  return { levels, capability: { source: 'heuristic', dialect, supportsOff } }
}

function endpointHost(baseUrl: string): string {
  try {
    return new URL(baseUrl).hostname.toLowerCase()
  } catch {
    return ''
  }
}

export function inferThinkingLevels(
  modelId: string,
  protocol: ProviderProtocol = 'openAiChat',
  baseUrl = ''
): ThinkingLevel[] {
  return detectModelThinking(modelId, protocol, baseUrl).levels
}

export function protocolSupportsThinkingOff(
  modelId: string,
  protocol: ProviderProtocol = 'openAiChat'
): boolean {
  return detectModelThinking(modelId, protocol).capability?.supportsOff ?? true
}

export function defaultThinkingLevel(levels: readonly ThinkingLevel[]): ThinkingLevel | null {
  if (levels.includes('medium')) return 'medium'
  if (levels.includes('high')) return 'high'
  return levels[0] ?? null
}

export function inferredThinkingCapability(
  modelId: string,
  protocol: ProviderProtocol = 'openAiChat',
  baseUrl = ''
): ThinkingCapability | null {
  return detectModelThinking(modelId, protocol, baseUrl).capability
}

export function isThinkingToggle(capability: ThinkingCapability | null | undefined): boolean {
  return capability?.dialect === 'enableThinking'
    || capability?.dialect === 'chatTemplateKwargs'
    || capability?.dialect === 'thinkingToggle'
}

export function clampThinkingMode(
  mode: ThinkingMode,
  levels: readonly ThinkingLevel[]
): ThinkingMode {
  if (mode === 'off') return 'off'
  if (mode === 'auto') return levels.length > 0 ? 'auto' : 'off'
  return levels.includes(mode) ? mode : 'off'
}

export function thinkingModeLabel(mode: ThinkingMode): string {
  if (mode === 'off') return '关闭思考'
  if (mode === 'auto') return '跟随模型默认'
  return THINKING_LEVEL_LABELS[mode]
}

/**
 * Levels to offer in the action model picker: prefer stored metadata, else
 * infer from the model id so thinking models still show intensity controls.
 */
export function effectiveThinkingLevels(
  modelId: string,
  storedLevels?: readonly ThinkingLevel[] | null,
  protocol: ProviderProtocol = 'openAiChat'
): ThinkingLevel[] {
  if (storedLevels && storedLevels.length > 0) return [...storedLevels]
  return inferThinkingLevels(modelId, protocol)
}

/**
 * Whether the action editor should offer “关闭思考”.
 * Always available when the model has thinking levels; backend adapts via
 * `supportsOff` (disable thinking or use the lowest supported level).
 */
export function modelSupportsThinkingOff(
  levels: readonly ThinkingLevel[],
  supportsOff = true
): boolean {
  return levels.length > 0 && supportsOff
}
