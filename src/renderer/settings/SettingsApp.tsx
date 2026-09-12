import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type JSX
} from 'react'
import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useDroppable,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragStartEvent
} from '@dnd-kit/core'
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy
} from '@dnd-kit/sortable'
import { CSS } from '@dnd-kit/utilities'
import {
  Check,
  CircleAlert,
  Eye,
  EyeOff,
  GripVertical,
  KeyRound,
  LoaderCircle,
  LockKeyhole,
  Pencil,
  Plus,
  RefreshCw,
  ShieldCheck,
  Sparkles,
  Trash2,
  X
} from 'lucide-react'

import {
  APP_NAME,
  DEFAULT_OPENAI_BASE_URL,
  DEFAULT_SEARCH_ENGINE_ID,
  MAX_ENABLED_ACTIONS,
  RESULT_FONT_SIZE_MAX,
  RESULT_FONT_SIZE_MIN,
  TRANSLATION_LANGUAGES,
  TRANSLATION_LANGUAGE_NAMES,
  actionDefinitionSchema,
  isAiActionDefinition,
  searchEngineDisplayName,
  type AccessibilityStatus,
  type ActionDefinition,
  type PublicProviderSettings,
  type PublicSettings,
  type ProviderModel,
  type SettingsGuidance,
  type SupportedLocale,
  type TranslationLanguage
} from '../../shared'
import { ActionIcon } from '../components/ActionIcon'
import { runDetached } from '../lib/asyncEffects'
import { getErrorMessage } from '../lib/errors'
import { CustomActionDialog, type ActionEditorValue } from './CustomActionDialog'
import { mergeProviderModelsOnPick } from './providerModels'
import {
  buildSettingsUpdate,
  moveActionToZone,
  removeProviderModelFromSettings,
  removeProviderFromSettings,
  sortAndNumberActions
} from './settingsForm'
import {
  formatKeyboardEventToTauriShortcut,
  SUGGESTED_CAPTURE_SHORTCUT
} from './shortcutCapture'
import {
  settingsGuidanceInbox,
  type SettingsGuidanceLease
} from './settingsGuidanceInbox'

type Operation = string | null
type Banner = { kind: 'success' | 'error'; text: string } | null
type ModelPickerState = {
  providerId: string
  providerName: string
  previous: ProviderModel[]
  remoteModels: ProviderModel[]
  checkedIds: Set<string>
}
type SettingsSectionId =
  | 'general'
  | 'providers'
  | 'actions'
  | 'language'
  | 'result'
  | 'capture'
  | 'filter'

const SETTINGS_SECTIONS: ReadonlyArray<{
  id: SettingsSectionId
  label: string
  blurb: string
}> = [
  {
    id: 'general',
    label: '通用',
    blurb: '开关助手、选择划词或快捷键触发，并调整工具条外观。'
  },
  {
    id: 'providers',
    label: '服务商',
    blurb: '按顺序：填写 API → 测试连接 → 获取模型 → 保存设置。'
  },
  {
    id: 'actions',
    label: '动作',
    blurb: '决定工具栏显示哪些功能，可拖拽排序；启用的会出现在划词工具栏。'
  },
  {
    id: 'language',
    label: '语言',
    blurb: '设置 AI 默认回复语言，以及翻译的源语言与目标语言。'
  },
  {
    id: 'result',
    label: '结果',
    blurb: '结果窗口出现位置、默认大小，以及点击外部时如何关闭。'
  },
  {
    id: 'capture',
    label: '划词获取',
    blurb: '为个别程序选择获取选中文本的方式。'
  },
  {
    id: 'filter',
    label: '过滤',
    blurb: '可限制只在部分应用中启用划词，或排除干扰较多的应用。'
  }
]

const SETTINGS_ACTIVE_SECTION_STORAGE_KEY = 'textlens.settings.active-section'

function restoredSettingsSection(): SettingsSectionId {
  try {
    const value = window.sessionStorage.getItem(SETTINGS_ACTIVE_SECTION_STORAGE_KEY)
    return SETTINGS_SECTIONS.some((section) => section.id === value)
      ? value as SettingsSectionId
      : 'general'
  } catch {
    return 'general'
  }
}

const ACTION_KIND_NAMES: Readonly<Record<ActionDefinition['kind'], string>> = {
  copy: '复制',
  search: '搜索',
  translate: '翻译',
  summary: '总结',
  explain: '解释',
  refine: '润色',
  ask: '问AI',
  custom: '自定义 AI'
}

const ACTION_ZONE_IDS = {
  enabled: 'action-zone-enabled',
  disabled: 'action-zone-disabled'
} as const

function actionZoneId(enabled: boolean): string {
  return enabled ? ACTION_ZONE_IDS.enabled : ACTION_ZONE_IDS.disabled
}

function createId(prefix: string): string {
  if (typeof crypto.randomUUID === 'function') return `${prefix}-${crypto.randomUUID()}`
  return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`
}


function normalizePublicSettings(settings: PublicSettings): PublicSettings {
  return { ...settings, actions: sortAndNumberActions(settings.actions) }
}

function normalizedModels(models: readonly ProviderModel[]): ProviderModel[] {
  const seen = new Set<string>()
  return models.flatMap((model) => {
    const id = model.id.trim()
    if (!id || seen.has(id)) return []
    seen.add(id)
    return [{
      id,
      name: model.name.trim() || id,
      thinkingLevels: model.thinkingLevels ?? [],
      thinkingCapability: model.thinkingCapability
    }]
  })
}

function usesPlainHttp(value: string): boolean {
  try {
    return new URL(value.trim()).protocol === 'http:'
  } catch {
    return /^http:/iu.test(value.trim())
  }
}

function accessibilityPresentation(status: AccessibilityStatus | null): {
  heading: string
  description: string
  state: string
  detail: string
  button: string
} {
  const available = status?.available ?? status?.trusted ?? false
  const monitorError = status?.diagnostics?.selectionMonitorError
  if (status?.platform === 'windows') {
    return {
      heading: 'Windows 选区访问',
      description: '优先通过 Windows UI Automation、Legacy UIA 与 MSAA 读取选区；不会改动系统剪贴板，也不需要单独授权。',
      state: available ? '选区访问可用' : '选区访问不可用',
      detail: monitorError ?? (available
        ? `${APP_NAME} 可以响应新的文本选择。`
        : '当前系统无法读取其他应用的选区，请重新启动应用或检查系统策略。'),
      button: available ? '可用' : '不可用'
    }
  }
  if (status?.platform === 'unsupported') {
    return {
      heading: '选区访问',
      description: '当前操作系统尚未提供划词捕获支持。',
      state: '当前平台不受支持',
      detail: '复制、搜索和 AI 动作需要先支持此平台的原生选区接口。',
      button: '不受支持'
    }
  }
  return {
    heading: '辅助功能权限',
    description: 'macOS 需要此权限才能识别其他应用里的选中文字。',
    state: status?.trusted
      ? (available ? '已获得权限' : '划词监听不可用')
      : '尚未获得权限',
    detail: monitorError ?? (status?.trusted
      ? `${APP_NAME} 可以响应新的文本选择。`
      : `请在“隐私与安全性 → 辅助功能”中允许 ${APP_NAME}。`),
    button: status?.trusted ? (available ? '已启用' : '不可用') : '打开系统设置'
  }
}

export function SettingsApp(): JSX.Element {
  const [draft, setDraft] = useState<PublicSettings | null>(null)
  const [keyInputs, setKeyInputs] = useState<Record<string, string>>({})
  const [keyBaselines, setKeyBaselines] = useState<Record<string, string>>({})
  const [keyVisible, setKeyVisible] = useState<Record<string, boolean>>({})
  const [newModelInputs, setNewModelInputs] = useState<Record<string, string>>({})
  const [draggingModelId, setDraggingModelId] = useState<string | null>(null)
  const [accessibility, setAccessibility] = useState<AccessibilityStatus | null>(null)
  const [operation, setOperation] = useState<Operation>(null)
  const [banner, setBanner] = useState<Banner>(null)
  const [bannerFading, setBannerFading] = useState(false)
  const [dirty, setDirty] = useState(false)
  const [editor, setEditor] = useState<ActionDefinition | 'new' | null>(null)
  const [providerPendingDelete, setProviderPendingDelete] = useState<PublicProviderSettings | null>(null)
  const [actionPendingDelete, setActionPendingDelete] = useState<ActionDefinition | null>(null)
  const [modelPicker, setModelPicker] = useState<ModelPickerState | null>(null)
  const [quitConfirmationOpen, setQuitConfirmationOpen] = useState(false)
  const [draggingId, setDraggingId] = useState<string | null>(null)
  const [activeSection, setActiveSection] = useState<SettingsSectionId>(restoredSettingsSection)
  const activeSectionRef = useRef(activeSection)
  activeSectionRef.current = activeSection
  const dirtyRef = useRef(false)
  const unsavedChangesRef = useRef(false)
  const quittingRef = useRef(false)
  // Tracks in-flight lazy key reveals so re-entry is ignored and the eye can disable.
  const keyRevealInFlightRef = useRef(new Set<string>())
  const [keyRevealInFlight, setKeyRevealInFlight] = useState<Record<string, boolean>>({})
  const keyInputsRef = useRef(keyInputs)
  keyInputsRef.current = keyInputs
  const dragSensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates })
  )

  useEffect(() => {
    dirtyRef.current = dirty
  }, [dirty])

  const pendingGuidanceLeaseRef = useRef<SettingsGuidanceLease | null>(null)
  const draftReadyRef = useRef(false)

  useEffect(() => {
    draftReadyRef.current = draft !== null
  }, [draft])

  useEffect(() => {
    // A hidden settings WebView normally stays mounted, preserving unsaved
    // form values (including API-key input). Keep only the active section as
    // a fallback for a later renderer recreation; never persist form data.
    const persistActiveSection = (): void => {
      if (document.visibilityState !== 'hidden') return
      try {
        window.sessionStorage.setItem(
          SETTINGS_ACTIVE_SECTION_STORAGE_KEY,
          activeSectionRef.current
        )
      } catch {
        // Storage may be unavailable in hardened WebView configurations.
      }
    }
    document.addEventListener('visibilitychange', persistActiveSection)
    window.addEventListener('pagehide', persistActiveSection)
    return () => {
      document.removeEventListener('visibilitychange', persistActiveSection)
      window.removeEventListener('pagehide', persistActiveSection)
    }
  }, [])

  const applySettingsGuidance = useCallback((lease: SettingsGuidanceLease): void => {
    if (!draftReadyRef.current) {
      if (!pendingGuidanceLeaseRef.current) pendingGuidanceLeaseRef.current = lease
      return
    }
    if (
      pendingGuidanceLeaseRef.current &&
      pendingGuidanceLeaseRef.current.id !== lease.id
    ) return

    pendingGuidanceLeaseRef.current = null
    const guidance: SettingsGuidance = lease.value
    const focus = guidance.focus?.trim()
    let targetId: string | null = null
    if (focus === 'providers') {
      setActiveSection('providers')
      targetId = 'providers-title'
    } else if (focus === 'actions') {
      setActiveSection('actions')
      targetId = 'actions-title'
    }
    if (targetId) {
      const scrollTargetId = targetId
      const scroll = (): void => {
        document.getElementById(scrollTargetId)?.scrollIntoView({ behavior: 'smooth', block: 'start' })
      }
      // Wait a frame so the focused section has mounted after the switch.
      window.requestAnimationFrame(() => {
        window.requestAnimationFrame(scroll)
      })
    }
    const notice = guidance.notice?.trim()
    if (notice) {
      setBanner({ kind: 'success', text: notice })
    }
    settingsGuidanceInbox.acknowledge(lease.id)
    runDetached(settingsGuidanceInbox.acquire(async () => null).then((nextLease) => {
      if (nextLease && nextLease.id !== lease.id) applySettingsGuidance(nextLease)
    }), {
      scope: 'settings',
      operation: 'apply-guidance'
    })
  }, [])

  useEffect(() => {
    let disposed = false
    const take = window.textLens.takeSettingsGuidance
    if (take) {
      runDetached(settingsGuidanceInbox.acquire(take).then((lease) => {
        if (!disposed && lease) applySettingsGuidance(lease)
      }), {
        scope: 'settings',
        operation: 'take-guidance'
      })
    }
    const unsubscribe = window.textLens.onSettingsGuidance?.((guidance) => {
      const lease = settingsGuidanceInbox.push(guidance)
      if (!disposed) applySettingsGuidance(lease)
    })
    return () => {
      disposed = true
      unsubscribe?.()
    }
  }, [applySettingsGuidance])

  useEffect(() => {
    const pendingLease = pendingGuidanceLeaseRef.current
    if (!draft || !pendingLease) return
    applySettingsGuidance(pendingLease)
  }, [draft, applySettingsGuidance])

  useEffect(() => {
    if (!banner) {
      setBannerFading(false)
      return
    }

    setBannerFading(false)
    // Shared toast lifetime: fade starts at 1.4s, remove at 2.0s (both green and red).
    const fadeTimer = window.setTimeout(() => setBannerFading(true), 1_400)
    const removeTimer = window.setTimeout(() => {
      setBanner(null)
      setBannerFading(false)
    }, 2_000)
    return () => {
      window.clearTimeout(fadeTimer)
      window.clearTimeout(removeTimer)
    }
  }, [banner])

  const keysDirty = useMemo(() => {
    const ids = new Set([...Object.keys(keyInputs), ...Object.keys(keyBaselines)])
    for (const id of ids) {
      if ((keyInputs[id] ?? '') !== (keyBaselines[id] ?? '')) return true
    }
    return false
  }, [keyInputs, keyBaselines])
  unsavedChangesRef.current = dirty || keysDirty

  const refreshRuntimeStatus = useCallback(async (): Promise<void> => {
    const status = await window.textLens.getAccessibilityStatus()
    setAccessibility(status)
  }, [])

  // Clear local key fields without fetching secrets. Saved keys stay in the
  // secret store; the form shows a placeholder until the user clicks show.
  const resetProviderKeyFields = useCallback((): void => {
    setKeyInputs({})
    setKeyBaselines({})
    setKeyVisible({})
  }, [])

  const revealProviderApiKey = useCallback(async (
    provider: PublicProviderSettings
  ): Promise<void> => {
    if (keyVisible[provider.id]) {
      // Hide only — never clear dirty user input.
      setKeyVisible((current) => ({ ...current, [provider.id]: false }))
      return
    }

    // Ignore re-entry while a reveal for this provider is already in flight.
    if (keyRevealInFlightRef.current.has(provider.id)) return

    const currentInput = keyInputs[provider.id] ?? ''
    if (
      currentInput === ''
      && provider.keyConfigured
      && window.textLens.getProviderApiKey
    ) {
      keyRevealInFlightRef.current.add(provider.id)
      setKeyRevealInFlight((current) => ({ ...current, [provider.id]: true }))
      try {
        const key = await window.textLens.getProviderApiKey(provider.id)
        if (typeof key === 'string' && key.length > 0) {
          // Only fill fields that are still empty — concurrent typing must not be clobbered.
          setKeyInputs((current) => {
            if ((current[provider.id] ?? '') !== '') return current
            return { ...current, [provider.id]: key }
          })
          setKeyBaselines((current) => {
            // Same empty-guard as keyInputs (ref mirrors latest typed value across the await).
            if ((keyInputsRef.current[provider.id] ?? '') !== '') return current
            return { ...current, [provider.id]: key }
          })
        }
      } catch {
        // Keep empty when a key cannot be revealed; placeholder still shows configured state.
      } finally {
        keyRevealInFlightRef.current.delete(provider.id)
        setKeyRevealInFlight((current) => {
          const next = { ...current }
          delete next[provider.id]
          return next
        })
      }
    }
    setKeyVisible((current) => ({ ...current, [provider.id]: true }))
  }, [keyInputs, keyVisible])

  useEffect(() => {
    let disposed = false
    void window.textLens.getSettings()
      .then((settings) => {
        if (disposed) return
        const normalized = normalizePublicSettings(settings)
        setDraft(normalized)
        // Do not preload API keys — fetch only when the user clicks show.
        resetProviderKeyFields()
      })
      .catch((error: unknown) => {
        if (!disposed) setBanner({ kind: 'error', text: getErrorMessage(error, '无法读取设置') })
      })
    void window.textLens.getAccessibilityStatus()
      .then((status) => {
        if (!disposed) setAccessibility(status)
      })
      .catch((error: unknown) => {
        if (!disposed) {
          setBanner({ kind: 'error', text: getErrorMessage(error, '无法读取选区状态') })
        }
      })

    const unsubscribe = window.textLens.onSettingsChanged((settings) => {
      if (!dirtyRef.current && !unsavedChangesRef.current) {
        const normalized = normalizePublicSettings(settings)
        setDraft(normalized)
        resetProviderKeyFields()
      }
      runDetached(refreshRuntimeStatus(), {
        scope: 'settings',
        operation: 'refresh-runtime-status'
      })
    })
    const refreshAccessibility = (): void => {
      runDetached(refreshRuntimeStatus(), {
        scope: 'settings',
        operation: 'refresh-runtime-status'
      })
    }
    window.addEventListener('focus', refreshAccessibility)
    return () => {
      disposed = true
      unsubscribe()
      window.removeEventListener('focus', refreshAccessibility)
    }
  }, [resetProviderKeyFields, refreshRuntimeStatus])

  const changeDraft = useCallback((updater: (current: PublicSettings) => PublicSettings): void => {
    setDraft((current) => (current ? updater(current) : current))
    dirtyRef.current = true
    setDirty(true)
    setBanner(null)
  }, [])

  const enabledActions = useMemo(
    () => sortAndNumberActions(draft?.actions ?? []).filter((action) => action.enabled),
    [draft?.actions]
  )
  const disabledActions = useMemo(
    () => sortAndNumberActions(draft?.actions ?? []).filter((action) => !action.enabled),
    [draft?.actions]
  )
  const persist = async (): Promise<PublicSettings | null> => {
    if (!draft) return null
    const validation = buildSettingsUpdate(draft)
    if (!validation.valid) {
      setBanner({ kind: 'error', text: validation.message })
      return null
    }

    let saved: PublicSettings
    try {
      // The full provider list is part of the ordinary settings transaction.
      // Rust removes orphaned secrets while committing that transaction, so a
      // staged deletion must never run as a separate destructive command first.
      saved = await window.textLens.updateSettings(validation.value)
    } catch (error) {
      await refreshRuntimeStatus().catch(() => undefined)
      setBanner({ kind: 'error', text: getErrorMessage(error, '保存设置失败') })
      return null
    }

    await refreshRuntimeStatus().catch(() => undefined)

    setDraft(normalizePublicSettings(saved))
    setDirty(false)
    dirtyRef.current = false

    const existingProviderIds = new Set(saved.providers.map((provider) => provider.id))
    const pendingKeyWrites = Object.entries(keyInputs).filter(([providerId, rawKey]) => {
      if (!existingProviderIds.has(providerId)) return false
      const next = rawKey.trim()
      const baseline = (keyBaselines[providerId] ?? '').trim()
      return next.length > 0 && next !== baseline
    })

    const remainingInputs = { ...keyInputs }
    for (const [providerId, rawKey] of pendingKeyWrites) {
      const apiKey = rawKey.trim()
      try {
        if (window.textLens.setProviderApiKey) {
          saved = await window.textLens.setProviderApiKey(providerId, apiKey)
        } else if (providerId === saved.providers[0]?.id && window.textLens.setApiKey) {
          saved = await window.textLens.setApiKey(apiKey)
        } else {
          throw new Error('当前后端尚不支持为多个服务商保存密钥')
        }
        remainingInputs[providerId] = apiKey
      } catch (error) {
        const refreshed = await window.textLens.getSettings().catch(() => saved)
        const normalized = normalizePublicSettings(refreshed)
        setDraft(normalized)
        setKeyInputs(remainingInputs)
        setBanner({
          kind: 'error',
          text: `普通设置已保存，但 API Key 保存失败：${getErrorMessage(error, '无法保存密钥')}`
        })
        return null
      }
    }

    const normalized = normalizePublicSettings(saved)
    setDraft(normalized)
    // Keep keyConfigured from saved settings; do not re-fetch plaintext keys.
    resetProviderKeyFields()
    return saved
  }


  const save = async (): Promise<void> => {
    setOperation('save')
    setBanner(null)
    const saved = await persist()
    if (saved) setBanner({ kind: 'success', text: '设置已保存' })
    setOperation(null)
  }

  const quitNow = useCallback(async (operationName = 'quit'): Promise<void> => {
    if (quittingRef.current) return
    quittingRef.current = true
    setOperation(operationName)
    setBanner(null)
    try {
      if (!window.textLens.quitApp) {
        throw new Error('当前后端尚不支持安全退出')
      }
      await window.textLens.quitApp()
    } catch (error) {
      setBanner({ kind: 'error', text: getErrorMessage(error, '无法退出 TextLens') })
    } finally {
      quittingRef.current = false
      setOperation(null)
    }
  }, [])

  const requestQuit = useCallback((): void => {
    if (unsavedChangesRef.current) {
      setQuitConfirmationOpen(true)
      return
    }
    void quitNow()
  }, [quitNow])

  const saveAndQuit = async (): Promise<void> => {
    setOperation('save-and-quit')
    setBanner(null)
    const saved = await persist()
    if (!saved) {
      setQuitConfirmationOpen(false)
      setOperation(null)
      return
    }
    setQuitConfirmationOpen(false)
    await quitNow('save-and-quit')
  }

  useEffect(() => {
    const unsubscribe = window.textLens.onSettingsCloseRequested?.(requestQuit)
    void window.textLens.settingsReady?.().catch((error: unknown) => {
      setBanner({ kind: 'error', text: getErrorMessage(error, '无法初始化窗口关闭处理') })
    })
    return unsubscribe ?? (() => undefined)
  }, [requestQuit])

  const resetResultSize = async (): Promise<void> => {
    setOperation('reset-result-size')
    setBanner(null)
    try {
      if (!window.textLens.resetResultSize) {
        throw new Error('当前后端尚不支持安全重置结果框尺寸')
      }
      await window.textLens.resetResultSize()
      setDraft((current) => current
        ? { ...current, result: { ...current.result, lastSize: null } }
        : current)
      setBanner({ kind: 'success', text: '已重置结果框尺寸' })
    } catch (error) {
      setBanner({ kind: 'error', text: getErrorMessage(error, '无法重置结果框尺寸') })
    } finally {
      setOperation(null)
    }
  }

  const requestAccessibility = async (): Promise<void> => {
    setOperation('accessibility')
    setBanner(null)
    try {
      const status = await window.textLens.requestAccessibility()
      setAccessibility(status)
      setBanner(
        status.trusted
          ? { kind: 'success', text: '辅助功能权限已启用' }
          : { kind: 'error', text: `请在系统设置中允许 ${APP_NAME}；授权后可能需要重新启动应用` }
      )
    } catch (error) {
      setBanner({ kind: 'error', text: getErrorMessage(error, '无法请求辅助功能权限') })
    } finally {
      setOperation(null)
    }
  }

  const addProvider = (): void => {
    const id = createId('provider')
    const provider: PublicProviderSettings = {
      id,
      name: `服务商 ${(draft?.providers.length ?? 0) + 1}`,
      enabled: true,
      baseUrl: DEFAULT_OPENAI_BASE_URL,
      keyConfigured: false,
      models: []
    }
    changeDraft((current) => ({ ...current, providers: [...current.providers, provider] }))
  }

  const changeProvider = (
    providerId: string,
    updater: (provider: PublicProviderSettings) => PublicProviderSettings
  ): void => {
    changeDraft((current) => ({
      ...current,
      providers: current.providers.map((provider) =>
        provider.id === providerId ? updater(provider) : provider
      )
    }))
  }

  const deleteProvider = (provider: PublicProviderSettings): void => {
    changeDraft((current) => removeProviderFromSettings(current, provider.id))
    setKeyInputs((current) => {
      const next = { ...current }
      delete next[provider.id]
      return next
    })
    setKeyBaselines((current) => {
      const next = { ...current }
      delete next[provider.id]
      return next
    })
    setProviderPendingDelete(null)
    setBanner({ kind: 'success', text: `已移除“${provider.name}”；保存设置后生效` })
  }

  const clearProviderKey = async (provider: PublicProviderSettings): Promise<void> => {
    setOperation(`key:${provider.id}`)
    setBanner(null)
    try {
      if (window.textLens.clearProviderApiKey) {
        await window.textLens.clearProviderApiKey(provider.id)
      } else if (draft?.providers[0]?.id === provider.id && window.textLens.clearApiKey) {
        await window.textLens.clearApiKey()
      } else {
        throw new Error('当前后端尚不支持清除该服务商密钥')
      }
      setDraft((current) => current
        ? {
            ...current,
            providers: current.providers.map((candidate) =>
              candidate.id === provider.id
                ? { ...candidate, keyConfigured: false }
                : candidate
            )
          }
        : current)
      setKeyInputs((current) => {
        const next = { ...current }
        delete next[provider.id]
        return next
      })
      setKeyBaselines((current) => {
        const next = { ...current }
        delete next[provider.id]
        return next
      })
      setKeyVisible((current) => ({ ...current, [provider.id]: false }))
      setBanner({ kind: 'success', text: `已清除“${provider.name}”的密钥` })
    } catch (error) {
      setBanner({ kind: 'error', text: getErrorMessage(error, '无法清除 API Key') })
    } finally {
      setOperation(null)
    }
  }

  const testProvider = async (provider: PublicProviderSettings): Promise<void> => {
    setOperation(`test:${provider.id}`)
    setBanner(null)
    const saved = await persist()
    if (!saved) {
      setOperation(null)
      return
    }
    try {
      const result = window.textLens.testProviderConnection
        ? await window.textLens.testProviderConnection(provider.id)
        : window.textLens.testConnection
          ? await window.textLens.testConnection()
          : { ok: false as const, message: '当前后端尚未提供连接测试接口' }
      if (result.ok) {
        const suffix = result.models.length ? `，发现 ${result.models.length} 个模型` : ''
        setBanner({ kind: 'success', text: `“${provider.name}”连接成功${suffix}` })
      } else {
        setBanner({ kind: 'error', text: result.message })
      }
    } catch (error) {
      setBanner({ kind: 'error', text: getErrorMessage(error, '连接测试失败') })
    } finally {
      setOperation(null)
    }
  }

  const openModelPicker = async (provider: PublicProviderSettings): Promise<void> => {
    setOperation(`fetch:${provider.id}`)
    setBanner(null)
    const saved = await persist()
    if (!saved) {
      setOperation(null)
      return
    }
    try {
      const result = window.textLens.listProviderModels
        ? await window.textLens.listProviderModels(provider.id)
        : window.textLens.testProviderConnection
          ? await window.textLens.testProviderConnection(provider.id)
          : window.textLens.testConnection
            ? await window.textLens.testConnection()
            : { ok: false as const, message: '当前后端尚未提供模型列表接口' }
      if (!result.ok) {
        setBanner({ kind: 'error', text: result.message })
        return
      }
      const remoteModels = normalizedModels(result.models)
      const current = draft?.providers.find((candidate) => candidate.id === provider.id)
        ?? saved.providers.find((candidate) => candidate.id === provider.id)
      if (!current) throw new Error('服务商已不存在')
      setModelPicker({
        providerId: provider.id,
        providerName: provider.name,
        previous: current.models.map((model) => ({ ...model })),
        remoteModels,
        checkedIds: new Set(current.models.map((model) => model.id))
      })
    } catch (error) {
      setBanner({ kind: 'error', text: getErrorMessage(error, '获取模型失败') })
    } finally {
      setOperation(null)
    }
  }

  const applyModelPicker = (checkedIds: ReadonlySet<string>): void => {
    if (!modelPicker) return
    const { providerId, previous, remoteModels } = modelPicker
    const nextModels = mergeProviderModelsOnPick({
      previous,
      remote: remoteModels,
      checkedIds
    })
    const removedIds = previous
      .map((model) => model.id)
      .filter((id) => !nextModels.some((model) => model.id === id))
    changeDraft((current) => {
      let next = current
      for (const modelId of removedIds) {
        next = removeProviderModelFromSettings(next, providerId, modelId)
      }
      return {
        ...next,
        providers: next.providers.map((provider) =>
          provider.id === providerId ? { ...provider, models: nextModels } : provider
        )
      }
    })
    setModelPicker(null)
    setBanner({
      kind: 'success',
      text: `已选择 ${nextModels.length} 个模型，请保存设置`
    })
  }

  const reorderProviderModels = (
    providerId: string,
    activeId: string,
    overId: string
  ): void => {
    if (activeId === overId) return
    changeProvider(providerId, (provider) => {
      const oldIndex = provider.models.findIndex((model) => model.id === activeId)
      const newIndex = provider.models.findIndex((model) => model.id === overId)
      if (oldIndex < 0 || newIndex < 0) return provider
      const models = [...provider.models]
      const [moved] = models.splice(oldIndex, 1)
      if (!moved) return provider
      models.splice(newIndex, 0, moved)
      return { ...provider, models }
    })
  }

  const addManualModel = (provider: PublicProviderSettings): void => {
    const id = (newModelInputs[provider.id] ?? '').trim()
    if (!id) return
    if (provider.models.some((model) => model.id === id)) {
      setBanner({ kind: 'error', text: `模型“${id}”已存在` })
      return
    }
    changeProvider(provider.id, (current) => ({
      ...current,
      models: [...current.models, { id, name: id, thinkingLevels: [] }]
    }))
    setNewModelInputs((current) => ({ ...current, [provider.id]: '' }))
  }

  const saveAction = (value: ActionEditorValue): void => {
    changeDraft((current) => {
      const existing = editor !== 'new' && editor ? editor : null
      const base = {
        id: existing?.id ?? createId(value.kind),
        name: value.name,
        icon: value.icon,
        kind: value.kind,
        enabled: existing?.enabled ?? false,
        order: existing?.order ?? current.actions.length
      }
      let candidate: ActionDefinition
      if (isAiActionDefinition({
        ...base,
        providerId: value.providerId ?? '',
        modelId: value.modelId ?? '',
        prompt: value.prompt ?? '',
        thinkingMode: value.thinkingMode ?? 'off'
      } as ActionDefinition)) {
        candidate = {
          ...base,
          providerId: value.providerId ?? '',
          modelId: value.modelId ?? '',
          prompt: value.prompt ?? '',
          thinkingMode: value.thinkingMode ?? 'off'
        } as ActionDefinition
      } else if (value.kind === 'search') {
        candidate = {
          ...base,
          kind: 'search',
          searchEngineId: value.searchEngineId ?? DEFAULT_SEARCH_ENGINE_ID
        } as ActionDefinition
      } else {
        candidate = base as ActionDefinition
      }
      const action = actionDefinitionSchema.parse(candidate)
      const actions = existing
        ? current.actions.map((item) => (item.id === existing.id ? action : item))
        : [...current.actions, action]
      return { ...current, actions: sortAndNumberActions(actions) }
    })
    setEditor(null)
  }

  const removeAction = (action: ActionDefinition): void => {
    if (draft && draft.actions.length <= 1) {
      setBanner({ kind: 'error', text: '至少需要保留一个动作' })
      return
    }
    setActionPendingDelete(action)
  }

  const deleteAction = (action: ActionDefinition): void => {
    changeDraft((current) => ({
      ...current,
      actions: sortAndNumberActions(current.actions.filter((item) => item.id !== action.id))
    }))
    setActionPendingDelete(null)
    setBanner({ kind: 'success', text: `已移除动作“${action.name}”；保存设置后生效` })
  }

  const moveToZone = (actionId: string, enabled: boolean, index: number): void => {
    if (!draft) return
    const action = draft.actions.find((candidate) => candidate.id === actionId)
    if (!action) return
    if (enabled && !action.enabled && enabledActions.length >= MAX_ENABLED_ACTIONS) {
      setBanner({ kind: 'error', text: `工具栏最多显示 ${MAX_ENABLED_ACTIONS} 个动作` })
      return
    }
    if (!enabled && action.enabled && enabledActions.length <= 1) {
      setBanner({ kind: 'error', text: '至少需要启用一个工具栏动作' })
      return
    }
    changeDraft((current) => ({
      ...current,
      actions: moveActionToZone(current.actions, actionId, enabled, index)
    }))
  }

  const handleDragStart = (event: DragStartEvent): void => {
    setDraggingId(String(event.active.id))
  }

  const handleDragEnd = (event: DragEndEvent): void => {
    setDraggingId(null)
    if (!draft || !event.over) return

    const actionId = String(event.active.id)
    const overId = String(event.over.id)
    if (overId === ACTION_ZONE_IDS.enabled || overId === ACTION_ZONE_IDS.disabled) {
      const enabled = overId === ACTION_ZONE_IDS.enabled
      const targetLength = enabled ? enabledActions.length : disabledActions.length
      moveToZone(actionId, enabled, targetLength)
      return
    }

    const overAction = draft.actions.find((action) => action.id === overId)
    if (!overAction) return
    const targetActions = overAction.enabled ? enabledActions : disabledActions
    const targetIndex = targetActions.findIndex((action) => action.id === overAction.id)
    if (targetIndex >= 0) moveToZone(actionId, overAction.enabled, targetIndex)
  }

  if (!draft) {
    return (
      <main className="settings-loading">
        <LoaderCircle className="settings-spin" size={24} aria-hidden="true" />
        <span>正在载入设置…</span>
        {banner?.kind === 'error' && <div className="notice notice--error">{banner.text}</div>}
      </main>
    )
  }

  const busy = operation !== null
  const permission = accessibilityPresentation(accessibility)
  const selectionAvailable = accessibility?.available ?? accessibility?.trusted ?? false
  // 「划词获取」的策略路由只有 Windows 选择监视器会消费（apps/windows selection），
  // macOS 的回退策略在原生桥内按应用硬编码。隐藏该分区，避免展示一个不生效的设置。
  // 普通计算即可：分区只有 7 个，且必须放在 loading 提前 return 之后，不能用 hook。
  const visibleSections = SETTINGS_SECTIONS.filter(
    (section) => section.id !== 'capture' || accessibility?.platform === 'windows'
  )
  const activeSectionId = visibleSections.some((section) => section.id === activeSection)
    ? activeSection
    : visibleSections[0]!.id
  const activeMeta = visibleSections.find((section) => section.id === activeSectionId)
    ?? visibleSections[0]!

  return (
    <main className="settings-page settings-page--shell">
      <aside className="settings-shell__nav">
        <header className="settings-hero">
          <div className="settings-logo" aria-hidden="true"><Sparkles size={22} /></div>
          <div>
            <h1>{APP_NAME} 设置</h1>
            <p>划词后快速复制、搜索或交给 AI。</p>
          </div>
        </header>
        <nav className="settings-nav" aria-label="设置分区">
          {visibleSections.map((section) => (
            <button
              key={section.id}
              type="button"
              className={`settings-nav__item ${activeSectionId === section.id ? 'is-selected' : ''}`}
              aria-current={activeSectionId === section.id ? 'page' : undefined}
              onClick={() => setActiveSection(section.id)}
            >
              {section.label}
            </button>
          ))}
        </nav>
      </aside>

      <div className="settings-shell__main">
        {banner && (
          <div
            className={`notice notice--${banner.kind} settings-banner ${
              bannerFading ? 'settings-banner--fading' : ''
            }`}
            role={banner.kind === 'error' ? 'alert' : 'status'}
          >
            {banner.kind === 'success' ? <Check size={16} aria-hidden="true" /> : <CircleAlert size={16} aria-hidden="true" />}
            <span>{banner.text}</span>
          </div>
        )}

        <div className="settings-shell__content" key={activeSectionId}>
          {activeSectionId === 'general' && (
            <>
              <section className="settings-section settings-section--enter" aria-labelledby="general-title">
                <div className="section-heading">
                  <div>
                    <h2 id="general-title">{activeMeta.label}</h2>
                    <p>{activeMeta.blurb}</p>
                  </div>
                </div>
                <div className="settings-tip" role="note">
                  <span className="settings-tip__label">使用提示</span>
                  <p>
                    推荐先保持「划词后」触发；若与其他软件冲突，可改用快捷键。改完后点右下角
                    <strong>保存设置</strong>才会生效。
                  </p>
                </div>
                <div className="settings-card settings-card--rows">
                  <SettingSwitch
                    title="启用划词助手"
                    description="关闭后不再监听新的文本选择，复制与搜索等动作也会暂停。"
                    checked={draft.enabled}
                    onChange={(enabled) => changeDraft((current) => ({ ...current, enabled }))}
                  />
                  <div className="setting-row setting-row--stackable">
                    <div><strong>触发方式</strong><p>「划词后」自动弹出工具栏；「快捷键」仅在按下组合键时捕获选区。</p></div>
                    <div className="segmented-control" role="radiogroup" aria-label="触发方式">
                      {(['selected', 'shortcut'] as const).map((mode) => (
                        <button type="button" role="radio" aria-checked={draft.trigger.mode === mode}
                          className={draft.trigger.mode === mode ? 'is-selected' : ''} key={mode}
                          onClick={() => changeDraft((current) => ({
                            ...current,
                            trigger: { mode },
                            captureShortcut:
                              mode === 'shortcut' && current.captureShortcut.trim() === ''
                                ? SUGGESTED_CAPTURE_SHORTCUT
                                : current.captureShortcut
                          }))}>
                          {mode === 'selected' ? '划词后' : '快捷键'}
                        </button>
                      ))}
                    </div>
                  </div>
                  {draft.trigger.mode === 'shortcut' && (
                    <div className="setting-row setting-row--stackable">
                      <div>
                        <strong>捕获当前选区快捷键</strong>
                        <p>点击输入框后按下组合键即可录制；Backspace 可清除。</p>
                      </div>
                      <div className="shortcut-capture">
                        <input
                          className="control shortcut-control"
                          value={draft.captureShortcut}
                          maxLength={128}
                          spellCheck={false}
                          readOnly
                          aria-label="捕获当前选区快捷键"
                          placeholder="点击后按下组合键"
                          onKeyDown={(event) => {
                            if (event.key === 'Tab') return
                            event.preventDefault()
                            event.stopPropagation()
                            if (event.key === 'Backspace' || event.key === 'Delete') {
                              changeDraft((current) => ({ ...current, captureShortcut: '' }))
                              return
                            }
                            if (event.key === 'Escape') {
                              event.currentTarget.blur()
                              return
                            }
                            const shortcut = formatKeyboardEventToTauriShortcut(event)
                            if (!shortcut) return
                            changeDraft((current) => ({ ...current, captureShortcut: shortcut }))
                          }}
                        />
                        {draft.captureShortcut.trim() !== '' && (
                          <button
                            className="button button--ghost shortcut-clear"
                            type="button"
                            onClick={() => changeDraft((current) => ({ ...current, captureShortcut: '' }))}
                          >
                            清除
                          </button>
                        )}
                      </div>
                    </div>
                  )}
                  <div className="setting-row">
                    <div><strong>工具条显示</strong><p>仅显示图标时仍保留悬浮提示和辅助功能名称。</p></div>
                    <select className="control compact-control" value={draft.toolbar.displayMode}
                      onChange={(event) => changeDraft((current) => ({
                        ...current,
                        toolbar: { displayMode: event.target.value as PublicSettings['toolbar']['displayMode'] }
                      }))}>
                      <option value="icon-label">图标和文字</option><option value="icon-only">仅图标</option>
                    </select>
                  </div>
                  {accessibility?.platform === 'windows' && (
                    <div className="setting-row">
                      <div>
                        <strong>关闭主窗口时</strong>
                        <p>隐藏后划词功能继续运行，也可选择直接退出 TextLens。</p>
                      </div>
                      <select
                        className="control compact-control"
                        aria-label="关闭主窗口时"
                        value={draft.application.closeBehavior}
                        onChange={(event) => changeDraft((current) => ({
                          ...current,
                          application: {
                            closeBehavior: event.target.value as PublicSettings['application']['closeBehavior']
                          }
                        }))}
                      >
                        <option value="hide-to-tray">隐藏到通知区域（默认）</option>
                        <option value="quit">退出 TextLens</option>
                      </select>
                    </div>
                  )}
                </div>
              </section>

              <section className="settings-section" aria-labelledby="permission-title">
                <div className="section-heading">
                  <div>
                    <h2 id="permission-title">{permission.heading}</h2>
                    <p>{permission.description}</p>
                  </div>
                </div>
                <div className="settings-card permission-card">
                  <div className={`permission-icon ${selectionAvailable ? 'permission-icon--ok' : ''}`}>
                    {selectionAvailable ? <ShieldCheck size={21} /> : <LockKeyhole size={21} />}
                  </div>
                  <div className="permission-copy"><strong>{permission.state}</strong>
                    <p>{permission.detail}</p></div>
                  <button
                    className={`button ${accessibility?.trusted ? 'button--status' : ''}`}
                    type="button"
                    disabled={busy || accessibility?.trusted || !accessibility?.canRequest}
                    onClick={() => void requestAccessibility()}
                  >
                    {operation === 'accessibility' && <LoaderCircle className="settings-spin" size={15} />}
                    {permission.button}
                  </button>
                </div>
                {accessibility?.diagnostics?.shortcutError && (
                  <div className="notice notice--error runtime-diagnostic" role="status">
                    <CircleAlert size={16} aria-hidden="true" />
                    <div><strong>全局快捷键不可用</strong><p>{accessibility.diagnostics.shortcutError}</p></div>
                  </div>
                )}
              </section>
            </>
          )}

          {activeSectionId === 'providers' && (
          <section className="settings-section settings-section--enter" aria-labelledby="providers-title">
        <div className="section-heading section-heading--actions">
          <div><h2 id="providers-title">AI 服务商与模型</h2><p>{activeMeta.blurb}</p></div>
          <button className="button" type="button" onClick={addProvider}><Plus size={15} />添加服务商</button>
        </div>
        <div className="settings-tip" role="note">
          <span className="settings-tip__label">配置顺序</span>
          <ol className="settings-tip__steps">
            <li>填写 API 地址与 Key</li>
            <li>测试连接确认可用</li>
            <li>获取模型并勾选常用项</li>
            <li>保存设置，再到「动作」里绑定模型</li>
          </ol>
        </div>
        <div className="provider-stack">
          {draft.providers.map((provider) => (
            <article
              className={`settings-card provider-card ${provider.enabled === false ? 'provider-card--disabled' : ''}`}
              key={provider.id}
            >
              <header className="provider-card__header">
                <div className="provider-card__identity"><span className="provider-avatar"><Sparkles size={17} /></span>
                  <input className="provider-name-input" value={provider.name} maxLength={80} aria-label="服务商名称"
                    onChange={(event) => changeProvider(provider.id, (current) => ({ ...current, name: event.target.value }))} /></div>
                <div className="provider-card__header-actions">
                  <label className="provider-enabled-toggle" title={provider.enabled === false ? '已关闭：动作与结果中不显示其模型' : '已启用'}>
                    <span className="provider-enabled-toggle__label">
                      {provider.enabled === false ? '已关闭' : '已启用'}
                    </span>
                    <input
                      type="checkbox"
                      role="switch"
                      aria-label={`${provider.name}：${provider.enabled === false ? '启用' : '关闭'}服务商`}
                      checked={provider.enabled !== false}
                      onChange={(event) => changeProvider(provider.id, (current) => ({
                        ...current,
                        enabled: event.target.checked
                      }))}
                    />
                  </label>
                  <button className="icon-button action-delete" type="button" title="删除服务商" aria-label={`删除${provider.name}`}
                    onClick={() => setProviderPendingDelete(provider)}><Trash2 size={16} /></button>
                </div>
              </header>
              <div className="provider-grid">
                <label className="field field--wide"><span className="field__label">API 地址（HTTP / HTTPS）</span>
                  <input className="control" type="url" value={provider.baseUrl} spellCheck={false}
                    placeholder="https://api.example.com/v1 或 http://localhost:11434/v1"
                    onChange={(event) => changeProvider(provider.id, (current) => ({ ...current, baseUrl: event.target.value }))} />
                  <span className="field__hint">
                    填写到版本前缀即可，例如 <code>…/v1</code>；不要带 <code>/chat/completions</code>。本地 Ollama 可用 HTTP。
                  </span>
                  {usesPlainHttp(provider.baseUrl) && <span className="provider-http-warning" role="alert">
                    <CircleAlert size={15} aria-hidden="true" />当前服务使用 HTTP，API Key 将通过网络明文传输。仅在你信任的网络和服务中使用。
                  </span>}
                </label>
                <label className="field"><span className="field__label">API Key</span>
                  <div className="key-control">
                    <KeyRound size={16} aria-hidden="true" />
                    <input
                      type={keyVisible[provider.id] ? 'text' : 'password'}
                      value={keyInputs[provider.id] ?? ''}
                      maxLength={16_384}
                      autoComplete="new-password"
                      spellCheck={false}
                      placeholder={provider.keyConfigured || (keyBaselines[provider.id] ?? '') ? '已保存的 API Key' : '输入 API Key'}
                      onChange={(event) => {
                        const value = event.target.value
                        // Keep ref in sync before re-render so in-flight reveal won't clobber baseline checks.
                        keyInputsRef.current = { ...keyInputsRef.current, [provider.id]: value }
                        setKeyInputs((current) => ({ ...current, [provider.id]: value }))
                      }}
                    />
                    <button
                      className="key-visibility-toggle"
                      type="button"
                      disabled={Boolean(keyRevealInFlight[provider.id])}
                      aria-label={keyVisible[provider.id] ? '隐藏 API Key' : '显示 API Key'}
                      title={keyVisible[provider.id] ? '隐藏' : '显示'}
                      onClick={() => void revealProviderApiKey(provider)}
                    >
                      {keyVisible[provider.id]
                        ? <EyeOff size={16} aria-hidden="true" />
                        : <Eye size={16} aria-hidden="true" />}
                    </button>
                  </div>
                </label>
                <div className="provider-actions">
                  <button className="button" type="button" disabled={busy} onClick={() => void testProvider(provider)}>
                    {operation === `test:${provider.id}` && <LoaderCircle className="settings-spin" size={15} />}测试连接</button>
                  <button className="button" type="button" disabled={busy} onClick={() => void openModelPicker(provider)}>
                    {operation === `fetch:${provider.id}` ? <LoaderCircle className="settings-spin" size={15} /> : <RefreshCw size={15} />}获取模型</button>
                  <button className="button button--danger" type="button" disabled={busy || !provider.keyConfigured}
                    onClick={() => void clearProviderKey(provider)}>清除密钥</button>
                </div>
                <div className="field field--wide"><span className="field__label">模型（{provider.models.length}）</span>
                  <div className="model-add-row"><input className="control" value={newModelInputs[provider.id] ?? ''}
                    placeholder="手动添加模型 ID" spellCheck={false}
                    onChange={(event) => setNewModelInputs((current) => ({ ...current, [provider.id]: event.target.value }))}
                    onKeyDown={(event) => { if (event.key === 'Enter') { event.preventDefault(); addManualModel(provider) } }} />
                    <button className="button" type="button" onClick={() => addManualModel(provider)}>添加</button></div>
                  <DndContext
                    sensors={dragSensors}
                    collisionDetection={closestCenter}
                    onDragStart={(event) => setDraggingModelId(String(event.active.id))}
                    onDragCancel={() => setDraggingModelId(null)}
                    onDragEnd={(event) => {
                      setDraggingModelId(null)
                      const { active, over } = event
                      if (!over) return
                      reorderProviderModels(provider.id, String(active.id), String(over.id))
                    }}
                  >
                    <SortableContext
                      items={provider.models.map((model) => model.id)}
                      strategy={verticalListSortingStrategy}
                    >
                      <div className="model-row-list" role="list">
                        {provider.models.map((model) => (
                          <SortableModelRow
                            key={model.id}
                            model={model}
                            dragging={draggingModelId === model.id}
                            onRemove={() => changeDraft((current) =>
                              removeProviderModelFromSettings(current, provider.id, model.id)
                            )}
                          />
                        ))}
                        {provider.models.length === 0 && (
                          <span className="model-empty">
                            尚无模型。可点「获取模型」多选添加，或在上方手动输入模型 ID。
                          </span>
                        )}
                      </div>
                    </SortableContext>
                  </DndContext>
                </div>
              </div>
            </article>
          ))}
          {draft.providers.length === 0 && (
            <div className="settings-card empty-card empty-card--guided">
              <strong>尚未配置服务商</strong>
              <p>本地「复制」「搜索」仍可使用。需要翻译、总结等 AI 功能时，点右上角「添加服务商」开始。</p>
            </div>
          )}
        </div>
      </section>
          )}

          {activeSectionId === 'actions' && (
      <section className="settings-section settings-section--enter" aria-labelledby="actions-title">
        <div className="section-heading section-heading--actions">
          <div>
            <h2 id="actions-title">工具栏动作</h2>
            <p>{activeMeta.blurb}</p>
          </div>
          <button className="button" type="button" onClick={() => setEditor('new')}>
            <Plus size={15} />添加动作
          </button>
        </div>
        <div className="settings-card toolbar-preview-card">
          <span className="toolbar-preview-label">实时预览</span>
          <div className={`toolbar-preview ${draft.toolbar.displayMode === 'icon-only' ? 'toolbar-preview--icons' : ''}`}>
            <span className="toolbar-preview__logo"><Sparkles size={14} /></span>
            {enabledActions.map((action) => <span className="toolbar-preview__action" key={action.id} title={action.name}>
              <ActionIcon name={action.icon} size={14} />{draft.toolbar.displayMode !== 'icon-only' && <span>{action.name}</span>}</span>)}
          </div>
          <p className="search-behavior-note">搜索动作：选中 HTTP(S) URL、域名或 IP 时直接打开；其他文字使用该动作所选的搜索引擎。</p>
        </div>
        <DndContext
          sensors={dragSensors}
          collisionDetection={closestCenter}
          onDragStart={handleDragStart}
          onDragCancel={() => setDraggingId(null)}
          onDragEnd={handleDragEnd}
        >
          <div className="action-zones">
            <ActionZone title="显示在工具栏" count={`${enabledActions.length} / ${MAX_ENABLED_ACTIONS}`} actions={enabledActions}
              enabled draggingId={draggingId}
              onEdit={setEditor} onDelete={removeAction} onToggle={(action) => moveToZone(action.id, false, disabledActions.length)} />
            <ActionZone title="未显示" count={`${disabledActions.length}`} actions={disabledActions}
              enabled={false} draggingId={draggingId}
              onEdit={setEditor} onDelete={removeAction} onToggle={(action) => moveToZone(action.id, true, enabledActions.length)} />
          </div>
        </DndContext>
      </section>
          )}

          {activeSectionId === 'language' && (
      <section className="settings-section settings-section--enter" aria-labelledby="language-title">
        <div className="section-heading">
          <div>
            <h2 id="language-title">{activeMeta.label}</h2>
            <p>{activeMeta.blurb}</p>
          </div>
        </div>
        <div className="settings-card settings-card--rows">
          <div className="setting-row">
            <div>
              <strong>AI 默认回复语言</strong>
              <p>仅影响使用 {'{{language}}'} 的动作（如总结、解释），不改变设置界面语言。</p>
            </div>
            <select
              className="control compact-control"
              aria-label="AI 默认回复语言"
              value={draft.locale}
              onChange={(event) =>
                changeDraft((current) => ({ ...current, locale: event.target.value as SupportedLocale }))
              }
            >
              <option value="zh-CN">简体中文</option>
              <option value="en-US">English</option>
            </select>
          </div>
        </div>
        <div className="settings-card form-grid settings-card--follow">
          <label className="field">
            <span className="field__label">翻译主要语言</span>
            <select
              className="control"
              aria-label="翻译主要语言"
              value={draft.translate.primaryLanguage}
              onChange={(event) => {
                const primaryLanguage = event.target.value as TranslationLanguage
                changeDraft((current) => {
                  const alternateLanguage =
                    current.translate.alternateLanguage === primaryLanguage
                      ? (primaryLanguage === 'zh-CN' ? 'en-US' : 'zh-CN')
                      : current.translate.alternateLanguage
                  return {
                    ...current,
                    translate: { primaryLanguage, alternateLanguage }
                  }
                })
              }}
            >
              {TRANSLATION_LANGUAGES.map((code) => (
                <option value={code} key={code}>{TRANSLATION_LANGUAGE_NAMES[code]}</option>
              ))}
            </select>
          </label>
          <label className="field">
            <span className="field__label">翻译另一语言</span>
            <select
              className="control"
              aria-label="翻译另一语言"
              value={draft.translate.alternateLanguage}
              onChange={(event) => {
                const alternateLanguage = event.target.value as TranslationLanguage
                changeDraft((current) => {
                  const primaryLanguage =
                    current.translate.primaryLanguage === alternateLanguage
                      ? (alternateLanguage === 'zh-CN' ? 'en-US' : 'zh-CN')
                      : current.translate.primaryLanguage
                  return {
                    ...current,
                    translate: { primaryLanguage, alternateLanguage }
                  }
                })
              }}
            >
              {TRANSLATION_LANGUAGES.map((code) => (
                <option value={code} key={code}>{TRANSLATION_LANGUAGE_NAMES[code]}</option>
              ))}
            </select>
          </label>
          <p className="translation-summary field--wide">
            默认：检测到 {TRANSLATION_LANGUAGE_NAMES[draft.translate.primaryLanguage]} 时译为{' '}
            {TRANSLATION_LANGUAGE_NAMES[draft.translate.alternateLanguage]}，反之亦然；
            其他语种默认译为中文，中文默认译为英语。结果框可切换为日语、韩语、俄语、德语、法语等。
          </p>
        </div>
      </section>
          )}

          {activeSectionId === 'result' && (
      <section className="settings-section settings-section--enter" aria-labelledby="result-title">
        <div className="section-heading"><div><h2 id="result-title">结果窗口</h2><p>{activeMeta.blurb}</p></div></div>
        <div className="settings-card settings-card--rows">
          <SettingSwitch title="跟随鼠标位置" description="在点击动作时的鼠标附近打开结果，空间不足时自动翻转。"
            checked={draft.result.followCursor} onChange={(followCursor) => changeDraft((current) => ({ ...current, result: { ...current.result, followCursor } }))} />
          <SettingSwitch title="记住结果框尺寸" description={draft.result.lastSize ? `当前记忆：${Math.round(draft.result.lastSize.width)} × ${Math.round(draft.result.lastSize.height)}` : '用户调整后，下次沿用相同逻辑尺寸。'}
            checked={draft.result.rememberSize} onChange={(rememberSize) => changeDraft((current) => ({ ...current, result: { ...current.result, rememberSize } }))}
            extra={draft.result.lastSize ? <button className="button button--quiet" type="button" disabled={busy} onClick={() => void resetResultSize()}>重置尺寸</button> : undefined} />
          <SettingSwitch title="默认置顶" description="新打开的结果窗口保持在其他窗口上方；也可在窗口标题栏临时切换。"
            checked={draft.result.defaultPinned} onChange={(defaultPinned) => changeDraft((current) => ({ ...current, result: { ...current.result, defaultPinned } }))} />
          <div className="setting-row"><div><strong>结果文字大小</strong><p>调整翻译、总结、解释等结果正文的字号。</p></div>
            <div className="range-control"><input type="range" aria-label="结果文字大小"
              min={RESULT_FONT_SIZE_MIN} max={RESULT_FONT_SIZE_MAX} step={1} value={draft.result.fontSize}
              onChange={(event) => changeDraft((current) => ({ ...current, result: { ...current.result, fontSize: Number(event.target.value) } }))} />
              <span>{draft.result.fontSize} px</span></div></div>
          <div className="setting-row"><div><strong>关闭方式</strong><p>仅未置顶窗口执行；置顶窗口始终忽略自动关闭。</p></div>
            <select className="control result-mode-control" value={draft.result.dismissMode}
              onChange={(event) => changeDraft((current) => ({ ...current, result: { ...current.result, dismissMode: event.target.value as PublicSettings['result']['dismissMode'] } }))}>
              <option value="blur">失去焦点时关闭（推荐）</option><option value="pointer-leave">鼠标移出后关闭</option><option value="manual">手动关闭</option>
            </select></div>
          {draft.result.dismissMode === 'pointer-leave' && <div className="setting-row"><div><strong>移出延迟</strong><p>短暂移出不会误关窗口。</p></div>
            <div className="range-control"><input type="range" min={100} max={2000} step={50} value={draft.result.dismissDelayMs}
              onChange={(event) => changeDraft((current) => ({ ...current, result: { ...current.result, dismissDelayMs: Number(event.target.value) } }))} /><span>{draft.result.dismissDelayMs} ms</span></div></div>}
          <div className="setting-row"><div><strong>窗口透明度</strong><p>内容和控件会保持统一透明度。</p></div>
            <div className="range-control"><input type="range" min={20} max={100} value={Math.round(draft.result.opacity * 100)}
              onChange={(event) => changeDraft((current) => ({ ...current, result: { ...current.result, opacity: Number(event.target.value) / 100 } }))} /><span>{Math.round(draft.result.opacity * 100)}%</span></div></div>
        </div>
      </section>
          )}

          {activeSectionId === 'capture' && (
      <section className="settings-section settings-section--enter" aria-labelledby="capture-title">
        <div className="section-heading"><div><h2 id="capture-title">划词获取</h2><p>{activeMeta.blurb}</p></div></div>
        <div className="settings-card settings-card--rows">
          <div className="setting-row setting-row--stackable">
            <div><strong>默认方式</strong><p>优先使用 Selection Hook；已知 PDF 和画布程序仅在安全校验通过时使用受控复制回退。</p></div>
            <select className="control compact-control" aria-label="默认划词获取方式" value={draft.selectionCapture.defaultStrategy}
              onChange={(event) => changeDraft((current) => ({
                ...current,
                selectionCapture: {
                  ...current.selectionCapture,
                  defaultStrategy: event.target.value as PublicSettings['selectionCapture']['defaultStrategy']
                }
              }))}>
              <option value="selection-hook">Selection Hook</option>
              <option value="auto">自动回退</option>
              <option value="clipboard">受控 Ctrl+C</option>
            </select>
          </div>
          <div className="setting-row setting-row--column">
            <div><strong>程序规则</strong><p>填写可执行文件名或完整路径。受控 Ctrl+C 仅在已完成的划词手势后运行。</p></div>
            <div className="capture-rule-list" role="list">
              {draft.selectionCapture.applications.map((rule, index) => (
                <div className="capture-rule" key={`${rule.application}-${index}`} role="listitem">
                  <input
                    className="control"
                    aria-label={`程序规则 ${index + 1}`}
                    value={rule.application}
                    placeholder="acrobat.exe"
                    spellCheck={false}
                    onChange={(event) => changeDraft((current) => ({
                      ...current,
                      selectionCapture: {
                        ...current.selectionCapture,
                        applications: current.selectionCapture.applications.map((candidate, candidateIndex) =>
                          candidateIndex === index ? { ...candidate, application: event.target.value } : candidate
                        )
                      }
                    }))}
                  />
                  <select
                    className="control"
                    aria-label={`${rule.application || '程序'} 的划词获取方式`}
                    value={rule.strategy}
                    onChange={(event) => changeDraft((current) => ({
                      ...current,
                      selectionCapture: {
                        ...current.selectionCapture,
                        applications: current.selectionCapture.applications.map((candidate, candidateIndex) =>
                          candidateIndex === index
                            ? { ...candidate, strategy: event.target.value as typeof candidate.strategy }
                            : candidate
                        )
                      }
                    }))}
                  >
                    <option value="selection-hook">Selection Hook</option>
                    <option value="auto">自动回退</option>
                    <option value="clipboard">受控 Ctrl+C</option>
                  </select>
                  <button
                    className="icon-button action-delete"
                    type="button"
                    title="移除程序规则"
                    aria-label={`移除 ${rule.application || '程序'} 规则`}
                    onClick={() => changeDraft((current) => ({
                      ...current,
                      selectionCapture: {
                        ...current.selectionCapture,
                        applications: current.selectionCapture.applications.filter((_, candidateIndex) => candidateIndex !== index)
                      }
                    }))}
                  ><Trash2 size={16} /></button>
                </div>
              ))}
            </div>
            <div className="setting-row__end">
              <button
                className="button button--quiet"
                type="button"
                onClick={() => changeDraft((current) => ({
                  ...current,
                  selectionCapture: {
                    ...current.selectionCapture,
                    applications: [...current.selectionCapture.applications, {
                      application: '',
                      strategy: 'clipboard'
                    }]
                  }
                }))}
              ><Plus size={15} />添加程序</button>
            </div>
          </div>
        </div>
      </section>
          )}

          {activeSectionId === 'filter' && (
      <section className="settings-section settings-section--enter" aria-labelledby="filter-title">
        <div className="section-heading"><div><h2 id="filter-title">应用过滤</h2><p>{activeMeta.blurb}</p></div></div>
        <div className="settings-card settings-card--rows">
          <div className="setting-row setting-row--stackable">
            <div><strong>过滤模式</strong><p>选择全部应用、仅列表应用或排除列表应用。</p></div>
            <select className="control compact-control" value={draft.filter.mode}
              onChange={(event) => changeDraft((current) => ({
                ...current,
                filter: { ...current.filter, mode: event.target.value as PublicSettings['filter']['mode'] }
              }))}>
              <option value="default">所有应用</option><option value="whitelist">仅列表应用</option><option value="blacklist">排除列表应用</option>
            </select>
          </div>
          {draft.filter.mode !== 'default' && (
            <label className="setting-row setting-row--column">
              <div><strong>应用列表</strong><p>每行填写一个应用名称、Bundle ID 或可执行文件名。</p></div>
              <textarea className="control filter-list-input" value={draft.filter.applications.join('\n')}
                onChange={(event) => changeDraft((current) => ({
                  ...current,
                  filter: { ...current.filter, applications: event.target.value.split(/\r?\n/u).map((line) => line.trim()).filter(Boolean) }
                }))} />
            </label>
          )}
        </div>
      </section>
          )}
        </div>

        <footer
          className={`settings-footer ${dirty || keysDirty ? 'settings-footer--dirty' : ''}`}
          aria-live="polite"
        >
          <span className="settings-footer__status">
            {dirty || keysDirty ? (
              <>
                <span className="settings-footer__dot" aria-hidden="true" />
                有尚未保存的更改
              </>
            ) : (
              '所有更改均已保存'
            )}
          </span>
          <button
            className="button button--primary settings-save"
            type="button"
            disabled={busy}
            onClick={() => void save()}
            title={
              dirty || keysDirty
                ? '将当前更改写入并立即生效'
                : '当前内容与已保存设置一致，仍可再次保存'
            }
          >
            {operation === 'save' && <LoaderCircle className="settings-spin" size={15} />}
            保存设置
          </button>
        </footer>
      </div>

      {editor && <CustomActionDialog action={editor === 'new' ? null : editor} providers={draft.providers}
        onCancel={() => setEditor(null)} onSave={saveAction} />}
      {providerPendingDelete && (
        <ProviderDeleteDialog
          provider={providerPendingDelete}
          onCancel={() => setProviderPendingDelete(null)}
          onConfirm={() => deleteProvider(providerPendingDelete)}
        />
      )}
      {actionPendingDelete && (
        <ActionDeleteDialog
          action={actionPendingDelete}
          onCancel={() => setActionPendingDelete(null)}
          onConfirm={() => deleteAction(actionPendingDelete)}
        />
      )}
      {quitConfirmationOpen && (
        <QuitConfirmationDialog
          busy={busy}
          onCancel={() => setQuitConfirmationOpen(false)}
          onDiscard={() => {
            setQuitConfirmationOpen(false)
            void quitNow()
          }}
          onSave={() => void saveAndQuit()}
        />
      )}
      {modelPicker && (
        <ModelPickerDialog
          providerName={modelPicker.providerName}
          previous={modelPicker.previous}
          remoteModels={modelPicker.remoteModels}
          initialCheckedIds={modelPicker.checkedIds}
          onCancel={() => setModelPicker(null)}
          onApply={applyModelPicker}
        />
      )}
    </main>
  )
}

export function QuitConfirmationDialog({ busy, onCancel, onDiscard, onSave }: {
  busy: boolean
  onCancel: () => void
  onDiscard: () => void
  onSave: () => void
}): JSX.Element {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape' && !busy) onCancel()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [busy, onCancel])

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => {
      if (event.target === event.currentTarget && !busy) onCancel()
    }}>
      <section
        className="dialog-card quit-confirmation-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={`退出 ${APP_NAME}？`}
      >
        <header className="dialog-header">
          <div>
            <h2>退出 {APP_NAME}？</h2>
            <p>当前有尚未保存的更改。退出前可以先保存，或放弃这些更改。</p>
          </div>
          <button
            className="icon-button"
            type="button"
            aria-label="关闭退出确认"
            disabled={busy}
            onClick={onCancel}
          >
            <X size={18} aria-hidden="true" />
          </button>
        </header>
        <footer className="dialog-actions quit-confirmation-dialog__actions">
          <button className="button" type="button" disabled={busy} onClick={onCancel}>取消</button>
          <button className="button button--danger" type="button" disabled={busy} onClick={onDiscard}>
            放弃更改并退出
          </button>
          <button className="button button--primary" type="button" disabled={busy} onClick={onSave}>
            {busy && <LoaderCircle className="settings-spin" size={15} />}
            保存并退出
          </button>
        </footer>
      </section>
    </div>
  )
}

export function ProviderDeleteDialog({ provider, onCancel, onConfirm }: {
  provider: PublicProviderSettings
  onCancel: () => void
  onConfirm: () => void
}): JSX.Element {
  return (
    <DeleteConfirmationDialog
      title={`删除“${provider.name}”？`}
      description="使用该服务商的动作会变为未配置状态，保存设置时同时清除其密钥。"
      confirmLabel="删除服务商"
      closeLabel="关闭删除确认"
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}

export function ActionDeleteDialog({ action, onCancel, onConfirm }: {
  action: ActionDefinition
  onCancel: () => void
  onConfirm: () => void
}): JSX.Element {
  return (
    <DeleteConfirmationDialog
      title={`移除动作“${action.name}”？`}
      description="该动作会从工具栏和设置中移除，保存设置后生效。"
      confirmLabel="移除动作"
      closeLabel="关闭动作删除确认"
      onCancel={onCancel}
      onConfirm={onConfirm}
    />
  )
}

function DeleteConfirmationDialog({
  title,
  description,
  confirmLabel,
  closeLabel,
  onCancel,
  onConfirm
}: {
  title: string
  description: string
  confirmLabel: string
  closeLabel: string
  onCancel: () => void
  onConfirm: () => void
}): JSX.Element {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') onCancel()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [onCancel])

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => {
      if (event.target === event.currentTarget) onCancel()
    }}>
      <section
        className="dialog-card provider-delete-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
      >
        <header className="dialog-header">
          <div>
            <h2>{title}</h2>
            <p>{description}</p>
          </div>
          <button className="icon-button" type="button" aria-label={closeLabel} onClick={onCancel}>
            <X size={18} aria-hidden="true" />
          </button>
        </header>
        <footer className="dialog-actions provider-delete-dialog__actions">
          <button className="button" type="button" autoFocus onClick={onCancel}>取消</button>
          <button className="button button--danger" type="button" onClick={onConfirm}>{confirmLabel}</button>
        </footer>
      </section>
    </div>
  )
}

function SettingSwitch({ title, description, checked, onChange, extra }: {
  title: string
  description: string
  checked: boolean
  onChange: (checked: boolean) => void
  extra?: JSX.Element
}): JSX.Element {
  return <div className="setting-row"><div><strong>{title}</strong><p>{description}</p></div>
    <div className="setting-row__end">{extra}<label className="switch"><input type="checkbox" checked={checked}
      onChange={(event) => onChange(event.target.checked)} /><span aria-hidden="true" /><span className="sr-only">{title}</span></label></div></div>
}

function ActionZone({ title, count, actions, enabled, draggingId, onEdit, onDelete, onToggle }: {
  title: string
  count: string
  actions: readonly ActionDefinition[]
  enabled: boolean
  draggingId: string | null
  onEdit: (action: ActionDefinition) => void
  onDelete: (action: ActionDefinition) => void
  onToggle: (action: ActionDefinition) => void
}): JSX.Element {
  const { isOver, setNodeRef } = useDroppable({ id: actionZoneId(enabled) })

  return <div
    ref={setNodeRef}
    className={`action-zone ${enabled ? 'action-zone--enabled' : ''} ${isOver ? 'is-drag-over' : ''}`}
    data-action-zone={enabled ? 'enabled' : 'disabled'}
  >
    <header className="action-zone__header"><strong>{title}</strong><span>{count}</span></header>
    <SortableContext items={actions.map((action) => action.id)} strategy={verticalListSortingStrategy}>
      <div className="action-zone__list">
        {actions.map((action) => <SortableActionRow
          key={action.id}
          action={action}
          enabled={enabled}
          dragging={draggingId === action.id}
          onEdit={onEdit}
          onDelete={onDelete}
          onToggle={onToggle}
        />)}
        {actions.length === 0 && <div className="action-zone__empty">拖动动作到这里</div>}
      </div>
    </SortableContext>
  </div>
}


function SortableModelRow({ model, dragging, onRemove }: {
  model: ProviderModel
  dragging: boolean
  onRemove: () => void
}): JSX.Element {
  const {
    attributes,
    listeners,
    isDragging,
    setActivatorNodeRef,
    setNodeRef,
    transform,
    transition
  } = useSortable({ id: model.id })
  const style: CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition
  }
  const showId = model.name !== model.id

  return (
    <div
      ref={setNodeRef}
      style={style}
      className={`model-row ${dragging || isDragging ? 'is-dragging' : ''}`}
      role="listitem"
      data-model-id={model.id}
    >
      <button
        ref={setActivatorNodeRef}
        className="model-dnd-handle"
        type="button"
        title={`拖动“${model.name}”调整顺序`}
        aria-label={`拖动模型 ${model.name}`}
        {...attributes}
        {...listeners}
      >
        <GripVertical size={14} aria-hidden="true" />
      </button>
      <div className="model-row__body">
        <span className="model-row__name">{model.name}</span>
        {showId && <span className="model-row__id">{model.id}</span>}
      </div>
      <button
        type="button"
        className="model-row__remove"
        aria-label={`移除模型 ${model.name}`}
        onClick={onRemove}
      >
        <X size={12} />
      </button>
    </div>
  )
}

function ModelPickerDialog({
  providerName,
  previous,
  remoteModels,
  initialCheckedIds,
  onCancel,
  onApply
}: {
  providerName: string
  previous: readonly ProviderModel[]
  remoteModels: readonly ProviderModel[]
  initialCheckedIds: ReadonlySet<string>
  onCancel: () => void
  onApply: (checkedIds: ReadonlySet<string>) => void
}): JSX.Element {
  const [checkedIds, setCheckedIds] = useState(() => new Set(initialCheckedIds))
  const [query, setQuery] = useState('')

  const listModels = useMemo(() => {
    const byId = new Map<string, ProviderModel>()
    for (const model of remoteModels) {
      if (!byId.has(model.id)) byId.set(model.id, model)
    }
    // Manual / previous-only models stay visible so they can be unchecked.
    for (const model of previous) {
      if (!byId.has(model.id)) byId.set(model.id, model)
    }
    return [...byId.values()]
  }, [previous, remoteModels])

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase()
    if (!needle) return listModels
    return listModels.filter((model) =>
      model.id.toLowerCase().includes(needle) || model.name.toLowerCase().includes(needle)
    )
  }, [listModels, query])

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') onCancel()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [onCancel])

  const toggle = (id: string): void => {
    setCheckedIds((current) => {
      const next = new Set(current)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={(event) => {
      if (event.target === event.currentTarget) onCancel()
    }}>
      <section
        className="dialog-card model-picker-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="model-picker-title"
      >
        <header className="dialog-header">
          <div>
            <h2 id="model-picker-title">选择模型</h2>
            <p>从“{providerName}”获取的模型中勾选要保留的项，应用后请保存设置。</p>
          </div>
          <button
            className="icon-button"
            type="button"
            aria-label="关闭模型选择"
            onClick={onCancel}
          >
            <X size={18} aria-hidden="true" />
          </button>
        </header>
        <div className="model-picker-body">
          <input
            className="control"
            value={query}
            placeholder="搜索模型 ID 或名称"
            aria-label="搜索模型"
            spellCheck={false}
            onChange={(event) => setQuery(event.target.value)}
          />
          <div className="model-picker-list" role="list">
            {filtered.map((model) => {
              const checked = checkedIds.has(model.id)
              const isManual = !remoteModels.some((remote) => remote.id === model.id)
              return (
                <label
                  key={model.id}
                  className={`model-picker-option ${checked ? 'is-checked' : ''}`}
                  role="listitem"
                >
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={() => toggle(model.id)}
                  />
                  <span className="model-picker-option__text">
                    <span className="model-picker-option__name">{model.name}</span>
                    {(model.name !== model.id || isManual) && (
                      <span className="model-picker-option__meta">
                        {model.name !== model.id ? model.id : ''}
                        {isManual ? (model.name !== model.id ? ' · 手动' : '手动') : ''}
                      </span>
                    )}
                  </span>
                </label>
              )
            })}
            {filtered.length === 0 && (
              <div className="model-empty">没有匹配的模型</div>
            )}
          </div>
        </div>
        <footer className="dialog-actions model-picker-dialog__actions">
          <button className="button" type="button" onClick={onCancel}>取消</button>
          <button
            className="button button--primary"
            type="button"
            onClick={() => onApply(checkedIds)}
          >
            应用所选（{checkedIds.size}）
          </button>
        </footer>
      </section>
    </div>
  )
}

function SortableActionRow({ action, enabled, dragging, onEdit, onDelete, onToggle }: {
  action: ActionDefinition
  enabled: boolean
  dragging: boolean
  onEdit: (action: ActionDefinition) => void
  onDelete: (action: ActionDefinition) => void
  onToggle: (action: ActionDefinition) => void
}): JSX.Element {
  const {
    attributes,
    listeners,
    isDragging,
    setActivatorNodeRef,
    setNodeRef,
    transform,
    transition
  } = useSortable({ id: action.id, data: { enabled } })
  const style: CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition
  }

  return <div
    ref={setNodeRef}
    style={style}
    className={`action-dnd-row ${dragging || isDragging ? 'is-dragging' : ''}`}
    data-action-id={action.id}
  >
        <button
          ref={setActivatorNodeRef}
          className="action-dnd-handle"
          type="button"
          title={`拖动“${action.name}”调整顺序`}
          aria-label={`拖动${action.name}`}
          {...attributes}
          {...listeners}
        >
          <GripVertical size={16} aria-hidden="true" />
        </button>
        <span className="action-row__icon"><ActionIcon name={action.icon} size={17} /></span>
        <div className="action-row__copy">
          <strong>{action.name}</strong>
          <span>
            {ACTION_KIND_NAMES[action.kind]}
            {action.kind === 'search' && 'searchEngineId' in action
              ? ` · ${searchEngineDisplayName(action.searchEngineId)}`
              : isAiActionDefinition(action)
                ? ` · ${action.modelId || '未选择模型'}`
                : ''}
          </span>
          {isAiActionDefinition(action) && (
            <span className="action-row__prompt" title={action.prompt}>
              提示词：{action.prompt}
            </span>
          )}
        </div>
        <div className="action-row__controls">
          <button className="icon-button" type="button" title="编辑" aria-label={`编辑${action.name}`} onClick={() => onEdit(action)}><Pencil size={15} /></button>
          <button className="icon-button action-delete" type="button" title="移除" aria-label={`移除${action.name}`} onClick={() => onDelete(action)}><Trash2 size={15} /></button>
          <label className="switch switch--small"><input type="checkbox" checked={enabled} onChange={() => onToggle(action)} />
            <span aria-hidden="true" /><span className="sr-only">{enabled ? '停用' : '启用'}{action.name}</span></label>
        </div>
      </div>
}
