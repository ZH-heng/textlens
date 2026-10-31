export const APP_NAME = 'TextLens'
export const APP_ID = 'com.local.textlens'

export const SETTINGS_VERSION = 13 as const
export const AI_TEXT_LIMIT = 20_000
export const AI_PROMPT_LIMIT = 50_000
export const AI_OUTPUT_LIMIT = 1_000_000
export const MAX_ENABLED_ACTIONS = 8
export const MAX_ACTIONS = 50
export const MAX_CUSTOM_ACTIONS = 10
export const MAX_PROVIDERS = 20
export const MAX_PROVIDER_MODELS = 200
export const RESULT_FONT_SIZE_MIN = 12
export const RESULT_FONT_SIZE_MAX = 24
export const DEFAULT_RESULT_FONT_SIZE = 14

export const TEXT_PLACEHOLDER = '{{text}}'
export const OUTPUT_LANGUAGE_PLACEHOLDER = '{{language}}'
export const TARGET_LANGUAGE_PLACEHOLDER = '{{target_language}}'
export const DEFAULT_SEARCH_TEMPLATE = `https://www.google.com/search?q=${TEXT_PLACEHOLDER}`
export const BING_CHINA_SEARCH_TEMPLATE = `https://cn.bing.com/search?q=${TEXT_PLACEHOLDER}`
export const BAIDU_SEARCH_TEMPLATE = `https://www.baidu.com/s?wd=${TEXT_PLACEHOLDER}`
export const BUILTIN_SEARCH_ENGINE_IDS = ['google', 'bing-china', 'baidu'] as const
export type BuiltinSearchEngineId = (typeof BUILTIN_SEARCH_ENGINE_IDS)[number]
export const DEFAULT_SEARCH_ENGINE_ID = 'google' as const
/** @deprecated Use DEFAULT_SEARCH_ENGINE_ID. */
export const DEFAULT_ACTIVE_SEARCH_ENGINE_ID = DEFAULT_SEARCH_ENGINE_ID
export const DEFAULT_SEARCH_ENGINES = [
  {
    id: 'google',
    name: 'Google',
    template: DEFAULT_SEARCH_TEMPLATE,
    builtin: true
  },
  {
    id: 'bing-china',
    name: 'Bing',
    template: BING_CHINA_SEARCH_TEMPLATE,
    builtin: true
  },
  {
    id: 'baidu',
    name: '百度',
    template: BAIDU_SEARCH_TEMPLATE,
    builtin: true
  }
] as const
export const DEFAULT_OPENAI_BASE_URL = 'https://api.openai.com/v1'
export const DEFAULT_ANTHROPIC_BASE_URL = 'https://api.anthropic.com/v1'
export const DEFAULT_GEMINI_BASE_URL = 'https://generativelanguage.googleapis.com/v1beta'

export const PROVIDER_PROTOCOL_IDS = [
  'openAiChat',
  'openAiResponses',
  'anthropicMessages',
  'gemini'
] as const
export type ProviderProtocol = (typeof PROVIDER_PROTOCOL_IDS)[number]

export const PROVIDER_PROTOCOLS = [
  {
    id: 'openAiChat',
    name: 'OpenAI Chat Completions',
    defaultBaseUrl: DEFAULT_OPENAI_BASE_URL,
    basePath: '/chat/completions',
    hint: '兼容 OpenAI Chat Completions 的接口；地址填到版本前缀，如 …/v1。'
  },
  {
    id: 'openAiResponses',
    name: 'OpenAI Responses',
    defaultBaseUrl: DEFAULT_OPENAI_BASE_URL,
    basePath: '/responses',
    hint: 'OpenAI Responses 接口；请求会设置 store:false，不在服务端留存。'
  },
  {
    id: 'anthropicMessages',
    name: 'Anthropic Messages',
    defaultBaseUrl: DEFAULT_ANTHROPIC_BASE_URL,
    basePath: '/messages',
    hint: 'Anthropic Messages 接口；使用 x-api-key 与 anthropic-version 请求头。'
  },
  {
    id: 'gemini',
    name: 'Google Gemini',
    defaultBaseUrl: DEFAULT_GEMINI_BASE_URL,
    basePath: '/models/{model}:streamGenerateContent',
    hint: 'Gemini 原生接口；地址填到版本前缀，如 …/v1beta。'
  }
] as const

export function providerProtocolOption(protocol: ProviderProtocol): (typeof PROVIDER_PROTOCOLS)[number] {
  return PROVIDER_PROTOCOLS.find((option) => option.id === protocol) ?? PROVIDER_PROTOCOLS[0]
}
export const DEFAULT_PROVIDER_ID = 'openai-compatible'
export const DEFAULT_PROVIDER_NAME = 'OpenAI Compatible'
/** @deprecated Models now live under providers. */
export const DEFAULT_MODEL = ''
export const DEFAULT_CAPTURE_SHORTCUT = ''

export const DEFAULT_LOCALE = 'zh-CN' as const
export const DEFAULT_TRANSLATION_SETTINGS = {
  defaultLanguage: 'en-US',
  targetLanguage: 'zh-CN'
} as const

export const DEFAULT_TOOLBAR_SETTINGS = {
  displayMode: 'icon-label'
} as const

export const DEFAULT_APPEARANCE_SETTINGS = {
  theme: 'warm'
} as const

/** Selectable appearance themes, in settings-UI order. */
export const THEME_OPTIONS = [
  { id: 'warm', name: '暖白' },
  { id: 'classic', name: '经典' },
  { id: 'sand', name: '米杏' },
  { id: 'sage', name: '苔绿' },
  { id: 'ocean', name: '海雾' }
] as const

export const DEFAULT_RESULT_SETTINGS = {
  followCursor: true,
  rememberSize: true,
  defaultPinned: false,
  dismissMode: 'blur',
  dismissDelayMs: 450,
  opacity: 1,
  fontSize: DEFAULT_RESULT_FONT_SIZE,
  lastSize: null
} as const

export const DEFAULT_TRIGGER_SETTINGS = {
  mode: 'selected'
} as const

export const DEFAULT_APPLICATION_SETTINGS = {
  closeBehavior: 'hide-to-tray'
} as const

export const DEFAULT_FILTER_SETTINGS = {
  mode: 'default',
  applications: []
} as const

export const DEFAULT_SELECTION_CAPTURE_SETTINGS = {
  defaultStrategy: 'selection-hook',
  applications: [
    { application: 'acrobat.exe', strategy: 'clipboard' },
    { application: 'acrord32.exe', strategy: 'clipboard' },
    { application: 'acrocef.exe', strategy: 'clipboard' },
    { application: 'rdrcef.exe', strategy: 'clipboard' },
    { application: 'docbox.exe', strategy: 'clipboard' },
    { application: 'docboxrenderer.exe', strategy: 'clipboard' },
    { application: 'emeditor.exe', strategy: 'clipboard' }
  ]
} as const

export const TOOLBAR_SCREEN_GAP = 8
export const TOOLBAR_SCREEN_MARGIN = 8

export function templateForSearchEngineId(id: string): string {
  if (id === 'bing-china') return BING_CHINA_SEARCH_TEMPLATE
  if (id === 'baidu') return BAIDU_SEARCH_TEMPLATE
  return DEFAULT_SEARCH_TEMPLATE
}

export function searchEngineDisplayName(id: string): string {
  if (id === 'bing-china') return 'Bing'
  if (id === 'baidu') return '百度'
  if (id === 'google') return 'Google'
  return id
}
