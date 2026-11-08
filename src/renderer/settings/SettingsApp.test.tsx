import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { StrictMode } from 'react'

import {
  DEFAULT_ACTION_PROMPTS,
  DEFAULT_PROVIDER_ID,
  DEFAULT_PUBLIC_SETTINGS,
  type PublicSettings,
  type SettingsGuidance,
  type SettingsUpdate,
  type WindowTextLensApi
} from '../../shared'
import { SettingsApp } from './SettingsApp'
import { settingsGuidanceInbox } from './settingsGuidanceInbox'

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise
  })
  return { promise, resolve }
}

function stubScrollIntoView(): ReturnType<typeof vi.fn> {
  const scrollIntoView = vi.fn()
  Object.defineProperty(HTMLElement.prototype, 'scrollIntoView', {
    configurable: true,
    value: scrollIntoView
  })
  return scrollIntoView
}

async function goToSettingsSection(label: string): Promise<void> {
  await screen.findByRole('navigation', { name: '设置分区' })
  fireEvent.click(screen.getByRole('button', { name: label }))
}

function withoutDefaultProvider(): PublicSettings {
  return {
    ...DEFAULT_PUBLIC_SETTINGS,
    providers: [],
    actions: DEFAULT_PUBLIC_SETTINGS.actions.map((action) =>
      'providerId' in action
        ? { ...action, providerId: '', modelId: '' }
        : action
    )
  }
}

function installDefaultBridge(overrides: Partial<WindowTextLensApi> = {}): void {
  window.textLens = {
    getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
    updateSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
    getAccessibilityStatus: vi.fn(async () => ({
      platform: 'windows',
      trusted: true,
      canRequest: false
    })),
    requestAccessibility: vi.fn(),
    onSettingsChanged: vi.fn(() => () => undefined),
    onSettingsCloseRequested: vi.fn(() => () => undefined),
    ...overrides
  } as unknown as WindowTextLensApi
}

describe('SettingsApp provider deletion', () => {
  it('retains guidance across StrictMode cleanup and applies it once after remount', async () => {
    const pendingGuidance = deferred<SettingsGuidance | null>()
    const takeSettingsGuidance = vi.fn(() => pendingGuidance.promise)
    const scrollIntoView = stubScrollIntoView()
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      takeSettingsGuidance,
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<StrictMode><SettingsApp /></StrictMode>)
    expect(takeSettingsGuidance).toHaveBeenCalledOnce()

    act(() => pendingGuidance.resolve({ focus: 'providers', notice: '请配置服务商' }))

    expect(await screen.findByText('请配置服务商')).toBeInTheDocument()
    expect(screen.getAllByText('请配置服务商')).toHaveLength(1)
    expect(await screen.findByRole('heading', { name: 'AI 服务商与模型' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'AI 服务商' })).toHaveAttribute('aria-current', 'page')
    await waitFor(() => expect(scrollIntoView).toHaveBeenCalled())
  })

  it('acknowledges guidance only after the settings draft is ready and focus is scheduled', async () => {
    const settings = deferred<PublicSettings>()
    const takeSettingsGuidance = vi.fn().mockResolvedValue({
      focus: 'providers',
      notice: '请先配置服务商'
    })
    const acknowledge = vi.spyOn(settingsGuidanceInbox, 'acknowledge')
    const scheduleFrame = vi.spyOn(window, 'requestAnimationFrame')
    stubScrollIntoView()
    window.textLens = {
      getSettings: vi.fn(() => settings.promise),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      takeSettingsGuidance,
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await waitFor(() => expect(takeSettingsGuidance).toHaveBeenCalledOnce())
    await act(async () => Promise.resolve())
    expect(acknowledge).not.toHaveBeenCalled()
    expect(screen.queryByText('请先配置服务商')).not.toBeInTheDocument()

    act(() => settings.resolve(DEFAULT_PUBLIC_SETTINGS))

    expect(await screen.findByText('请先配置服务商')).toBeInTheDocument()
    expect(await screen.findByRole('heading', { name: 'AI 服务商与模型' })).toBeInTheDocument()
    expect(scheduleFrame).toHaveBeenCalled()
    expect(acknowledge).toHaveBeenCalledOnce()
  })

  it('defaults to the general section and switches sections from the sidebar nav', async () => {
    installDefaultBridge()
    render(<SettingsApp />)

    expect(await screen.findByRole('heading', { name: '常规' })).toBeInTheDocument()
    expect(screen.getByRole('checkbox', { name: '启用划词助手' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '常规' })).toHaveAttribute('aria-current', 'page')
    expect(screen.queryByRole('heading', { name: 'AI 服务商与模型' })).not.toBeInTheDocument()
    expect(screen.queryByRole('heading', { name: '工具栏动作' })).not.toBeInTheDocument()
    expect(screen.queryByRole('radio', { name: '使用百度' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /添加搜索引擎/ })).not.toBeInTheDocument()

    await goToSettingsSection('AI 服务商')
    expect(await screen.findByRole('heading', { name: 'AI 服务商与模型' })).toBeInTheDocument()
    expect(screen.queryByRole('checkbox', { name: '启用划词助手' })).not.toBeInTheDocument()

    await goToSettingsSection('工具栏动作')
    expect(await screen.findByRole('heading', { name: '工具栏动作' })).toBeInTheDocument()
    expect(screen.queryByRole('heading', { name: 'AI 服务商与模型' })).not.toBeInTheDocument()
  })

  it('returns the settings page to the top when switching sections', async () => {
    installDefaultBridge()
    render(<SettingsApp />)

    expect(await screen.findByRole('heading', { name: '常规' })).toBeInTheDocument()
    const scrollingElement = document.scrollingElement ?? document.documentElement
    scrollingElement.scrollTop = 480

    await goToSettingsSection('权限与诊断')

    expect(screen.getByRole('button', { name: '权限与诊断' })).toHaveAttribute('aria-current', 'page')
    expect(scrollingElement.scrollTop).toBe(0)
  })

  it('shows AI reply and translation languages under the language section', async () => {
    installDefaultBridge()
    render(<SettingsApp />)

    await goToSettingsSection('语言与翻译')
    expect(await screen.findByRole('combobox', { name: 'AI 默认回复语言' })).toBeInTheDocument()
    expect(screen.getByRole('combobox', { name: '翻译默认语言' })).toHaveValue('en-US')
    expect(screen.getByRole('combobox', { name: '翻译目标语言' })).toHaveValue('zh-CN')
    expect(screen.getByText(/检测到 简体中文 时反向译为 English/)).toBeInTheDocument()
    expect(screen.queryByText('默认回答语言')).not.toBeInTheDocument()
  })

  it('persists the default and target translation languages', async () => {
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      translate: update.translate ?? DEFAULT_PUBLIC_SETTINGS.translate,
      providers: (update.providers ?? DEFAULT_PUBLIC_SETTINGS.providers).map((provider) => ({
        ...provider,
        keyConfigured: false
      })),
      actions: update.actions ?? DEFAULT_PUBLIC_SETTINGS.actions
    }))
    installDefaultBridge({ updateSettings })
    render(<SettingsApp />)

    await goToSettingsSection('语言与翻译')
    fireEvent.change(await screen.findByRole('combobox', { name: '翻译默认语言' }), {
      target: { value: 'ja-JP' }
    })
    fireEvent.change(screen.getByRole('combobox', { name: '翻译目标语言' }), {
      target: { value: 'en-US' }
    })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))

    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.calls[0]?.[0].translate).toEqual({
      defaultLanguage: 'ja-JP',
      targetLanguage: 'en-US'
    })
  })

  it('switches to the actions section when guidance focuses actions', async () => {
    const scrollIntoView = stubScrollIntoView()
    installDefaultBridge({
      takeSettingsGuidance: vi.fn(async () => ({ focus: 'actions' as const, notice: '请配置动作' }))
    })
    render(<SettingsApp />)

    expect(await screen.findByText('请配置动作')).toBeInTheDocument()
    expect(await screen.findByRole('heading', { name: '工具栏动作' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '工具栏动作' })).toHaveAttribute('aria-current', 'page')
    await waitFor(() => expect(scrollIntoView).toHaveBeenCalled())
  })

  it('shows and persists the Windows close behavior without a settings-page quit button', async () => {
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      application: update.application ?? DEFAULT_PUBLIC_SETTINGS.application,
      providers: (update.providers ?? DEFAULT_PUBLIC_SETTINGS.providers).map((provider) => ({
        ...provider,
        keyConfigured: false
      })),
      actions: update.actions ?? DEFAULT_PUBLIC_SETTINGS.actions
    }))
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)

    const closeBehavior = await screen.findByRole('combobox', { name: '关闭主窗口时' })
    expect(closeBehavior).toHaveValue('hide-to-tray')
    fireEvent.change(closeBehavior, { target: { value: 'quit' } })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))

    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.calls[0]?.[0].application).toEqual({ closeBehavior: 'quit' })

    expect(screen.queryByRole('button', { name: '退出 TextLens' })).not.toBeInTheDocument()
  })

  it('previews appearance themes and persists the selected theme', async () => {
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      appearance: update.appearance ?? DEFAULT_PUBLIC_SETTINGS.appearance
    }))
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('外观')

    const group = await screen.findByRole('radiogroup', { name: '界面主题' })
    const options = within(group).getAllByRole('radio')
    expect(options).toHaveLength(5)
    expect(within(group).getByRole('radio', { name: '暖白' })).toHaveAttribute('aria-checked', 'true')
    expect(document.documentElement).not.toHaveAttribute('data-theme')

    fireEvent.click(within(group).getByRole('radio', { name: '苔绿' }))
    expect(within(group).getByRole('radio', { name: '苔绿' })).toHaveAttribute('aria-checked', 'true')
    expect(within(group).getByRole('radio', { name: '暖白' })).toHaveAttribute('aria-checked', 'false')
    expect(document.documentElement).toHaveAttribute('data-theme', 'sage')

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.calls[0]?.[0].appearance).toEqual({ theme: 'sage' })
  })

  it('moves the theme selection with arrow keys and keeps one tab stop', async () => {
    installDefaultBridge()
    render(<SettingsApp />)
    await goToSettingsSection('外观')

    const group = await screen.findByRole('radiogroup', { name: '界面主题' })
    const warm = within(group).getByRole('radio', { name: '暖白' })
    const classic = within(group).getByRole('radio', { name: '经典' })
    expect(warm).toHaveAttribute('tabindex', '0')
    expect(classic).toHaveAttribute('tabindex', '-1')

    warm.focus()
    fireEvent.keyDown(warm, { key: 'ArrowRight' })

    expect(classic).toHaveAttribute('aria-checked', 'true')
    expect(classic).toHaveAttribute('tabindex', '0')
    expect(warm).toHaveAttribute('tabindex', '-1')
    expect(classic).toHaveFocus()
    expect(document.documentElement).toHaveAttribute('data-theme', 'classic')
  })

  it('fades a successful save after 1.4 seconds and removes it after 2 seconds', async () => {
    const updateSettings = vi.fn(async (): Promise<PublicSettings> => DEFAULT_PUBLIC_SETTINGS)
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    fireEvent.click(await screen.findByRole('button', { name: '保存设置' }))

    await waitFor(() => expect(updateSettings).toHaveBeenCalledOnce())
    const banner = await screen.findByText('设置已保存')
    expect(banner.parentElement).not.toHaveClass('settings-banner--fading')

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 1_450))
    })
    expect(banner.parentElement).toHaveClass('settings-banner--fading')

    await waitFor(() => {
      expect(screen.queryByText('设置已保存')).not.toBeInTheDocument()
    }, { timeout: 1_000 })
  })

  it('fades an error banner after 1.4 seconds and removes it after 2 seconds', async () => {
    const updateSettings = vi.fn(async (): Promise<PublicSettings> => {
      throw new Error('保存失败了')
    })
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    fireEvent.change(await screen.findByRole('combobox', { name: '关闭主窗口时' }), {
      target: { value: 'quit' }
    })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))

    const banner = await screen.findByText('保存失败了')
    expect(banner.parentElement).toHaveClass('notice--error')
    expect(banner.parentElement).not.toHaveClass('settings-banner--fading')

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 1_450))
    })
    expect(banner.parentElement).toHaveClass('settings-banner--fading')

    await waitFor(() => {
      expect(screen.queryByText('保存失败了')).not.toBeInTheDocument()
    }, { timeout: 1_000 })
  })

  it('keeps Windows lifecycle controls out of the macOS settings UI', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)

    await goToSettingsSection('权限与诊断')
    await screen.findByRole('heading', { name: '辅助功能权限' })
    expect(screen.queryByRole('combobox', { name: '关闭主窗口时' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '退出 TextLens' })).not.toBeInTheDocument()
  })

  it('keeps the Windows-only capture strategy out of the macOS capture section', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)

    await goToSettingsSection('捕获与过滤')
    expect(await screen.findByRole('heading', { name: '捕获与过滤' })).toBeInTheDocument()
    expect(screen.queryByRole('combobox', { name: '默认划词获取方式' })).not.toBeInTheDocument()
    expect(screen.getByRole('combobox', { name: '过滤模式' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '捕获与过滤' })).toHaveAttribute('aria-current', 'page')
  })

  it('keeps the Windows-only capture strategy section visible on Windows', async () => {
    // 深链/记忆分区指向 capture 时 Windows 上仍能正常渲染。
    window.sessionStorage.setItem('textlens.settings.active-section', 'capture')
    try {
      window.textLens = {
        getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
        updateSettings: vi.fn(),
        getAccessibilityStatus: vi.fn(async () => ({
          platform: 'windows',
          trusted: true,
          canRequest: false
        })),
        requestAccessibility: vi.fn(),
        onSettingsChanged: vi.fn(() => () => undefined),
        onSettingsCloseRequested: vi.fn(() => () => undefined)
      } as unknown as WindowTextLensApi

      render(<SettingsApp />)

      expect(await screen.findByRole('combobox', { name: '默认划词获取方式' })).toBeInTheDocument()
    } finally {
      window.sessionStorage.removeItem('textlens.settings.active-section')
    }
  })

  it('keeps settings editable when the accessibility diagnostic IPC fails', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => {
        throw new Error('diagnostic unavailable')
      }),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)

    expect(await screen.findByRole('button', { name: '保存设置' })).toBeEnabled()
    expect(screen.getByText('diagnostic unavailable')).toBeInTheDocument()
  })

  it('offers cancel and discard choices for a native close request with unsaved changes', async () => {
    let requestClose: (() => void) | undefined
    const quitApp = vi.fn(async () => undefined)
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      quitApp,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn((listener: () => void) => {
        requestClose = listener
        return () => undefined
      })
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    fireEvent.change(await screen.findByRole('combobox', { name: '关闭主窗口时' }), {
      target: { value: 'quit' }
    })

    act(() => requestClose?.())
    expect(screen.getByRole('dialog', { name: '退出 TextLens？' })).toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '取消' }))
    expect(screen.queryByRole('dialog', { name: '退出 TextLens？' })).not.toBeInTheDocument()
    expect(quitApp).not.toHaveBeenCalled()

    act(() => requestClose?.())
    fireEvent.click(screen.getByRole('button', { name: '放弃更改并退出' }))
    await waitFor(() => expect(quitApp).toHaveBeenCalledTimes(1))
  })

  it('saves before quitting when requested from the unsaved-changes dialog', async () => {
    let requestClose: (() => void) | undefined
    const quitApp = vi.fn(async () => undefined)
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      application: update.application ?? DEFAULT_PUBLIC_SETTINGS.application,
      providers: (update.providers ?? DEFAULT_PUBLIC_SETTINGS.providers).map((provider) => ({
        ...provider,
        keyConfigured: false
      })),
      actions: update.actions ?? DEFAULT_PUBLIC_SETTINGS.actions
    }))
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      quitApp,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined),
      onSettingsCloseRequested: vi.fn((listener: () => void) => {
        requestClose = listener
        return () => undefined
      })
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    fireEvent.change(await screen.findByRole('combobox', { name: '关闭主窗口时' }), {
      target: { value: 'quit' }
    })
    act(() => requestClose?.())
    fireEvent.click(screen.getByRole('button', { name: '保存并退出' }))

    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    await waitFor(() => expect(quitApp).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.invocationCallOrder[0]).toBeLessThan(
      quitApp.mock.invocationCallOrder[0]!
    )
  })

  it('shows Windows selection access without macOS permission instructions', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)

    await goToSettingsSection('权限与诊断')
    expect(await screen.findByRole('heading', { name: 'Windows 选区访问' })).toBeInTheDocument()
    expect(screen.getByText(/不需要单独授权/)).toBeInTheDocument()
    expect(screen.queryByText(/隐私与安全性/)).not.toBeInTheDocument()
  })

  it('shows replayed Windows listener and global-shortcut failures', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'windows',
        trusted: true,
        canRequest: false,
        available: false,
        diagnostics: {
          selectionMonitorError: '无法启动系统划词监听。请重新启动 TextLens。',
          shortcutError: '全局快捷键注册失败，可能已被其他应用占用。'
        }
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)

    await goToSettingsSection('权限与诊断')
    expect(await screen.findByText('选区访问不可用')).toBeInTheDocument()
    expect(screen.getByText('无法启动系统划词监听。请重新启动 TextLens。')).toBeInTheDocument()
    expect(screen.getByText('全局快捷键不可用')).toBeInTheDocument()
    expect(screen.getByText('全局快捷键注册失败，可能已被其他应用占用。')).toBeInTheDocument()
    expect(screen.queryByText('选区访问可用')).not.toBeInTheDocument()
  })

  it('records a capture shortcut from keyboard input instead of free typing', async () => {
    const updateSettings = vi.fn(async (update: SettingsUpdate) => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      ...update,
      trigger: update.trigger ?? DEFAULT_PUBLIC_SETTINGS.trigger,
      captureShortcut: update.captureShortcut ?? DEFAULT_PUBLIC_SETTINGS.captureShortcut
    }))
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin' as const,
        trusted: true,
        canRequest: true,
        available: true,
        diagnostics: {}
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    fireEvent.click(await screen.findByRole('radio', { name: '快捷键' }))

    const input = screen.getByLabelText('捕获当前选区快捷键')
    expect(input).toHaveValue('CommandOrControl+Shift+S')

    fireEvent.keyDown(input, {
      key: 'k',
      code: 'KeyK',
      metaKey: true,
      altKey: true,
      bubbles: true
    })
    // macOS maps Meta → CommandOrControl; Linux/Windows map Meta → Super.
    const platform = `${navigator.platform ?? ''} ${navigator.userAgent ?? ''}`.toLowerCase()
    const isMac = platform.includes('mac') || platform.includes('darwin')
    const recorded = isMac ? 'CommandOrControl+Alt+K' : 'Super+Alt+K'
    expect(input).toHaveValue(recorded)

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => {
      expect(updateSettings).toHaveBeenCalledWith(expect.objectContaining({
        trigger: { mode: 'shortcut' },
        captureShortcut: recorded
      }))
    })

    fireEvent.keyDown(input, { key: 'Backspace', code: 'Backspace', bubbles: true })
    expect(input).toHaveValue('')
  })

  it('refreshes the replayable shortcut diagnostic immediately after save fails', async () => {
    const updateSettings = vi.fn(async () => {
      throw new Error('无法注册快捷键')
    })
    const getAccessibilityStatus = vi.fn(async () => updateSettings.mock.calls.length > 0
      ? {
          platform: 'windows' as const,
          trusted: true,
          canRequest: false,
          available: true,
          diagnostics: {
            shortcutError: '全局快捷键注册失败，可能已被其他应用占用。'
          }
        }
      : {
          platform: 'windows' as const,
          trusted: true,
          canRequest: false,
          available: true,
          diagnostics: {}
        })
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      getAccessibilityStatus,
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)

    fireEvent.click(await screen.findByRole('radio', { name: '快捷键' }))
    // Switching to shortcut mode seeds a valid default capture chord.
    expect(screen.getByLabelText('捕获当前选区快捷键')).toHaveValue('CommandOrControl+Shift+S')
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))

    await goToSettingsSection('权限与诊断')
    expect(await screen.findByText('全局快捷键不可用')).toBeInTheDocument()
    expect(screen.getByText('全局快捷键注册失败，可能已被其他应用占用。')).toBeInTheDocument()
    expect(getAccessibilityStatus).toHaveBeenCalledTimes(2)
  })

  it('reorders toolbar actions with the keyboard drag handle and persists order values', async () => {
    const rowOrder = new Map(
      DEFAULT_PUBLIC_SETTINGS.actions.map((action, index) => [action.id, index])
    )
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
      const element = this
      const row = element.closest<HTMLElement>('[data-action-id]')
      const actionId = row?.dataset.actionId
      const zone = element.closest<HTMLElement>('[data-action-zone]')
      const index = actionId ? rowOrder.get(actionId) ?? 0 : 0
      const left = zone?.dataset.actionZone === 'disabled' ? 420 : 20
      const top = actionId ? 500 + index * 64 : 480
      const width = actionId ? 360 : 380
      const height = actionId ? 54 : 460
      return {
        x: left,
        y: top,
        left,
        top,
        right: left + width,
        bottom: top + height,
        width,
        height,
        toJSON: () => ({})
      } as DOMRect
    })

    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      ...update,
      providers: (update.providers ?? DEFAULT_PUBLIC_SETTINGS.providers)
        .map((provider) => ({ ...provider, keyConfigured: false })),
      actions: update.actions ?? DEFAULT_PUBLIC_SETTINGS.actions
    }))
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('工具栏动作')

    const handle = await screen.findByRole('button', { name: '拖动复制' })
    handle.focus()
    fireEvent.keyDown(handle, { key: ' ', code: 'Space' })
    await waitFor(() => {
      expect(document.body.textContent).toContain('Draggable item copy was moved over droppable area copy')
    })
    fireEvent.keyDown(document, { key: 'ArrowUp', code: 'ArrowUp' })
    await waitFor(() => {
      expect(document.body.textContent).toContain('Draggable item copy was moved over droppable area search')
    })
    fireEvent.keyDown(document, { key: ' ', code: 'Space' })

    await waitFor(() => {
      const enabledZone = document.querySelector<HTMLElement>('[data-action-zone="enabled"]')
      expect(
        [...(enabledZone?.querySelectorAll<HTMLElement>('[data-action-id]') ?? [])]
          .map((row) => row.dataset.actionId)
      ).toEqual([
        'translate',
        'explain',
        'summary',
        'copy',
        'search',
        'ask-ai'
      ])
    })

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect((updateSettings.mock.calls[0]?.[0].actions ?? []).map(({ id, order }) => ({ id, order })))
      .toEqual([
        { id: 'translate', order: 0 },
        { id: 'explain', order: 1 },
        { id: 'summary', order: 2 },
        { id: 'copy', order: 3 },
        { id: 'search', order: 4 },
        { id: 'ask-ai', order: 5 },
        { id: 'refine', order: 6 }
      ])
  })

  it('shows a non-blocking plaintext warning for HTTP provider addresses', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    const baseUrl = await screen.findByDisplayValue('https://api.openai.com/v1')
    expect(screen.queryByText(/API Key 将通过网络明文传输/)).not.toBeInTheDocument()
    fireEvent.change(baseUrl, { target: { value: 'http://api.example.com/v1' } })
    expect(screen.getByText(/API Key 将通过网络明文传输/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '保存设置' })).toBeEnabled()
  })

  it('edits the persisted result text size through the supported range control', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('外观')

    const textSize = await screen.findByRole('slider', { name: '结果文字大小' })
    expect(textSize).toHaveValue('14')
    fireEvent.change(textSize, { target: { value: '18' } })
    expect(textSize).toHaveValue('18')
    expect(screen.getByText('18 px')).toBeInTheDocument()
  })

  it('shows the Cherry default prompt and lets the user edit it', async () => {
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('工具栏动作')

    fireEvent.click(await screen.findByRole('button', { name: '编辑翻译' }))
    const prompt = document.querySelector<HTMLTextAreaElement>('#action-prompt')
    expect(prompt).toHaveValue(DEFAULT_ACTION_PROMPTS.translate)

    fireEvent.change(prompt!, { target: { value: '我的翻译规则：{{text}}' } })
    fireEvent.click(screen.getByRole('button', { name: '保存修改' }))

    expect(screen.getByText('提示词：我的翻译规则：{{text}}')).toBeInTheDocument()
  })

  it('uses an in-app confirmation and removes providers in the ordinary settings commit', async () => {
    const deletedSettings = withoutDefaultProvider()
    const deleteProvider = vi.fn(async () => deletedSettings)
    const updateSettings = vi.fn(async (_update: SettingsUpdate) => deletedSettings)
    const browserConfirm = vi.spyOn(window, 'confirm')

    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings,
      deleteProvider,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    const removeButton = await screen.findByRole('button', { name: /删除OpenAI Compatible/ })
    fireEvent.click(removeButton)

    expect(screen.getByRole('dialog', { name: /删除“OpenAI Compatible”/ })).toBeInTheDocument()
    expect(browserConfirm).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole('button', { name: '删除服务商' }))
    expect(screen.queryByRole('button', { name: /删除OpenAI Compatible/ })).not.toBeInTheDocument()
    expect(screen.getByText(/保存设置后生效/)).toBeInTheDocument()

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))

    await waitFor(() => {
      expect(updateSettings).toHaveBeenCalledTimes(1)
    })
    expect(deleteProvider).not.toHaveBeenCalled()
    expect(updateSettings.mock.calls[0]?.[0].providers).toEqual([])
    expect(
      updateSettings.mock.calls[0]?.[0].actions
        ?.filter((action) => 'providerId' in action)
        .every((action) => action.providerId === '' && action.modelId === '')
    ).toBe(true)
  })

  it('preserves unsaved providers, keys and fields when another provider key is cleared', async () => {
    const configured = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        keyConfigured: true
      }))
    }
    const backendCleared = {
      ...configured,
      providers: configured.providers.map((provider) => ({
        ...provider,
        keyConfigured: false
      }))
    }
    const clearProviderApiKey = vi.fn(async () => backendCleared)

    window.textLens = {
      getSettings: vi.fn(async () => configured),
      updateSettings: vi.fn(),
      clearProviderApiKey,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    const baseUrl = await screen.findByDisplayValue('https://api.openai.com/v1')
    fireEvent.change(baseUrl, { target: { value: 'https://gateway.example/v1' } })
    fireEvent.click(screen.getByRole('button', { name: '添加服务商' }))
    expect(screen.getByDisplayValue('服务商 2')).toBeInTheDocument()

    const keyInputs = document.querySelectorAll<HTMLInputElement>('input[type="password"]')
    expect(keyInputs).toHaveLength(2)
    fireEvent.change(keyInputs[1]!, { target: { value: 'sk-new-provider' } })
    const clearKey = screen
      .getAllByRole('button', { name: '清除密钥' })
      .find((button) => !(button as HTMLButtonElement).disabled)
    expect(clearKey).toBeDefined()
    fireEvent.click(clearKey!)

    await waitFor(() => expect(clearProviderApiKey).toHaveBeenCalledWith(DEFAULT_PROVIDER_ID))
    expect(screen.getByDisplayValue('https://gateway.example/v1')).toBeInTheDocument()
    expect(screen.getByDisplayValue('服务商 2')).toBeInTheDocument()
    expect(document.querySelectorAll<HTMLInputElement>('input[type="password"]')[1]).toHaveValue(
      'sk-new-provider'
    )
  })

  it('uses an in-app confirmation when removing an action', async () => {
    const browserConfirm = vi.spyOn(window, 'confirm')
    window.textLens = {
      getSettings: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('工具栏动作')

    fireEvent.click(await screen.findByRole('button', { name: '移除润色' }))
    expect(screen.getByRole('dialog', { name: '移除动作“润色”？' })).toBeInTheDocument()
    expect(browserConfirm).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole('button', { name: '移除动作' }))
    expect(screen.queryByRole('button', { name: '移除润色' })).not.toBeInTheDocument()
    expect(screen.getByText(/已移除动作“润色”/)).toBeInTheDocument()
  })

  it('commits ordinary settings first and keeps recoverable key input after a key failure', async () => {
    let persisted: PublicSettings | null = null
    const getSettings = vi.fn(async () => persisted ?? DEFAULT_PUBLIC_SETTINGS)
    const updateSettings = vi.fn(async (update: SettingsUpdate) => {
      persisted = {
        ...DEFAULT_PUBLIC_SETTINGS,
        ...update,
        providers: (update.providers ?? []).map((provider) => ({
          ...provider,
          keyConfigured: false
        })),
        actions: update.actions ?? DEFAULT_PUBLIC_SETTINGS.actions
      }
      return persisted
    })
    const setProviderApiKey = vi.fn(async () => {
      throw new Error('Encrypted secret store unavailable')
    })

    window.textLens = {
      getSettings,
      updateSettings,
      setProviderApiKey,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    await screen.findByDisplayValue('OpenAI Compatible')
    fireEvent.click(screen.getByRole('button', { name: '添加服务商' }))
    const keyInputs = document.querySelectorAll<HTMLInputElement>('input[type="password"]')
    fireEvent.change(keyInputs[1]!, { target: { value: 'sk-retry-me' } })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))

    expect(await screen.findByText(/普通设置已保存，但 API Key 保存失败/)).toBeInTheDocument()
    expect(updateSettings).toHaveBeenCalledTimes(1)
    expect(setProviderApiKey).toHaveBeenCalledTimes(1)
    expect(updateSettings.mock.invocationCallOrder[0]).toBeLessThan(
      setProviderApiKey.mock.invocationCallOrder[0]!
    )
    expect(screen.getByDisplayValue('服务商 2')).toBeInTheDocument()
    expect(document.querySelectorAll<HTMLInputElement>('input[type="password"]')[1]).toHaveValue(
      'sk-retry-me'
    )
  })

  it('does not fetch saved API keys until the user clicks show', async () => {
    const getProviderApiKey = vi.fn(async () => 'sk-secret-should-not-preload')
    const configured: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        keyConfigured: true
      }))
    }
    window.textLens = {
      getSettings: vi.fn(async () => configured),
      updateSettings: vi.fn(),
      getProviderApiKey,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    await waitFor(() => {
      expect(screen.getByRole('heading', { name: 'AI 服务商与模型' })).toBeInTheDocument()
    })
    expect(getProviderApiKey).not.toHaveBeenCalled()
  })

  it('loads the saved API key when the user clicks show', async () => {
    const getProviderApiKey = vi.fn(async () => 'sk-revealed-key')
    const configured: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        keyConfigured: true
      }))
    }
    window.textLens = {
      getSettings: vi.fn(async () => configured),
      updateSettings: vi.fn(),
      getProviderApiKey,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    const toggle = await screen.findByRole('button', { name: /显示 API Key/ })
    fireEvent.click(toggle)
    await waitFor(() => {
      expect(getProviderApiKey).toHaveBeenCalled()
    })
    const input = screen.getByDisplayValue('sk-revealed-key')
    expect(input).toHaveAttribute('type', 'text')
  })

  it('shows provider models as vertical reorderable rows without thinking badges', async () => {
    const settingsWithModels: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        models: [
          { id: 'gpt-4o-mini', name: 'gpt-4o-mini', thinkingLevels: [] },
          { id: 'o3-mini', name: 'o3-mini', thinkingLevels: ['low', 'medium', 'high'] }
        ]
      }))
    }
    window.textLens = {
      getSettings: vi.fn(async () => settingsWithModels),
      updateSettings: vi.fn(),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    expect(await screen.findByRole('button', { name: '获取模型' })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '同步模型' })).not.toBeInTheDocument()

    expect(await screen.findByRole('button', { name: '拖动模型 gpt-4o-mini' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '拖动模型 o3-mini' })).toBeInTheDocument()
    expect(screen.getByText('gpt-4o-mini', { selector: '.model-row__name' })).toBeInTheDocument()
    expect(screen.getByText('o3-mini', { selector: '.model-row__name' })).toBeInTheDocument()
    expect(document.querySelector('.model-row-list')).toBeInTheDocument()

    // Provider model list must not show thinking badges
    expect(screen.queryByText(/思考\s*:/)).not.toBeInTheDocument()
    expect(screen.queryByText(/思考:\s*低/)).not.toBeInTheDocument()

    expect(screen.getAllByRole('button', { name: /拖动模型/ }).length).toBeGreaterThan(0)
    expect(screen.getAllByRole('button', { name: /移除模型/ }).length).toBeGreaterThan(0)
  })

  it('fetches models into a multi-select picker and merges the subset into the draft', async () => {
    const settingsWithModels: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        models: [
          { id: 'gpt-4o-mini', name: 'gpt-4o-mini', thinkingLevels: [] },
          { id: 'manual-keep', name: 'manual-keep', thinkingLevels: [] }
        ]
      }))
    }
    const listProviderModels = vi.fn(async () => ({
      ok: true as const,
      models: [
        { id: 'gpt-4o-mini', name: 'GPT-4o Mini', thinkingLevels: [] },
        { id: 'gpt-4o', name: 'GPT-4o', thinkingLevels: [] },
        { id: 'o3-mini', name: 'o3-mini', thinkingLevels: ['low', 'medium', 'high'] }
      ]
    }))
    const syncProviderModels = vi.fn()
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...settingsWithModels,
      ...update,
      providers: (update.providers ?? settingsWithModels.providers).map((provider) => ({
        ...provider,
        keyConfigured: false
      })),
      actions: update.actions ?? settingsWithModels.actions
    }))
    window.textLens = {
      getSettings: vi.fn(async () => settingsWithModels),
      updateSettings,
      listProviderModels,
      syncProviderModels,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    fireEvent.click(await screen.findByRole('button', { name: '获取模型' }))
    expect(await screen.findByRole('dialog', { name: '选择模型' })).toBeInTheDocument()
    await waitFor(() => expect(listProviderModels).toHaveBeenCalled())
    expect(syncProviderModels).not.toHaveBeenCalled()

    const gpt4oMini = screen.getByRole('checkbox', { name: /GPT-4o Mini/i })
    const gpt4o = screen.getByRole('checkbox', { name: /GPT-4o$/i })
    const o3 = screen.getByRole('checkbox', { name: /o3-mini/i })
    const manual = screen.getByRole('checkbox', { name: /manual-keep/i })
    expect(gpt4oMini).toBeChecked()
    expect(manual).toBeChecked()
    expect(gpt4o).not.toBeChecked()
    expect(o3).not.toBeChecked()

    fireEvent.click(gpt4o)
    fireEvent.click(gpt4oMini)
    fireEvent.click(screen.getByRole('button', { name: /应用所选/ }))

    await waitFor(() => {
      expect(screen.queryByRole('dialog', { name: '选择模型' })).not.toBeInTheDocument()
    })
    expect(screen.getByText(/已选择 2 个模型，请保存设置/)).toBeInTheDocument()
    expect(screen.getByText('GPT-4o', { selector: '.model-row__name' })).toBeInTheDocument()
    expect(screen.getByText('manual-keep', { selector: '.model-row__name' })).toBeInTheDocument()
    expect(screen.queryByText('gpt-4o-mini', { selector: '.model-row__name' })).not.toBeInTheDocument()
    expect(syncProviderModels).not.toHaveBeenCalled()
  })

  it('cancels the model picker without changing draft models', async () => {
    const settingsWithModels: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        models: [{ id: 'gpt-4o-mini', name: 'gpt-4o-mini', thinkingLevels: [] }]
      }))
    }
    window.textLens = {
      getSettings: vi.fn(async () => settingsWithModels),
      updateSettings: vi.fn(async () => settingsWithModels),
      listProviderModels: vi.fn(async () => ({
        ok: true as const,
        models: [
          { id: 'gpt-4o-mini', name: 'gpt-4o-mini', thinkingLevels: [] },
          { id: 'gpt-4o', name: 'gpt-4o', thinkingLevels: [] }
        ]
      })),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')
    fireEvent.click(await screen.findByRole('button', { name: '获取模型' }))
    expect(await screen.findByRole('dialog', { name: '选择模型' })).toBeInTheDocument()
    fireEvent.click(screen.getByRole('checkbox', { name: /gpt-4o$/i }))
    fireEvent.click(screen.getByRole('button', { name: '取消' }))
    await waitFor(() => {
      expect(screen.queryByRole('dialog', { name: '选择模型' })).not.toBeInTheDocument()
    })
    expect(screen.getByText('gpt-4o-mini', { selector: '.model-row__name' })).toBeInTheDocument()
    expect(screen.queryByText('gpt-4o', { selector: '.model-row__name' })).not.toBeInTheDocument()
  })

  it('shows an error banner when model fetch fails and leaves models unchanged', async () => {
    const settingsWithModels: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        models: [{ id: 'gpt-4o-mini', name: 'gpt-4o-mini', thinkingLevels: [] }]
      }))
    }
    window.textLens = {
      getSettings: vi.fn(async () => settingsWithModels),
      updateSettings: vi.fn(async () => settingsWithModels),
      listProviderModels: vi.fn(async () => ({
        ok: false as const,
        message: '无法连接服务商'
      })),
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')
    fireEvent.click(await screen.findByRole('button', { name: '获取模型' }))
    expect(await screen.findByText('无法连接服务商')).toBeInTheDocument()
    expect(screen.queryByRole('dialog', { name: '选择模型' })).not.toBeInTheDocument()
    expect(screen.getByText('gpt-4o-mini', { selector: '.model-row__name' })).toBeInTheDocument()
  })

  it('reorders provider models with the keyboard drag handle', async () => {
    const modelIds = ['gpt-4o-mini', 'o3-mini']
    const modelOrder = new Map<string, number>(modelIds.map((id, index) => [id, index]))
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
      this: HTMLElement
    ) {
      const row = this.closest<HTMLElement>('[data-model-id]')
      const modelId = row?.dataset.modelId
      const index = modelId ? modelOrder.get(modelId) ?? 0 : 0
      const left = 20
      const top = modelId ? 500 + index * 64 : 480
      const width = modelId ? 360 : 380
      const height = modelId ? 54 : 200
      return {
        x: left,
        y: top,
        left,
        top,
        right: left + width,
        bottom: top + height,
        width,
        height,
        toJSON: () => ({})
      } as DOMRect
    })

    const settingsWithModels: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({
        ...provider,
        models: [
          { id: 'gpt-4o-mini', name: 'gpt-4o-mini', thinkingLevels: [] },
          { id: 'o3-mini', name: 'o3-mini', thinkingLevels: ['low', 'medium', 'high'] }
        ]
      }))
    }
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...settingsWithModels,
      ...update,
      providers: (update.providers ?? settingsWithModels.providers).map((provider) => ({
        ...provider,
        keyConfigured: false
      })),
      actions: update.actions ?? settingsWithModels.actions
    }))
    window.textLens = {
      getSettings: vi.fn(async () => settingsWithModels),
      updateSettings,
      getAccessibilityStatus: vi.fn(async () => ({
        platform: 'darwin',
        trusted: true,
        canRequest: true
      })),
      requestAccessibility: vi.fn(),
      onSettingsChanged: vi.fn(() => () => undefined)
    } as unknown as WindowTextLensApi

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    const handle = await screen.findByRole('button', { name: '拖动模型 gpt-4o-mini' })
    handle.focus()
    fireEvent.keyDown(handle, { key: ' ', code: 'Space' })
    await waitFor(() => {
      expect(document.body.textContent).toContain(
        'Draggable item gpt-4o-mini was moved over droppable area gpt-4o-mini'
      )
    })
    fireEvent.keyDown(document, { key: 'ArrowDown', code: 'ArrowDown' })
    await waitFor(() => {
      expect(document.body.textContent).toContain(
        'Draggable item gpt-4o-mini was moved over droppable area o3-mini'
      )
    })
    fireEvent.keyDown(document, { key: ' ', code: 'Space' })

    await waitFor(() => {
      const dragHandles = screen.getAllByRole('button', { name: /拖动模型/ })
      expect(dragHandles.map((button) => button.getAttribute('aria-label'))).toEqual([
        '拖动模型 o3-mini',
        '拖动模型 gpt-4o-mini'
      ])
    })

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(
      (updateSettings.mock.calls[0]?.[0].providers?.[0]?.models ?? []).map((model) => model.id)
    ).toEqual(['o3-mini', 'gpt-4o-mini'])
  })
})

describe('SettingsApp provider protocols', () => {
  it('switches protocol, reseeds model thinking and saves the new fields', async () => {
    const provider = DEFAULT_PUBLIC_SETTINGS.providers[0]!
    const settings: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: [{
        ...provider,
        enabled: true,
        protocol: 'openAiChat',
        baseUrl: 'https://api.openai.com/v1',
        keyConfigured: true,
        models: [{ id: 'claude-sonnet-4-5', name: 'Claude Sonnet 4.5', thinkingLevels: [] }]
      }]
    }
    const updateSettings = vi.fn(async (_update: SettingsUpdate) => settings)
    installDefaultBridge({
      getSettings: vi.fn(async () => settings),
      updateSettings
    } as Partial<WindowTextLensApi>)

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    const protocol = await screen.findByLabelText('OpenAI Compatible：接口协议')
    fireEvent.change(protocol, { target: { value: 'anthropicMessages' } })
    expect(protocol).toHaveValue('anthropicMessages')
    expect(screen.getByDisplayValue('https://api.anthropic.com/v1')).toBeInTheDocument()
    expect(screen.getByText(/默认 中 · 低\/中\/高/)).toBeInTheDocument()

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    const update = updateSettings.mock.calls[0]?.[0]
    const saved = update?.providers?.[0]
    expect(saved?.protocol).toBe('anthropicMessages')
    expect(saved?.baseUrl).toBe('https://api.anthropic.com/v1')
    expect(saved?.models[0]).toMatchObject({
      thinkingLevels: ['low', 'medium', 'high'],
      thinkingCapability: {
        source: 'heuristic',
        dialect: null,
        supportsOff: true
      },
      thinkingEnabled: true,
      thinkingDefault: 'medium'
    })
  })
})

describe('SettingsApp model thinking', () => {
  it('shows disabled thinking without levels and re-detects DeepSeek V4', async () => {
    const provider = DEFAULT_PUBLIC_SETTINGS.providers[0]!
    const settings: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: [{
        ...provider,
        enabled: true,
        protocol: 'openAiChat',
        keyConfigured: true,
        models: [{
          id: 'deepseek-ai/deepseek-v4.1-flash-1017',
          name: 'DeepSeek-V4.1-Flash-1017',
          thinkingLevels: [],
          thinkingEnabled: false
        }]
      }]
    }
    const updateSettings = vi.fn(async (_update: SettingsUpdate) => settings)
    installDefaultBridge({
      getSettings: vi.fn(async () => settings),
      updateSettings
    } as Partial<WindowTextLensApi>)

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    expect(
      await screen.findByText('不启用思考', { selector: '.model-row__thinking-summary' })
    ).toBeInTheDocument()
    expect(screen.queryByText('未识别思考档位')).not.toBeInTheDocument()

    fireEvent.click(screen.getByRole('button', { name: 'DeepSeek-V4.1-Flash-1017：思考设置' }))
    fireEvent.click(screen.getByRole('button', { name: '重新自动识别' }))
    expect(await screen.findByText('默认 高 · 低/高/最高')).toBeInTheDocument()

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.calls[0]?.[0].providers?.[0]?.models[0]).toMatchObject({
      thinkingLevels: ['low', 'high', 'xhigh'],
      thinkingCapability: {
        source: 'heuristic',
        dialect: 'reasoningEffortMax',
        supportsOff: true
      },
      thinkingEnabled: true,
      thinkingDefault: 'high'
    })
  })

  it('auto-detects versioned GPT thinking levels on manual add', async () => {
    const provider = DEFAULT_PUBLIC_SETTINGS.providers[0]!
    const settings: PublicSettings = {
      ...DEFAULT_PUBLIC_SETTINGS,
      providers: [{
        ...provider,
        enabled: true,
        protocol: 'openAiChat',
        keyConfigured: true,
        models: []
      }]
    }
    const updateSettings = vi.fn(async (_update: SettingsUpdate) => settings)
    installDefaultBridge({
      getSettings: vi.fn(async () => settings),
      updateSettings
    } as Partial<WindowTextLensApi>)

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    fireEvent.change(screen.getByPlaceholderText('手动添加模型 ID'), {
      target: { value: 'gpt6-luna' }
    })
    fireEvent.click(screen.getByRole('button', { name: '添加' }))

    expect(await screen.findByText('默认 中 · 最低/低/中/高/最高')).toBeInTheDocument()

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.calls[0]?.[0].providers?.[0]?.models[0]).toMatchObject({
      id: 'gpt6-luna',
      thinkingLevels: ['minimal', 'low', 'medium', 'high', 'xhigh'],
      thinkingCapability: {
        source: 'heuristic',
        dialect: 'reasoningEffort',
        supportsOff: true
      },
      thinkingEnabled: true,
      thinkingDefault: 'medium'
    })
  })
})

describe('SettingsApp filter editing', () => {
  it('keeps raw line breaks while editing the application filter list', async () => {
    const updateSettings = vi.fn(async (_update: SettingsUpdate) => DEFAULT_PUBLIC_SETTINGS)
    installDefaultBridge({ updateSettings })

    render(<SettingsApp />)
    await goToSettingsSection('捕获与过滤')

    fireEvent.change(screen.getByRole('combobox', { name: '过滤模式' }), {
      target: { value: 'whitelist' }
    })
    const textarea = screen.getByRole('textbox', { name: '应用过滤列表' })
    fireEvent.change(textarea, { target: { value: 'line one\n\n  line two  \n' } })

    expect(textarea).toHaveValue('line one\n\n  line two  \n')

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.calls[0]?.[0].filter).toMatchObject({
      mode: 'whitelist',
      applications: ['line one', 'line two']
    })
  })

  it('removes a filter tag and keeps the textarea in sync', async () => {
    installDefaultBridge()
    render(<SettingsApp />)
    await goToSettingsSection('捕获与过滤')

    fireEvent.change(screen.getByRole('combobox', { name: '过滤模式' }), {
      target: { value: 'whitelist' }
    })
    const textarea = screen.getByRole('textbox', { name: '应用过滤列表' })
    fireEvent.change(textarea, { target: { value: 'a.exe\nb.exe' } })
    expect(await screen.findByText('a.exe', { selector: '.filter-tag' })).toBeInTheDocument()

    fireEvent.click(screen.getByRole('button', { name: '移除 a.exe' }))
    await waitFor(() => expect(textarea).toHaveValue('b.exe'))
    expect(screen.queryByText('a.exe', { selector: '.filter-tag' })).not.toBeInTheDocument()
  })
})

describe('SettingsApp safeguards', () => {
  it('asks before saving a dirty draft for a connection test', async () => {
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      ...update,
      providers: (update.providers ?? DEFAULT_PUBLIC_SETTINGS.providers).map((provider) => ({
        ...provider,
        keyConfigured: false
      }))
    }))
    const testProviderConnection = vi.fn(async () => ({ ok: true as const, models: [] }))
    installDefaultBridge({ updateSettings, testProviderConnection })

    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')
    fireEvent.change(await screen.findByLabelText('服务商名称'), {
      target: { value: '我的服务商' }
    })
    fireEvent.click(screen.getByRole('button', { name: '测试连接' }))

    expect(await screen.findByRole('dialog', { name: '测试连接前先保存？' })).toBeInTheDocument()
    expect(updateSettings).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole('button', { name: '保存并继续' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    await waitFor(() => expect(testProviderConnection).toHaveBeenCalled())
  })

  it('focuses the invalid provider URL and shows the message inline', async () => {
    installDefaultBridge()
    render(<SettingsApp />)
    await goToSettingsSection('AI 服务商')

    const baseUrl = await screen.findByRole('textbox', { name: 'API 地址' })
    fireEvent.change(baseUrl, { target: { value: 'ftp://example.com' } })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))

    await waitFor(() => expect(baseUrl).toHaveFocus())
    const inline = document.querySelector('.field-error')
    expect(inline).not.toBeNull()
    expect(inline).toHaveTextContent(/API 地址/)
  })

  it('swaps translation directions explicitly instead of silently adjusting', async () => {
    const updateSettings = vi.fn(async (update: SettingsUpdate): Promise<PublicSettings> => ({
      ...DEFAULT_PUBLIC_SETTINGS,
      ...update,
      providers: (update.providers ?? DEFAULT_PUBLIC_SETTINGS.providers).map((provider) => ({
        ...provider,
        keyConfigured: false
      }))
    }))
    installDefaultBridge({ updateSettings })
    render(<SettingsApp />)
    await goToSettingsSection('语言与翻译')

    expect(screen.getByRole('combobox', { name: '翻译目标语言' })).toHaveValue('zh-CN')
    expect(screen.getByRole('combobox', { name: '翻译默认语言' })).toHaveValue('en-US')

    fireEvent.click(screen.getByRole('button', { name: '交换翻译方向' }))
    expect(screen.getByRole('combobox', { name: '翻译目标语言' })).toHaveValue('en-US')
    expect(screen.getByRole('combobox', { name: '翻译默认语言' })).toHaveValue('zh-CN')

    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1))
    expect(updateSettings.mock.calls[0]?.[0].translate).toEqual({
      defaultLanguage: 'zh-CN',
      targetLanguage: 'en-US'
    })
  })

  it('maps a legacy result section to appearance', async () => {
    window.sessionStorage.setItem('textlens.settings.active-section', 'result')
    try {
      installDefaultBridge()
      render(<SettingsApp />)
      expect(await screen.findByRole('heading', { name: '外观' })).toBeInTheDocument()
      expect(screen.getByRole('button', { name: '外观' })).toHaveAttribute('aria-current', 'page')
    } finally {
      window.sessionStorage.removeItem('textlens.settings.active-section')
    }
  })
})
