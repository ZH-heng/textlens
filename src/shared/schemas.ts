import { z } from 'zod'

import {
  AI_OUTPUT_LIMIT,
  AI_TEXT_LIMIT,
  BUILTIN_SEARCH_ENGINE_IDS,
  DEFAULT_RESULT_FONT_SIZE,
  MAX_ACTIONS,
  MAX_ENABLED_ACTIONS,
  MAX_PROVIDERS,
  MAX_PROVIDER_MODELS,
  RESULT_FONT_SIZE_MAX,
  RESULT_FONT_SIZE_MIN,
  SETTINGS_VERSION,
  TEXT_PLACEHOLDER
} from './constants'
import { TRANSLATION_LANGUAGES, UI_LOCALES } from './languages'
import {
  THINKING_LEVELS,
  thinkingCapabilitySchema,
  thinkingLevelSchema,
  thinkingModeSchema
} from './thinking'
import { countUnicodeScalars, hasAtMostUnicodeScalars } from './unicode'
import { isSafeExternalUrl, validateOpenAiBaseUrl } from './urls'

/** UI / AI output language (summary, explain). */
export const supportedLocaleSchema = z.enum(UI_LOCALES)
export type SupportedLocale = z.infer<typeof supportedLocaleSchema>

/** Translate action target languages (result-box switcher + settings pair). */
export const translationLanguageSchema = z.enum(TRANSLATION_LANGUAGES)
export type { TranslationLanguage } from './languages'

/** Fixed built-in search engines (templates live in constants, not user settings). */
export const searchEngineIdSchema = z.enum(BUILTIN_SEARCH_ENGINE_IDS)
export type SearchEngineId = z.infer<typeof searchEngineIdSchema>
/** @deprecated Prefer searchEngineIdSchema. */
export const legacySearchEngineIdSchema = searchEngineIdSchema
export type LegacySearchEngineId = SearchEngineId

export const pointSchema = z
  .object({ x: z.number().finite(), y: z.number().finite() })
  .strict()
export type Point = z.infer<typeof pointSchema>

export const rectangleSchema = pointSchema
  .extend({
    width: z.number().finite().nonnegative(),
    height: z.number().finite().nonnegative()
  })
  .strict()
export type Rectangle = z.infer<typeof rectangleSchema>

export const windowSizeSchema = z
  .object({
    width: z.number().finite().min(300).max(4_096),
    height: z.number().finite().min(200).max(4_096)
  })
  .strict()
export type WindowSize = z.infer<typeof windowSizeSchema>

export const selectionAnchorSchema = z.discriminatedUnion('kind', [
  rectangleSchema.extend({ kind: z.literal('selection') }).strict(),
  pointSchema.extend({ kind: z.literal('cursor') }).strict()
])
export type SelectionAnchor = z.infer<typeof selectionAnchorSchema>

export const sourceApplicationSchema = z
  .object({
    name: z.string().trim().min(1).max(256),
    bundleId: z.string().trim().min(1).max(512).nullable().default(null)
  })
  .strict()
export type SourceApplication = z.infer<typeof sourceApplicationSchema>

export const selectionDirectionSchema = z.enum(['forward', 'backward', 'unknown'])
export type SelectionDirection = z.infer<typeof selectionDirectionSchema>

export const selectionPayloadSchema = z
  .object({
    selectionId: z.string().trim().min(1).max(128).optional(),
    text: z.string().min(1).max(1_000_000),
    sourceApp: sourceApplicationSchema,
    anchor: selectionAnchorSchema,
    direction: selectionDirectionSchema.default('unknown'),
    isFullscreen: z.boolean().default(false)
  })
  .strict()
export type SelectionPayload = z.infer<typeof selectionPayloadSchema>

export const localActionKindSchema = z.enum(['copy', 'search'])
export const aiActionKindSchema = z.enum([
  'translate',
  'summary',
  'explain',
  'refine',
  'custom',
  'ask'
])
export const actionKindSchema = z.enum([
  'copy',
  'search',
  'translate',
  'summary',
  'explain',
  'refine',
  'custom',
  'ask'
])
/** @deprecated Prefer ActionKind. */
export const actionTypeSchema = actionKindSchema
export type ActionKind = z.infer<typeof actionKindSchema>
/** @deprecated Prefer ActionKind. */
export type ActionType = ActionKind

// Keep these aliases aligned with global-hotkey 0.8.0, which is the parser
// used by tauri-plugin-global-shortcut 2.3.2. Keep platform-specific names out:
// accepting them here would make the settings form pass and native
// registration fail later.
const TAURI_SHORTCUT_MODIFIERS = new Set([
  'command',
  'cmd',
  'control',
  'ctrl',
  'commandorcontrol',
  'commandorctrl',
  'cmdorctrl',
  'cmdorcontrol',
  'alt',
  'option',
  'shift',
  'super'
])

const TAURI_SHORTCUT_NAMED_KEYS = new Set([
  'backquote',
  'backslash',
  'bracketleft',
  'bracketright',
  'pause',
  'pausebreak',
  'comma',
  'equal',
  'minus',
  'period',
  'quote',
  'semicolon',
  'slash',
  'space',
  'tab',
  'capslock',
  'numlock',
  'scrolllock',
  'backspace',
  'delete',
  'insert',
  'enter',
  'up',
  'arrowup',
  'down',
  'arrowdown',
  'left',
  'arrowleft',
  'right',
  'arrowright',
  'home',
  'end',
  'pageup',
  'pagedown',
  'escape',
  'esc',
  'numpadadd',
  'numadd',
  'numpadplus',
  'numplus',
  'numpaddecimal',
  'numdecimal',
  'numpaddivide',
  'numdivide',
  'numpadenter',
  'numenter',
  'numpadequal',
  'numequal',
  'numpadmultiply',
  'nummultiply',
  'numpadsubtract',
  'numsubtract',
  'audiovolumeup',
  'volumeup',
  'audiovolumedown',
  'volumedown',
  'audiovolumemute',
  'volumemute',
  'mediaplay',
  'mediapause',
  'mediaplaypause',
  'mediastop',
  'mediatracknext',
  'mediatrackprev',
  'mediatrackprevious',
  'printscreen'
])

const TAURI_SHORTCUT_SYMBOL_KEYS = new Set([
  '`',
  '\\',
  '[',
  ']',
  ',',
  '=',
  '-',
  '.',
  "'",
  ';',
  '/'
])

function isTauriShortcutKey(value: string): boolean {
  const key = value.toLowerCase()
  return (
    /^[a-z0-9]$/u.test(key) ||
    /^(?:key[a-z]|digit[0-9])$/u.test(key) ||
    /^f(?:[1-9]|1\d|2[0-4])$/u.test(key) ||
    /^(?:numpad|num)[0-9]$/u.test(key) ||
    TAURI_SHORTCUT_NAMED_KEYS.has(key) ||
    TAURI_SHORTCUT_SYMBOL_KEYS.has(key)
  )
}

export function isValidTauriGlobalShortcut(value: string): boolean {
  const trimmed = value.trim()
  if (trimmed === '') return true

  const tokens = trimmed.split('+').map((part) => part.trim())
  if (tokens.some((token) => token.length === 0)) return false
  if (tokens.length === 1) return isTauriShortcutKey(tokens[0]!)

  let foundKey = false
  for (const token of tokens) {
    if (foundKey) return false
    if (TAURI_SHORTCUT_MODIFIERS.has(token.toLowerCase())) continue
    if (!isTauriShortcutKey(token)) return false
    foundKey = true
  }
  return foundKey
}

export const captureShortcutSchema = z
  .string()
  .trim()
  .max(128)
  .refine(isValidTauriGlobalShortcut, '全局快捷键格式无效')

export const actionIdSchema = z
  .string()
  .trim()
  .min(1)
  .max(64)
  .regex(/^[a-zA-Z0-9][a-zA-Z0-9._-]*$/u, '动作 ID 格式无效')

export const lucideIconNameSchema = z
  .string()
  .trim()
  .min(1)
  .max(64)
  .regex(/^[a-z0-9]+(?:-[a-z0-9]+)*$/u, '图标必须是安全的 Lucide 名称')

const actionBaseShape = {
  id: actionIdSchema,
  name: z.string().trim().min(1).max(40),
  icon: lucideIconNameSchema,
  enabled: z.boolean(),
  order: z.number().int().nonnegative().max(10_000)
} as const

export const copyActionSchema = z
  .object({
    ...actionBaseShape,
    kind: z.literal('copy')
  })
  .strict()

export const searchActionSchema = z
  .object({
    ...actionBaseShape,
    kind: z.literal('search'),
    searchEngineId: searchEngineIdSchema
  })
  .strict()

export const localActionSchema = z.discriminatedUnion('kind', [
  copyActionSchema,
  searchActionSchema
])

const aiActionBaseShape = {
  ...actionBaseShape,
  providerId: z.string().trim().max(64),
  modelId: z.string().trim().max(256),
  prompt: z
    .string()
    .trim()
    .min(1)
    .max(10_000)
    .refine((prompt) => prompt.includes(TEXT_PLACEHOLDER), {
      message: `AI 提示词必须包含 ${TEXT_PLACEHOLDER}`
    }),
  thinkingMode: thinkingModeSchema.default('off')
} as const

export const aiActionSchema = z
  .object({
    ...aiActionBaseShape,
    kind: aiActionKindSchema
  })
  .strict()

export const customActionSchema = z
  .object({
    ...aiActionBaseShape,
    kind: z.literal('custom')
  })
  .strict()

/** Actions shipped by default are editable and use the same runtime shape as user actions. */
export const builtInActionSchema = z.discriminatedUnion('kind', [
  copyActionSchema,
  searchActionSchema,
  z
    .object({
      ...aiActionBaseShape,
      kind: z.enum(['translate', 'summary', 'explain', 'refine', 'ask'])
    })
    .strict()
])

export const actionDefinitionSchema = z.discriminatedUnion('kind', [
  copyActionSchema,
  searchActionSchema,
  aiActionSchema
])
export type ActionDefinition = z.infer<typeof actionDefinitionSchema>
export type LocalActionDefinition = z.infer<typeof localActionSchema>
export type SearchActionDefinition = z.infer<typeof searchActionSchema>
export type AiActionDefinition = z.infer<typeof aiActionSchema>
export type BuiltInActionDefinition = z.infer<typeof builtInActionSchema>
export type CustomActionDefinition = z.infer<typeof customActionSchema>

export function isAiActionDefinition(action: ActionDefinition): action is AiActionDefinition {
  return aiActionKindSchema.safeParse(action.kind).success
}

export function isSearchActionDefinition(action: ActionDefinition): action is SearchActionDefinition {
  return action.kind === 'search'
}

export const actionsSchema = z
  .array(actionDefinitionSchema)
  .max(MAX_ACTIONS)
  .superRefine((actions, context) => {
    const ids = new Set<string>()
    for (const action of actions) {
      if (ids.has(action.id)) {
        context.addIssue({ code: z.ZodIssueCode.custom, message: `动作 ID 重复：${action.id}` })
      }
      ids.add(action.id)
    }

    const enabledCount = actions.filter((action) => action.enabled).length
    if (enabledCount < 1) {
      context.addIssue({ code: z.ZodIssueCode.custom, message: '至少需要启用一个工具栏动作' })
    }
    if (enabledCount > MAX_ENABLED_ACTIONS) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        message: `最多只能启用 ${MAX_ENABLED_ACTIONS} 个动作`
      })
    }
  })

export const translationSettingsSchema = z
  .object({
    primaryLanguage: translationLanguageSchema,
    alternateLanguage: translationLanguageSchema
  })
  .strict()
  .refine((value) => value.primaryLanguage !== value.alternateLanguage, {
    message: '翻译语言必须不同'
  })
export type TranslationSettings = z.infer<typeof translationSettingsSchema>

export const providerModelSchema = z
  .object({
    id: z.string().trim().min(1).max(256),
    name: z.string().trim().min(1).max(256),
    thinkingLevels: z.array(thinkingLevelSchema).max(THINKING_LEVELS.length).default([]),
    thinkingCapability: thinkingCapabilitySchema.optional()
  })
  .strict()
export type ProviderModel = z.infer<typeof providerModelSchema>

const providerMetadataShape = {
  id: z
    .string()
    .trim()
    .min(1)
    .max(64)
    .regex(/^[a-zA-Z0-9][a-zA-Z0-9._-]*$/u, '服务商 ID 格式无效'),
  name: z.string().trim().min(1).max(80),
  /** When false, hidden from action model pickers and the result model switcher. */
  enabled: z.boolean().default(true),
  baseUrl: z
    .string()
    .trim()
    .refine((value) => validateOpenAiBaseUrl(value).valid, 'API 地址无效'),
  models: z.array(providerModelSchema).max(MAX_PROVIDER_MODELS)
} as const

function validateUniqueModels(
  value: { models: ProviderModel[] },
  context: z.RefinementCtx
): void {
  const ids = new Set<string>()
  for (const model of value.models) {
    if (ids.has(model.id)) {
      context.addIssue({ code: z.ZodIssueCode.custom, message: `模型 ID 重复：${model.id}` })
    }
    ids.add(model.id)
  }
}

export const providerMetadataSchema = z
  .object(providerMetadataShape)
  .strict()
  .superRefine(validateUniqueModels)
export type ProviderMetadata = z.infer<typeof providerMetadataSchema>

export const providerSettingsSchema = z
  .object({
    ...providerMetadataShape,
    /** Internal-only plaintext value. Repositories must keep it outside ordinary settings. */
    apiKey: z.string().max(16_384)
  })
  .strict()
  .superRefine(validateUniqueModels)
export type ProviderSettings = z.infer<typeof providerSettingsSchema>

export const publicProviderSettingsSchema = z
  .object({
    ...providerMetadataShape,
    keyConfigured: z.boolean()
  })
  .strict()
  .superRefine(validateUniqueModels)
export type PublicProviderSettings = z.infer<typeof publicProviderSettingsSchema>

function providersArraySchema<T extends z.ZodTypeAny>(provider: T) {
  return z.array(provider).max(MAX_PROVIDERS).superRefine((providers, context) => {
    const ids = new Set<string>()
    for (const candidate of providers as Array<{ id: string }>) {
      if (ids.has(candidate.id)) {
        context.addIssue({ code: z.ZodIssueCode.custom, message: `服务商 ID 重复：${candidate.id}` })
      }
      ids.add(candidate.id)
    }
  })
}

export const toolbarDisplayModeSchema = z.enum(['icon-label', 'icon-only'])
export const toolbarSettingsSchema = z
  .object({ displayMode: toolbarDisplayModeSchema })
  .strict()
export type ToolbarSettings = z.infer<typeof toolbarSettingsSchema>

export const resultDismissModeSchema = z.enum(['manual', 'blur', 'pointer-leave'])
export const resultSettingsSchema = z
  .object({
    followCursor: z.boolean(),
    rememberSize: z.boolean(),
    defaultPinned: z.boolean(),
    dismissMode: resultDismissModeSchema.default('blur'),
    dismissDelayMs: z.number().int().min(100).max(5_000),
    opacity: z.number().finite().min(0.2).max(1),
    fontSize: z
      .number()
      .int()
      .min(RESULT_FONT_SIZE_MIN)
      .max(RESULT_FONT_SIZE_MAX)
      .default(DEFAULT_RESULT_FONT_SIZE),
    lastSize: windowSizeSchema.nullable().default(null)
  })
  .strict()
export type ResultSettings = z.infer<typeof resultSettingsSchema>

export const triggerSettingsSchema = z
  .object({ mode: z.enum(['selected', 'shortcut']) })
  .strict()
export type TriggerSettings = z.infer<typeof triggerSettingsSchema>

export const applicationCloseBehaviorSchema = z.enum(['hide-to-tray', 'quit'])
export type ApplicationCloseBehavior = z.infer<typeof applicationCloseBehaviorSchema>

export const applicationSettingsSchema = z
  .object({ closeBehavior: applicationCloseBehaviorSchema })
  .strict()
export type ApplicationSettings = z.infer<typeof applicationSettingsSchema>

export const filterSettingsSchema = z
  .object({
    mode: z.enum(['default', 'whitelist', 'blacklist']),
    applications: z.array(z.string().trim().min(1).max(512)).max(200)
  })
  .strict()
export type FilterSettings = z.infer<typeof filterSettingsSchema>

export const selectionCaptureStrategySchema = z.enum(['selection-hook', 'clipboard', 'auto'])
export type SelectionCaptureStrategy = z.infer<typeof selectionCaptureStrategySchema>

const DEFAULT_SELECTION_CAPTURE_RULES = [
  { application: 'acrobat.exe', strategy: 'clipboard' as const },
  { application: 'acrord32.exe', strategy: 'clipboard' as const },
  { application: 'acrocef.exe', strategy: 'clipboard' as const },
  { application: 'rdrcef.exe', strategy: 'clipboard' as const },
  { application: 'docbox.exe', strategy: 'clipboard' as const },
  { application: 'docboxrenderer.exe', strategy: 'clipboard' as const },
  { application: 'emeditor.exe', strategy: 'clipboard' as const }
]

export const selectionCaptureRuleSchema = z
  .object({
    application: z.string().trim().min(1).max(512),
    strategy: selectionCaptureStrategySchema
  })
  .strict()

export type SelectionCaptureRule = z.infer<typeof selectionCaptureRuleSchema>

export const selectionCaptureSettingsSchema = z
  .object({
    defaultStrategy: selectionCaptureStrategySchema.default('selection-hook'),
    applications: z.array(selectionCaptureRuleSchema).max(64).default(DEFAULT_SELECTION_CAPTURE_RULES)
  })
  .strict()
  .superRefine((value, context) => {
    const seen = new Set<string>()
    for (const rule of value.applications) {
      const application = rule.application.trim().replaceAll('/', '\\').toLowerCase()
      if (seen.has(application)) {
        context.addIssue({ code: z.ZodIssueCode.custom, message: '划词获取应用规则不能重复' })
      }
      seen.add(application)
    }
  })
export type SelectionCaptureSettings = z.infer<typeof selectionCaptureSettingsSchema>

const commonSettingsShape = {
  version: z.literal(SETTINGS_VERSION),
  enabled: z.boolean(),
  captureShortcut: captureShortcutSchema,
  locale: supportedLocaleSchema,
  translate: translationSettingsSchema,
  toolbar: toolbarSettingsSchema,
  result: resultSettingsSchema,
  trigger: triggerSettingsSchema,
  application: applicationSettingsSchema,
  filter: filterSettingsSchema,
  selectionCapture: selectionCaptureSettingsSchema.default({
    defaultStrategy: 'selection-hook',
    applications: DEFAULT_SELECTION_CAPTURE_RULES
  }),
  actions: actionsSchema
} as const

function validateSettingsReferences(
  value: {
    providers: Array<{ id: string; models: ProviderModel[] }>
    actions: ActionDefinition[]
  },
  context: z.RefinementCtx
): void {
  const providers = new Map(value.providers.map((provider) => [provider.id, provider]))
  for (const action of value.actions) {
    if (!isAiActionDefinition(action) || !action.providerId) continue
    const provider = providers.get(action.providerId)
    if (!provider) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        message: `动作“${action.name}”引用了不存在的服务商`
      })
      continue
    }
    if (action.modelId && !provider.models.some((model) => model.id === action.modelId)) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        message: `动作“${action.name}”引用了不存在的模型`
      })
    }
  }
}

export const appSettingsSchema = z
  .object({
    ...commonSettingsShape,
    providers: providersArraySchema(providerSettingsSchema)
  })
  .strict()
  .superRefine(validateSettingsReferences)
export type AppSettings = z.infer<typeof appSettingsSchema>

export const publicSettingsSchema = z
  .object({
    ...commonSettingsShape,
    providers: providersArraySchema(publicProviderSettingsSchema)
  })
  .strict()
  .superRefine(validateSettingsReferences)
export type PublicSettings = z.infer<typeof publicSettingsSchema>

export const settingsUpdateSchema = z
  .object({
    enabled: z.boolean().optional(),
    captureShortcut: captureShortcutSchema.optional(),
    locale: supportedLocaleSchema.optional(),
    translate: translationSettingsSchema.optional(),
    toolbar: toolbarSettingsSchema.optional(),
    result: resultSettingsSchema.optional(),
    trigger: triggerSettingsSchema.optional(),
  application: applicationSettingsSchema.optional(),
  filter: filterSettingsSchema.optional(),
  selectionCapture: selectionCaptureSettingsSchema.optional(),
    providers: providersArraySchema(providerMetadataSchema).optional(),
    actions: actionsSchema.optional()
  })
  .strict()
export type SettingsUpdate = z.infer<typeof settingsUpdateSchema>

export const providerCreateInputSchema = z
  .object({ name: z.string().trim().min(1).max(80), baseUrl: providerMetadataShape.baseUrl })
  .strict()
export type ProviderCreateInput = z.infer<typeof providerCreateInputSchema>

export const providerUpdateInputSchema = z
  .object({
    name: z.string().trim().min(1).max(80).optional(),
    baseUrl: providerMetadataShape.baseUrl.optional(),
    models: z.array(providerModelSchema).max(MAX_PROVIDER_MODELS).optional()
  })
  .strict()
export type ProviderUpdateInput = z.infer<typeof providerUpdateInputSchema>

export const resultStatusSchema = z.enum(['idle', 'streaming', 'completed', 'cancelled', 'error'])

const wireCounterSchema = z.number().int().nonnegative().finite().max(Number.MAX_SAFE_INTEGER)
const eventSequenceSchema = wireCounterSchema.min(1)
const outputStringSchema = z.string().refine(
  (value) => hasAtMostUnicodeScalars(value, AI_OUTPUT_LIMIT),
  `输出最多包含 ${AI_OUTPUT_LIMIT} 个 Unicode scalar`
)

export const actionNoticeSchema = z
  .object({
    code: z.string().trim().min(1).max(128),
    message: z.string().trim().min(1).max(2_000)
  })
  .strict()
export type ActionNotice = z.infer<typeof actionNoticeSchema>

const actionEventBaseShape = {
  sessionId: z.string().trim().min(1).max(128),
  sessionGeneration: wireCounterSchema,
  requestId: z.string().trim().min(1).max(128),
  requestGeneration: wireCounterSchema,
  sequence: eventSequenceSchema,
  actionId: actionIdSchema
} as const

export const actionStreamEventSchema = z.discriminatedUnion('type', [
  z.object({ ...actionEventBaseShape, type: z.literal('started') }).strict(),
  z.object({
    ...actionEventBaseShape,
    type: z.literal('delta'),
    delta: outputStringSchema
  }).strict(),
  z.object({
    ...actionEventBaseShape,
    type: z.literal('thinkingDelta'),
    delta: outputStringSchema
  }).strict(),
  z.object({
    ...actionEventBaseShape,
    type: z.literal('completed'),
    lastContentSequence: wireCounterSchema,
    contentScalarCount: wireCounterSchema.max(AI_OUTPUT_LIMIT)
  }).strict(),
  z.object({ ...actionEventBaseShape, type: z.literal('cancelled') }).strict(),
  z.object({
    ...actionEventBaseShape,
    type: z.literal('notice'),
    code: actionNoticeSchema.shape.code,
    message: actionNoticeSchema.shape.message
  }).strict(),
  z.object({
    ...actionEventBaseShape,
    type: z.literal('error'),
    code: z.string().trim().min(1).max(128),
    message: z.string().trim().min(1).max(2_000),
    retryable: z.boolean()
  }).strict(),
  z.object({
    ...actionEventBaseShape,
    type: z.literal('resyncRequired'),
    snapshotLastSequence: wireCounterSchema
  }).strict()
])
export type ActionStreamEvent = z.infer<typeof actionStreamEventSchema>

export const resultSessionSnapshotSchema = z
  .object({
    sessionId: z.string().trim().min(1).max(128),
    sessionGeneration: wireCounterSchema,
    requestId: z.string().trim().min(1).max(128),
    requestGeneration: wireCounterSchema,
    actionId: actionIdSchema,
    providerId: providerMetadataShape.id.optional(),
    modelId: z.string().trim().min(1).max(256).optional(),
    selection: selectionPayloadSchema,
    status: resultStatusSchema,
    content: outputStringSchema,
    /** Live CoT for the current request; not part of answer integrity counters. */
    thinkingContent: outputStringSchema.default(''),
    lastSequence: wireCounterSchema,
    lastContentSequence: wireCounterSchema,
    contentScalarCount: wireCounterSchema.max(AI_OUTPUT_LIMIT),
    handshakeGeneration: wireCounterSchema,
    generationNotice: actionNoticeSchema.optional(),
    errorMessage: z.string().max(2_000),
    retryable: z.boolean(),
    pinned: z.boolean()
  })
  .strict()
  .superRefine((snapshot, context) => {
    const hasProvider = snapshot.providerId !== undefined
    const hasModel = snapshot.modelId !== undefined
    if (hasProvider !== hasModel) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        path: ['providerId'],
        message: 'providerId and modelId must be provided together'
      })
    }
    if (countUnicodeScalars(snapshot.content) !== snapshot.contentScalarCount) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        path: ['contentScalarCount'],
        message: 'contentScalarCount must equal the Unicode scalar count of content'
      })
    }
  })
export type ResultSessionSnapshot = z.infer<typeof resultSessionSnapshotSchema>

export const resultReadyAckSchema = z
  .object({
    sessionId: z.string().trim().min(1).max(128),
    sessionGeneration: wireCounterSchema,
    requestGeneration: wireCounterSchema,
    lastSequence: wireCounterSchema,
    handshakeGeneration: wireCounterSchema
  })
  .strict()
export type ResultReadyAck = z.infer<typeof resultReadyAckSchema>

export const toolbarSizeSchema = z
  .object({
    width: z.number().finite().positive().max(4_096),
    height: z.number().finite().positive().max(4_096)
  })
  .strict()
export type ToolbarSize = z.infer<typeof toolbarSizeSchema>

/**
 * Pointer coordinates reported by the native toolbar tracker.
 *
 * `x` and `y` are CSS-pixel client coordinates relative to the toolbar
 * WebView. Native tracking is needed because a non-activating macOS toolbar
 * does not reliably forward ordinary mouse-move events to WKWebView.
 */
export const toolbarPointerEventSchema = z
  .object({
    x: z.number().finite(),
    y: z.number().finite(),
    inside: z.boolean()
  })
  .strict()
export type ToolbarPointerEvent = z.infer<typeof toolbarPointerEventSchema>

/**
 * Native toolbar dismiss notification.
 *
 * Emitted after the runtime force-hides the selection toolbar (outside click,
 * Escape, etc.). The renderer clears selection / copy-success UI so local
 * state cannot outlive the native window.
 */
export const toolbarDismissedEventSchema = z
  .object({
    selectionId: z.string().min(1).optional(),
    reason: z.string()
  })
  .strict()
export type ToolbarDismissedEvent = z.infer<typeof toolbarDismissedEventSchema>

export const providerIdSchema = providerMetadataShape.id
export const apiKeyInputSchema = z.string().trim().min(1).max(16_384)
export const copyTextInputSchema = z.string().max(1_000_000)
export const externalUrlInputSchema = z
  .string()
  .trim()
  .max(2_048)
  .refine(isSafeExternalUrl, '外部链接必须是安全的 HTTP(S) 地址')

/** @deprecated Kept for legacy callers while provider-aware code migrates. */
export const publicAiSettingsSchema = z
  .object({
    baseUrl: providerMetadataShape.baseUrl,
    model: z.string().trim().max(256),
    maxTextLength: z.number().int().positive().max(AI_TEXT_LIMIT)
  })
  .strict()
/** @deprecated Kept for legacy callers while provider-aware code migrates. */
export const aiSettingsSchema = publicAiSettingsSchema.extend({ apiKey: z.string().max(16_384) }).strict()
export type PublicAiSettings = z.infer<typeof publicAiSettingsSchema>
export type AiSettings = z.infer<typeof aiSettingsSchema>
