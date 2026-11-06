import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { DEFAULT_PUBLIC_SETTINGS, type SelectionPayload, type WindowTextLensApi } from '../../shared'
import { ToolbarApp } from './ToolbarApp'

const selection: SelectionPayload = {
  selectionId: 'selection-layout-1',
  text: '布局测试选区',
  sourceApp: { name: 'TextEdit', bundleId: 'com.apple.TextEdit' },
  anchor: { kind: 'cursor', x: 200, y: 120 },
  direction: 'unknown',
  isFullscreen: false,
  toolbarMaxWidth: 278
}

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void
  return { promise: new Promise<T>((accept) => { resolve = accept }), resolve: (value) => resolve(value) }
}

function bridge(overrides: Partial<WindowTextLensApi> = {}): void {
  window.textLens = {
    getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
    getCurrentSelection: vi.fn(async () => selection),
    runAction: vi.fn(async () => ({ accepted: true as const })),
    hideToolbar: vi.fn(async () => undefined),
    reportToolbarSize: vi.fn(async () => undefined),
    presentToolbar: vi.fn(async () => true),
    onSelection: vi.fn(() => () => undefined),
    onSettingsChanged: vi.fn(() => () => undefined),
    ...overrides
  } as WindowTextLensApi
}

describe('toolbar layout and dispatch', () => {
  let resize: (() => void) | undefined
  beforeEach(() => {
    vi.stubGlobal('ResizeObserver', class {
      constructor(callback: ResizeObserverCallback) {
        resize = () => callback([], this as unknown as ResizeObserver)
      }
      observe(): void {}
      disconnect(): void {}
    })
  })
  afterEach(() => {
    vi.unstubAllGlobals()
    resize = undefined
  })

  it('uses the monitor budget for both the initial presentation and later message resizing', async () => {
    let height = 38
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(() => new DOMRect(0, 0, 1000, height))
    bridge({ runAction: vi.fn(async () => ({ accepted: false as const, message: '复制失败，请重试' })) })
    render(<ToolbarApp />)
    const copy = await screen.findByRole('button', { name: '复制' })
    await waitFor(() => expect(window.textLens.presentToolbar).toHaveBeenCalledWith(selection.selectionId, { width: 278, height: 38 }))
    expect(screen.getByRole('toolbar')).toHaveStyle({ '--toolbar-max-width': '278px' })
    act(() => resize?.())
    await act(async () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())))
    expect(window.textLens.reportToolbarSize).not.toHaveBeenCalled()
    fireEvent.click(copy)
    await screen.findByRole('alert')
    height = 80
    act(() => resize?.())
    await waitFor(() => expect(window.textLens.reportToolbarSize).toHaveBeenCalledWith({ width: 278, height: 80 }, selection.selectionId))
    expect(window.textLens.presentToolbar).toHaveBeenCalledOnce()
  })

  it('accepts only one action when two click events arrive in the same React batch', async () => {
    const pending = deferred<{ accepted: true }>()
    bridge({ runAction: vi.fn(() => pending.promise) })
    render(<ToolbarApp />)
    const copy = await screen.findByRole('button', { name: '复制' })
    act(() => {
      copy.dispatchEvent(new MouseEvent('click', { bubbles: true }))
      copy.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    })
    expect(window.textLens.runAction).toHaveBeenCalledOnce()
    await act(async () => pending.resolve({ accepted: true }))
    expect(copy.querySelector('.toolbar-copy-success')).not.toBeNull()
  })

  it('replays the selection reveal while retaining the action buttons', async () => {
    let publish: ((value: SelectionPayload) => void) | undefined
    bridge({ onSelection: vi.fn((listener) => { publish = listener; return () => undefined }) })
    render(<ToolbarApp />)
    const copy = await screen.findByRole('button', { name: '复制' })
    await waitFor(() => expect(window.textLens.presentToolbar).toHaveBeenCalled())
    const pill = copy.parentElement!
    const cancel = vi.fn()
    const animate = vi.fn(() => ({ cancel }) as unknown as Animation)
    Object.defineProperty(pill, 'animate', { configurable: true, value: animate })
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false })))
    act(() => publish?.({ ...selection, selectionId: 'selection-layout-2' }))
    expect(screen.getByRole('button', { name: '复制' })).toBe(copy)
    expect(animate).toHaveBeenCalledWith([{ opacity: 0.35 }, { opacity: 1 }], { duration: 90, easing: 'ease-out' })
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: true })))
    act(() => publish?.({ ...selection, selectionId: 'selection-layout-3' }))
    expect(cancel).toHaveBeenCalledOnce()
    expect(animate).toHaveBeenCalledOnce()
  })

  it('does not dismiss a newer selection after an older configuration redirect completes', async () => {
    const pending = deferred<void>()
    let publish: ((value: SelectionPayload) => void) | undefined
    bridge({
      runAction: vi.fn(async () => ({ accepted: false as const, message: '请先为动作选择模型' })),
      openSettings: vi.fn(() => pending.promise),
      onSelection: vi.fn((listener) => { publish = listener; return () => undefined })
    })
    render(<ToolbarApp />)
    const translate = await screen.findByRole('button', { name: '翻译' })
    fireEvent.click(translate)
    await waitFor(() => expect(window.textLens.openSettings).toHaveBeenCalledOnce())
    act(() => publish?.({ ...selection, selectionId: 'selection-layout-2' }))
    await act(async () => pending.resolve())
    expect(window.textLens.hideToolbar).not.toHaveBeenCalled()
    expect(screen.getByRole('toolbar')).toHaveAccessibleName('处理来自 TextEdit 的选中文本')
    expect(translate).toBeEnabled()
  })
})
