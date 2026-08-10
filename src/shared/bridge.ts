import type {
  ActionStreamEvent,
  HistoryEntry,
  Point,
  ProviderCreateInput,
  ProviderModel,
  ProviderUpdateInput,
  PublicSettings,
  ResultReadyAck,
  ResultRendererMarker,
  ResultSessionSnapshot,
  SelectionPayload,
  SettingsUpdate,
  ToolbarDismissedEvent,
  ToolbarPointerEvent,
  ToolbarSize,
  TranslationLanguage
} from './schemas'

export type Unsubscribe = () => void

export interface RuntimeDiagnostics {
  selectionMonitorError?: string
  shortcutError?: string
}

export interface AccessibilityStatus {
  platform: 'darwin' | 'windows' | 'unsupported'
  trusted: boolean
  canRequest: boolean
  /** Whether selection capture is usable, including runtime hook startup. */
  available: boolean
  /** Replayable, non-sensitive failures retained by the native runtime. */
  diagnostics: RuntimeDiagnostics
}

export type ConnectionTestResult =
  | { ok: true; models: ProviderModel[] }
  | { ok: false; message: string; status?: number }

export type ProviderModelsSyncResult =
  | { ok: true; models: ProviderModel[]; settings?: PublicSettings }
  | { ok: false; message: string; status?: number }

export type RunActionResult =
  | { accepted: true; sessionId?: string; requestId?: string }
  | { accepted: false; message: string }

export interface ActionRetryOptions {
  targetLanguage?: TranslationLanguage
  providerId?: string
  modelId?: string
}


export type SettingsFocusSection = 'actions' | 'providers'

export interface OpenSettingsOptions {
  focus?: SettingsFocusSection | string
  notice?: string
}

export interface SettingsGuidance {
  focus?: SettingsFocusSection | string
  notice?: string
}

export interface WindowTextLensApi {
  getSettings(): Promise<PublicSettings>
  settingsReady?(): Promise<void>
  updateSettings(update: SettingsUpdate): Promise<PublicSettings>
  resetResultSize?(): Promise<PublicSettings>
  createProvider?(input: ProviderCreateInput): Promise<PublicSettings>
  updateProvider?(providerId: string, update: ProviderUpdateInput): Promise<PublicSettings>
  deleteProvider?(providerId: string): Promise<PublicSettings>
  setProviderApiKey?(providerId: string, apiKey: string): Promise<PublicSettings>
  clearProviderApiKey?(providerId: string): Promise<PublicSettings>
  /** Settings window only: load a saved provider key for masked display. */
  getProviderApiKey?(providerId: string): Promise<string | null>
  testProviderConnection?(providerId: string): Promise<ConnectionTestResult>
  /** List remote models without writing settings (selective pick). */
  listProviderModels?(providerId: string): Promise<ConnectionTestResult>
  syncProviderModels?(providerId: string): Promise<ProviderModelsSyncResult>
  /** @deprecated Use setProviderApiKey. */
  setApiKey?(apiKey: string): Promise<PublicSettings>
  /** @deprecated Use clearProviderApiKey. */
  clearApiKey?(): Promise<PublicSettings>
  /** @deprecated Use testProviderConnection. */
  testConnection?(): Promise<ConnectionTestResult>
  getAccessibilityStatus(): Promise<AccessibilityStatus>
  requestAccessibility(): Promise<AccessibilityStatus>
  getCurrentSelection?(): Promise<SelectionPayload | null>
  /** Windows-only hidden prepare/visible commit handshake for the toolbar. */
  presentToolbar?(selectionId: string, size: ToolbarSize): Promise<boolean>
  /** Internal Windows-only recovery for an unresponsive singleton toolbar. */
  recoverToolbar?(selectionId: string): Promise<boolean>
  runAction(
    actionId: string,
    cursor?: Point,
    selectionId?: string,
    searchEngineId?: string
  ): Promise<RunActionResult>
  hideToolbar(selectionId?: string): Promise<void>
  reportToolbarSize(size: ToolbarSize, selectionId?: string): Promise<void>
  beginResultReady(sessionId: string): Promise<ResultSessionSnapshot>
  ackResultReady(ack: ResultReadyAck): Promise<boolean>
  recordResultRendererMarker(
    sessionId: string,
    requestId: string,
    marker: ResultRendererMarker
  ): Promise<void>
  /** Internal result-window reveal handshake. */
  prepareResultReveal?(sessionId: string): Promise<void>
  /** Internal result-window reveal handshake. */
  commitResultReveal?(sessionId: string): Promise<void>
  /** Reports a renderer hydration failure before the result becomes visible. */
  failResultReveal?(sessionId: string, message: string): Promise<void>
  setResultPinned?(sessionId: string, pinned: boolean): Promise<boolean | void>
  setResultPointerInside?(sessionId: string, inside: boolean): Promise<void>
  showResultSelection?(sessionId: string, text: string, cursor: Point): Promise<void>
  /** Result window only: dismiss toolbar opened from an in-result selection. */
  hideResultSelection?(sessionId: string): Promise<void>
  cancelAction(sessionId?: string): Promise<void>
  retryAction(
    sessionId?: string,
    options?: ActionRetryOptions
  ): Promise<RunActionResult>
  continueAction?(sessionId: string, question: string): Promise<RunActionResult>
  copyText(text: string): Promise<void>
  saveHistoryEntry(entry: HistoryEntry): Promise<void>
  listHistory(retention: string): Promise<HistoryEntry[]>
  deleteHistory(ids: string[]): Promise<void>
  clearHistory(): Promise<void>
  exportHistory(ids: string[], format: string, merge: boolean): Promise<string>
  openExternal(url: string): Promise<void>
  openSettings?(options?: OpenSettingsOptions): Promise<void>
  /** Settings window only: consume one-shot open guidance (focus + notice). */
  takeSettingsGuidance?(): Promise<SettingsGuidance | null>
  hideResult?(sessionId?: string): Promise<void>
  closeResult(sessionId?: string): Promise<void>
  quitApp?(): Promise<void>
  onSelection(listener: (selection: SelectionPayload) => void): Unsubscribe
  onActionEvent(listener: (event: ActionStreamEvent) => void): Unsubscribe
  onSettingsChanged(listener: (settings: PublicSettings) => void): Unsubscribe
  onSettingsCloseRequested?(listener: () => void): Unsubscribe
  onSettingsGuidance?(listener: (guidance: SettingsGuidance) => void): Unsubscribe
  /**
   * Native pointer movement for the non-activating selection toolbar.
   * Optional so a renderer can still hot-reload against an older backend.
   */
  onToolbarPointer?(listener: (event: ToolbarPointerEvent) => void): Unsubscribe
  /**
   * Native force-dismiss of the selection toolbar (outside click / Escape).
   * Optional so a renderer can still hot-reload against an older backend.
   */
  onToolbarDismissed?(listener: (payload: ToolbarDismissedEvent) => void): Unsubscribe
  /** 历史记录发生变化（有新回答保存）时触发。 */
  onHistoryChanged?(listener: () => void): Unsubscribe
}

declare global {
  interface Window {
    textLens: WindowTextLensApi
    /** @deprecated Use textLens. */
    selectionBar: WindowTextLensApi
  }
}
