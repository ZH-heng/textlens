/** Live Tauri invoke command names used by the renderer bridge. */
export const TAURI_COMMANDS = {
  getSettings: 'get_settings',
  settingsReady: 'settings_ready',
  updateSettings: 'update_settings',
  resetResultSize: 'reset_result_size',
  createProvider: 'create_provider',
  updateProvider: 'update_provider',
  deleteProvider: 'delete_provider',
  setProviderApiKey: 'set_provider_api_key',
  clearProviderApiKey: 'clear_provider_api_key',
  getProviderApiKey: 'get_provider_api_key',
  testProviderConnection: 'test_provider_connection',
  listProviderModels: 'list_provider_models',
  syncProviderModels: 'sync_provider_models',
  getAccessibilityStatus: 'get_accessibility_status',
  requestAccessibility: 'request_accessibility',
  toolbarReady: 'toolbar_ready',
  presentToolbar: 'present_toolbar',
  recoverToolbar: 'recover_toolbar',
  runAction: 'run_action',
  hideToolbar: 'hide_toolbar',
  reportToolbarSize: 'report_toolbar_size',
  beginResultReady: 'begin_result_ready',
  ackResultReady: 'ack_result_ready',
  prepareResultReveal: 'prepare_result_reveal',
  commitResultReveal: 'commit_result_reveal',
  failResultReveal: 'fail_result_reveal',
  setResultPinned: 'set_result_pinned',
  setResultPointerInside: 'set_result_pointer_inside',
  showResultSelection: 'show_result_selection',
  hideResultSelection: 'hide_result_selection',
  cancelAction: 'cancel_action',
  retryAction: 'retry_action',
  continueAction: 'continue_action',
  copyText: 'copy_text',
  openExternal: 'open_external',
  openSettings: 'open_settings',
  takeSettingsGuidance: 'take_settings_guidance',
  hideResult: 'hide_result',
  closeResult: 'close_result',
  quitApp: 'quit_app'
} as const

/** Live Tauri event names emitted by the native runtime and listened by the bridge. */
export const TAURI_EVENTS = {
  selection: 'textlens:selection',
  actionStream: 'textlens:action-stream',
  settingsChanged: 'textlens:settings-changed',
  settingsCloseRequested: 'textlens:settings-close-requested',
  settingsGuidance: 'textlens:settings-guidance',
  toolbarPointer: 'textlens:toolbar-pointer',
  toolbarDismissed: 'textlens:toolbar-dismissed'
} as const

export type TauriCommand = (typeof TAURI_COMMANDS)[keyof typeof TAURI_COMMANDS]
export type TauriEvent = (typeof TAURI_EVENTS)[keyof typeof TAURI_EVENTS]
