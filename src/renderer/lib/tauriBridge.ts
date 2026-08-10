import { invoke } from '@tauri-apps/api/core'
import { listen, type Event } from '@tauri-apps/api/event'

import {
  actionStreamEventSchema,
  historyEntryListSchema,
  providerModelSchema,
  publicSettingsSchema,
  resultReadyAckSchema,
  rendererMarkerIdSchema,
  resultRendererMarkerSchema,
  resultSessionSnapshotSchema,
  selectionPayloadSchema,
  toolbarDismissedEventSchema,
  toolbarPointerEventSchema,
  TAURI_COMMANDS,
  TAURI_EVENTS,
  type AccessibilityStatus,
  type ActionStreamEvent,
  type ConnectionTestResult,
  type HistoryEntry,
  type Point,
  type ActionRetryOptions,
  type ProviderCreateInput,
  type ProviderModelsSyncResult,
  type ProviderUpdateInput,
  type PublicSettings,
  type ResultReadyAck,
  type ResultRendererMarker,
  type ResultSessionSnapshot,
  type RunActionResult,
  type SelectionPayload,
  type OpenSettingsOptions,
  type SettingsGuidance,
  type SettingsUpdate,
  type SupportedLocale,
  type ToolbarDismissedEvent,
  type ToolbarPointerEvent,
  type ToolbarSize,
  type Unsubscribe,
  type WindowTextLensApi
} from '../../shared'

const EVENTS = TAURI_EVENTS

const SELECTION_LISTEN_RETRY_DELAYS_MS = [0, 16, 64] as const

function rendererErrorClass(error: unknown): string {
  if (error instanceof TypeError) return 'TypeError'
  if (error instanceof RangeError) return 'RangeError'
  if (error instanceof DOMException) return 'DOMException'
  if (error instanceof Error) return 'Error'
  if (error === null) return 'null'
  return typeof error
}

function waitForRetry(delayMs: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, delayMs))
}


function parseSettingsGuidance(value: unknown): SettingsGuidance {
  const record = value && typeof value === 'object' ? value as Record<string, unknown> : {}
  const focus = typeof record.focus === 'string' ? record.focus.trim() : ''
  const notice = typeof record.notice === 'string' ? record.notice.trim() : ''
  return {
    ...(focus ? { focus } : {}),
    ...(notice ? { notice } : {})
  }
}

class EventHub<T> {
  private readonly listeners = new Set<(value: T) => void>()
  private readonly readyPromise: Promise<void>
  private error: unknown = null

  constructor(
    eventName: string,
    parse: (payload: unknown) => T,
    retryDelaysMs: readonly number[] = [0]
  ) {
    const forward = (event: Event<unknown>): void => {
      let value: T
      try {
        value = parse(event.payload)
      } catch {
        console.error(`[TextLens] ignored invalid ${eventName} event payload`)
        return
      }
      for (const listener of this.listeners) {
        try {
          listener(value)
        } catch {
          // One renderer subscriber must not prevent other windows/components
          // from receiving the same validated native event.
          console.error(`[TextLens] ${eventName} event listener failed`)
        }
      }
    }
    this.readyPromise = this.install(eventName, forward, retryDelaysMs)
  }

  private async install(
    eventName: string,
    forward: (event: Event<unknown>) => void,
    retryDelaysMs: readonly number[]
  ): Promise<void> {
    const attempts = retryDelaysMs.length > 0 ? retryDelaysMs : [0]
    for (let index = 0; index < attempts.length; index += 1) {
      const delayMs = attempts[index] ?? 0
      if (delayMs > 0) await waitForRetry(delayMs)
      try {
        await listen(eventName, forward)
        return
      } catch (error: unknown) {
        console.warn('[TextLens][renderer]', {
          stage: `event-listen:${index + 1}`,
          eventId: eventName,
          errorClass: rendererErrorClass(error)
        })
        if (index === attempts.length - 1) this.error = error
      }
    }
  }

  subscribe(listener: (value: T) => void): Unsubscribe {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  async ready(): Promise<void> {
    await this.readyPromise
    if (this.error) throw this.error
  }
}

function parseConnectionResult(value: unknown): ConnectionTestResult {
  const record = value && typeof value === 'object' ? value as Record<string, unknown> : {}
  const models = providerModelSchema.array().parse(record.models ?? [])
  if (record.ok === true) return { ok: true, models }
  return {
    ok: false,
    message: typeof record.message === 'string' ? record.message : '连接测试失败',
    ...(typeof record.status === 'number' ? { status: record.status } : {})
  }
}

function parseSyncResult(value: unknown): ProviderModelsSyncResult {
  const record = value && typeof value === 'object' ? value as Record<string, unknown> : {}
  const models = providerModelSchema.array().parse(record.models ?? [])
  if (record.ok === true) {
    return {
      ok: true,
      models,
      ...(record.settings ? { settings: publicSettingsSchema.parse(record.settings) } : {})
    }
  }
  return {
    ok: false,
    message: typeof record.message === 'string' ? record.message : '同步模型失败',
    ...(typeof record.status === 'number' ? { status: record.status } : {})
  }
}

function parseRunActionResult(value: unknown): RunActionResult {
  const record = value && typeof value === 'object' ? value as Record<string, unknown> : {}
  if (record.accepted === true) {
    return {
      accepted: true,
      ...(typeof record.sessionId === 'string' ? { sessionId: record.sessionId } : {}),
      ...(typeof record.requestId === 'string' ? { requestId: record.requestId } : {})
    }
  }
  return {
    accepted: false,
    message: typeof record.message === 'string' ? record.message : '动作未被接受'
  }
}

function parseAccessibility(value: unknown): AccessibilityStatus {
  const record = value && typeof value === 'object' ? value as Record<string, unknown> : {}
  const platform = record.platform === 'darwin' || record.platform === 'windows'
    ? record.platform
    : 'unsupported'
  const trusted = record.trusted === true
  const diagnosticsRecord = record.diagnostics && typeof record.diagnostics === 'object'
    ? record.diagnostics as Record<string, unknown>
    : {}
  const diagnostics = {
    ...(typeof diagnosticsRecord.selectionMonitorError === 'string'
      ? { selectionMonitorError: diagnosticsRecord.selectionMonitorError }
      : {}),
    ...(typeof diagnosticsRecord.shortcutError === 'string'
      ? { shortcutError: diagnosticsRecord.shortcutError }
      : {})
  }
  return {
    platform,
    trusted,
    canRequest: record.canRequest === true,
    // Keep the renderer compatible with a 0.3.5 backend during hot reload.
    available: typeof record.available === 'boolean' ? record.available : trusted,
    diagnostics
  }
}

function querySessionId(): string | undefined {
  return new URLSearchParams(window.location.search).get('sessionId') || undefined
}

function requiredSessionId(sessionId?: string): string {
  const resolved = sessionId || querySessionId()
  if (!resolved) throw new Error('结果会话无效')
  return resolved
}

function defineTextLensApi(value: WindowTextLensApi): void {
  if ('textLens' in window) return
  Object.defineProperty(window, 'textLens', {
    configurable: false,
    enumerable: false,
    writable: false,
    value
  })
}

/** Compatibility-only adapter for renderers built before the product rename. */
function defineLegacySelectionBarAlias(value: WindowTextLensApi): void {
  if ('selectionBar' in window) return
  Object.defineProperty(window, 'selectionBar', {
    configurable: false,
    enumerable: false,
    writable: false,
    value
  })
}

/** Installs the narrow renderer API before React mounts. */
export function installTauriBridge(): void {
  if ('textLens' in window && window.textLens) {
    defineLegacySelectionBarAlias(window.textLens)
    return
  }
  if ('selectionBar' in window && window.selectionBar) {
    defineTextLensApi(window.selectionBar)
    return
  }

  const selectionEvents = new EventHub<SelectionPayload>(
    EVENTS.selection,
    (payload) => selectionPayloadSchema.parse(payload),
    SELECTION_LISTEN_RETRY_DELAYS_MS
  )
  const actionEvents = new EventHub<ActionStreamEvent>(EVENTS.actionStream, (payload) =>
    actionStreamEventSchema.parse(payload)
  )
  const settingsEvents = new EventHub<PublicSettings>(EVENTS.settingsChanged, (payload) =>
    publicSettingsSchema.parse(payload)
  )
  const settingsCloseRequestEvents = new EventHub<void>(
    EVENTS.settingsCloseRequested,
    () => undefined
  )
  const settingsGuidanceEvents = new EventHub<SettingsGuidance>(
    EVENTS.settingsGuidance,
    parseSettingsGuidance
  )
  const toolbarPointerEvents = new EventHub<ToolbarPointerEvent>(
    EVENTS.toolbarPointer,
    (payload) => toolbarPointerEventSchema.parse(payload)
  )
  const historyChangedEvents = new EventHub<void>(
    EVENTS.historyChanged,
    () => undefined,
    [0]
  )
  const toolbarDismissedEvents = new EventHub<ToolbarDismissedEvent>(
    EVENTS.toolbarDismissed,
    (payload) => toolbarDismissedEventSchema.parse(payload)
  )

  const api: WindowTextLensApi = {
    async getSettings() {
      return publicSettingsSchema.parse(await invoke(TAURI_COMMANDS.getSettings))
    },
    async settingsReady() {
      await settingsCloseRequestEvents.ready()
      await invoke(TAURI_COMMANDS.settingsReady)
    },
    async updateSettings(update: SettingsUpdate) {
      return publicSettingsSchema.parse(await invoke(TAURI_COMMANDS.updateSettings, { update }))
    },
    async resetResultSize() {
      return publicSettingsSchema.parse(await invoke(TAURI_COMMANDS.resetResultSize))
    },
    async createProvider(input: ProviderCreateInput) {
      return publicSettingsSchema.parse(await invoke(TAURI_COMMANDS.createProvider, { input }))
    },
    async updateProvider(providerId: string, update: ProviderUpdateInput) {
      return publicSettingsSchema.parse(
        await invoke(TAURI_COMMANDS.updateProvider, { providerId, update })
      )
    },
    async deleteProvider(providerId: string) {
      return publicSettingsSchema.parse(await invoke(TAURI_COMMANDS.deleteProvider, { providerId }))
    },
    async setProviderApiKey(providerId: string, apiKey: string) {
      return publicSettingsSchema.parse(
        await invoke(TAURI_COMMANDS.setProviderApiKey, { providerId, apiKey })
      )
    },
    async clearProviderApiKey(providerId: string) {
      return publicSettingsSchema.parse(
        await invoke(TAURI_COMMANDS.clearProviderApiKey, { providerId })
      )
    },
    async getProviderApiKey(providerId: string) {
      const value = await invoke<string | null>(TAURI_COMMANDS.getProviderApiKey, { providerId })
      return value == null || value === '' ? null : value
    },
    async testProviderConnection(providerId: string) {
      return parseConnectionResult(
        await invoke(TAURI_COMMANDS.testProviderConnection, { providerId })
      )
    },
    async listProviderModels(providerId: string) {
      return parseConnectionResult(
        await invoke(TAURI_COMMANDS.listProviderModels, { providerId })
      )
    },
    async syncProviderModels(providerId: string) {
      return parseSyncResult(await invoke(TAURI_COMMANDS.syncProviderModels, { providerId }))
    },
    async getAccessibilityStatus() {
      return parseAccessibility(await invoke(TAURI_COMMANDS.getAccessibilityStatus))
    },
    async requestAccessibility() {
      return parseAccessibility(await invoke(TAURI_COMMANDS.requestAccessibility))
    },
    async getCurrentSelection() {
      await selectionEvents.ready()
      const payload = await invoke<unknown>(TAURI_COMMANDS.toolbarReady)
      return payload == null ? null : selectionPayloadSchema.parse(payload)
    },
    async presentToolbar(selectionId: string, size: ToolbarSize) {
      return await invoke<boolean>(TAURI_COMMANDS.presentToolbar, { selectionId, size })
    },
    async recoverToolbar(selectionId: string) {
      return await invoke<boolean>(TAURI_COMMANDS.recoverToolbar, { selectionId })
    },
    async runAction(
      actionId: string,
      cursor?: Point,
      selectionId?: string,
      searchEngineId?: string
    ) {
      return parseRunActionResult(await invoke(TAURI_COMMANDS.runAction, {
        actionId,
        cursor: cursor ?? null,
        selectionId: selectionId ?? null,
        searchEngineId: searchEngineId ?? null
      }))
    },
    async hideToolbar(selectionId?: string) {
      await invoke(TAURI_COMMANDS.hideToolbar, { selectionId: selectionId ?? null })
    },
    async reportToolbarSize(size: ToolbarSize, selectionId?: string) {
      await invoke(TAURI_COMMANDS.reportToolbarSize, { size, selectionId: selectionId ?? null })
    },
    async beginResultReady(sessionId: string): Promise<ResultSessionSnapshot> {
      await actionEvents.ready()
      return resultSessionSnapshotSchema.parse(
        await invoke(TAURI_COMMANDS.beginResultReady, {
          sessionId: requiredSessionId(sessionId)
        })
      )
    },
    async ackResultReady(ack: ResultReadyAck): Promise<boolean> {
      const parsed = resultReadyAckSchema.parse(ack)
      return await invoke<boolean>(TAURI_COMMANDS.ackResultReady, { ack: parsed })
    },
    async recordResultRendererMarker(
      sessionId: string,
      requestId: string,
      marker: ResultRendererMarker
    ): Promise<void> {
      await invoke(TAURI_COMMANDS.recordResultRendererMarker, {
        sessionId: rendererMarkerIdSchema.parse(sessionId),
        requestId: rendererMarkerIdSchema.parse(requestId),
        marker: resultRendererMarkerSchema.parse(marker)
      })
    },
    async prepareResultReveal(sessionId: string) {
      await invoke(TAURI_COMMANDS.prepareResultReveal, { sessionId: requiredSessionId(sessionId) })
    },
    async commitResultReveal(sessionId: string) {
      await invoke(TAURI_COMMANDS.commitResultReveal, { sessionId: requiredSessionId(sessionId) })
    },
    async failResultReveal(sessionId: string, message: string) {
      await invoke(TAURI_COMMANDS.failResultReveal, {
        sessionId: requiredSessionId(sessionId),
        message
      })
    },
    async setResultPinned(sessionId: string, pinned: boolean) {
      await invoke(TAURI_COMMANDS.setResultPinned, { sessionId, pinned })
    },
    async setResultPointerInside(sessionId: string, inside: boolean) {
      await invoke(TAURI_COMMANDS.setResultPointerInside, { sessionId, inside })
    },
    async showResultSelection(sessionId: string, text: string, cursor: Point) {
      await invoke(TAURI_COMMANDS.showResultSelection, { sessionId, text, cursor })
    },
    async hideResultSelection(sessionId: string) {
      await invoke(TAURI_COMMANDS.hideResultSelection, {
        sessionId: requiredSessionId(sessionId)
      })
    },
    async cancelAction(sessionId?: string) {
      await invoke(TAURI_COMMANDS.cancelAction, { sessionId: requiredSessionId(sessionId) })
    },
    async retryAction(
      sessionId?: string,
      options?: ActionRetryOptions
    ) {
      return parseRunActionResult(await invoke(TAURI_COMMANDS.retryAction, {
        sessionId: requiredSessionId(sessionId),
        targetLanguage: options?.targetLanguage ?? null,
        providerId: options?.providerId ?? null,
        modelId: options?.modelId ?? null
      }))
    },
    async continueAction(sessionId: string, question: string) {
      return parseRunActionResult(await invoke(TAURI_COMMANDS.continueAction, {
        sessionId: requiredSessionId(sessionId),
        question
      }))
    },
    async saveHistoryEntry(entry: HistoryEntry) {
      await invoke(TAURI_COMMANDS.saveHistoryEntry, { entry })
    },
    async listHistory(retention: string) {
      return historyEntryListSchema.parse(await invoke(TAURI_COMMANDS.listHistory, { retention }))
    },
    async deleteHistory(ids: string[]) {
      await invoke(TAURI_COMMANDS.deleteHistory, { ids })
    },
    async clearHistory() {
      await invoke(TAURI_COMMANDS.clearHistory)
    },
    async exportHistory(ids: string[], format: string, merge: boolean) {
      return String(await invoke(TAURI_COMMANDS.exportHistory, { ids, format, merge }))
    },
    async copyText(text: string) {
      await invoke(TAURI_COMMANDS.copyText, { text })
    },
    async openExternal(url: string) {
      await invoke(TAURI_COMMANDS.openExternal, { url })
    },
    async openSettings(options?: OpenSettingsOptions) {
      await invoke(TAURI_COMMANDS.openSettings, { options: options ?? null })
    },
    async takeSettingsGuidance() {
      const value = await invoke(TAURI_COMMANDS.takeSettingsGuidance)
      if (value == null) return null
      return parseSettingsGuidance(value)
    },
    async hideResult(sessionId?: string) {
      await invoke(TAURI_COMMANDS.hideResult, { sessionId: requiredSessionId(sessionId) })
    },
    async closeResult(sessionId?: string) {
      await invoke(TAURI_COMMANDS.closeResult, { sessionId: requiredSessionId(sessionId) })
    },
    async quitApp() {
      await invoke(TAURI_COMMANDS.quitApp)
    },
    onSelection(listener) {
      return selectionEvents.subscribe(listener)
    },
    onActionEvent(listener) {
      return actionEvents.subscribe(listener)
    },
    onSettingsChanged(listener) {
      return settingsEvents.subscribe(listener)
    },
    onSettingsCloseRequested(listener) {
      return settingsCloseRequestEvents.subscribe(listener)
    },
    onSettingsGuidance(listener) {
      return settingsGuidanceEvents.subscribe(listener)
    },
    onToolbarPointer(listener) {
      return toolbarPointerEvents.subscribe(listener)
    },
    onToolbarDismissed(listener) {
      return toolbarDismissedEvents.subscribe(listener)
    },
    onHistoryChanged(listener) {
      return historyChangedEvents.subscribe(listener)
    }
  }

  defineTextLensApi(api)
  defineLegacySelectionBarAlias(api)
}
