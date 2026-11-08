import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { StrictMode } from 'react'

import {
  countUnicodeScalars,
  DEFAULT_PUBLIC_SETTINGS,
  type PublicSettings,
  type ActionStreamEvent,
  type ResultSessionSnapshot,
  type WindowTextLensApi
} from '../../shared'
import type { ResultSessionBootstrap } from './resultSessionBootstrap'

const { startDragging, startResizeDragging } = vi.hoisted(() => ({
  startDragging: vi.fn(),
  startResizeDragging: vi.fn()
}))

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({ startDragging, startResizeDragging })
}))

function resultSnapshot(
  overrides: Partial<ResultSessionSnapshot> = {},
  includeRoute = true
): ResultSessionSnapshot {
  const content = overrides.content ?? '可选择的结果'
  return {
    sessionId: 'session-1',
    sessionGeneration: 1,
    requestId: crypto.randomUUID(),
    requestGeneration: 1,
    actionId: 'translate',
    ...(includeRoute
      ? { providerId: 'openai-compatible', modelId: 'model-1' }
      : {}),
    selection: {
      selectionId: 'selection-1',
      text: 'hello',
      sourceApp: { name: 'TextEdit', bundleId: 'com.apple.TextEdit' },
      anchor: { kind: 'cursor', x: 100, y: 120 },
      direction: 'unknown',
      isFullscreen: false
    },
    status: 'completed',
    content,
    thinkingContent: '',
    lastSequence: 3,
    lastContentSequence: 2,
    contentScalarCount: countUnicodeScalars(content),
    handshakeGeneration: 1,
    errorMessage: '',
    retryable: false,
    pinned: false,
    ...overrides
  }
}

const completedSession = resultSnapshot()
const askCompletedSession = resultSnapshot({ actionId: 'ask-ai' })

function settingsWithFontSize(fontSize = 18): PublicSettings {
  return {
    ...DEFAULT_PUBLIC_SETTINGS,
    result: { ...DEFAULT_PUBLIC_SETTINGS.result, fontSize },
    providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
      ...provider,
      keyConfigured: true,
      models: [{ id: 'model-1', name: '模型一' }]
    })),
    actions: DEFAULT_PUBLIC_SETTINGS.actions.map((action) =>
      'modelId' in action ? { ...action, modelId: 'model-1' } : action
    )
  } as PublicSettings
}

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((resolver) => {
    resolve = resolver
  })
  return { promise, resolve }
}

function askTranscript(pairs: Array<[string, string]>, streaming = false): NonNullable<ResultSessionSnapshot['transcript']> {
  return pairs.flatMap(([question, answer], index) => [
    { id: `session-1:turn:${index * 2}`, role: 'user' as const, content: question, streaming: false, scalarCount: countUnicodeScalars(question) },
    { id: `session-1:turn:${index * 2 + 1}`, role: 'assistant' as const, content: answer, streaming: streaming && index === pairs.length - 1, scalarCount: countUnicodeScalars(answer) }
  ])
}

function actionEventBridge() {
  let listener: ((event: ActionStreamEvent) => void) | null = null
  return {
    onActionEvent: vi.fn((next: (event: ActionStreamEvent) => void) => {
      listener = next
      return () => { listener = null }
    }),
    emit(event: ActionStreamEvent): void {
      if (!listener) throw new Error('Missing event listener')
      listener(event)
    }
  }
}

async function waitForAnimationFrame(): Promise<void> {
  await act(async () => {
    await new Promise<void>((resolve) => {
      window.requestAnimationFrame(() => resolve())
    })
  })
}

async function renderResult(
  snapshot: ResultSessionSnapshot = completedSession,
  settings: PublicSettings = settingsWithFontSize(),
  apiOverrides: Partial<WindowTextLensApi> = {},
  renderOptions: {
    strict?: boolean
    bootstrap?: Partial<ResultSessionBootstrap>
  } = {}
) {
  const showResultSelection = vi.fn().mockResolvedValue(undefined)
  const hideResultSelection = vi.fn().mockResolvedValue(undefined)
  const continueAction = vi.fn().mockResolvedValue({
    accepted: true,
    requestId: 'request-followup'
  })
  const retryAction = vi.fn().mockResolvedValue({ accepted: true })
  const prepareResultReveal = vi.fn().mockResolvedValue(undefined)
  const commitResultReveal = vi.fn().mockResolvedValue(undefined)
  const failResultReveal = vi.fn().mockResolvedValue(undefined)
  const ackResultReady = vi.fn().mockResolvedValue(true)
  let settingsListener: ((next: PublicSettings) => void) | null = null
  const api = {
    getSettings: vi.fn().mockResolvedValue(settings),
    beginResultReady: vi.fn().mockResolvedValue(snapshot),
    ackResultReady,
    prepareResultReveal,
    commitResultReveal,
    failResultReveal,
    setResultPinned: vi.fn().mockResolvedValue(true),
    setResultPointerInside: vi.fn().mockResolvedValue(undefined),
    showResultSelection,
    hideResultSelection,
    cancelAction: vi.fn().mockResolvedValue(undefined),
    retryAction,
    continueAction,
    copyText: vi.fn().mockResolvedValue(undefined),
    openExternal: vi.fn().mockResolvedValue(undefined),
    closeResult: vi.fn().mockResolvedValue(undefined),
    onSettingsChanged: vi.fn((listener: (next: PublicSettings) => void) => {
      settingsListener = listener
      return () => {
        if (settingsListener === listener) settingsListener = null
      }
    }),
    ...apiOverrides
  } as unknown as WindowTextLensApi
  Object.defineProperty(window, 'textLens', { configurable: true, value: api })
  window.history.replaceState({}, '', '/result/index.html?sessionId=session-1')

  const store = await import('./actionEventStore')
  store.hydrateActionEventStore(snapshot)
  const { ResultApp } = await import('./ResultApp')
  const bootstrap: ResultSessionBootstrap = {
    sessionId: 'session-1',
    start: vi.fn().mockResolvedValue(snapshot),
    retryStart: vi.fn().mockResolvedValue(snapshot),
    recover: vi.fn().mockResolvedValue(snapshot),
    reveal: vi.fn(async (run: () => Promise<void>) => run()),
    ...renderOptions.bootstrap
  }
  const view = render(
    renderOptions.strict ? <StrictMode><ResultApp bootstrap={bootstrap} /></StrictMode> :
      <ResultApp bootstrap={bootstrap} />
  )
  await screen.findByRole('heading', { name: settings.actions.find((action) => action.id === snapshot.actionId)?.name })
  if (snapshot.actionId === 'translate' && !renderOptions.bootstrap?.start) {
    // The heading renders from default settings, so it can appear before the
    // bootstrap snapshot lands; the route needs the session selection.
    await screen.findByLabelText('翻译方向')
  }
  return {
    ...view,
    api,
    showResultSelection,
    hideResultSelection,
    continueAction,
    retryAction,
    prepareResultReveal,
    commitResultReveal,
    failResultReveal,
    bootstrap,
    emitSettings: (next: PublicSettings) => settingsListener?.(next)
  }
}

beforeEach(() => {
  vi.resetModules()
  startDragging.mockReset()
  startDragging.mockResolvedValue(undefined)
  startResizeDragging.mockReset()
  startResizeDragging.mockResolvedValue(undefined)
})

afterEach(() => {
  Reflect.deleteProperty(window, 'textLens')
})

describe('ResultApp sessionId query', () => {
  async function renderWithoutSessionId(search = ''): Promise<{
    beginResultReady: ReturnType<typeof vi.fn>
    closeResult: ReturnType<typeof vi.fn>
  }> {
    const beginResultReady = vi.fn().mockResolvedValue(completedSession)
    const closeResult = vi.fn().mockResolvedValue(undefined)
    const api = {
      getSettings: vi.fn().mockResolvedValue(settingsWithFontSize()),
      beginResultReady,
      ackResultReady: vi.fn().mockResolvedValue(true),
      closeResult,
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi
    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    window.history.replaceState({}, '', `/result/index.html${search}`)

    const { ResultApp } = await import('./ResultApp')
    render(<ResultApp />)
    return { beginResultReady, closeResult }
  }

  it('shows a fatal error when sessionId is missing and does not begin a legacy session', async () => {
    const { beginResultReady, closeResult } = await renderWithoutSessionId()

    expect(await screen.findByRole('alert')).toHaveTextContent('结果会话无效')
    await waitFor(() => {
      expect(beginResultReady).not.toHaveBeenCalled()
    })
    expect(closeResult).not.toHaveBeenCalledWith('legacy')
    expect(closeResult).not.toHaveBeenCalled()
  })

  it('treats blank sessionId query values as missing', async () => {
    const { beginResultReady, closeResult } = await renderWithoutSessionId('?sessionId=%20%20')

    expect(await screen.findByRole('alert')).toHaveTextContent('结果会话无效')
    expect(beginResultReady).not.toHaveBeenCalled()
    expect(closeResult).not.toHaveBeenCalledWith('legacy')
  })
})

describe('ResultApp window interactions', () => {
  it('hydrates and reveals while getSettings remains pending', async () => {
    const settings = deferred<PublicSettings>()
    const { prepareResultReveal } = await renderResult(
      completedSession,
      settingsWithFontSize(),
      { getSettings: vi.fn(() => settings.promise) }
    )

    await waitFor(() => expect(prepareResultReveal).toHaveBeenCalledWith('session-1'))
    expect(screen.getByText(completedSession.content)).toBeInTheDocument()
    await act(async () => settings.resolve(settingsWithFontSize()))
  })

  it('uses defaults and still reveals when getSettings rejects', async () => {
    const { container, commitResultReveal } = await renderResult(
      completedSession,
      settingsWithFontSize(),
      { getSettings: vi.fn().mockRejectedValue(new Error('settings unavailable')) }
    )

    await waitFor(() => expect(commitResultReveal).toHaveBeenCalled())
    expect(container.querySelector('.result-window')).toHaveStyle(
      `--result-font-size: ${DEFAULT_PUBLIC_SETTINGS.result.fontSize}px`
    )
  })

  it('does not let an older settings read overwrite a newer settings event', async () => {
    const initial = deferred<PublicSettings>()
    const { container, emitSettings } = await renderResult(
      completedSession,
      settingsWithFontSize(12),
      { getSettings: vi.fn(() => initial.promise) }
    )

    act(() => emitSettings(settingsWithFontSize(20)))
    await act(async () => initial.resolve(settingsWithFontSize(12)))
    await waitFor(() => {
      expect(container.querySelector('.result-window')).toHaveStyle('--result-font-size: 20px')
    })
  })

  it('shows an explicit retry when bootstrap startup fails', async () => {
    const start = vi.fn().mockRejectedValue(new Error('session unavailable'))
    const retryStart = vi.fn().mockResolvedValue(completedSession)
    const { bootstrap } = await renderResult(
      completedSession,
      settingsWithFontSize(),
      {},
      { bootstrap: { start, retryStart } }
    )

    expect(await screen.findByRole('button', { name: '重新连接结果会话' })).toBeInTheDocument()
    retryStart.mockResolvedValueOnce(completedSession)
    fireEvent.click(screen.getByRole('button', { name: '重新连接结果会话' }))
    await waitFor(() => expect(retryStart).toHaveBeenCalledOnce())
    expect(bootstrap.start).toHaveBeenCalledOnce()
  })

  it('publishes native reveal completion and renders snapshot notices independently', async () => {
    const store = await import('./actionEventStore')
    const flush = vi.spyOn(store, 'flushPendingActionEvents')
    const notice = resultSnapshot({
      generationNotice: { code: 'THINKING_CONTROL_FALLBACK', message: '当前设置暂不支持关闭思考，已按默认设置继续。' }
    })
    const { commitResultReveal } = await renderResult(notice)

    await waitFor(() => expect(commitResultReveal).toHaveBeenCalled())
    await waitFor(() => expect(flush).toHaveBeenCalledWith('native-reveal'))
    expect(screen.getByRole('status')).toHaveTextContent('当前设置暂不支持关闭思考')
    flush.mockRestore()
  })

  it('accepts the Task 4 bootstrap prop while the legacy adapter remains active', async () => {
    const bootstrap: ResultSessionBootstrap = {
      sessionId: 'session-1',
      start: vi.fn().mockResolvedValue(completedSession),
      retryStart: vi.fn().mockResolvedValue(completedSession),
      recover: vi.fn().mockResolvedValue(completedSession),
      reveal: vi.fn().mockResolvedValue(undefined)
    }
    const { ResultApp } = await import('./ResultApp')
    const settings = settingsWithFontSize()
    const api = {
      getSettings: vi.fn().mockResolvedValue(settings),
      beginResultReady: vi.fn().mockResolvedValue(completedSession),
      ackResultReady: vi.fn().mockResolvedValue(true),
      prepareResultReveal: vi.fn().mockResolvedValue(undefined),
      commitResultReveal: vi.fn().mockResolvedValue(undefined),
      failResultReveal: vi.fn().mockResolvedValue(undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi
    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    window.history.replaceState({}, '', '/result/index.html?sessionId=session-1')

    render(<ResultApp bootstrap={bootstrap} />)
    expect(await screen.findByRole('heading')).toBeInTheDocument()
  })

  it('coalesces repeated retry shortcuts while one retry IPC is in flight', async () => {
    const pending = deferred<{ accepted: true }>()
    const { retryAction } = await renderResult()
    retryAction.mockReturnValue(pending.promise)

    fireEvent.keyDown(window, { key: 'r' })
    fireEvent.keyDown(window, { key: 'r' })

    await waitFor(() => expect(retryAction).toHaveBeenCalledTimes(1))
    await act(async () => pending.resolve({ accepted: true }))
  })

  it('exposes all eight native resize directions in Windows WebView2', async () => {
    const userAgent = vi
      .spyOn(window.navigator, 'userAgent', 'get')
      .mockReturnValue('Mozilla/5.0 (Windows NT 10.0; Win64; x64) WebView2')
    try {
      const { container } = await renderResult()
      const handles = [...container.querySelectorAll<HTMLElement>('[data-resize-direction]')]
      expect(handles.map((handle) => handle.dataset.resizeDirection)).toEqual([
        'NorthWest',
        'North',
        'NorthEast',
        'East',
        'SouthEast',
        'South',
        'SouthWest',
        'West'
      ])

      fireEvent.pointerDown(handles[4]!, { button: 0, isPrimary: true })
      expect(startResizeDragging).toHaveBeenCalledWith('SouthEast')
      fireEvent.pointerDown(handles[0]!, { button: 2, isPrimary: true })
      expect(startResizeDragging).toHaveBeenCalledTimes(1)
    } finally {
      userAgent.mockRestore()
    }
  })

  it('prepares the hydrated renderer and commits after one animation frame', async () => {
    const callbacks: FrameRequestCallback[] = []
    const scheduleFrame = vi.fn((callback: FrameRequestCallback) => {
      callbacks.push(callback)
      return callbacks.length
    })
    const { waitForResultRevealFrames } = await import('./ResultApp')
    const frames = waitForResultRevealFrames(scheduleFrame)
    expect(callbacks).toHaveLength(1)
    const firstFrame = callbacks.shift()
    expect(firstFrame).toBeTypeOf('function')
    firstFrame!(performance.now())
    await frames
    expect(scheduleFrame).toHaveBeenCalledTimes(1)
    expect(callbacks).toHaveLength(0)

    const { prepareResultReveal, commitResultReveal } = await renderResult()
    await waitFor(() => expect(commitResultReveal).toHaveBeenCalledWith('session-1'))
    expect(prepareResultReveal).toHaveBeenCalledWith('session-1')
    expect(
      prepareResultReveal.mock.invocationCallOrder[0]!
    ).toBeLessThan(commitResultReveal.mock.invocationCallOrder[0]!)
  })

  it('starts native dragging only from a non-interactive primary-button header area', async () => {
    const { container } = await renderResult()
    const header = container.querySelector<HTMLElement>('.result-header')!

    fireEvent.pointerDown(header, { button: 0, isPrimary: true })
    expect(startDragging).toHaveBeenCalledOnce()

    fireEvent.pointerDown(screen.getByRole('button', { name: '置顶结果窗口' }), {
      button: 0,
      isPrimary: true
    })
    fireEvent.pointerDown(screen.getByRole('combobox', { name: '翻译目标语言' }), {
      button: 0,
      isPrimary: true
    })
    fireEvent.pointerDown(screen.getByRole('button', { name: '切换模型' }), {
      button: 0,
      isPrimary: true
    })
    fireEvent.pointerDown(header, { button: 2, isPrimary: true })
    expect(startDragging).toHaveBeenCalledOnce()
  })

  it('consumes detached result-window rejections without exposing private error text', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const closeResult = vi.fn().mockRejectedValue(new Error('private selected text'))
    const setResultPointerInside = vi.fn().mockRejectedValue(
      new Error('result session no longer exists')
    )
    startDragging.mockRejectedValue(new Error('private drag detail'))
    const { container } = await renderResult(completedSession, settingsWithFontSize(), {
      closeResult,
      setResultPointerInside
    })

    const resultWindow = container.querySelector<HTMLElement>('.result-window')!
    fireEvent.pointerEnter(resultWindow)
    fireEvent.pointerLeave(resultWindow)
    fireEvent.pointerDown(container.querySelector<HTMLElement>('.result-header')!, {
      button: 0,
      isPrimary: true
    })
    fireEvent.click(screen.getByRole('button', { name: '关闭结果窗口' }))

    await waitFor(() => expect(closeResult).toHaveBeenCalledWith('session-1'))
    await act(async () => Promise.resolve())
    expect(screen.getByRole('alert')).toHaveTextContent('private selected text')
    expect(JSON.stringify(warn.mock.calls)).not.toContain('private drag detail')
    expect(JSON.stringify(warn.mock.calls)).not.toContain('result session no longer exists')
    warn.mockRestore()
  })

  it('shows aligned compact translation controls and pin/close actions on Windows', async () => {
    const userAgent = vi
      .spyOn(window.navigator, 'userAgent', 'get')
      .mockReturnValue('Mozilla/5.0 (Windows NT 10.0; Win64; x64) WebView2')
    try {
      const { api, container } = await renderResult()
      const resultWindow = container.querySelector('.result-window')
      expect(resultWindow).toHaveClass('result-window--windows')
      expect(container.querySelector('.translation-route__code')).toHaveTextContent('EN')
      expect(container.querySelector('.translation-route__target > span')).toHaveTextContent('CN')

      const actions = container.querySelector('.result-window-actions')!
      const buttons = [...actions.querySelectorAll('button')]
      expect(buttons.map((button) => button.getAttribute('aria-label'))).toEqual([
        '置顶结果窗口',
        '关闭结果窗口'
      ])
      expect(screen.queryByRole('button', { name: '关闭' })).not.toBeInTheDocument()

      fireEvent.pointerDown(screen.getByRole('button', { name: '关闭结果窗口' }), {
        button: 0,
        isPrimary: true
      })
      expect(startDragging).not.toHaveBeenCalled()

      fireEvent.click(screen.getByRole('button', { name: '关闭结果窗口' }))
      await waitFor(() => expect(api.closeResult).toHaveBeenCalledWith('session-1'))
    } finally {
      userAgent.mockRestore()
    }
  })

  it('keeps translation metadata in the compact header and applies result font size', async () => {
    const { api, container, continueAction } = await renderResult()
    const resultWindow = container.querySelector<HTMLElement>('.result-window')!
    const footer = container.querySelector<HTMLElement>('.result-footer')!
    const footerActions = container.querySelector<HTMLElement>('.result-actions')!

    expect(container.querySelector('.result-header .translation-route')).toBeInTheDocument()
    expect(container.querySelector('.result-header .result-status')).not.toBeInTheDocument()
    expect(container.querySelector('.result-header')).not.toHaveTextContent('已完成')
    expect(resultWindow.style.getPropertyValue('--result-font-size')).toBe('18px')
    expect(screen.getByRole('button', { name: '重试' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '复制' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '置顶结果窗口' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '关闭结果窗口' })).toBeInTheDocument()
    expect(container.querySelector('.result-count')).not.toBeInTheDocument()
    expect(footer).not.toHaveTextContent(/\d[\d,]*\s*字/u)
    expect(footer.querySelector('.result-followup')).toBeInTheDocument()
    expect(screen.getByRole('textbox', { name: '继续提问' })).toHaveAttribute(
      'placeholder',
      '输入继续提问'
    )
    expect(footer.firstElementChild).toHaveClass('result-followup')
    expect([...footerActions.children]).toHaveLength(2)
    for (const control of footerActions.children) {
      expect(control).toHaveClass('result-footer-button')
    }

    const followUp = screen.getByRole('textbox', { name: '继续提问' })
    fireEvent.change(followUp, { target: { value: '继续解释这一段' } })
    fireEvent.keyDown(followUp, { key: 'Enter' })
    await waitFor(() => {
      expect(continueAction).toHaveBeenCalledWith('session-1', '继续解释这一段')
    })

    // An accepted follow-up stays busy until the committed request arrives.
    const store = await import('./actionEventStore')
    act(() => store.hydrateActionEventStore(resultSnapshot({
      requestId: 'request-followup', requestGeneration: 2,
      content: '继续解释的答案', lastSequence: 6, lastContentSequence: 5
    })))
    await waitFor(() => expect(screen.getByRole('button', { name: '重试' })).not.toBeDisabled())

    fireEvent.click(screen.getByRole('button', { name: '重试' }))
    await waitFor(() => {
      expect(api.retryAction).toHaveBeenCalledWith('session-1', undefined)
    })

    fireEvent.change(screen.getByRole('combobox', { name: '翻译目标语言' }), {
      target: { value: 'en-US' }
    })
    await waitFor(() => {
      expect(api.retryAction).toHaveBeenLastCalledWith('session-1', {
        targetLanguage: 'en-US'
      })
    })

    fireEvent.click(screen.getByRole('button', { name: '关闭结果窗口' }))
    await waitFor(() => expect(api.closeResult).toHaveBeenCalledWith('session-1'))
  })

  it('keeps a temporary translation target when settings change', async () => {
    const { emitSettings } = await renderResult()
    const target = screen.getByRole('combobox', { name: '翻译目标语言' })
    expect(target).toHaveValue('zh-CN')

    fireEvent.change(target, { target: { value: 'fr-FR' } })
    await waitFor(() => expect(target).not.toBeDisabled())
    expect(target).toHaveValue('fr-FR')

    act(() => emitSettings(settingsWithFontSize(20)))

    expect(screen.getByRole('combobox', { name: '翻译目标语言' })).toHaveValue('fr-FR')
  })

  it('shows the close button only in manual dismiss mode', async () => {
    const manualSettings = settingsWithFontSize()
    manualSettings.result = { ...manualSettings.result, dismissMode: 'manual' }
    const { api, container } = await renderResult(completedSession, manualSettings)

    const close = screen.getByRole('button', { name: '关闭' })
    expect(container.querySelector('.result-actions')).toContainElement(close)
    fireEvent.click(close)

    await waitFor(() => expect(api.closeResult).toHaveBeenCalledWith('session-1'))
  })

  it('renders model output as Markdown inside the result window', async () => {
    const content = [
      '# Markdown 标题',
      '',
      '- 第一项',
      '- 第二项',
      '',
      '> 引用内容',
      '',
      '```ts',
      'const answer = 42',
      '```'
    ].join('\n')
    await renderResult({
      ...completedSession,
      content,
      contentScalarCount: countUnicodeScalars(content)
    })

    expect(
      await screen.findByRole('heading', { name: 'Markdown 标题', level: 1 }, { timeout: 5_000 })
    ).toBeInTheDocument()
    expect(screen.getAllByRole('listitem')).toHaveLength(2)
    expect(screen.getByText('引用内容').closest('blockquote')).toBeInTheDocument()
    expect(screen.getByText('const answer = 42').closest('pre')).toBeInTheDocument()
  })

  it('keeps Markdown plain during streaming and renders it after completion', async () => {
    await renderResult({
      ...completedSession,
      status: 'streaming',
      content: '# 尚未完成'
    })

    expect(screen.getByText('# 尚未完成')).toHaveClass('stream-plain-text')

    const store = await import('./actionEventStore')
    act(() => {
      store.hydrateActionEventStore({
        ...completedSession,
        content: '# 尚未完成'
      })
    })

    expect(await screen.findByRole('heading', { name: '尚未完成', level: 1 })).toBeInTheDocument()
    expect(screen.queryByText('# 尚未完成')).not.toBeInTheDocument()
  })

  it('copies the rendered result through the native bridge', async () => {
    const { api } = await renderResult()

    fireEvent.click(screen.getByRole('button', { name: '复制' }))

    await waitFor(() => {
      expect(api.copyText).toHaveBeenCalledWith('可选择的结果')
    })
    expect(await screen.findByRole('button', { name: '已复制' })).toBeInTheDocument()
  })

  it('expands the follow-up input and submits with Enter using the current session', async () => {
    const { continueAction, container } = await renderResult(askCompletedSession)
    const input = screen.getByRole('textbox', { name: '继续提问' })
    expect(input).toHaveAttribute('placeholder', '输入问题，基于选中文本提问')

    const expand = screen.getByRole('button', { name: '放大继续提问输入框' })
    fireEvent.click(expand)
    expect(screen.getByRole('button', { name: '收起继续提问输入框' })).toHaveAttribute(
      'aria-expanded',
      'true'
    )
    expect(container.querySelector('.result-window')).toHaveClass(
      'result-window--followup-expanded'
    )

    fireEvent.change(input, { target: { value: '  解释第二句话  ' } })
    fireEvent.keyDown(input, { key: 'Enter', shiftKey: true })
    expect(continueAction).not.toHaveBeenCalled()
    fireEvent.keyDown(input, { key: 'Enter' })

    await waitFor(() => {
      expect(continueAction).toHaveBeenCalledWith('session-1', '解释第二句话')
    })
    await waitFor(() => expect(input).toHaveValue(''))
  })

  it('keeps a follow-up question when the backend rejects it', async () => {
    const { continueAction } = await renderResult(askCompletedSession)
    continueAction.mockResolvedValueOnce({ accepted: false, message: '上下文过长' })
    const input = screen.getByRole('textbox', { name: '继续提问' })
    fireEvent.change(input, { target: { value: '继续说明' } })
    fireEvent.keyDown(input, { key: 'Enter' })

    expect(await screen.findByRole('alert')).toHaveTextContent('上下文过长')
    expect(input).toHaveValue('继续说明')
  })

  it('restores an accepted follow-up question if streaming later fails', async () => {
    const { continueAction, retryAction } = await renderResult(askCompletedSession)
    const input = screen.getByRole('textbox', { name: '继续提问' })
    fireEvent.change(input, { target: { value: '继续说明失败原因' } })
    fireEvent.keyDown(input, { key: 'Enter' })

    await waitFor(() => expect(continueAction).toHaveBeenCalledOnce())
    await waitFor(() => expect(input).toHaveValue(''))

    const store = await import('./actionEventStore')
    act(() => {
      store.hydrateActionEventStore({
        ...askCompletedSession,
        requestId: 'request-followup',
        status: 'error',
        content: '',
        contentScalarCount: 0,
        errorMessage: '网络中断',
        retryable: true
      })
    })

    await waitFor(() => expect(input).toHaveValue('继续说明失败原因'))

    let resolveRetry!: (value: { accepted: true; requestId: string }) => void
    const retryPromise = new Promise<{ accepted: true; requestId: string }>((resolve) => {
      resolveRetry = resolve
    })
    retryAction.mockReturnValueOnce(retryPromise)
    fireEvent.click(screen.getByRole('button', { name: '重试' }))
    await waitFor(() => expect(retryAction).toHaveBeenCalled())

    act(() => {
      store.hydrateActionEventStore({
        ...askCompletedSession,
        requestId: 'request-followup-retry',
        status: 'completed',
        content: '重试后的回答',
        errorMessage: '',
        retryable: false
      })
    })
    expect(input).toHaveValue('继续说明失败原因')

    await act(async () => {
      resolveRetry({ accepted: true, requestId: 'request-followup-retry' })
      await retryPromise
    })
    await waitFor(() => expect(input).toHaveValue(''))
  })

  it('enables ask input on empty completed session and renders multi-turn transcript', async () => {
    const askSettings = settingsWithFontSize()
    const askSession = resultSnapshot({
      actionId: 'ask-ai',
      status: 'completed',
      content: '',
      contentScalarCount: 0,
      selection: {
        selectionId: 'selection-ask',
        text: '被选中的上下文',
        sourceApp: { name: 'TextEdit', bundleId: 'com.apple.TextEdit' },
        anchor: { kind: 'cursor', x: 100, y: 120 },
        direction: 'unknown',
        isFullscreen: false
      }
    })
    const { continueAction, container } = await renderResult(askSession, askSettings)

    expect(screen.getByText('已载入选中文本。请在下方输入问题。')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '隐藏原文' })).toBeInTheDocument()
    expect(screen.getByText('被选中的上下文')).toBeInTheDocument()

    const input = screen.getByRole('textbox', { name: '继续提问' })
    expect(input).not.toBeDisabled()
    expect(input).toHaveAttribute('placeholder', '输入问题，基于选中文本提问')

    fireEvent.change(input, { target: { value: '这段话什么意思？' } })
    fireEvent.keyDown(input, { key: 'Enter' })

    await waitFor(() => {
      expect(continueAction).toHaveBeenCalledWith('session-1', '这段话什么意思？')
    })
    expect(screen.getByText('这段话什么意思？')).toBeInTheDocument()
    expect(container.querySelector('.result-turn--user')).toBeInTheDocument()
    expect(container.querySelector('.result-turn--assistant')).toBeInTheDocument()

    const store = await import('./actionEventStore')
    act(() => {
      store.hydrateActionEventStore({
        ...askSession,
        requestId: 'request-followup',
        status: 'streaming',
        content: '这是',
        contentScalarCount: countUnicodeScalars('这是')
      })
    })
    expect(screen.getByText('这是')).toHaveClass('stream-plain-text')

    act(() => {
      store.hydrateActionEventStore({
        ...askSession,
        requestId: 'request-followup',
        status: 'streaming',
        content: '这是持续输出',
        contentScalarCount: countUnicodeScalars('这是持续输出')
      })
    })
    expect(await screen.findByText('这是持续输出')).toHaveClass('stream-plain-text')

    act(() => {
      store.hydrateActionEventStore({
        ...askSession,
        requestId: 'request-followup',
        status: 'completed',
        content: '这是解释。',
        contentScalarCount: countUnicodeScalars('这是解释。')
      })
    })
    await waitFor(() => {
      expect(screen.getByText('这是解释。')).toBeInTheDocument()
    })
    expect(screen.queryByText('已载入选中文本。请在下方输入问题。')).not.toBeInTheDocument()
  })

  it('shows the selected provider and model in non-translation result headers', async () => {
    const summary = { ...completedSession, actionId: 'summary' }
    const { container } = await renderResult(summary)

    expect(container.querySelector('.result-header .translation-route')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: '切换模型' })).toHaveTextContent('模型一')
    expect(container.querySelector('.result-model-switch')).toHaveAttribute(
      'title',
      'OpenAI Compatible · 模型一'
    )
    expect(container.querySelector('.result-model-switch select')).not.toBeInTheDocument()
  })

  it('hydrates initial preparing snapshot without route and uses default model route', async () => {
    const initialPreparing = resultSnapshot({
      status: 'streaming',
      content: '',
      lastSequence: 0,
      lastContentSequence: 0,
      contentScalarCount: 0
    }, false)
    const { api, bootstrap } = await renderResult(initialPreparing)

    expect(screen.getByText('正在等待模型响应…')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '切换模型' })).toHaveTextContent('模型一')
    expect(bootstrap.start).toHaveBeenCalledOnce()
    expect(api.ackResultReady).not.toHaveBeenCalled()
    expect(api.failResultReveal).not.toHaveBeenCalled()
  })

  it('groups available models by provider and regenerates in the same session', async () => {
    const settings = settingsWithFontSize()
    settings.providers.push({
      id: 'provider-two',
      name: '备用服务商',
      enabled: true,
      baseUrl: 'http://localhost:11434/v1',
      keyConfigured: true,
      models: [
        { id: 'model-fast', name: '快速模型', thinkingLevels: [] },
        { id: 'model-deep', name: '深度模型', thinkingLevels: [] }
      ]
    })
    const { retryAction } = await renderResult(completedSession, settings)
    const selector = screen.getByRole('button', { name: '切换模型' })
    fireEvent.click(selector)

    expect(screen.getByRole('listbox', { name: '可用模型' })).toBeInTheDocument()
    expect(screen.getByRole('group', { name: 'OpenAI Compatible' })).toBeInTheDocument()
    expect(screen.getByRole('group', { name: '备用服务商' })).toBeInTheDocument()

    fireEvent.click(screen.getByRole('option', { name: '深度模型' }))

    await waitFor(() => {
      expect(retryAction).toHaveBeenLastCalledWith('session-1', {
        providerId: 'provider-two',
        modelId: 'model-deep'
      })
    })
    // The committed route changes with Started, not the accepted reservation.
    expect(selector).toHaveTextContent('模型一')
    const store = await import('./actionEventStore')
    act(() => store.hydrateActionEventStore(resultSnapshot({
      providerId: 'provider-two', modelId: 'model-deep', requestId: 'new-model',
      requestGeneration: 2, status: 'streaming', content: '', contentScalarCount: 0
    })))
    expect(selector).toHaveTextContent('深度模型')
  })

  it('navigates the model listbox with the keyboard and restores trigger focus', async () => {
    const settings = settingsWithFontSize()
    settings.providers.push({
      id: 'provider-two',
      name: '备用服务商',
      enabled: true,
      baseUrl: 'http://localhost:11434/v1',
      keyConfigured: true,
      models: [
        { id: 'model-fast', name: '快速模型', thinkingLevels: [] },
        { id: 'model-deep', name: '深度模型', thinkingLevels: [] }
      ]
    })
    const { retryAction } = await renderResult(completedSession, settings)
    const selector = screen.getByRole('button', { name: '切换模型' })
    fireEvent.click(selector)

    const listbox = screen.getByRole('listbox', { name: '可用模型' })
    const current = within(listbox).getByRole('option', { name: '模型一' })
    const fast = within(listbox).getByRole('option', { name: '快速模型' })
    const deep = within(listbox).getByRole('option', { name: '深度模型' })
    await waitFor(() => expect(current).toHaveFocus())
    expect(current).toHaveAttribute('tabindex', '0')
    expect(fast).toHaveAttribute('tabindex', '-1')

    fireEvent.keyDown(current, { key: 'ArrowDown' })
    expect(fast).toHaveFocus()
    fireEvent.keyDown(fast, { key: 'End' })
    expect(deep).toHaveFocus()
    fireEvent.keyDown(deep, { key: 'Enter' })

    await waitFor(() => {
      expect(retryAction).toHaveBeenLastCalledWith('session-1', {
        providerId: 'provider-two',
        modelId: 'model-deep'
      })
    })
    await waitFor(() => expect(selector).toHaveFocus())
  })

  it('copies the whole ask transcript once follow-up turns exist', async () => {
    const askSession = resultSnapshot({
      actionId: 'ask-ai',
      status: 'completed',
      content: '',
      contentScalarCount: 0
    })
    const { api } = await renderResult(askSession, settingsWithFontSize())
    const input = screen.getByRole('textbox', { name: '继续提问' })
    fireEvent.change(input, { target: { value: '补充解释' } })
    fireEvent.keyDown(input, { key: 'Enter' })
    await waitFor(() => expect(screen.getByText('补充解释')).toBeInTheDocument())

    const store = await import('./actionEventStore')
    act(() => {
      store.hydrateActionEventStore({
        ...askSession,
        requestId: 'request-followup',
        status: 'streaming',
        content: '补充',
        contentScalarCount: countUnicodeScalars('补充')
      })
    })
    act(() => {
      store.hydrateActionEventStore({
        ...askSession,
        requestId: 'request-followup',
        status: 'completed',
        content: '补充回答',
        contentScalarCount: countUnicodeScalars('补充回答')
      })
    })
    await waitFor(() => expect(screen.getByText('补充回答')).toBeInTheDocument())

    fireEvent.click(screen.getByRole('button', { name: '复制' }))
    await waitFor(() => {
      expect(api.copyText).toHaveBeenCalledWith('你：\n补充解释\n\nAI：\n补充回答')
    })
  })

  it('commits and copies an Ask answer when Started, Delta and Completed arrive in one batch', async () => {
    const initial = resultSnapshot({ actionId: 'ask-ai', content: '', status: 'idle', contentScalarCount: 0, lastSequence: 0, lastContentSequence: 0, transcript: [] })
    const bridge = actionEventBridge()
    const { api } = await renderResult(initial, settingsWithFontSize(), { onActionEvent: bridge.onActionEvent })
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    const input = screen.getByRole('textbox', { name: '继续提问' })
    fireEvent.change(input, { target: { value: '快速问题' } })
    fireEvent.keyDown(input, { key: 'Enter' })
    await waitFor(() => expect(api.continueAction).toHaveBeenCalledOnce())
    const identity = { sessionId: 'session-1', sessionGeneration: 1, requestGeneration: 2, requestId: 'request-followup', actionId: 'ask-ai' }
    act(() => {
      bridge.emit({ ...identity, type: 'started', sequence: 1, question: '快速问题', transcript: askTranscript([['快速问题', '']], true) })
      bridge.emit({ ...identity, type: 'delta', sequence: 2, delta: '瞬间回答😀' })
      bridge.emit({ ...identity, type: 'completed', sequence: 3, lastContentSequence: 2, contentScalarCount: countUnicodeScalars('瞬间回答😀') })
    })
    await screen.findByText('瞬间回答😀')
    expect(screen.queryByText('正在等待模型响应…')).not.toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '复制' }))
    await waitFor(() => expect(api.copyText).toHaveBeenCalledWith('你：\n快速问题\n\nAI：\n瞬间回答😀'))
    stop()
  })

  it('restores repeated questions and their separate answers from a completed Ask snapshot', async () => {
    const { api } = await renderResult(resultSnapshot({
      actionId: 'ask-ai', content: '第二轮答案', transcript: askTranscript([['同一个问题', '第一轮答案'], ['同一个问题', '第二轮答案']])
    }))
    await screen.findByText('第一轮答案')
    expect(screen.getAllByText('同一个问题')).toHaveLength(2)
    expect(screen.getByText('第二轮答案')).toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '复制' }))
    await waitFor(() => expect(api.copyText).toHaveBeenCalledWith('你：\n同一个问题\n\nAI：\n第一轮答案\n\n你：\n同一个问题\n\nAI：\n第二轮答案'))
  })

  it.each(['error', 'cancelled'] as const)('keeps prior Ask answers copyable after a %s before the first token', async (status) => {
    const { api } = await renderResult(resultSnapshot({
      actionId: 'ask-ai', status, content: '', contentScalarCount: 0,
      errorMessage: status === 'error' ? '网络断开' : '', retryable: true,
      transcript: askTranscript([['问题A', '已有答案'], ['问题B', '']])
    }))
    await screen.findByText('已有答案')
    expect(screen.queryByText('正在等待模型响应…')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: '复制' })).not.toBeDisabled()
    fireEvent.keyDown(window, { key: 'c' })
    await waitFor(() => expect(api.copyText).toHaveBeenCalledWith('你：\n问题A\n\nAI：\n已有答案\n\n你：\n问题B'))
  })

  it('restores preparation failure feedback and the unsubmitted question on hydration', async () => {
    await renderResult(resultSnapshot({
      actionId: 'ask-ai', content: '已有答案', transcript: askTranscript([['问题A', '已有答案']]),
      preparationFailure: { requestId: 'request-failed', requestGeneration: 2, code: 'CONFIGURATION', message: '服务商已停用', retryable: false, question: '尚未提交的问题B' }
    }))
    expect(await screen.findByRole('alert')).toHaveTextContent('服务商已停用')
    expect(screen.getByRole('textbox', { name: '继续提问' })).toHaveValue('尚未提交的问题B')
    expect(screen.queryByText('正在等待模型响应…')).not.toBeInTheDocument()
  })

  it('retires a failed preparation even when its event arrives before accepted, so retry cannot erase the restored question', async () => {
    const initial = resultSnapshot({ actionId: 'ask-ai', content: '已有答案', transcript: askTranscript([['问题A', '已有答案']]) })
    const accepted = deferred<{ accepted: true; requestId: string }>()
    const bridge = actionEventBridge()
    const { api } = await renderResult(initial, settingsWithFontSize(), {
      onActionEvent: bridge.onActionEvent,
      continueAction: vi.fn().mockReturnValue(accepted.promise),
      retryAction: vi.fn().mockResolvedValue({ accepted: true, requestId: 'request-retry-a' })
    })
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    const input = screen.getByRole('textbox', { name: '继续提问' })
    fireEvent.change(input, { target: { value: '问题B' } })
    fireEvent.keyDown(input, { key: 'Enter' })
    act(() => bridge.emit({ sessionId: 'session-1', sessionGeneration: 1, requestGeneration: 2, requestId: 'request-failed-b', actionId: 'ask-ai', type: 'preparationFailed', sequence: 4, code: 'CONFIGURATION', message: '服务商已停用', retryable: false, question: '问题B' }))
    await act(async () => accepted.resolve({ accepted: true, requestId: 'request-failed-b' }))
    await waitFor(() => expect(input).toHaveValue('问题B'))
    expect(screen.getByRole('alert')).toHaveTextContent('服务商已停用')
    fireEvent.click(screen.getByRole('button', { name: '重试' }))
    await waitFor(() => expect(api.retryAction).toHaveBeenCalledOnce())
    const identity = { sessionId: 'session-1', sessionGeneration: 1, requestGeneration: 3, requestId: 'request-retry-a', actionId: 'ask-ai' }
    act(() => {
      bridge.emit({ ...identity, type: 'started', sequence: 5, question: '问题A', transcript: askTranscript([['问题A', '']], true) })
      bridge.emit({ ...identity, type: 'delta', sequence: 6, delta: '重试A的答案' })
      bridge.emit({ ...identity, type: 'completed', sequence: 7, lastContentSequence: 6, contentScalarCount: countUnicodeScalars('重试A的答案') })
    })
    await screen.findByText('重试A的答案')
    expect(input).toHaveValue('问题B')
    stop()
  })

  it('blocks another dispatch while a fast Continue terminal is still awaiting its invoke reply', async () => {
    const initial = resultSnapshot({ actionId: 'ask-ai', content: '', contentScalarCount: 0, lastSequence: 0, lastContentSequence: 0, transcript: [] })
    const accepted = deferred<{ accepted: true; requestId: string }>()
    const bridge = actionEventBridge()
    const { api } = await renderResult(initial, settingsWithFontSize(), { onActionEvent: bridge.onActionEvent, continueAction: vi.fn().mockReturnValue(accepted.promise) })
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    const input = screen.getByRole('textbox', { name: '继续提问' })
    fireEvent.change(input, { target: { value: '问题' } })
    fireEvent.keyDown(input, { key: 'Enter' })
    const identity = { sessionId: 'session-1', sessionGeneration: 1, requestGeneration: 2, requestId: 'fast-request', actionId: 'ask-ai' }
    act(() => {
      bridge.emit({ ...identity, type: 'started', sequence: 1, transcript: askTranscript([['问题', '']], true) })
      bridge.emit({ ...identity, type: 'delta', sequence: 2, delta: '回答' })
      bridge.emit({ ...identity, type: 'completed', sequence: 3, lastContentSequence: 2, contentScalarCount: 2 })
    })
    await screen.findByText('回答')
    expect(screen.getByRole('button', { name: '重试' })).toBeDisabled()
    fireEvent.keyDown(window, { key: 'r' })
    expect(api.retryAction).not.toHaveBeenCalled()
    await act(async () => accepted.resolve({ accepted: true, requestId: 'fast-request' }))
    await waitFor(() => expect(input).not.toBeDisabled())
    expect(screen.getByRole('button', { name: '重试' })).not.toBeDisabled()
    stop()
  })

  it('rolls back a rejected translation preparation and keeps its old answer', async () => {
    const bridge = actionEventBridge()
    const { api } = await renderResult(completedSession, settingsWithFontSize(), { onActionEvent: bridge.onActionEvent, retryAction: vi.fn().mockResolvedValue({ accepted: true, requestId: 'request-language' }) })
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    const target = screen.getByRole('combobox', { name: '翻译目标语言' })
    const previous = (target as HTMLSelectElement).value
    fireEvent.change(target, { target: { value: 'fr-FR' } })
    await waitFor(() => expect(api.retryAction).toHaveBeenCalledOnce())
    expect(target).toBeDisabled()
    act(() => bridge.emit({ sessionId: 'session-1', sessionGeneration: 1, requestGeneration: 2, requestId: 'request-language', actionId: 'translate', type: 'preparationFailed', sequence: 4, code: 'CONFIGURATION', message: '模型已删除', retryable: false }))
    await waitFor(() => expect(target).toHaveValue(previous))
    expect(screen.getByText('可选择的结果')).toBeInTheDocument()
    expect(screen.getByRole('alert')).toHaveTextContent('模型已删除')
    stop()
  })

  it('keeps the committed model and releases its busy state when model preparation fails before the invoke reply', async () => {
    const settings = settingsWithFontSize()
    settings.providers[0]!.models.push({ id: 'model-2', name: '模型二', thinkingLevels: [] })
    const accepted = deferred<{ accepted: true; requestId: string }>()
    const bridge = actionEventBridge()
    await renderResult(completedSession, settings, {
      onActionEvent: bridge.onActionEvent,
      retryAction: vi.fn().mockReturnValue(accepted.promise)
    })
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    const model = screen.getByRole('button', { name: '切换模型' })
    fireEvent.click(model)
    fireEvent.click(screen.getByRole('option', { name: '模型二' }))
    expect(model).toHaveTextContent('模型一')
    act(() => bridge.emit({
      sessionId: 'session-1', sessionGeneration: 1, requestGeneration: 2,
      requestId: 'request-model-2', actionId: 'translate', type: 'preparationFailed',
      sequence: 4, code: 'PREPARATION_FAILED', message: '服务商已停用', retryable: false
    }))
    await act(async () => accepted.resolve({ accepted: true, requestId: 'request-model-2' }))
    await waitFor(() => expect(model).not.toBeDisabled())
    expect(model).toHaveTextContent('模型一')
    expect(screen.getByRole('alert')).toHaveTextContent('服务商已停用')
    expect(screen.getByText('可选择的结果')).toBeInTheDocument()
    stop()
  })

  it('shows the app toolbar for a real text selection inside result content', async () => {
    const { container, showResultSelection } = await renderResult()
    expect(container.querySelector('.stream-plain-text')).toBeInTheDocument()

    const selected = screen.getByText('可选择的结果')
    const range = document.createRange()
    range.selectNodeContents(selected)
    const selection = window.getSelection()!
    selection.removeAllRanges()
    selection.addRange(range)

    fireEvent.pointerUp(selected, {
      button: 0,
      isPrimary: true,
      screenX: 320,
      screenY: 240
    })

    await waitFor(() => {
      expect(showResultSelection).toHaveBeenCalledWith(
        'session-1',
        '可选择的结果',
        { x: 320, y: 240 }
      )
    })

    showResultSelection.mockClear()
    const originalToggle = screen.getByRole('button', { name: '显示原文' })
    fireEvent.pointerUp(originalToggle, { button: 0, isPrimary: true, screenX: 10, screenY: 10 })
    await waitForAnimationFrame()
    expect(showResultSelection).not.toHaveBeenCalled()

    const outsideRange = document.createRange()
    outsideRange.selectNodeContents(screen.getByRole('heading', { name: '翻译' }))
    selection.removeAllRanges()
    selection.addRange(outsideRange)
    fireEvent.pointerUp(container.querySelector('.result-content')!, {
      button: 0,
      isPrimary: true,
      screenX: 20,
      screenY: 30
    })
    await waitForAnimationFrame()
    expect(showResultSelection).not.toHaveBeenCalled()
    // The deferred Markdown chunk may replace its initial plain-text node, so
    // assert against the live result container instead of the stale span.
    expect(container.querySelector('.result-content')).toHaveTextContent(completedSession.content)
  })

  it('ignores single-character selections inside result content', async () => {
    const { container, hideResultSelection, showResultSelection } = await renderResult()

    const selected = screen.getByText('可选择的结果')
    const range = document.createRange()
    range.setStart(selected.firstChild!, 0)
    range.setEnd(selected.firstChild!, 1)
    const selection = window.getSelection()!
    selection.removeAllRanges()
    selection.addRange(range)

    fireEvent.pointerUp(selected, {
      button: 0,
      isPrimary: true,
      screenX: 320,
      screenY: 240
    })
    await waitForAnimationFrame()

    expect(showResultSelection).not.toHaveBeenCalled()
    expect(hideResultSelection).toHaveBeenCalledWith('session-1')
    expect(container.querySelector('.result-content')).toHaveTextContent(completedSession.content)
  })

  it('dismisses the in-result selection toolbar when clicking elsewhere inside the result window', async () => {
    const { container, hideResultSelection, showResultSelection } = await renderResult()

    // Any primary pointer-down inside the result chrome should dismiss a
    // toolbar that was opened from a prior in-result selection (macOS does not
    // deliver own-process outside-click dismiss to the native hook).
    fireEvent.pointerDown(container.querySelector('.result-header')!, {
      button: 0,
      isPrimary: true
    })
    expect(hideResultSelection).toHaveBeenCalledWith('session-1')

    hideResultSelection.mockClear()
    fireEvent.pointerDown(container.querySelector('.result-content')!, {
      button: 0,
      isPrimary: true
    })
    expect(hideResultSelection).toHaveBeenCalledWith('session-1')

    // Non-primary / right-click must not dismiss.
    hideResultSelection.mockClear()
    fireEvent.pointerDown(container.querySelector('.result-content')!, {
      button: 2,
      isPrimary: false
    })
    expect(hideResultSelection).not.toHaveBeenCalled()

    // pointer-up with no selectable text also dismisses (covers cleared selection).
    hideResultSelection.mockClear()
    window.getSelection()?.removeAllRanges()
    fireEvent.pointerUp(container.querySelector('.result-content')!, {
      button: 0,
      isPrimary: true,
      screenX: 40,
      screenY: 50
    })
    await waitForAnimationFrame()
    expect(hideResultSelection).toHaveBeenCalledWith('session-1')
    expect(showResultSelection).not.toHaveBeenCalled()
  })

  it('auto-expands thinking while reasoning and collapses on manual toggle', async () => {
    await renderResult(
      resultSnapshot({
        status: 'streaming',
        content: '',
        contentScalarCount: 0,
        thinkingContent: '先拆解题意，再给出解释。'
      })
    )

    expect(screen.getByTestId('result-thinking')).toBeInTheDocument()
    // Badge-only chrome (no「思考过程」); live state still auto-expands body.
    expect(screen.queryByText('思考过程')).not.toBeInTheDocument()
    expect(screen.getByText('思考')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /思考中/ })).toHaveAttribute(
      'aria-expanded',
      'true'
    )
    const thinkingBody = screen.getByText('先拆解题意，再给出解释。')
    expect(thinkingBody).toBeInTheDocument()
    // Must not share .stream-plain-text (full body font-size) or the shrink CSS loses.
    expect(thinkingBody).toHaveClass('result-thinking__body')
    expect(thinkingBody).not.toHaveClass('stream-plain-text')

    fireEvent.click(screen.getByRole('button', { name: /思考中/ }))
    expect(screen.getByRole('button', { name: /思考中/ })).toHaveAttribute(
      'aria-expanded',
      'false'
    )
    expect(screen.queryByText('先拆解题意，再给出解释。')).not.toBeInTheDocument()
  })

  it('shows a single-line waiting label without the streaming-hint subtitle', async () => {
    await renderResult(
      resultSnapshot({
        status: 'streaming',
        content: '',
        contentScalarCount: 0
      })
    )

    expect(screen.getByText('正在等待模型响应…')).toBeInTheDocument()
    expect(screen.queryByText(/收到首字后会/)).not.toBeInTheDocument()
  })

  it('uses stop while streaming and keeps error/loading states operable', async () => {
    const streaming = {
      ...completedSession,
      status: 'streaming' as const,
      content: '',
      contentScalarCount: 0
    }
    const first = await renderResult(streaming)
    expect(screen.getByRole('button', { name: '停止' })).toBeInTheDocument()
    expect(screen.getByText('正在等待模型响应…')).toBeInTheDocument()
    expect(screen.queryByRole('textbox', { name: '继续提问' })).not.toBeInTheDocument()
    first.unmount()

    vi.resetModules()
    const failed = {
      ...completedSession,
      status: 'error' as const,
      content: '',
      contentScalarCount: 0,
      errorMessage: '连接失败',
      retryable: true
    }
    await renderResult(failed)
    expect(screen.getByRole('alert')).toHaveTextContent('连接失败')
    expect(screen.getByRole('button', { name: '重试' })).toBeEnabled()
    expect(screen.queryByRole('button', { name: '关闭' })).not.toBeInTheDocument()
  })
})
