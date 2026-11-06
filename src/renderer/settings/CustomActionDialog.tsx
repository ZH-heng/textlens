import { useMemo, useRef, useState, type FormEvent, type JSX } from 'react'
import { Braces, RotateCcw, X } from 'lucide-react'

import {
  DEFAULT_ACTION_PROMPTS,
  DEFAULT_SEARCH_ENGINE_ID,
  DEFAULT_SEARCH_ENGINES,
  TEXT_PLACEHOLDER,
  THINKING_LEVEL_LABELS,
  aiActionKindSchema,
  clampThinkingMode,
  defaultThinkingLevel,
  effectiveThinkingLevels,
  modelSupportsThinkingOff,
  protocolSupportsThinkingOff,
  type ActionDefinition,
  type ActionKind,
  type ProviderProtocol,
  type PublicProviderSettings,
  type SearchEngineId,
  type ThinkingLevel,
  type ThinkingMode
} from '../../shared'
import { isLucideIconName } from '../components/lucideIconRegistry'
import { ActionIconPicker } from './ActionIconPicker'
import { Dialog } from '../components/Dialog'

const ACTION_KIND_NAMES: Readonly<Record<ActionKind, string>> = {
  copy: '复制',
  search: '搜索 / 打开网址',
  translate: '翻译',
  summary: '总结',
  explain: '解释',
  refine: '润色',
  ask: '问AI',
  custom: '自定义 AI'
}

function isAiKind(kind: ActionKind): boolean {
  return aiActionKindSchema.safeParse(kind).success
}

function resolveThinkingMode(choice: ModelChoice | null, mode: ThinkingMode): ThinkingMode {
  if (!choice || !choice.thinkingEnabled) return 'off'
  const clamped = clampThinkingMode(mode, choice.thinkingLevels)
  if (clamped !== 'off' || choice.supportsOff) return clamped
  // The model cannot disable thinking; fall back to its default gear instead
  // of leaving the request on an unavailable option.
  return choice.thinkingDefault ?? choice.thinkingLevels[0] ?? 'off'
}

function defaultPrompt(kind: ActionKind): string {
  if (
    kind === 'translate' ||
    kind === 'summary' ||
    kind === 'explain' ||
    kind === 'refine' ||
    kind === 'ask'
  ) {
    return DEFAULT_ACTION_PROMPTS[kind]
  }
  return DEFAULT_ACTION_PROMPTS.custom
}

export function promptAfterKindChange(
  currentKind: ActionKind,
  nextKind: ActionKind,
  prompt: string
): string {
  if (!isAiKind(nextKind)) return prompt

  const cleanPrompt = prompt.trim()
  const currentDefault = defaultPrompt(currentKind).trim()
  return !cleanPrompt || cleanPrompt === currentDefault ? defaultPrompt(nextKind) : prompt
}

export interface ActionEditorValue {
  name: string
  icon: string
  kind: ActionKind
  prompt?: string
  providerId?: string
  modelId?: string
  thinkingMode?: ThinkingMode
  searchEngineId?: SearchEngineId
}

interface ModelRoute {
  providerId: string
  modelId: string
}

function modelRouteValue(route: ModelRoute): string {
  return JSON.stringify([route.providerId, route.modelId])
}

function parseModelRoute(value: string): ModelRoute | null {
  if (!value) return null
  try {
    const parsed = JSON.parse(value) as unknown
    if (
      Array.isArray(parsed) &&
      parsed.length === 2 &&
      typeof parsed[0] === 'string' &&
      typeof parsed[1] === 'string'
    ) {
      return { providerId: parsed[0], modelId: parsed[1] }
    }
  } catch {
    /* ignore */
  }
  return null
}

interface ModelChoice extends ModelRoute {
  value: string
  providerName: string
  modelName: string
  protocol: ProviderProtocol
  thinkingLevels: ReturnType<typeof effectiveThinkingLevels>
  thinkingEnabled: boolean
  thinkingDefault: ThinkingLevel | null
  supportsOff: boolean
}

export function CustomActionDialog({
  action,
  providers,
  onCancel,
  onSave
}: {
  action: ActionDefinition | null
  providers: readonly PublicProviderSettings[]
  onCancel: () => void
  onSave: (value: ActionEditorValue) => void
}): JSX.Element {
  const initialAi = action && 'prompt' in action ? action : null
  const initialSearchEngineId =
    action && action.kind === 'search' && 'searchEngineId' in action
      ? action.searchEngineId
      : DEFAULT_SEARCH_ENGINE_ID
  const [name, setName] = useState(action?.name ?? '')
  const [icon, setIcon] = useState(action?.icon ?? 'sparkles')
  const [kind, setKind] = useState<ActionKind>(action?.kind ?? 'custom')
  const [providerId, setProviderId] = useState(initialAi?.providerId ?? '')
  const [modelId, setModelId] = useState(initialAi?.modelId ?? '')
  const [thinkingMode, setThinkingMode] = useState<ThinkingMode>(initialAi?.thinkingMode ?? 'off')
  const [searchEngineId, setSearchEngineId] = useState<SearchEngineId>(initialSearchEngineId)
  const [prompt, setPrompt] = useState(initialAi?.prompt ?? DEFAULT_ACTION_PROMPTS.custom)
  const [error, setError] = useState('')
  const nameRef = useRef<HTMLInputElement>(null)

  const enabledProviders = useMemo(
    () => providers.filter((provider) => provider.enabled !== false),
    [providers]
  )

  const modelGroups = useMemo(() => {
    return enabledProviders
      .filter((provider) => provider.models.length > 0)
      .map((provider) => {
        const protocol = provider.protocol ?? 'openAiChat'
        return {
          id: provider.id,
          name: provider.name,
          keyConfigured: provider.keyConfigured,
          choices: provider.models.map<ModelChoice>((model) => {
            const thinkingLevels = effectiveThinkingLevels(
              model.id,
              model.thinkingLevels,
              protocol
            )
            return {
              providerId: provider.id,
              modelId: model.id,
              providerName: provider.name,
              modelName: model.name,
              protocol,
              thinkingLevels,
              thinkingEnabled: model.thinkingEnabled !== false,
              thinkingDefault: model.thinkingDefault ?? defaultThinkingLevel(thinkingLevels),
              supportsOff:
                model.thinkingCapability?.supportsOff
                ?? protocolSupportsThinkingOff(model.id, protocol),
              value: modelRouteValue({ providerId: provider.id, modelId: model.id })
            }
          })
        }
      })
  }, [enabledProviders])

  const modelChoices = useMemo(
    () => modelGroups.flatMap((group) => group.choices),
    [modelGroups]
  )

  const selectedChoice = useMemo(() => {
    if (!providerId || !modelId) return null
    return modelChoices.find(
      (choice) => choice.providerId === providerId && choice.modelId === modelId
    ) ?? null
  }, [modelChoices, modelId, providerId])

  const modelSelectValue = providerId && modelId
    ? modelRouteValue({ providerId, modelId })
    : ''

  const thinkingLevels = selectedChoice?.thinkingEnabled ? selectedChoice.thinkingLevels : []
  const showThinkingOff = modelSupportsThinkingOff(
    thinkingLevels,
    selectedChoice?.supportsOff ?? true
  )
  const showThinkingAuto = Boolean(
    selectedChoice?.thinkingEnabled && selectedChoice.thinkingDefault && thinkingLevels.length > 0
  )

  const submit = (event: FormEvent): void => {
    event.preventDefault()
    const cleanName = name.trim()
    const cleanPrompt = prompt.trim()
    if (!cleanName) return setError('请输入动作名称')
    if (cleanName.length > 40) return setError('动作名称不能超过 40 个字符')
    if (!isLucideIconName(icon)) return setError('请选择有效的 Lucide 图标')

    if (isAiKind(kind)) {
      if (!cleanPrompt.includes(TEXT_PLACEHOLDER)) {
        return setError(`提示词必须包含 ${TEXT_PLACEHOLDER}`)
      }
      if (cleanPrompt.length > 10_000) return setError('提示词不能超过 10,000 个字符')
      onSave({
        name: cleanName,
        icon,
        kind,
        prompt: cleanPrompt,
        providerId,
        modelId,
        thinkingMode: resolveThinkingMode(selectedChoice, thinkingMode)
      })
      return
    }
    if (kind === 'search') {
      onSave({ name: cleanName, icon, kind, searchEngineId })
      return
    }
    onSave({ name: cleanName, icon, kind })
  }

  const insertPlaceholder = (): void => {
    const textarea = document.getElementById('action-prompt') as HTMLTextAreaElement | null
    if (!textarea) return setPrompt((value) => `${value}${TEXT_PLACEHOLDER}`)
    const start = textarea.selectionStart
    const end = textarea.selectionEnd
    setPrompt((value) => `${value.slice(0, start)}${TEXT_PLACEHOLDER}${value.slice(end)}`)
    requestAnimationFrame(() => {
      const position = start + TEXT_PLACEHOLDER.length
      textarea.focus()
      textarea.setSelectionRange(position, position)
    })
  }

  const changeKind = (next: ActionKind): void => {
    setPrompt((current) => promptAfterKindChange(kind, next, current))
    setKind(next)
    setError('')
  }

  const changeModelRoute = (value: string): void => {
    const route = parseModelRoute(value)
    if (!route) {
      setProviderId('')
      setModelId('')
      setThinkingMode('off')
      return
    }
    const choice = modelChoices.find(
      (item) => item.providerId === route.providerId && item.modelId === route.modelId
    )
    setProviderId(route.providerId)
    setModelId(route.modelId)
    setThinkingMode((current) => resolveThinkingMode(choice ?? null, current))
  }

  return (
    <Dialog className="dialog-card--action" labelledBy="action-dialog-title" initialFocusRef={nameRef} onClose={onCancel}>
        <header className="dialog-header">
          <div>
            <h2 id="action-dialog-title">{action ? '编辑动作' : '添加动作'}</h2>
            <p>
              {kind === 'search'
                ? '搜索动作可选择默认搜索引擎；选中网址时仍会直接打开。'
                : isAiKind(kind)
                  ? '同一种动作可以添加多次，并分别绑定服务商、模型和提示词。'
                  : '可调整名称、类型与图标；本地动作无需配置模型。'}
            </p>
          </div>
          <button className="icon-button" type="button" aria-label="关闭" onClick={onCancel}>
            <X size={18} aria-hidden="true" />
          </button>
        </header>
        <form className="dialog-form" onSubmit={submit}>
          <div className={`dialog-form-grid${action ? ' dialog-form-grid--single' : ''}`}>
            <label className="field">
              <span className="field__label">动作名称</span>
              <input
                ref={nameRef}
                className="control"
                value={name}
                maxLength={40}
                placeholder="例如：翻译成日语"
                onChange={(event) => {
                  setName(event.target.value)
                  setError('')
                }}
              />
            </label>
            {!action && (
              <label className="field">
                <span className="field__label">动作类型</span>
                <select
                  className="control"
                  value={kind}
                  onChange={(event) => changeKind(event.target.value as ActionKind)}
                >
                  {Object.entries(ACTION_KIND_NAMES).map(([value, label]) => (
                    <option value={value} key={value}>{label}</option>
                  ))}
                </select>
              </label>
            )}
          </div>

          <label className="field">
            <span className="field__label">图标</span>
            <ActionIconPicker value={icon} onChange={(value) => {
              setIcon(value)
              setError('')
            }} />
          </label>

          {kind === 'search' && (
            <label className="field">
              <span className="field__label">默认搜索引擎</span>
              <select
                className="control"
                value={searchEngineId}
                onChange={(event) => {
                  setSearchEngineId(event.target.value as SearchEngineId)
                  setError('')
                }}
              >
                {DEFAULT_SEARCH_ENGINES.map((engine) => (
                  <option value={engine.id} key={engine.id}>{engine.name}</option>
                ))}
              </select>
              <span className="field__hint">划词搜索普通文字时使用该引擎；选中网址、域名或 IP 时直接打开。</span>
            </label>
          )}

          {isAiKind(kind) && (
            <>
              <label className="field">
                <span className="field__label">模型</span>
                <select
                  className="control"
                  aria-label="模型"
                  value={modelSelectValue}
                  onChange={(event) => changeModelRoute(event.target.value)}
                >
                  <option value="">尚未选择</option>
                  {modelGroups.map((group) => (
                    <optgroup
                      key={group.id}
                      label={group.keyConfigured ? group.name : `${group.name}（未配置密钥）`}
                    >
                      {group.choices.map((choice) => (
                        <option key={choice.value} value={choice.value}>
                          {choice.modelName}
                        </option>
                      ))}
                    </optgroup>
                  ))}
                </select>
                <span className="field__hint">
                  已启用的服务商与其模型列在同一列表中；可在「服务商」中关闭不需要的服务商。
                </span>
              </label>

              {thinkingLevels.length > 0 && (
                <label className="field">
                  <span className="field__label">思考强度</span>
                  <select
                    className="control"
                    aria-label="思考强度"
                    value={resolveThinkingMode(selectedChoice, thinkingMode)}
                    onChange={(event) => setThinkingMode(event.target.value as ThinkingMode)}
                  >
                    {showThinkingAuto && (
                      <option value="auto">
                        跟随模型默认（{THINKING_LEVEL_LABELS[selectedChoice?.thinkingDefault ?? 'medium']}）
                      </option>
                    )}
                    {showThinkingOff && (
                      <option value="off">关闭思考（更快首字）</option>
                    )}
                    {thinkingLevels.map((level) => (
                      <option key={level} value={level}>
                        {THINKING_LEVEL_LABELS[level]}
                      </option>
                    ))}
                  </select>
                  <span className="field__hint">
                    档位来自服务商的模型列表，也可在「服务商」里为单个模型手动调整或关闭思考。
                  </span>
                </label>
              )}

              <label className="field">
                <span className="field__label dialog-prompt-label">
                  提示词
                  <span className="dialog-prompt-tools">
                    <button
                      className="prompt-reset-button"
                      type="button"
                      aria-label="恢复默认提示词"
                      disabled={prompt.trim() === defaultPrompt(kind).trim()}
                      onClick={() => {
                        setPrompt(defaultPrompt(kind))
                        setError('')
                      }}
                    >
                      <RotateCcw size={13} aria-hidden="true" />
                      恢复默认提示词
                    </button>
                    <button className="placeholder-button" type="button" onClick={insertPlaceholder}>
                      <Braces size={13} aria-hidden="true" />
                      插入 {TEXT_PLACEHOLDER}
                    </button>
                  </span>
                </span>
                <textarea
                  id="action-prompt"
                  className="control dialog-prompt"
                  aria-label="提示词"
                  value={prompt}
                  maxLength={10_000}
                  spellCheck={false}
                  onChange={(event) => {
                    setPrompt(event.target.value)
                    setError('')
                  }}
                />
                <span className="field__hint">
                  默认提示词可以直接修改；恢复默认后需保存设置才会生效；{TEXT_PLACEHOLDER} 会替换为选中文本。
                </span>
              </label>
            </>
          )}

          {error && <div className="notice notice--error" role="alert">{error}</div>}
          <footer className="dialog-actions">
            <button className="button" type="button" onClick={onCancel}>取消</button>
            <button className="button button--primary" type="submit">
              {action ? '保存修改' : '添加动作'}
            </button>
          </footer>
        </form>
    </Dialog>
  )
}
