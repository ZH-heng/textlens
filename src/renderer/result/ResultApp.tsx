import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  memo,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
  type JSX
} from 'react'
import { getCurrentWindow } from '@tauri-apps/api/window'
import {
  ArrowRight,
  Check,
  ChevronDown,
  CircleAlert,
  Copy,
  LoaderCircle,
  Maximize2,
  Minimize2,
  Pin,
  RefreshCw,
  Settings2,
  Square,
  X
} from 'lucide-react'
import {
  DEFAULT_PUBLIC_SETTINGS,
  DEFAULT_RESULT_FONT_SIZE,
  RESULT_FONT_SIZE_MAX,
  RESULT_FONT_SIZE_MIN,
  TRANSLATION_LANGUAGES,
  TRANSLATION_LANGUAGE_CODES,
  countUnicodeScalars,
  defaultTranslationTarget,
  detectTranslationLanguage,
  isAiActionDefinition,
  type ActionRetryOptions,
  type PublicSettings,
  type ResultSessionSnapshot,
  type TranslationLanguage
} from '../../shared'
import { ActionIcon } from '../components/ActionIcon'
import { runDetached } from '../lib/asyncEffects'
import { getErrorMessage } from '../lib/errors'
import { applyTheme } from '../lib/theme'
import {
  flushPendingActionEvents,
  getActionEventSnapshot,
  subscribeToActionEvents
} from './actionEventStore'
import { useAutoFollowOutput } from './autoFollowOutput'
import {
  appendUserTurn,
  beginAssistantTurn,
  isAskAction,
  reconcileTranscriptContent,
  transcriptToPlainText,
  type TranscriptTurn
} from './conversationTranscript'
import { ResultOutput, stripLeadingBlankLines } from './ResultOutput'
import { useResultContentOverflow } from './resultContentOverflow'
import type { ResultSessionBootstrap } from './resultSessionBootstrap'
import type { ResultState } from './resultState'
import { useSmoothStreamText } from './useSmoothStreamText'

const RESULT_RESIZE_DIRECTIONS = [
  'NorthWest',
  'North',
  'NorthEast',
  'East',
  'SouthEast',
  'South',
  'SouthWest',
  'West'
] as const

type ResultResizeDirection = (typeof RESULT_RESIZE_DIRECTIONS)[number]

/**
 * Collapse WebKit/AppKit native popup menus (e.g. `<select>`) before the
 * result window is destroyed. An open NSMenu tracking session nests a run
 * loop that can re-enter window Destroyed handling; combined with a stale
 * WindowState lock this previously deadlocked the tray.
 */
function dismissNativeOverlays(): void {
  const active = document.activeElement
  if (active instanceof HTMLElement) {
    active.blur()
  }
  for (const select of document.querySelectorAll('select')) {
    if (select instanceof HTMLSelectElement) {
      select.blur()
    }
  }
}

interface ModelRoute {
  providerId: string
  modelId: string
}

interface ModelChoice extends ModelRoute {
  value: string
  providerName: string
  modelName: string
  keyConfigured: boolean
}

function modelRouteValue(route: ModelRoute): string {
  return JSON.stringify([route.providerId, route.modelId])
}

function modelRouteFromSnapshot(snapshot: ResultSessionSnapshot | null): ModelRoute | null {
  if (!snapshot) return null
  const { providerId, modelId } = snapshot
  if (providerId === undefined && modelId === undefined) return null
  if (providerId === undefined || modelId === undefined) {
    throw new Error('validated result snapshot has an incomplete model route')
  }
  return { providerId, modelId }
}

function isEditableTarget(target: EventTarget | null): boolean {
  return target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement ||
    (target instanceof HTMLElement && target.isContentEditable)
}

function isInteractiveElement(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest(
    'button, select, a, input, textarea, [contenteditable="true"], [data-no-drag]'
  ) !== null
}

function nodeIsWithin(container: HTMLElement, node: Node | null): boolean {
  if (!node) return false
  return container === node || container.contains(node.nodeType === Node.TEXT_NODE ? node.parentNode : node)
}

export function selectedTextWithin(
  container: HTMLElement | null,
  selection: Selection | null
): string | null {
  if (!container || !selection || selection.isCollapsed || selection.rangeCount === 0) return null
  const range = selection.getRangeAt(0)
  if (!nodeIsWithin(container, range.startContainer) || !nodeIsWithin(container, range.endContainer)) {
    return null
  }
  const text = selection.toString()
  return text.trim() ? text : null
}

export function resultFontSize(settings: PublicSettings | null): number {
  const configured = settings?.result.fontSize
  return typeof configured === 'number' && Number.isFinite(configured)
    ? Math.min(RESULT_FONT_SIZE_MAX, Math.max(RESULT_FONT_SIZE_MIN, configured))
    : DEFAULT_RESULT_FONT_SIZE
}

export function isWindowsResultRenderer(userAgent = window.navigator.userAgent): boolean {
  return /Windows/i.test(userAgent)
}

/**
 * Wait one paint frame before committing the native reveal so layout is ready
 * without paying a second frame of empty-window latency (Cherry-style TTFB).
 */
export async function waitForResultRevealFrames(
  scheduleFrame: (callback: FrameRequestCallback) => number = window.requestAnimationFrame
): Promise<void> {
  await new Promise<void>((resolve) => scheduleFrame(() => resolve()))
}

/** Subscribe to a single field so content deltas do not re-render chrome. */
function useResultField<T>(select: (state: ResultState) => T): T {
  return useSyncExternalStore(
    subscribeToActionEvents,
    () => select(getActionEventSnapshot()),
    () => select(getActionEventSnapshot())
  )
}

/**
 * Stream body only: subscribes to content fields so translate/explain/summary
 * tokens re-render this subtree without rebuilding header/footer chrome.
 * Display uses adaptive typewriter so network bursts feel smooth.
 */
function ResultStreamBody({
  revealCommitted,
  onOpenExternal
}: {
  revealCommitted: boolean
  onOpenExternal: (url: string) => void
}): JSX.Element {
  const status = useResultField((state) => state.status)
  const content = useResultField((state) => state.content)
  const contentScalarCount = useResultField((state) => state.contentScalarCount)
  const contentRevision = useResultField((state) => state.contentRevision)
  const sessionGeneration = useResultField((state) => state.sessionGeneration)
  const requestGeneration = useResultField((state) => state.requestGeneration)
  const requestId = useResultField((state) => state.requestId)
  const requestKey = `${sessionGeneration ?? 'none'}:${requestGeneration ?? 'none'}:${requestId ?? 'none'}`
  const stream = useSmoothStreamText(content, status === 'streaming', requestKey)

  return (
    <article className="markdown-body">
      <ResultOutput
        requestKey={requestKey}
        status={status}
        content={stream.text}
        contentScalarCount={
          status === 'streaming' ? stream.scalarCount : contentScalarCount
        }
        contentRevision={contentRevision}
        revealCommitted={revealCommitted}
        onOpenExternal={onOpenExternal}
      />
    </article>
  )
}

/**
 * Thinking panel: auto-expands while the model is still reasoning (no answer yet)
 * so users see progress; collapses once an answer arrives unless the user toggled.
 * Click always wins after the first manual interaction.
 *
 * Chrome is intentionally minimal: only the accent「思考」badge + chevron.
 */
function ThinkingPanel(): JSX.Element | null {
  const thinkingContent = useResultField((state) => state.thinkingContent)
  const status = useResultField((state) => state.status)
  const requestGeneration = useResultField((state) => state.requestGeneration)
  const hasAnswer = useResultField((state) => state.content.length > 0)
  const [expanded, setExpanded] = useState(false)
  const userToggledRef = useRef(false)

  const live = status === 'streaming' && !hasAnswer
  const hasThinking = thinkingContent.length > 0

  // New request (retry / continue / model switch) always restarts collapse preference.
  useEffect(() => {
    setExpanded(false)
    userToggledRef.current = false
  }, [requestGeneration])

  // Auto open while reasoning; auto close when the answer starts (unless user toggled).
  useEffect(() => {
    if (!hasThinking || userToggledRef.current) return
    if (live) setExpanded(true)
    else if (hasAnswer) setExpanded(false)
  }, [hasThinking, live, hasAnswer])

  if (!hasThinking) return null

  return (
    <section
      className={`result-thinking ${live ? 'result-thinking--live' : ''} ${expanded ? 'result-thinking--open' : ''}`}
      data-testid="result-thinking"
    >
      <button
        type="button"
        className="result-thinking__toggle"
        aria-expanded={expanded}
        aria-label={live ? '思考中，点击展开或收起' : '思考，点击展开或收起'}
        onClick={() => {
          userToggledRef.current = true
          setExpanded((current) => !current)
        }}
      >
        <span className="result-thinking__title">
          <span
            className={`result-thinking__badge ${live ? 'result-thinking__badge--live' : ''}`}
            aria-hidden="true"
          >
            思考
          </span>
        </span>
        <span className="result-thinking__meta" aria-hidden="true">
          <ChevronDown size={12} className={expanded ? 'is-expanded' : ''} />
        </span>
      </button>
      {expanded && (
        <div className="result-thinking__body">
          {stripLeadingBlankLines(thinkingContent)}
          {live && <span className="stream-caret" aria-hidden="true" />}
        </div>
      )}
    </section>
  )
}

/**
 * Ask-only bridge: patches transcript turns from stream content without
 * forcing the non-ask chrome path to subscribe to the content string.
 * Applies the same smooth typewriter as translate/explain/summary.
 */
function AskStreamBridge({
  enabled,
  onStream
}: {
  enabled: boolean
  onStream: (
    status: ResultState['status'],
    content: string,
    scalarCount: number,
    transcript: TranscriptTurn[] | null,
    requestId: string | null
  ) => void
}): null {
  const status = useResultField((state) => state.status)
  const content = useResultField((state) => state.content)
  const contentScalarCount = useResultField((state) => state.contentScalarCount)
  const sessionGeneration = useResultField((state) => state.sessionGeneration)
  const requestGeneration = useResultField((state) => state.requestGeneration)
  const requestId = useResultField((state) => state.requestId)
  const transcript = useResultField((state) => state.transcript)
  const requestKey = `${sessionGeneration ?? 'none'}:${requestGeneration ?? 'none'}:${requestId ?? 'none'}`
  const stream = useSmoothStreamText(content, status === 'streaming', requestKey)
  // Terminal states use store content and the store's authoritative scalar
  // count, so a lagging typewriter frame cannot seal the assistant turn with
  // truncated values.
  const streamPatch = status === 'streaming' ? stream.text : content
  const streamScalarCount =
    status === 'streaming' ? stream.scalarCount : contentScalarCount

  useEffect(() => {
    if (!enabled) return
    onStream(status, streamPatch, streamScalarCount, transcript, requestId)
  }, [enabled, onStream, status, streamPatch, streamScalarCount, transcript, requestId])

  return null
}

/**
 * Memoized per-turn output. The ask stream pumps the latest assistant turn
 * once per typewriter frame; a stable-props memo keeps every completed turn's
 * subtree (including its scalar scan) out of those frames.
 */
const AssistantTurnOutput = memo(function AssistantTurnOutput({
  requestKey,
  streaming,
  content,
  scalarCount,
  revealCommitted,
  onOpenExternal
}: {
  requestKey: string
  streaming: boolean
  content: string
  scalarCount: number | undefined
  revealCommitted: boolean
  onOpenExternal: (url: string) => void
}): JSX.Element {
  const fallbackCount = useMemo(
    () => (scalarCount === undefined ? countUnicodeScalars(content) : 0),
    [scalarCount, content]
  )
  return (
    <ResultOutput
      requestKey={requestKey}
      status={streaming ? 'streaming' : 'completed'}
      content={content}
      contentScalarCount={scalarCount ?? fallbackCount}
      contentRevision={content.length}
      revealCommitted={revealCommitted}
      onOpenExternal={onOpenExternal}
    />
  )
})

export interface ResultAppProps {
  bootstrap?: ResultSessionBootstrap | null
}

export function ResultApp({ bootstrap = null }: ResultAppProps = {}): JSX.Element {
  const sessionId = bootstrap?.sessionId.trim() ?? ''
  if (!bootstrap || !sessionId) {
    return (
      <main className="result-window">
        <div className="result-error" role="alert">
          <CircleAlert size={20} />
          <div>
            <strong>结果会话无效</strong>
            <p>缺少有效的会话标识，无法显示结果窗口。</p>
          </div>
        </div>
      </main>
    )
  }
  return <ResultSessionApp sessionId={sessionId} bootstrap={bootstrap} />
}

function ResultSessionApp({
  sessionId,
  bootstrap
}: {
  sessionId: string
  bootstrap: ResultSessionBootstrap
}): JSX.Element {
  const windowsRenderer = useMemo(() => isWindowsResultRenderer(), [])
  // Field-level subscriptions: content growth must not re-render chrome/footer.
  const status = useResultField((state) => state.status)
  const requestId = useResultField((state) => state.requestId)
  const actionId = useResultField((state) => state.actionId)
  const errorMessage = useResultField((state) => state.errorMessage)
  const generationNotice = useResultField((state) => state.generationNotice)
  const retryable = useResultField((state) => state.retryable)
  const preparationFailure = useResultField((state) => state.preparationFailure)
  const committedProviderId = useResultField((state) => state.providerId)
  const committedModelId = useResultField((state) => state.modelId)
  const hasContent = useResultField((state) => state.content.length > 0)
  const hasThinking = useResultField((state) => state.thinkingContent.length > 0)
  const [settings, setSettings] = useState<PublicSettings>(DEFAULT_PUBLIC_SETTINGS)
  // The initial value above is only a warm-theme placeholder. Applying it
  // before the real snapshot arrives would overwrite the theme seeded from
  // localStorage in main.tsx and reintroduce a first-frame flash.
  const [settingsLoaded, setSettingsLoaded] = useState(false)
  const settingsRevisionRef = useRef(0)

  useEffect(() => {
    if (!settingsLoaded) return
    applyTheme(settings.appearance.theme)
  }, [settings.appearance.theme, settingsLoaded])

  const [session, setSession] = useState<ResultSessionSnapshot | null>(null)
  const [pinned, setPinned] = useState(false)
  const [showOriginal, setShowOriginal] = useState(false)
  const [turns, setTurns] = useState<TranscriptTurn[]>([])
  const [translationTarget, setTranslationTarget] = useState<TranslationLanguage | null>(null)
  const [modelMenuOpen, setModelMenuOpen] = useState(false)
  const [routeSwitching, setRouteSwitching] = useState(false)
  const [retrying, setRetrying] = useState(false)
  const [preparingRequestId, setPreparingRequestId] = useState<string | null>(null)
  const [commandMessage, setCommandMessage] = useState('')
  const [copyComplete, setCopyComplete] = useState(false)
  const [followUpQuestion, setFollowUpQuestion] = useState('')
  const [followUpExpanded, setFollowUpExpanded] = useState(false)
  const [followUpSubmitting, setFollowUpSubmitting] = useState(false)
  const [revealCommitted, setRevealCommitted] = useState(false)
  const [bootstrapMessage, setBootstrapMessage] = useState('')
  const [bootstrapRetrying, setBootstrapRetrying] = useState(false)
  const contentRef = useRef<HTMLDivElement>(null)
  const contentInnerRef = useRef<HTMLDivElement>(null)
  const modelSwitchRef = useRef<HTMLDivElement>(null)
  const modelMenuRef = useRef<HTMLDivElement>(null)
  const modelToggleRef = useRef<HTMLButtonElement>(null)
  const copyResetTimer = useRef<number | null>(null)
  const selectionFrame = useRef<number | null>(null)
  const retryInFlight = useRef(false)
  const followUpInFlight = useRef(false)
  const preparingRequestRef = useRef<string | null>(null)
  const pendingTranslation = useRef<{
    previous: TranslationLanguage | null
    previousRequestId: string | null
    requestId: string | null
  } | null>(null)
  const bootstrapRetryInFlight = useRef(false)
  const disposedRef = useRef(false)
  const followUpComposing = useRef(false)
  const askOriginalDefaulted = useRef(false)
  const pendingFollowUp = useRef<{ question: string; requestId: string | null } | null>(null)
  const handleScroll = useAutoFollowOutput(contentRef, contentInnerRef, requestId)
  // Thinking counts as visible activity — hide the empty waiting spinner early.
  const isWaitingForFirstContent = status === 'streaming' && !hasContent && !hasThinking
  const contentOverflow = useResultContentOverflow(
    contentRef,
    contentInnerRef,
    !isWaitingForFirstContent
  )

  const action = useMemo(
    () => settings?.actions.find((item) => item.id === (actionId ?? session?.actionId)),
    [session?.actionId, settings, actionId]
  )
  const isAsk = isAskAction(action?.kind)
  // Every AI result is a resumable session. Keep the follow-up control for
  // translate/explain/summary/custom actions too; only hide it while a new
  // response is actively streaming.
  const isAiAction = action !== undefined && isAiActionDefinition(action)
  const showFollowUp = isAiAction && status !== 'streaming'
  const followUpDisabled =
    followUpSubmitting || preparingRequestId !== null ||
    status === 'streaming' ||
    (!isAsk && status !== 'completed') ||
    (isAsk && status !== 'completed' && status !== 'idle')

  const defaultModelRoute = useMemo<ModelRoute | null>(() => {
    if (!action || !('providerId' in action) || !action.providerId || !action.modelId) return null
    return { providerId: action.providerId, modelId: action.modelId }
  }, [action])

  const effectiveModelRoute = committedProviderId && committedModelId
    ? { providerId: committedProviderId, modelId: committedModelId }
    : modelRouteFromSnapshot(session) ?? defaultModelRoute
  const canCopy = hasContent || (isAsk && turns.some((turn) => turn.content.trim().length > 0))
  const modelGroups = useMemo(() => (
    settings?.providers
      .filter((provider) => provider.enabled !== false && provider.models.length > 0)
      .map((provider) => ({
        id: provider.id,
        name: provider.name,
        keyConfigured: provider.keyConfigured,
        choices: provider.models.map<ModelChoice>((model) => ({
          providerId: provider.id,
          modelId: model.id,
          providerName: provider.name,
          modelName: model.name,
          keyConfigured: provider.keyConfigured,
          value: modelRouteValue({ providerId: provider.id, modelId: model.id })
        }))
      })) ?? []
  ), [settings?.providers])
  const modelChoices = useMemo(
    () => modelGroups.flatMap((provider) => provider.choices),
    [modelGroups]
  )
  const selectedModel = useMemo(() => {
    if (!effectiveModelRoute) return null
    return modelChoices.find((choice) =>
      choice.providerId === effectiveModelRoute.providerId &&
      choice.modelId === effectiveModelRoute.modelId
    ) ?? null
  }, [effectiveModelRoute, modelChoices])
  const modelSummary = selectedModel
    ? `${selectedModel.providerName} · ${selectedModel.modelName}`
    : '未选择模型'
  const modelSwitchDisabled = status === 'streaming' || routeSwitching || retrying || preparingRequestId !== null

  const trackAcceptedRequest = useCallback((acceptedRequestId: string | undefined): void => {
    if (!acceptedRequestId) return
    const latest = getActionEventSnapshot()
    const pendingId =
      latest.requestId === acceptedRequestId || latest.preparationFailure?.requestId === acceptedRequestId
        ? null : acceptedRequestId
    preparingRequestRef.current = pendingId
    setPreparingRequestId(pendingId)
  }, [])

  const rollbackFailedTranslation = useCallback((failure: ResultState['preparationFailure']): void => {
    const pending = pendingTranslation.current
    if (!failure || !pending || (pending.requestId && pending.requestId !== failure.requestId)) return
    setTranslationTarget(pending.previous)
    pendingTranslation.current = null
  }, [])

  const reconcilePendingFollowUp = useCallback(
    (pending: { question: string; requestId: string | null }): void => {
      const latest = getActionEventSnapshot()
      if (latest.preparationFailure && (!pending.requestId || latest.preparationFailure.requestId === pending.requestId)) {
        setFollowUpQuestion((current) => current || latest.preparationFailure?.question || pending.question)
        pendingFollowUp.current = null
        return
      }
      if (pending.requestId && latest.requestId !== pending.requestId) return
      if (latest.status === 'completed') {
        pendingFollowUp.current = null
        setFollowUpQuestion((current) => current === pending.question ? '' : current)
      } else if (latest.status === 'error' || latest.status === 'cancelled') {
        setFollowUpQuestion((current) => current || pending.question)
      }
    },
    []
  )

  const handleAskStream = useCallback(
    (
      streamStatus: ResultState['status'],
      content: string,
      scalarCount: number,
      transcript: TranscriptTurn[] | null,
      streamRequestId: string | null
    ): void => {
      setTurns((current) => reconcileTranscriptContent(
        transcript ?? current, content, streamStatus === 'streaming', streamRequestId, scalarCount
      ))
    },
    []
  )

  const closeWindow = useCallback(async (): Promise<void> => {
    dismissNativeOverlays()
    await window.textLens.closeResult(sessionId)
  }, [sessionId])

  const cancel = useCallback(async (): Promise<void> => {
    setCommandMessage('')
    try {
      await window.textLens.cancelAction(sessionId)
    } catch (error) {
      setCommandMessage(getErrorMessage(error, '无法取消当前操作'))
    }
  }, [sessionId])

  const retry = useCallback(async (options?: ActionRetryOptions): Promise<boolean> => {
    if (retryInFlight.current || followUpInFlight.current || preparingRequestRef.current) return false
    retryInFlight.current = true
    setRetrying(true)
    setCommandMessage('')
    try {
      const result = await window.textLens.retryAction(sessionId, options)
      if (!result.accepted) {
        setCommandMessage(result.message)
        return false
      }
      trackAcceptedRequest(result.requestId)
      if (options?.targetLanguage && pendingTranslation.current) {
        pendingTranslation.current.requestId = result.requestId ?? null
        rollbackFailedTranslation(getActionEventSnapshot().preparationFailure)
      }
      if (pendingFollowUp.current) {
        pendingFollowUp.current.requestId = result.requestId ?? null
        reconcilePendingFollowUp(pendingFollowUp.current)
      }
      return true
    } catch (error) {
      setCommandMessage(getErrorMessage(error, '无法重试当前操作'))
      return false
    } finally {
      retryInFlight.current = false
      setRetrying(false)
    }
  }, [reconcilePendingFollowUp, rollbackFailedTranslation, sessionId, trackAcceptedRequest])

  const switchModel = useCallback(async (value: string): Promise<void> => {
    const next = modelChoices.find((choice) => choice.value === value)
    if (
      !next || routeSwitching || status === 'streaming' ||
      (effectiveModelRoute?.providerId === next.providerId &&
        effectiveModelRoute.modelId === next.modelId)
    ) return

    setRouteSwitching(true)
    await retry({ providerId: next.providerId, modelId: next.modelId })
    setRouteSwitching(false)
  }, [effectiveModelRoute, modelChoices, retry, routeSwitching, status])

  const chooseModel = useCallback((value: string): void => {
    setModelMenuOpen(false)
    window.requestAnimationFrame(() => modelToggleRef.current?.focus())
    void switchModel(value)
  }, [switchModel])

  const copy = useCallback(async (): Promise<void> => {
    const latest = getActionEventSnapshot()
    const completeTurns = reconcileTranscriptContent(
      latest.transcript ?? turns, latest.content, latest.status === 'streaming',
      latest.requestId, latest.contentScalarCount
    )
    const content = isAsk && completeTurns.length > 0
      ? transcriptToPlainText(completeTurns)
      : latest.content
    if (!content) return
    setCommandMessage('')
    try {
      await window.textLens.copyText(content)
      setCopyComplete(true)
      if (copyResetTimer.current !== null) window.clearTimeout(copyResetTimer.current)
      copyResetTimer.current = window.setTimeout(() => {
        copyResetTimer.current = null
        setCopyComplete(false)
      }, 1_500)
    } catch (error) {
      setCommandMessage(getErrorMessage(error, '复制失败'))
    }
  }, [isAsk, turns])

  const submitFollowUp = useCallback(async (): Promise<void> => {
    const question = followUpQuestion.trim()
    const canSubmit =
      isAsk
        ? status === 'completed' || status === 'idle'
        : status === 'completed'
    if (!question || followUpInFlight.current || retryInFlight.current || preparingRequestRef.current || !canSubmit) return
    followUpInFlight.current = true
    setCommandMessage('')
    setFollowUpSubmitting(true)
    try {
      if (!window.textLens.continueAction) {
        throw new Error('当前版本不支持继续提问')
      }
      if (isAsk) {
        const turnId =
          typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
            ? crypto.randomUUID()
            : `assistant-${Date.now()}`
        setTurns((current) => beginAssistantTurn(appendUserTurn(current, question), turnId))
      }
      const result = await window.textLens.continueAction(sessionId, question)
      if (!result.accepted) {
        setCommandMessage(result.message)
        if (isAsk) {
          setTurns((current) => (current.length < 2 ? current : current.slice(0, -2)))
        }
        return
      }
      const pending = { question, requestId: result.requestId ?? null }
      pendingFollowUp.current = pending
      trackAcceptedRequest(result.requestId)
      setFollowUpQuestion('')
      reconcilePendingFollowUp(pending)
    } catch (error) {
      setCommandMessage(getErrorMessage(error, '无法继续提问'))
      if (isAsk) {
        setTurns((current) => (current.length < 2 ? current : current.slice(0, -2)))
      }
    } finally {
      followUpInFlight.current = false
      setFollowUpSubmitting(false)
    }
  }, [followUpQuestion, followUpSubmitting, isAsk, preparingRequestId, reconcilePendingFollowUp, sessionId, status, trackAcceptedRequest])

  useEffect(() => {
    let disposed = false
    const startedAtRevision = settingsRevisionRef.current
    const unsubscribe = window.textLens.onSettingsChanged((next) => {
      settingsRevisionRef.current += 1
      setSettings(next)
      setSettingsLoaded(true)
    })
    void window.textLens.getSettings().then((initial) => {
      if (!disposed && settingsRevisionRef.current === startedAtRevision) {
        setSettings(initial)
        setSettingsLoaded(true)
      }
    }).catch((error: unknown) => {
      if (!disposed) {
        setCommandMessage(getErrorMessage(error, '无法读取结果窗口设置'))
      }
    })
    return () => {
      disposed = true
      unsubscribe()
    }
  }, [sessionId])

  const consumeBootstrap = useCallback(async (
    start: () => Promise<ResultSessionSnapshot | null>,
    isDisposed: () => boolean
  ): Promise<void> => {
    let snapshot: ResultSessionSnapshot | null
    try {
      snapshot = await start()
      if (isDisposed()) return
      if (!snapshot) throw new Error('结果会话不可用')
      setSession(snapshot)
      setPinned(snapshot.pinned)
      setBootstrapMessage('')
    } catch {
      if (!isDisposed()) {
        setBootstrapMessage('无法连接结果会话，请重试。')
      }
      return
    }

    try {
      await bootstrap.reveal(async () => {
        // Native layout and the first hydrated React frame are independent.
        // Overlap them so showing the window adds no extra serial frame.
        await Promise.all([
          window.textLens.prepareResultReveal?.(sessionId),
          waitForResultRevealFrames()
        ])
        if (window.textLens.commitResultReveal) {
          await window.textLens.commitResultReveal(sessionId)
        }
      })
      if (isDisposed()) return
      flushPendingActionEvents('native-reveal')
      setRevealCommitted(true)
    } catch {
      if (isDisposed()) return
      const revealMessage = '结果窗口显示失败，请重试。'
      setCommandMessage(revealMessage)
      runDetached(window.textLens.failResultReveal?.(sessionId, revealMessage), {
        scope: 'result',
        operation: 'fail-result-reveal',
        onError: (error) => setCommandMessage(getErrorMessage(error, revealMessage))
      })
    }
  }, [bootstrap, sessionId])

  useEffect(() => {
    let disposed = false
    disposedRef.current = false
    runDetached(consumeBootstrap(bootstrap.start, () => disposed), {
      scope: 'result',
      operation: 'result-ready-consume'
    })
    return () => {
      disposed = true
      disposedRef.current = true
    }
  }, [bootstrap, consumeBootstrap])

  const retryBootstrap = useCallback((): void => {
    if (bootstrapRetryInFlight.current) return
    bootstrapRetryInFlight.current = true
    setBootstrapRetrying(true)
    runDetached(consumeBootstrap(bootstrap.retryStart, () => disposedRef.current).finally(() => {
      bootstrapRetryInFlight.current = false
      if (!disposedRef.current) setBootstrapRetrying(false)
    }), {
      scope: 'result',
      operation: 'result-ready-retry',
      onError: (error) => setCommandMessage(getErrorMessage(error, '无法连接结果会话，请重试。'))
    })
  }, [bootstrap, consumeBootstrap])

  useEffect(() => {
    const latest = getActionEventSnapshot()
    setTurns(reconcileTranscriptContent(
      latest.transcript ?? [], latest.content, latest.status === 'streaming',
      latest.requestId, latest.contentScalarCount
    ))
    askOriginalDefaulted.current = false
    pendingFollowUp.current = null
    pendingTranslation.current = null
    preparingRequestRef.current = null
    setPreparingRequestId(null)
  }, [sessionId])

  useEffect(() => {
    if (preparingRequestId === requestId || preparationFailure?.requestId === preparingRequestId) {
      preparingRequestRef.current = null
      setPreparingRequestId(null)
    }
  }, [preparingRequestId, preparationFailure, requestId])

  useEffect(() => {
    if (!preparationFailure) return
    setCommandMessage(preparationFailure.message)
    rollbackFailedTranslation(preparationFailure)
    const pending = pendingFollowUp.current
    const question = preparationFailure.question ?? pending?.question
    if (question) setFollowUpQuestion((current) => current || question)
    if (pending && (!pending.requestId || pending.requestId === preparationFailure.requestId)) {
      pendingFollowUp.current = null
    }
    if (isAsk) {
      const latest = getActionEventSnapshot()
      setTurns(reconcileTranscriptContent(
        latest.transcript ?? [], latest.content, latest.status === 'streaming',
        latest.requestId, latest.contentScalarCount
      ))
    }
  }, [isAsk, preparationFailure, rollbackFailedTranslation])

  useEffect(() => {
    if (!isAsk || askOriginalDefaulted.current) return
    askOriginalDefaulted.current = true
    setShowOriginal(true)
  }, [isAsk, sessionId])

  useEffect(() => {
    if (!requestId) return
    if (!getActionEventSnapshot().preparationFailure) setCommandMessage('')
    if (pendingTranslation.current && requestId !== pendingTranslation.current.previousRequestId) {
      pendingTranslation.current = null
    }
    setCopyComplete(false)
    if (!isAsk) setShowOriginal(false)
    if (copyResetTimer.current !== null) {
      window.clearTimeout(copyResetTimer.current)
      copyResetTimer.current = null
    }
  }, [isAsk, requestId])

  useEffect(() => {
    const pending = pendingFollowUp.current
    if (pending) reconcilePendingFollowUp(pending)
  }, [reconcilePendingFollowUp, requestId, status, preparationFailure])

  useEffect(() => {
    const onPageHide = (): void => {
      dismissNativeOverlays()
    }
    window.addEventListener('pagehide', onPageHide)
    return () => {
      window.removeEventListener('pagehide', onPageHide)
      dismissNativeOverlays()
      if (copyResetTimer.current !== null) window.clearTimeout(copyResetTimer.current)
      if (selectionFrame.current !== null) window.cancelAnimationFrame(selectionFrame.current)
    }
  }, [])

  useEffect(() => {
    if (!modelMenuOpen) return
    const close = (restoreFocus: boolean): void => {
      setModelMenuOpen(false)
      if (restoreFocus) {
        window.requestAnimationFrame(() => modelToggleRef.current?.focus())
      }
    }
    const focusFrame = window.requestAnimationFrame(() => {
      const menu = modelMenuRef.current
      const target =
        menu?.querySelector<HTMLButtonElement>('[role="option"][aria-selected="true"]:not(:disabled)')
        ?? menu?.querySelector<HTMLButtonElement>('[role="option"]:not(:disabled)')
      target?.focus()
    })
    const enabledOptions = (): HTMLButtonElement[] => {
      const menu = modelMenuRef.current
      return menu
        ? Array.from(menu.querySelectorAll<HTMLButtonElement>('[role="option"]:not(:disabled)'))
        : []
    }
    const onPointerDown = (event: PointerEvent): void => {
      const target = event.target
      if (target instanceof Node && modelSwitchRef.current?.contains(target)) return
      close(false)
    }
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopPropagation()
        close(true)
        return
      }
      const options = enabledOptions()
      if (options.length === 0) return
      const index = options.indexOf(document.activeElement as HTMLButtonElement)
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault()
        const delta = event.key === 'ArrowDown' ? 1 : -1
        const next = index < 0
          ? (delta > 0 ? 0 : options.length - 1)
          : (index + delta + options.length) % options.length
        options[next]?.focus()
        return
      }
      if (event.key === 'Home' || event.key === 'End') {
        event.preventDefault()
        options[event.key === 'Home' ? 0 : options.length - 1]?.focus()
        return
      }
      if ((event.key === 'Enter' || event.key === ' ') && index >= 0) {
        event.preventDefault()
        options[index]?.click()
        return
      }
      if (event.key === 'Tab') close(false)
    }
    document.addEventListener('pointerdown', onPointerDown, true)
    window.addEventListener('keydown', onKeyDown, true)
    return () => {
      window.cancelAnimationFrame(focusFrame)
      document.removeEventListener('pointerdown', onPointerDown, true)
      window.removeEventListener('keydown', onKeyDown, true)
    }
  }, [modelMenuOpen])

  useEffect(() => {
    if (modelMenuOpen && modelSwitchDisabled) setModelMenuOpen(false)
  }, [modelMenuOpen, modelSwitchDisabled])

  const keyboardState = useRef({ status, canCopy, cancel, closeWindow, retry, copy })
  keyboardState.current = { status, canCopy, cancel, closeWindow, retry, copy }
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent): void => {
      if (isEditableTarget(event.target) || event.metaKey || event.ctrlKey || event.altKey) return
      const current = keyboardState.current
      const key = event.key.toLowerCase()
      if (key === 'escape') {
        event.preventDefault()
        if (current.status === 'streaming') void current.cancel()
        else runDetached(current.closeWindow(), {
          scope: 'result',
          operation: 'close',
          onError: (error) => setCommandMessage(getErrorMessage(error, '无法关闭结果窗口'))
        })
        return
      }
      if (key === 'r' && current.status !== 'streaming') {
        event.preventDefault()
        void current.retry()
        return
      }
      if (key === 'c' && current.canCopy && !window.getSelection()?.toString()) {
        event.preventDefault()
        void current.copy()
      }
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [])

  const translationTargetSessionRef = useRef<string | null>(null)

  useEffect(() => {
    if (action?.kind !== 'translate' || !session?.selection || !settingsLoaded) {
      translationTargetSessionRef.current = null
      setTranslationTarget(null)
      return
    }
    // Derive the default route once per session. Re-running on every settings
    // identity change would clobber the user's temporary target-language pick.
    if (translationTargetSessionRef.current === session.sessionId) return
    translationTargetSessionRef.current = session.sessionId
    const detected = detectTranslationLanguage(session.selection.text)
    setTranslationTarget(defaultTranslationTarget(detected, settings.translate))
  }, [action?.kind, session?.sessionId, settingsLoaded])

  const openExternal = useCallback(async (url: string): Promise<void> => {
    setCommandMessage('')
    try {
      await window.textLens.openExternal(url)
    } catch (error) {
      setCommandMessage(getErrorMessage(error, '无法打开外部链接'))
    }
  }, [])

  const openSettings = async (): Promise<void> => {
    if (!window.textLens.openSettings) return
    setCommandMessage('')
    try {
      // Deliberately omit guidance so a hidden settings window resumes the
      // section and draft the user was already configuring.
      await window.textLens.openSettings()
    } catch (error) {
      setCommandMessage(getErrorMessage(error, '无法打开设置'))
    }
  }

  const togglePin = async (): Promise<void> => {
    const next = !pinned
    setCommandMessage('')
    setPinned(next)
    try {
      if (window.textLens.setResultPinned) {
        await window.textLens.setResultPinned(sessionId, next)
      }
    } catch (error) {
      setPinned(!next)
      setCommandMessage(getErrorMessage(error, '无法更改置顶状态'))
    }
  }

  const pointerEnter = (): void => {
    runDetached(window.textLens.setResultPointerInside?.(sessionId, true), {
      scope: 'result',
      operation: 'pointer-inside'
    })
  }

  const pointerLeave = (): void => {
    runDetached(window.textLens.setResultPointerInside?.(sessionId, false), {
      scope: 'result',
      operation: 'pointer-outside'
    })
  }

  const startWindowDrag = (event: ReactPointerEvent<HTMLElement>): void => {
    if (event.button !== 0 || !event.isPrimary || isInteractiveElement(event.target)) return
    event.preventDefault()
    runDetached(getCurrentWindow().startDragging(), {
      scope: 'result',
      operation: 'start-dragging'
    })
  }

  /**
   * In-result clicks never reach the native outside-click dismiss path on
   * macOS (own-process events are filtered). Mirror external “click outside
   * toolbar → hide” on pointer-down; pointer-up may re-present if text is
   * still selected in the result body.
   */
  const dismissResultSelectionToolbar = useCallback((): void => {
    if (!window.textLens.hideResultSelection) return
    runDetached(window.textLens.hideResultSelection(sessionId), {
      scope: 'result',
      operation: 'hide-result-selection'
    })
  }, [sessionId])

  const handleResultPointerDown = (event: ReactPointerEvent<HTMLElement>): void => {
    if (event.button !== 0 || !event.isPrimary) return
    dismissResultSelectionToolbar()
  }

  const showSelectionToolbar = (event: ReactPointerEvent<HTMLDivElement>): void => {
    if (event.button !== 0 || !event.isPrimary || isInteractiveElement(event.target)) return
    const cursor = { x: event.screenX, y: event.screenY }
    if (selectionFrame.current !== null) window.cancelAnimationFrame(selectionFrame.current)
    selectionFrame.current = window.requestAnimationFrame(() => {
      selectionFrame.current = null
      const text = selectedTextWithin(contentRef.current, window.getSelection())
      if (!text || countUnicodeScalars(text.trim()) < 2 || !window.textLens.showResultSelection) {
        // Click cleared the selection or landed outside selectable text —
        // ensure any result-sourced toolbar is gone (pointer-down may have
        // already done this; this covers edge cases / older backends).
        dismissResultSelectionToolbar()
        return
      }
      void window.textLens
        .showResultSelection(sessionId, text, cursor)
        .catch((error: unknown) => {
          setCommandMessage(getErrorMessage(error, '无法处理所选文字'))
        })
    })
  }

  const selection = session?.selection
  const translationRoute = useMemo(() => {
    if (action?.kind !== 'translate' || !selection || !settings) return null
    const detected = detectTranslationLanguage(selection.text)
    const defaultTarget = defaultTranslationTarget(detected, settings.translate)
    return { detected, target: translationTarget ?? defaultTarget }
  }, [action?.kind, selection, settings, translationTarget])
  const translationSwitching = status === 'streaming' || routeSwitching || retrying || followUpSubmitting || preparingRequestId !== null
  const changeTranslationTarget = (target: TranslationLanguage): void => {
    if (translationSwitching || retryInFlight.current || followUpInFlight.current || preparingRequestRef.current) return
    const previous = translationTarget
    pendingTranslation.current = { previous, previousRequestId: requestId, requestId: null }
    setRouteSwitching(true)
    setTranslationTarget(target)
    void retry({ targetLanguage: target }).then((accepted) => {
      if (!accepted) {
        setTranslationTarget(previous)
        pendingTranslation.current = null
      }
    }).finally(() => setRouteSwitching(false))
  }

  const style = {
    '--result-font-size': `${resultFontSize(settings)}px`,
    '--result-scrollbar-track-margin': `${contentOverflow.trackMarginPx}px`
  } as CSSProperties

  const startWindowResize = (
    event: ReactPointerEvent<HTMLDivElement>,
    direction: ResultResizeDirection
  ): void => {
    if (event.button !== 0 || !event.isPrimary) return
    event.preventDefault()
    event.stopPropagation()
    runDetached(getCurrentWindow().startResizeDragging(direction), {
      scope: 'result',
      operation: 'start-resize-dragging'
    })
  }

  return (
    <main
      className={`result-window ${windowsRenderer ? 'result-window--windows' : ''} ${
        showFollowUp && followUpExpanded ? 'result-window--followup-expanded' : ''
      }`}
      style={style}
      data-reveal-state={bootstrapMessage ? 'error' : revealCommitted ? 'visible' : 'preparing'}
      onPointerEnter={pointerEnter}
      onPointerLeave={pointerLeave}
      onPointerDown={handleResultPointerDown}
    >
      {windowsRenderer && RESULT_RESIZE_DIRECTIONS.map((direction) => (
        <div
          key={direction}
          className={`result-resize-handle result-resize-handle--${direction.toLowerCase()}`}
          data-no-drag
          data-resize-direction={direction}
          aria-hidden="true"
          onPointerDown={(event) => startWindowResize(event, direction)}
        />
      ))}
      <header className="result-header" onPointerDown={startWindowDrag}>
        <div className="result-heading">
          <span className="result-heading__icon">
            <ActionIcon name={action?.icon ?? 'sparkles'} size={16} />
          </span>
          <div className="result-heading__line">
            <h1>{action?.name ?? 'AI 结果'}</h1>
            {translationRoute ? (
              <div className="translation-route" aria-label="翻译方向" data-no-drag>
                {windowsRenderer ? (
                  <>
                    <span className="translation-route__code">
                      {TRANSLATION_LANGUAGE_CODES[translationRoute.detected]}
                    </span>
                    <ArrowRight className="translation-route__arrow" size={10} />
                    <span
                      className="translation-route__target"
                      data-disabled={translationSwitching ? 'true' : undefined}
                    >
                      <span aria-hidden="true">
                        {TRANSLATION_LANGUAGE_CODES[translationRoute.target]}
                      </span>
                      <ChevronDown size={9} aria-hidden="true" />
                      <select
                        className="translation-route__native-select"
                        aria-label="翻译目标语言"
                        value={translationRoute.target}
                        disabled={translationSwitching}
                        onChange={(event) => changeTranslationTarget(
                          event.target.value as TranslationLanguage
                        )}
                      >
                        {TRANSLATION_LANGUAGES.map((code) => (
                          <option key={code} value={code}>
                            {TRANSLATION_LANGUAGE_CODES[code]}
                          </option>
                        ))}
                      </select>
                    </span>
                  </>
                ) : (
                  <>
                    <span>{TRANSLATION_LANGUAGE_CODES[translationRoute.detected]}</span>
                    <ArrowRight size={11} />
                    <select
                      aria-label="翻译目标语言"
                      value={translationRoute.target}
                      disabled={translationSwitching}
                      onChange={(event) => changeTranslationTarget(
                        event.target.value as TranslationLanguage
                      )}
                    >
                      {TRANSLATION_LANGUAGES.map((code) => (
                        <option key={code} value={code}>
                          {TRANSLATION_LANGUAGE_CODES[code]}
                        </option>
                      ))}
                    </select>
                  </>
                )}
              </div>
            ) : null}
            {effectiveModelRoute && modelChoices.length > 0 ? (
              <div
                ref={modelSwitchRef}
                className={`result-model-switch ${modelMenuOpen ? 'result-model-switch--open' : ''}`}
                title={modelSummary}
                data-no-drag
              >
                <button
                  ref={modelToggleRef}
                  className="result-model-switch__trigger"
                  type="button"
                  aria-label="切换模型"
                  aria-haspopup="listbox"
                  aria-expanded={modelMenuOpen}
                  aria-controls="result-model-menu"
                  disabled={modelSwitchDisabled}
                  onClick={() => setModelMenuOpen((current) => !current)}
                >
                  <span>{selectedModel?.modelName ?? '未选择模型'}</span>
                  <ChevronDown size={13} aria-hidden="true" />
                </button>
                {modelMenuOpen && (
                  <div
                    ref={modelMenuRef}
                    id="result-model-menu"
                    className="result-model-menu"
                    role="listbox"
                    aria-label="可用模型"
                  >
                    {modelGroups.map((provider) => (
                      <div
                        key={provider.id}
                        className="result-model-menu__group"
                        role="group"
                        aria-label={provider.keyConfigured ? provider.name : `${provider.name}（未配置密钥）`}
                      >
                        <div className="result-model-menu__provider">
                          <span>{provider.name}</span>
                          {!provider.keyConfigured && <span>未配置密钥</span>}
                        </div>
                        <div className="result-model-menu__options">
                          {provider.choices.map((choice) => {
                            const selected = choice.value === modelRouteValue(effectiveModelRoute)
                            return (
                              <button
                                key={choice.value}
                                className={`result-model-menu__option ${selected ? 'is-selected' : ''}`}
                                type="button"
                                role="option"
                                aria-selected={selected}
                                tabIndex={selected ? 0 : -1}
                                disabled={!provider.keyConfigured || modelSwitchDisabled}
                                onClick={() => chooseModel(choice.value)}
                              >
                                <span>{choice.modelName}</span>
                                {selected && <Check size={13} aria-hidden="true" />}
                              </button>
                            )
                          })}
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            ) : (
              <span className="result-model-summary" title={modelSummary}>{modelSummary}</span>
            )}
          </div>
        </div>
        <div className="result-window-actions" data-no-drag>
          {window.textLens.openSettings && (
            <button
              className="icon-button result-settings"
              type="button"
              aria-label="打开设置"
              title="打开设置"
              onClick={() => void openSettings()}
            >
              <Settings2 size={15} aria-hidden="true" />
            </button>
          )}
          <button className={`icon-button result-pin ${pinned ? 'result-pin--active' : ''}`} type="button"
            aria-label={pinned ? '取消置顶' : '置顶结果窗口'} aria-pressed={pinned}
            title={pinned ? '取消置顶' : '置顶'} onClick={() => void togglePin()}>
            <Pin size={16} className={pinned ? 'result-pin__icon--active' : ''} />
          </button>
          <button className="icon-button result-close" type="button" aria-label="关闭结果窗口"
            title="关闭" onClick={() => runDetached(closeWindow(), {
              scope: 'result',
              operation: 'close',
              onError: (error) => setCommandMessage(getErrorMessage(error, '无法关闭结果窗口'))
            })}>
            <X size={15} aria-hidden="true" />
          </button>
        </div>
      </header>

      <div
        ref={contentRef}
        className={`result-content ${contentOverflow.isOverflowing ? 'result-content--scrollable' : ''}`}
        onScroll={handleScroll}
        onPointerUp={showSelectionToolbar}
      >
        <div ref={contentInnerRef} className="result-content__inner">
          {selection && (
            <section className="result-original">
              <button type="button" onClick={() => setShowOriginal((current) => !current)} aria-expanded={showOriginal}>
                <span>{showOriginal ? '隐藏原文' : '显示原文'}</span>
                <ChevronDown size={14} className={showOriginal ? 'is-expanded' : ''} />
              </button>
              {showOriginal && <div className="result-original__content">{selection.text}</div>}
            </section>
          )}

          {isAsk ? (
            <>
              <AskStreamBridge enabled={isAsk} onStream={handleAskStream} />
              {turns.length === 0 && status !== 'streaming' && status !== 'error' && (
                <div className="result-placeholder">
                  <span>已载入选中文本。请在下方输入问题。</span>
                </div>
              )}
              {turns.map((turn, index) => {
                const isLatestAssistant =
                  turn.role === 'assistant' && index === turns.length - 1
                return (
                  <div
                    key={turn.id}
                    className={`result-turn result-turn--${turn.role}`}
                    data-role={turn.role}
                  >
                    <div className="result-turn__label">{turn.role === 'user' ? '你' : 'AI'}</div>
                    {turn.role === 'user' ? (
                      <div className="result-turn__content">{turn.content}</div>
                    ) : (
                      <>
                        {/* CoT attaches to the latest assistant bubble (Cherry-style). */}
                        {isLatestAssistant && <ThinkingPanel />}
                        {turn.content ? (
                          <article className="markdown-body result-turn__content">
                            <AssistantTurnOutput
                              requestKey={`${sessionId}:${turn.id}`}
                              streaming={turn.streaming}
                              content={turn.content}
                              scalarCount={turn.scalarCount}
                              revealCommitted={revealCommitted}
                              onOpenExternal={openExternal}
                            />
                          </article>
                        ) : turn.streaming && status === 'streaming' && !hasThinking ? (
                          <div className="result-turn__waiting">
                            <LoaderCircle className="result-spin" size={16} />
                            <span>正在等待模型响应…</span>
                          </div>
                        ) : null}
                      </>
                    )}
                  </div>
                )
              })}
            </>
          ) : (
            <>
              <ThinkingPanel />
              {hasContent ? (
                <ResultStreamBody
                  revealCommitted={revealCommitted}
                  onOpenExternal={openExternal}
                />
              ) : status === 'error' ? null : hasThinking ? null : (
                <div
                  className={`result-placeholder ${
                    status === 'streaming' ? 'result-placeholder--waiting' : ''
                  }`}
                >
                  {status === 'streaming' ? (
                    <>
                      <span className="result-waiting-dots" aria-hidden="true">
                        <span />
                        <span />
                        <span />
                      </span>
                      <span className="result-placeholder__label">正在等待模型响应…</span>
                    </>
                  ) : (
                    <span className="result-placeholder__label">正在准备结果…</span>
                  )}
                </div>
              )}
            </>
          )}

          {status === 'error' && <div className="result-error" role="alert"><CircleAlert size={20} /><div>
            <strong>未能生成结果</strong><p>{errorMessage}</p></div></div>}
          {generationNotice && (
            <div className="result-inline-notice" role="status">{generationNotice}</div>
          )}
          {status === 'cancelled' && <div className="result-inline-notice" role="status">本次生成已取消。已生成的内容仍可复制。</div>}
        </div>
      </div>

      {bootstrapMessage && (
        <div className="result-command-error" role="alert">
          <span>{bootstrapMessage}</span>
          <button type="button" disabled={bootstrapRetrying} onClick={retryBootstrap}>
            重新连接结果会话
          </button>
        </div>
      )}
      {commandMessage && <div className="result-command-error" role="alert">{commandMessage}</div>}

      <footer className={`result-footer ${showFollowUp ? '' : 'result-footer--actions-only'}`}>
        {showFollowUp && <div className={`result-followup ${followUpExpanded ? 'result-followup--expanded' : ''}`}>
          <textarea
            aria-label="继续提问"
            placeholder={isAsk ? '输入问题，基于选中文本提问' : '输入继续提问'}
            title="Enter 发送，Shift+Enter 换行"
            rows={followUpExpanded ? 4 : 1}
            maxLength={20_000}
            value={followUpQuestion}
            disabled={followUpDisabled}
            onChange={(event) => setFollowUpQuestion(event.target.value)}
            onCompositionStart={() => { followUpComposing.current = true }}
            onCompositionEnd={() => { followUpComposing.current = false }}
            onKeyDown={(event) => {
              if (
                event.key === 'Enter' &&
                !event.shiftKey &&
                !event.nativeEvent.isComposing &&
                !followUpComposing.current
              ) {
                event.preventDefault()
                void submitFollowUp()
              }
            }}
          />
          <button
            className="result-followup-expand"
            type="button"
            aria-label={followUpExpanded ? '收起继续提问输入框' : '放大继续提问输入框'}
            aria-expanded={followUpExpanded}
            title={followUpExpanded ? '收起输入框' : '放大输入框'}
            onClick={() => setFollowUpExpanded((current) => !current)}
          >
            {followUpExpanded ? <Minimize2 size={13} /> : <Maximize2 size={13} />}
          </button>
        </div>}
        <div className="result-actions">
          {settings?.result.dismissMode === 'manual' && (
            <button className="result-footer-button" type="button" onClick={() => runDetached(closeWindow(), {
              scope: 'result',
              operation: 'close',
              onError: (error) => setCommandMessage(getErrorMessage(error, '无法关闭结果窗口'))
            })}>
              <X size={15} />关闭
            </button>
          )}
          {status === 'streaming' ? (
            <button
              className="result-footer-button result-footer-button--stop"
              type="button"
              onClick={() => void cancel()}
              title="停止本次生成"
            >
              <Square size={13} fill="currentColor" />
              停止
            </button>
          ) : (
            <button
              className="result-footer-button"
              type="button"
              disabled={retrying || followUpSubmitting || preparingRequestId !== null || !requestId || (status === 'error' && !retryable)}
              onClick={() => void retry()}
              title="使用相同选区重新生成"
            >
              <RefreshCw size={14} />
              重试
            </button>
          )}
          <button
            className={`result-footer-button ${copyComplete ? 'result-footer-button--done' : ''}`}
            type="button"
            disabled={!canCopy}
            onClick={() => void copy()}
            title={canCopy ? '复制结果到剪贴板' : '生成完成后可复制'}
          >
            {copyComplete ? <Check size={14} /> : <Copy size={14} />}
            {copyComplete ? '已复制' : '复制'}
          </button>
        </div>
      </footer>
    </main>
  )
}
