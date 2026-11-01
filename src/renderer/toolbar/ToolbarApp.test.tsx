/// <reference types="node" />

import { readFileSync } from 'node:fs'
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'

import {
  DEFAULT_PUBLIC_SETTINGS,
  type PublicSettings,
  type SelectionPayload,
  type WindowTextLensApi
} from '../../shared'
import { ToolbarApp } from './ToolbarApp'

const toolbarCss = readFileSync('src/renderer/toolbar/toolbar.css', 'utf8')

const selection: SelectionPayload = {
  selectionId: 'selection-1',
  text: '需要复制的文字',
  sourceApp: { name: 'TextEdit', bundleId: 'com.apple.TextEdit' },
  anchor: { kind: 'cursor', x: 200, y: 120 },
  direction: 'unknown',
  isFullscreen: false
}

function deferred<T>(): {
  promise: Promise<T>
  resolve: (value: T) => void
} {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((resolver) => {
    resolve = resolver
  })
  return { promise, resolve }
}

describe('ToolbarApp', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'ResizeObserver',
      class {
        observe(): void {}
        disconnect(): void {}
      }
    )
  })

  afterAll(() => {
    vi.unstubAllGlobals()
  })

  afterEach(() => {
    vi.useRealTimers()
    // jsdom does not currently implement layout hit-testing. Individual
    // native-pointer tests install the narrow mock they need.
    Reflect.deleteProperty(document, 'elementFromPoint')
  })

  it('keeps the toolbar visible and shows a temporary success icon after copy succeeds', async () => {
    const hideToolbar = vi.fn().mockResolvedValue(undefined)
    const runAction = vi.fn().mockResolvedValue({ accepted: true })
    const presentToolbar = vi.fn().mockResolvedValue(true)
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar,
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', {
      configurable: true,
      value: api
    })
    const view = render(<ToolbarApp />)
    const copy = await screen.findByRole('button', { name: '复制' })
    await waitFor(() => expect(api.getCurrentSelection).toHaveBeenCalled())
    await waitFor(() => expect(presentToolbar).toHaveBeenCalled())
    expect(api.reportToolbarSize).not.toHaveBeenCalled()
    const presentationsBeforeAction = presentToolbar.mock.calls.length
    fireEvent.click(copy)

    await waitFor(() => {
      expect(runAction).toHaveBeenCalledWith('copy', undefined, 'selection-1')
    })
    expect(hideToolbar).not.toHaveBeenCalled()
    await waitFor(() => expect(copy.querySelector('.toolbar-copy-success')).not.toBeNull())
    await act(async () => Promise.resolve())
    expect(presentToolbar).toHaveBeenCalledTimes(presentationsBeforeAction)

    fireEvent.click(copy)
    await waitFor(() => expect(runAction).toHaveBeenCalledTimes(2))
    expect(hideToolbar).not.toHaveBeenCalled()
    await waitFor(() => expect(copy.querySelector('.toolbar-copy-success')).toBeNull(), {
      timeout: 2_500
    })

    view.unmount()
  })

  it('hides after a single outside dismiss following copy success', async () => {
    const hideToolbar = vi.fn().mockResolvedValue(undefined)
    const runAction = vi.fn().mockResolvedValue({ accepted: true })
    const presentToolbar = vi.fn().mockResolvedValue(true)
    let dismissListener:
      | ((payload: { selectionId?: string; reason: string }) => void)
      | undefined
    const unsubscribeDismiss = vi.fn()
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar,
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined),
      onToolbarDismissed: vi.fn(
        (listener: (payload: { selectionId?: string; reason: string }) => void) => {
          dismissListener = listener
          return unsubscribeDismiss
        }
      )
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', {
      configurable: true,
      value: api
    })
    const view = render(<ToolbarApp />)
    const copy = await screen.findByRole('button', { name: '复制' })
    await waitFor(() => expect(presentToolbar).toHaveBeenCalled())
    fireEvent.click(copy)

    await waitFor(() => {
      expect(runAction).toHaveBeenCalledWith('copy', undefined, 'selection-1')
    })
    await waitFor(() => expect(copy.querySelector('.toolbar-copy-success')).not.toBeNull())
    expect(hideToolbar).not.toHaveBeenCalled()

    act(() => {
      dismissListener?.({ selectionId: 'selection-1', reason: 'mouseDown' })
    })

    // Native already hid the window; React must drop selection + copy success
    // so a stale shell cannot re-present. Action buttons still render from
    // settings, but the toolbar enters the no-selection empty state.
    await waitFor(() => {
      expect(screen.getByRole('toolbar', { name: '划词动作' })).toBeInTheDocument()
    })
    expect(document.querySelector('.toolbar-copy-success')).toBeNull()
    expect(hideToolbar).not.toHaveBeenCalled()

    view.unmount()
    expect(unsubscribeDismiss).toHaveBeenCalledOnce()
  })

  it('keeps the original copy icon and toolbar when copy is rejected', async () => {
    const hideToolbar = vi.fn().mockResolvedValue(undefined)
    const runAction = vi.fn().mockResolvedValue({
      accepted: false,
      message: '无法写入系统剪贴板'
    })
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar,
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    const copy = await screen.findByRole('button', { name: '复制' })
    fireEvent.click(copy)

    await waitFor(() => expect(runAction).toHaveBeenCalledWith('copy', undefined, 'selection-1'))
    expect(screen.getByRole('alert')).toHaveTextContent('无法写入系统剪贴板')
    expect(copy.querySelector('.toolbar-copy-success')).toBeNull()
    expect(hideToolbar).not.toHaveBeenCalled()
  })

  it('uses the saved search engine without adding controls to the toolbar', async () => {
    const runAction = vi.fn().mockResolvedValue({ accepted: false, message: 'search unavailable' })
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    const toolbar = await screen.findByRole('toolbar')
    const primary = toolbar.querySelector<HTMLButtonElement>('[data-toolbar-control="action:search"]')
    expect(primary).not.toBeNull()
    expect(toolbar.querySelector('[data-toolbar-control="search-engines"]')).toBeNull()
    expect(document.querySelector('[data-toolbar-control^="search-engine:"]')).toBeNull()

    fireEvent.click(primary!)
    await waitFor(() => {
      expect(runAction).toHaveBeenLastCalledWith('search', undefined, 'selection-1')
    })
  })

  it('clears copy success when a newer selection replaces the current one', async () => {
    let selectionListener: ((payload: SelectionPayload) => void) | undefined
    const runAction = vi.fn().mockResolvedValue({ accepted: true })
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn((listener: (payload: SelectionPayload) => void) => {
        selectionListener = listener
        return () => undefined
      }),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    const copy = await screen.findByRole('button', { name: '复制' })
    fireEvent.click(copy)
    await waitFor(() => expect(copy.querySelector('.toolbar-copy-success')).not.toBeNull())

    act(() => {
      selectionListener?.({
        ...selection,
        selectionId: 'selection-2',
        text: '新的选中文本'
      })
    })

    await waitFor(() => expect(copy.querySelector('.toolbar-copy-success')).toBeNull())
  })

  it('presents only after selection, settings, and the committed layout are ready', async () => {
    const settings = deferred<PublicSettings>()
    const presentToolbar = vi.fn().mockResolvedValue(true)
    const layout = vi
      .spyOn(HTMLElement.prototype, 'getBoundingClientRect')
      .mockReturnValue(new DOMRect(0, 0, 212, 38))
    const api = {
      getSettings: vi.fn().mockReturnValue(settings.promise),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    await waitFor(() => expect(api.getCurrentSelection).toHaveBeenCalled())
    expect(presentToolbar).not.toHaveBeenCalled()
    layout.mockClear()

    act(() => settings.resolve(DEFAULT_PUBLIC_SETTINGS))

    await waitFor(() => {
      expect(presentToolbar).toHaveBeenCalledWith('selection-1', {
        width: 212,
        height: 38
      })
    })
    expect(layout).toHaveBeenCalled()
    const presentOrder = presentToolbar.mock.invocationCallOrder[0]
    expect(presentOrder).toBeDefined()
    expect(layout.mock.invocationCallOrder.some((order) => order < presentOrder!)).toBe(true)
  })

  it('checks the current selection and retries when native presentation returns false', async () => {
    const presentToolbar = vi
      .fn()
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(true)
    const recoverToolbar = vi.fn().mockResolvedValue(true)
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect')
      .mockReturnValue(new DOMRect(0, 0, 212, 38))
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      recoverToolbar,
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    await waitFor(() => expect(presentToolbar).toHaveBeenCalledTimes(2))
    expect(api.getCurrentSelection).toHaveBeenCalledTimes(2)
    expect(recoverToolbar).not.toHaveBeenCalled()
    expect(api.hideToolbar).not.toHaveBeenCalled()
  })

  it('retries transient settings reads before presenting the latest selection', async () => {
    const consoleWarn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const getSettings = vi
      .fn()
      .mockRejectedValueOnce(new TypeError('first transient failure'))
      .mockRejectedValueOnce(new Error('second transient failure'))
      .mockResolvedValue(DEFAULT_PUBLIC_SETTINGS)
    const presentToolbar = vi.fn().mockResolvedValue(true)
    const api = {
      getSettings,
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    await waitFor(() => expect(getSettings).toHaveBeenCalledTimes(3))
    await waitFor(() => expect(presentToolbar).toHaveBeenCalled())
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
    consoleWarn.mockRestore()
  })

  it('consumes rejected Escape and disabled-settings hides with sanitized diagnostics', async () => {
    let settingsListener: ((settings: PublicSettings) => void) | undefined
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const hideToolbar = vi.fn().mockRejectedValue(new Error('selected secret text'))
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar,
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn((listener: (settings: PublicSettings) => void) => {
        settingsListener = listener
        return () => undefined
      })
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    await screen.findByRole('toolbar')

    fireEvent.keyDown(window, { key: 'Escape' })
    act(() => settingsListener?.({ ...DEFAULT_PUBLIC_SETTINGS, enabled: false }))

    await waitFor(() => expect(hideToolbar).toHaveBeenCalledTimes(2))
    await act(async () => Promise.resolve())
    expect(warn).toHaveBeenCalledWith('[TextLens][renderer]', {
      scope: 'toolbar',
      operation: 'hide-escape',
      errorClass: 'Error'
    })
    expect(JSON.stringify(warn.mock.calls)).not.toContain('selected secret text')
    warn.mockRestore()
  })

  it('presents a newer selection without allowing a stale promise to clear it', async () => {
    let selectionListener: ((payload: SelectionPayload) => void) | undefined
    const stalePresentation = deferred<boolean>()
    const presentToolbar = vi
      .fn()
      .mockImplementationOnce(() => stalePresentation.promise)
      .mockResolvedValue(true)
    const runAction = vi.fn().mockResolvedValue({ accepted: true })
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect')
      .mockReturnValue(new DOMRect(0, 0, 212, 38))
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      onSelection: vi.fn((listener: (payload: SelectionPayload) => void) => {
        selectionListener = listener
        return () => undefined
      }),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    await waitFor(() => {
      expect(presentToolbar).toHaveBeenCalledWith('selection-1', {
        width: 212,
        height: 38
      })
    })

    act(() => {
      selectionListener?.({
        ...selection,
        selectionId: 'selection-2',
        text: 'new selection'
      })
    })
    await waitFor(() => {
      expect(presentToolbar).toHaveBeenCalledWith('selection-2', {
        width: 212,
        height: 38
      })
    })

    act(() => stalePresentation.resolve(false))
    const copy = screen
      .getByRole('toolbar')
      .querySelector<HTMLButtonElement>('[data-toolbar-control="action:copy"]')
    expect(copy).not.toBeNull()
    fireEvent.click(copy!)
    await waitFor(() => {
      expect(runAction).toHaveBeenCalledWith('copy', undefined, 'selection-2')
    })
  })

  it('rebuilds after bounded presentation failures without clearing the selection', async () => {
    let selectionListener: ((payload: SelectionPayload) => void) | undefined
    const consoleWarn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
    const presentToolbar = vi
      .fn()
      .mockRejectedValueOnce(new Error('selected text must not enter diagnostics'))
      .mockRejectedValueOnce(new Error('second native presentation failed'))
      .mockRejectedValueOnce(new Error('third native presentation failed'))
      .mockResolvedValue(true)
    const hideToolbar = vi.fn().mockResolvedValue(undefined)
    const recoverToolbar = vi.fn().mockResolvedValue(true)
    const runAction = vi.fn().mockResolvedValue({
      accepted: false,
      message: 'unavailable in test'
    })
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect')
      .mockReturnValue(new DOMRect(0, 0, 212, 38))
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar,
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      recoverToolbar,
      onSelection: vi.fn((listener: (payload: SelectionPayload) => void) => {
        selectionListener = listener
        return () => undefined
      }),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    await waitFor(() => {
      expect(presentToolbar).toHaveBeenNthCalledWith(1, 'selection-1', {
        width: 212,
        height: 38
      })
      expect(presentToolbar).toHaveBeenNthCalledWith(2, 'selection-1', {
        width: 212,
        height: 38
      })
      expect(presentToolbar).toHaveBeenNthCalledWith(3, 'selection-1', {
        width: 212,
        height: 38
      })
      expect(recoverToolbar).toHaveBeenCalledOnce()
      expect(recoverToolbar).toHaveBeenCalledWith('selection-1')
    })
    expect(hideToolbar).not.toHaveBeenCalled()
    expect(JSON.stringify(consoleWarn.mock.calls)).not.toContain(
      'selected text must not enter diagnostics'
    )

    act(() => {
      selectionListener?.({
        ...selection,
        selectionId: 'selection-2',
        text: 'new selection survives toolbar recovery'
      })
    })
    await waitFor(() => {
      expect(presentToolbar).toHaveBeenCalledWith('selection-2', {
        width: 212,
        height: 38
      })
    })
    const copy = screen
      .getByRole('toolbar')
      .querySelector<HTMLButtonElement>('[data-toolbar-control="action:copy"]')
    expect(copy).not.toBeNull()
    fireEvent.click(copy!)
    await waitFor(() => {
      expect(runAction).toHaveBeenCalledWith('copy', undefined, 'selection-2')
    })
    expect(hideToolbar).not.toHaveBeenCalled()
    consoleWarn.mockRestore()
  })

  it('releases pointer focus and forwards the pointer position to an action', async () => {
    const runAction = vi.fn().mockResolvedValue({ accepted: false, message: '暂不可用' })
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    const translate = await screen.findByRole('button', { name: '翻译' })
    translate.focus()
    expect(translate).toHaveFocus()

    fireEvent.click(translate, { detail: 1, screenX: 420, screenY: 300 })

    expect(translate).not.toHaveFocus()
    await waitFor(() => {
      expect(runAction).toHaveBeenCalledWith(
        'translate',
        { x: 420, y: 300 },
        'selection-1'
      )
    })
  })

  it('opens settings with guidance when an AI model is missing', async () => {
    const openSettings = vi.fn().mockResolvedValue(undefined)
    const hideToolbar = vi.fn().mockResolvedValue(undefined)
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({
        accepted: false,
        message: '请先为动作选择模型'
      }),
      openSettings,
      hideToolbar,
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    fireEvent.click(await screen.findByRole('button', { name: '翻译' }))

    await waitFor(() => expect(openSettings).toHaveBeenCalledOnce())
    expect(openSettings).toHaveBeenCalledWith({
      focus: 'actions',
      notice: '请先配置 AI 服务商与模型，再为工具栏动作选择模型。'
    })
    expect(hideToolbar).toHaveBeenCalledWith('selection-1')
    expect(screen.queryByText('AI 功能尚未配置')).not.toBeInTheDocument()
  })

  it('keeps keyboard actions operable without relying on pointer coordinates', async () => {
    const runAction = vi.fn().mockResolvedValue({ accepted: false, message: '暂不可用' })
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    const translate = await screen.findByRole('button', { name: '翻译' })
    await act(async () => {
      await new Promise<void>((resolve) => {
        requestAnimationFrame(() => requestAnimationFrame(() => resolve()))
      })
    })
    translate.focus()

    fireEvent.click(translate, { detail: 0 })

    await waitFor(() => expect(runAction).toHaveBeenCalled())
    expect(translate).toHaveFocus()
    expect(runAction).toHaveBeenCalledWith('translate', undefined, 'selection-1')
  })

  it('keeps a single AI action in flight while showing the busy spinner', async () => {
    const pending = deferred<{ accepted: true }>()
    const runAction = vi.fn().mockReturnValue(pending.promise)
    const hideToolbar = vi.fn().mockResolvedValue(undefined)
    const presentToolbar = vi.fn().mockResolvedValue(true)
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction,
      hideToolbar,
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar,
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    const translate = await screen.findByRole('button', { name: '翻译' })
    const copy = screen.getByRole('button', { name: '复制' })
    await waitFor(() => expect(presentToolbar).toHaveBeenCalled())
    const presentationsBeforeAction = presentToolbar.mock.calls.length

    fireEvent.click(translate, { detail: 1, screenX: 420, screenY: 300 })

    await waitFor(() => {
      expect(runAction).toHaveBeenCalledWith(
        'translate',
        { x: 420, y: 300 },
        'selection-1'
      )
      expect(translate).toBeDisabled()
      expect(copy).not.toBeDisabled()
      expect(copy).toHaveAttribute('aria-disabled', 'true')
      expect(copy).toHaveClass('toolbar-action--muted')
    })
    expect(translate.querySelector('.spin')).not.toBeNull()
    expect(presentToolbar).toHaveBeenCalledTimes(presentationsBeforeAction)

    fireEvent.click(translate, { detail: 1, screenX: 420, screenY: 300 })
    expect(runAction).toHaveBeenCalledTimes(1)

    fireEvent.click(copy)
    expect(runAction).toHaveBeenCalledTimes(1)

    await act(async () => pending.resolve({ accepted: true }))
    await waitFor(() => expect(hideToolbar).toHaveBeenCalledWith('selection-1'))
  })

  it('clears focus left in the singleton toolbar when a new selection arrives', async () => {
    let selectionListener: ((payload: SelectionPayload) => void) | undefined
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn((listener: (payload: SelectionPayload) => void) => {
        selectionListener = listener
        return () => undefined
      }),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    const translate = await screen.findByRole('button', { name: '翻译' })
    translate.focus()
    expect(translate).toHaveFocus()

    act(() => {
      selectionListener?.({
        ...selection,
        selectionId: 'selection-2',
        text: '新的选区'
      })
    })

    expect(translate).not.toHaveFocus()

    // WKWebView can restore the old first responder just after a singleton window is shown.
    translate.focus()
    expect(translate).toHaveFocus()
    await waitFor(() => expect(translate).not.toHaveFocus())
  })

  it('moves explicit hover feedback between controls and clears it on leave', async () => {
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    const toolbar = await screen.findByRole('toolbar')
    const copy = screen.getByRole('button', { name: '复制' })
    const translate = screen.getByRole('button', { name: '翻译' })
    expect(screen.queryByRole('button', { name: '关闭工具栏' })).not.toBeInTheDocument()

    expect(copy).not.toHaveAttribute('data-hovered')
    expect(translate).not.toHaveAttribute('data-hovered')
    // Showing a non-activating WKWebView beneath a stationary cursor can
    // synthesize an enter/over event. It must not revive a previous action.
    fireEvent.pointerOver(translate)
    expect(translate).not.toHaveAttribute('data-hovered')

    fireEvent.pointerMove(translate)
    expect(translate).toHaveAttribute('data-hovered', 'true')
    expect(copy).not.toHaveAttribute('data-hovered')

    fireEvent.pointerMove(copy)
    expect(copy).toHaveAttribute('data-hovered', 'true')
    expect(translate).not.toHaveAttribute('data-hovered')

    fireEvent.mouseMove(translate)
    expect(translate).toHaveAttribute('data-hovered', 'true')
    expect(copy).not.toHaveAttribute('data-hovered')

    fireEvent.pointerLeave(toolbar)
    expect(translate).not.toHaveAttribute('data-hovered')
    expect(toolbarCss).toMatch(/\.toolbar-action\[data-hovered='true'\]:not\(:disabled\)[^{}]*\{/)
    expect(toolbarCss).not.toMatch(/\.toolbar-action:hover/)
  })

  it('uses native client coordinates to move hover without a pressed mouse button', async () => {
    let pointerListener:
      | ((pointer: { x: number; y: number; inside: boolean }) => void)
      | undefined
    const unsubscribePointer = vi.fn()
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn().mockReturnValue(() => undefined),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined),
      onToolbarPointer: vi.fn((listener: typeof pointerListener) => {
        pointerListener = listener
        return unsubscribePointer
      })
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    const elementFromPoint = vi.fn<(x: number, y: number) => Element | null>()
    Object.defineProperty(document, 'elementFromPoint', {
      configurable: true,
      value: elementFromPoint
    })

    const view = render(<ToolbarApp />)
    const copy = await screen.findByRole('button', { name: '复制' })
    const translate = screen.getByRole('button', { name: '翻译' })

    elementFromPoint.mockReturnValue(translate.querySelector('svg'))
    act(() => pointerListener?.({ x: 73.5, y: 20, inside: true }))
    expect(elementFromPoint).toHaveBeenLastCalledWith(73.5, 20)
    expect(translate).toHaveAttribute('data-hovered', 'true')
    expect(copy).not.toHaveAttribute('data-hovered')

    elementFromPoint.mockReturnValue(copy)
    act(() => pointerListener?.({ x: 248, y: 20, inside: true }))
    expect(copy).toHaveAttribute('data-hovered', 'true')
    expect(translate).not.toHaveAttribute('data-hovered')

    // A native leave clears the explicit state without consulting stale
    // WKWebView :hover/focus state.
    elementFromPoint.mockClear()
    act(() => pointerListener?.({ x: 249, y: 48, inside: false }))
    expect(elementFromPoint).not.toHaveBeenCalled()
    expect(copy).not.toHaveAttribute('data-hovered')

    view.unmount()
    expect(unsubscribePointer).toHaveBeenCalledOnce()
  })

  it('clears the explicit hover left in the singleton toolbar for a new selection', async () => {
    let selectionListener: ((payload: SelectionPayload) => void) | undefined
    const api = {
      getSettings: vi.fn().mockResolvedValue(DEFAULT_PUBLIC_SETTINGS),
      getCurrentSelection: vi.fn().mockResolvedValue(selection),
      runAction: vi.fn().mockResolvedValue({ accepted: true }),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      presentToolbar: vi.fn().mockResolvedValue(true),
      onSelection: vi.fn((listener: (payload: SelectionPayload) => void) => {
        selectionListener = listener
        return () => undefined
      }),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)
    const translate = await screen.findByRole('button', { name: '翻译' })

    fireEvent.pointerMove(translate)
    expect(translate).toHaveAttribute('data-hovered', 'true')

    act(() => {
      selectionListener?.({
        ...selection,
        selectionId: 'selection-2',
        text: '新的选区'
      })
    })

    expect(translate).not.toHaveAttribute('data-hovered')
  })

  it('retries the settings read when a selection arrives after a startup failure', async () => {
    let getSettingsCalls = 0
    const getSettings = vi.fn(() => {
      getSettingsCalls += 1
      // Fail the whole startup retry chain (up to three attempts) so only a
      // selection-triggered re-arm can succeed.
      if (getSettingsCalls <= 3) {
        return Promise.reject(new Error('settings ipc not ready'))
      }
      return Promise.resolve(DEFAULT_PUBLIC_SETTINGS)
    })
    let selectionListener: ((payload: SelectionPayload) => void) | undefined
    const api = {
      getSettings,
      getCurrentSelection: vi.fn().mockResolvedValue(null),
      presentToolbar: vi.fn().mockResolvedValue(true),
      hideToolbar: vi.fn().mockResolvedValue(undefined),
      reportToolbarSize: vi.fn().mockResolvedValue(undefined),
      onSelection: vi.fn((listener: (payload: SelectionPayload) => void) => {
        selectionListener = listener
        return () => undefined
      }),
      onSettingsChanged: vi.fn().mockReturnValue(() => undefined)
    } as unknown as WindowTextLensApi

    Object.defineProperty(window, 'textLens', { configurable: true, value: api })
    render(<ToolbarApp />)

    // The startup read fails; the bounded retry chain exhausts itself and
    // the toolbar renders without any actions.
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 200))
    })
    const callsAfterStartupFailure = getSettings.mock.calls.length
    expect(callsAfterStartupFailure).toBeGreaterThanOrEqual(1)
    expect(screen.queryByRole('button', { name: '复制' })).toBeNull()

    // The next selection re-arms the retry chain and this time the read
    // succeeds, so the toolbar still becomes usable without an app restart.
    act(() => {
      selectionListener?.({ ...selection })
    })
    await waitFor(() =>
      expect(screen.getByRole('button', { name: '复制' })).not.toBeNull()
    )
    expect(getSettings.mock.calls.length).toBeGreaterThan(callsAfterStartupFailure)
  })

  it('keeps mouse states neutral but shows a focus ring for keyboard users', () => {
    expect(toolbarCss).toContain('background: var(--surface-muted)')
    expect(toolbarCss).toMatch(
      /\.toolbar-action\s*\{[^}]*transition:\s*background-color\s+\d+ms[^,;]*,\s*color\s+\d+ms/s
    )

    const focusVisible = toolbarCss.match(/\.toolbar-action:focus-visible[^{}]*\{([^}]*)\}/s)
    expect(focusVisible?.[1]).toMatch(/outline:\s*2px solid/)
    expect(toolbarCss).toMatch(
      /\.toolbar-action:focus\s*\{[^}]*outline:\s*none/s
    )

    const activeRules = toolbarCss.matchAll(/[^{}]*:active[^{}]*\{([^}]*)\}/g)
    for (const [, declarations] of activeRules) {
      expect(declarations).not.toMatch(/background\s*:/)
      expect(declarations).not.toMatch(/transform\s*:/)
    }

    const hoverOrBusy = toolbarCss.match(
      /\.toolbar-action\[data-hovered='true'\][^{}]*,[^{}]*\[data-busy='true'\]\s*\{([^}]*)\}/s
    )
    expect(hoverOrBusy?.[1]).toMatch(/color:\s*var\(--accent\)/)
  })

  it('uses the compact action sizes and has no close-control styling', () => {
    expect(toolbarCss).toMatch(/\.toolbar-pill\s*\{[^}]*min-height:\s*34px/s)
    expect(toolbarCss).toMatch(/\.toolbar-action\s*\{[^}]*height:\s*28px/s)
    expect(toolbarCss).toMatch(/\.toolbar-action--icon-only\s*\{[^}]*width:\s*28px/s)
    expect(toolbarCss).not.toContain('.toolbar-close')
    expect(toolbarCss).not.toContain('.toolbar-divider')
  })

  it('keeps disabled toolbar controls on a neutral cursor', () => {
    const disabledRule = toolbarCss.match(
      /\.toolbar-shell button:disabled,\s*\.toolbar-shell input:disabled,\s*\.toolbar-shell select:disabled,\s*\.toolbar-shell textarea:disabled\s*\{([^}]*)\}/s
    )
    expect(disabledRule?.[1]).toMatch(/cursor:\s*default/)
  })
})
