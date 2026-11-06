import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { DEFAULT_PUBLIC_SETTINGS, type PublicSettings, type SettingsUpdate, type WindowTextLensApi } from '../../shared'
import { SettingsApp } from './SettingsApp'

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
  let resolve!: (value: T) => void
  return { promise: new Promise<T>((accept) => { resolve = accept }), resolve: (value) => resolve(value) }
}

function bridge(initial: PublicSettings = structuredClone(DEFAULT_PUBLIC_SETTINGS), overrides: Partial<WindowTextLensApi> = {}): void {
  let stored = initial
  window.textLens = {
    getSettings: vi.fn(async () => structuredClone(stored)),
    updateSettings: vi.fn(async (update: SettingsUpdate) => {
      stored = { ...stored, ...update, providers: (update.providers ?? stored.providers).map((provider) => ({
        ...provider, keyConfigured: stored.providers.find((previous) => previous.id === provider.id)?.keyConfigured ?? false
      })) }
      return structuredClone(stored)
    }),
    setProviderApiKey: vi.fn(async (id: string) => {
      stored = { ...stored, providers: stored.providers.map((provider) => provider.id === id ? { ...provider, keyConfigured: true } : provider) }
      return structuredClone(stored)
    }),
    getAccessibilityStatus: vi.fn(async () => ({ platform: 'windows', trusted: true, canRequest: false, available: true, diagnostics: {} })),
    onSettingsChanged: vi.fn(() => () => undefined),
    onSettingsCloseRequested: vi.fn(() => () => undefined),
    ...overrides
  } as WindowTextLensApi
}

async function section(name: string): Promise<void> {
  await screen.findByRole('navigation', { name: '设置分区' })
  fireEvent.click(screen.getByRole('button', { name }))
}

describe('settings asynchronous transactions', () => {
  it('retains newer form edits and saves them on the next submission', async () => {
    const pending = deferred<PublicSettings>()
    bridge()
    const update = vi.mocked(window.textLens.updateSettings)
    update.mockImplementationOnce(() => pending.promise)
    render(<SettingsApp />)
    await section('AI 服务商')
    const name = screen.getByRole('textbox', { name: '服务商名称' })
    fireEvent.change(name, { target: { value: '已提交的名称' } })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    fireEvent.change(name, { target: { value: '保存期间的新名称' } })
    await act(async () => pending.resolve({ ...DEFAULT_PUBLIC_SETTINGS, providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({ ...provider, name: '已提交的名称' })) }))
    expect(name).toHaveValue('保存期间的新名称')
    expect(screen.getByText('有尚未保存的更改')).toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await screen.findByText('所有更改均已保存')
    expect(update.mock.calls[1]?.[0].providers?.[0]?.name).toBe('保存期间的新名称')
    expect(name).toHaveValue('保存期间的新名称')
  })

  it('keeps a Key pasted during an ordinary save dirty until the next save writes it', async () => {
    const pending = deferred<PublicSettings>()
    bridge()
    vi.mocked(window.textLens.updateSettings).mockImplementationOnce(() => pending.promise)
    render(<SettingsApp />)
    await section('AI 服务商')
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    const input = document.querySelector<HTMLInputElement>('input[type="password"]')!
    fireEvent.change(input, { target: { value: 'sk-new-input' } })
    await act(async () => pending.resolve(structuredClone(DEFAULT_PUBLIC_SETTINGS)))
    expect(input).toHaveValue('sk-new-input')
    expect(window.textLens.setProviderApiKey).not.toHaveBeenCalled()
    expect(screen.getByText('有尚未保存的更改')).toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await screen.findByText('所有更改均已保存')
    expect(window.textLens.setProviderApiKey).toHaveBeenCalledWith(DEFAULT_PUBLIC_SETTINGS.providers[0]!.id, 'sk-new-input')
    expect(input).toHaveValue('')
  })

  it('acknowledges only the submitted Key when a newer value is typed during its write', async () => {
    const pending = deferred<PublicSettings>()
    bridge()
    vi.mocked(window.textLens.setProviderApiKey!).mockImplementationOnce(() => pending.promise)
    render(<SettingsApp />)
    await section('AI 服务商')
    const input = document.querySelector<HTMLInputElement>('input[type="password"]')!
    fireEvent.change(input, { target: { value: 'sk-submitted' } })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await waitFor(() => expect(window.textLens.setProviderApiKey).toHaveBeenCalledOnce())
    fireEvent.change(input, { target: { value: 'sk-newer' } })
    await act(async () => pending.resolve({ ...DEFAULT_PUBLIC_SETTINGS, providers: DEFAULT_PUBLIC_SETTINGS.providers.map((provider) => ({ ...provider, keyConfigured: true })) }))
    expect(input).toHaveValue('sk-newer')
    expect(screen.getByText('有尚未保存的更改')).toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await screen.findByText('所有更改均已保存')
    expect(window.textLens.setProviderApiKey).toHaveBeenLastCalledWith(DEFAULT_PUBLIC_SETTINGS.providers[0]!.id, 'sk-newer')
  })

  it('retries only unsaved Keys after a partial failure and displays a native string error', async () => {
    const initial = structuredClone(DEFAULT_PUBLIC_SETTINGS)
    initial.providers.push({ ...initial.providers[0]!, id: 'provider-two', name: '服务商二' })
    bridge(initial)
    const write = vi.mocked(window.textLens.setProviderApiKey!)
    const successfulWrite = write.getMockImplementation()!
    write.mockImplementationOnce(successfulWrite).mockRejectedValueOnce('磁盘空间不足')
    render(<SettingsApp />)
    await section('AI 服务商')
    const inputs = document.querySelectorAll<HTMLInputElement>('input[type="password"]')
    fireEvent.change(inputs[0]!, { target: { value: 'sk-retry' } })
    fireEvent.change(inputs[1]!, { target: { value: 'sk-second' } })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await screen.findByText(/API Key 保存失败：磁盘空间不足/)
    expect(inputs[0]).toHaveValue('')
    expect(inputs[1]).toHaveValue('sk-second')
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await screen.findByText('所有更改均已保存')
    expect(write).toHaveBeenCalledTimes(3)
  })

  it('rejects a reveal response after the Key has been cleared', async () => {
    const pending = deferred<string | null>()
    const initial = structuredClone(DEFAULT_PUBLIC_SETTINGS)
    initial.providers[0]!.keyConfigured = true
    bridge(initial, { getProviderApiKey: vi.fn(() => pending.promise), clearProviderApiKey: vi.fn(async () => DEFAULT_PUBLIC_SETTINGS) })
    render(<SettingsApp />)
    await section('AI 服务商')
    fireEvent.click(screen.getByRole('button', { name: '显示 API Key' }))
    fireEvent.click(screen.getByRole('button', { name: '清除密钥' }))
    await screen.findByText(/已清除.*的密钥/)
    await act(async () => pending.resolve('sk-deleted'))
    expect(screen.queryByDisplayValue('sk-deleted')).not.toBeInTheDocument()
    expect(document.querySelector('input[type="password"]')).toHaveValue('')
    expect(screen.getByRole('button', { name: '显示 API Key' })).toBeEnabled()
  })

  it('cancels a reveal when the user starts typing a replacement Key', async () => {
    const pending = deferred<string | null>()
    const initial = structuredClone(DEFAULT_PUBLIC_SETTINGS)
    initial.providers[0]!.keyConfigured = true
    bridge(initial, { getProviderApiKey: vi.fn(() => pending.promise) })
    render(<SettingsApp />)
    await section('AI 服务商')
    fireEvent.click(screen.getByRole('button', { name: '显示 API Key' }))
    const input = document.querySelector('input[type="password"]')!
    fireEvent.change(input, { target: { value: 'sk-replacement' } })
    await act(async () => pending.resolve('sk-old'))
    expect(input).toHaveValue('sk-replacement')
    expect(input).toHaveAttribute('type', 'password')
    expect(screen.getByText('有尚未保存的更改')).toBeInTheDocument()
  })

  it('retains a replacement Key typed while clearing the saved Key', async () => {
    const pending = deferred<PublicSettings>()
    const initial = structuredClone(DEFAULT_PUBLIC_SETTINGS)
    initial.providers[0]!.keyConfigured = true
    bridge(initial, { clearProviderApiKey: vi.fn(() => pending.promise) })
    render(<SettingsApp />)
    await section('AI 服务商')
    fireEvent.click(screen.getByRole('button', { name: '清除密钥' }))
    const input = document.querySelector('input[type="password"]')!
    fireEvent.change(input, { target: { value: 'sk-new-after-clear' } })
    await act(async () => pending.resolve(structuredClone(DEFAULT_PUBLIC_SETTINGS)))
    expect(input).toHaveValue('sk-new-after-clear')
    expect(screen.getByText('有尚未保存的更改')).toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await screen.findByText('所有更改均已保存')
    expect(window.textLens.setProviderApiKey).toHaveBeenCalledWith(initial.providers[0]!.id, 'sk-new-after-clear')
  })

  it('invalidates a delayed reveal before saving an ordinary edit', async () => {
    const pending = deferred<string | null>()
    const initial = structuredClone(DEFAULT_PUBLIC_SETTINGS)
    initial.providers[0]!.keyConfigured = true
    bridge(initial, { getProviderApiKey: vi.fn(() => pending.promise) })
    render(<SettingsApp />)
    await section('AI 服务商')
    fireEvent.click(screen.getByRole('button', { name: '显示 API Key' }))
    fireEvent.change(screen.getByRole('textbox', { name: '服务商名称' }), { target: { value: '已保存名称' } })
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    await screen.findByText('所有更改均已保存')
    await act(async () => pending.resolve('sk-stale-reveal'))
    expect(screen.queryByDisplayValue('sk-stale-reveal')).not.toBeInTheDocument()
    expect(document.querySelector('input[type="password"]')).toHaveValue('')
  })
})

describe('settings editing and keyboard navigation', () => {
  it('keeps a capture-rule input and caret focused across typing and an adjacent deletion', async () => {
    bridge()
    render(<SettingsApp />)
    await section('捕获与过滤')
    fireEvent.click(screen.getByRole('button', { name: '添加程序' }))
    const rule = screen.getAllByRole('textbox', { name: /程序规则/ }).at(-1) as HTMLInputElement
    rule.focus()
    for (const value of ['w', 'wp', 'wps.exe']) {
      fireEvent.change(rule, { target: { value } })
      expect(screen.getByDisplayValue(value)).toBe(rule)
      expect(rule).toHaveFocus()
    }
    fireEvent.click(screen.getByRole('button', { name: '移除 acrobat.exe 规则' }))
    expect(screen.getByDisplayValue('wps.exe')).toBe(rule)
    expect(rule).toHaveFocus()
  })

  it('navigates to an invalid field before focusing it when saved from another section', async () => {
    bridge()
    render(<SettingsApp />)
    await section('AI 服务商')
    fireEvent.change(screen.getByRole('textbox', { name: 'API 地址' }), { target: { value: 'ftp://bad.example' } })
    await section('常规')
    fireEvent.click(screen.getByRole('button', { name: '保存设置' }))
    expect(await screen.findByRole('textbox', { name: 'API 地址' })).toHaveFocus()
    expect(screen.getByRole('button', { name: 'AI 服务商' })).toHaveAttribute('aria-current', 'page')
  })

  it('focuses the model picker and restores its trigger after Escape', async () => {
    bridge(undefined, { listProviderModels: vi.fn(async () => ({ ok: true as const, models: [{ id: 'model-a', name: 'model-a', thinkingLevels: [] }] })) })
    render(<SettingsApp />)
    await section('AI 服务商')
    const trigger = screen.getByRole('button', { name: '获取模型' })
    trigger.focus()
    fireEvent.click(trigger)
    expect(await screen.findByRole('textbox', { name: '搜索模型' })).toHaveFocus()
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(screen.queryByRole('dialog', { name: '选择模型' })).not.toBeInTheDocument()
    expect(trigger).toHaveFocus()
  })
})
