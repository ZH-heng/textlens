import {
  countUnicodeScalars,
  type ActionStreamEvent,
  type ResultReadyAck,
  type ResultSessionSnapshot,
  type Unsubscribe
} from '../../shared'
import {
  appendResultDeltaBatch,
  appendResultThinkingBatch,
  INITIAL_RESULT_STATE,
  reduceActionEvent,
  resultStateFromSnapshot,
  type ResultState
} from './resultState'

type StorePhase = 'hydrating' | 'live' | 'recovering'
export type ActionEventRecoveryReason = 'sequence-gap' | 'resync-required'
export type ActionEventFlushReason =
  | 'frame'
  | 'timeout'
  | 'visibility'
  | 'pageshow'
  | 'native-reveal'

interface LogicalCursor {
  sessionGeneration: number
  requestGeneration: number
  latestRequestGeneration: number
  lastSequence: number
  lastContentSequence: number
  contentScalarCount: number
}

export interface ResultHydrationOutcome {
  ack: ResultReadyAck
  recoveryNeeded: boolean
}

let publishedSnapshot = INITIAL_RESULT_STATE
let phase: StorePhase = 'hydrating'
let cursor: LogicalCursor | null = null
let bufferedEvents: ActionStreamEvent[] = []
let logicalRevision = 0
let hasPublishedNonEmptyContent = false

let stopBridgeListener: Unsubscribe | null = null
let activeSessionId: string | null = null
let onRecoveryRequired: ((reason: ActionEventRecoveryReason) => void) | null = null
const listeners = new Set<() => void>()

function notifySubscribers(): void {
  for (const listener of listeners) listener()
}

/**
 * Compatibility flush: deltas now publish synchronously, so this is a no-op
 * for content. Kept so pageshow/visibility/native-reveal call sites stay valid.
 */
export function flushPendingActionEvents(_reason: ActionEventFlushReason): void {
  // no-op: content deltas are applied immediately in acceptDelta
}

function publishReducedEvent(event: ActionStreamEvent, notify: boolean): boolean {
  const next = reduceActionEvent(publishedSnapshot, event)
  if (next === publishedSnapshot) return false
  publishedSnapshot = next
  if (notify) notifySubscribers()
  return true
}

function beginRecovery(
  reason: ActionEventRecoveryReason,
  event: ActionStreamEvent,
  reportRecovery: boolean
): void {
  if (phase !== 'live') {
    bufferedEvents.push(event)
    return
  }
  phase = 'recovering'
  bufferedEvents.push(event)
  if (reportRecovery) onRecoveryRequired?.(reason)
}

function acceptInitialStarted(
  event: Extract<ActionStreamEvent, { type: 'started' }>,
  notify: boolean
): void {
  cursor = {
    sessionGeneration: event.sessionGeneration,
    requestGeneration: event.requestGeneration,
    latestRequestGeneration: event.requestGeneration,
    lastSequence: event.sequence,
    lastContentSequence: 0,
    contentScalarCount: 0
  }
  logicalRevision += 1
  hasPublishedNonEmptyContent = false
  publishReducedEvent(event, notify)
}

function acceptDelta(
  event: Extract<ActionStreamEvent, { type: 'delta' }>,
  notify: boolean,
  _schedule: boolean
): void {
  if (!cursor) return
  cursor.lastSequence = event.sequence
  logicalRevision += 1
  if (!event.delta) return

  cursor.lastContentSequence = event.sequence
  cursor.contentScalarCount += countUnicodeScalars(event.delta)

  // Always sync-apply first and later tokens (no rAF / grapheme batching).
  const next = appendResultDeltaBatch(publishedSnapshot, {
    requestId: event.requestId,
    sessionGeneration: event.sessionGeneration,
    requestGeneration: event.requestGeneration,
    delta: event.delta,
    contentScalarCount: cursor.contentScalarCount
  })
  hasPublishedNonEmptyContent = true
  if (next !== publishedSnapshot) {
    publishedSnapshot = next
    if (notify) notifySubscribers()
  }
}

function acceptThinkingDelta(
  event: Extract<ActionStreamEvent, { type: 'thinkingDelta' }>,
  notify: boolean
): void {
  if (!cursor) return
  cursor.lastSequence = event.sequence
  logicalRevision += 1
  if (!event.delta) return

  // Thinking advances sequence only — not answer integrity counters.
  const next = appendResultThinkingBatch(publishedSnapshot, {
    requestId: event.requestId,
    sessionGeneration: event.sessionGeneration,
    requestGeneration: event.requestGeneration,
    delta: event.delta
  })
  if (next !== publishedSnapshot) {
    publishedSnapshot = next
    if (notify) notifySubscribers()
  }
}

function acceptTerminal(
  event: Extract<ActionStreamEvent, { type: 'completed' | 'cancelled' | 'error' }>,
  notify: boolean,
  reportRecovery: boolean
): void {
  if (!cursor) return
  if (
    event.type === 'completed' &&
    (event.lastContentSequence !== cursor.lastContentSequence ||
      event.contentScalarCount !== cursor.contentScalarCount)
  ) {
    beginRecovery('sequence-gap', event, reportRecovery)
    return
  }

  cursor.lastSequence = event.sequence
  logicalRevision += 1
  const stateBeforeTerminal = publishedSnapshot
  const terminalChanged = publishReducedEvent(event, false)
  if (notify && (terminalChanged || publishedSnapshot !== stateBeforeTerminal)) {
    notifySubscribers()
  }
}

function processLiveEvent(
  event: ActionStreamEvent,
  options: { notify: boolean; reportRecovery: boolean; schedule: boolean }
): void {
  if (!cursor) {
    if (event.type === 'started') {
      acceptInitialStarted(event, options.notify)
    } else {
      beginRecovery('sequence-gap', event, options.reportRecovery)
    }
    return
  }

  if (event.sessionGeneration < cursor.sessionGeneration) return
  if (event.sessionGeneration > cursor.sessionGeneration) {
    beginRecovery('sequence-gap', event, options.reportRecovery)
    return
  }
  // Queue compaction is a session control message. Its identity follows the
  // committed snapshot, even if a newer replacement failed preparation, and
  // it must still recover after that committed request has completed.
  if (event.type === 'resyncRequired') {
    if (event.sequence <= cursor.lastSequence) return
    beginRecovery('resync-required', event, options.reportRecovery)
    return
  }
  if (event.requestGeneration < cursor.latestRequestGeneration) return
  if (event.sequence <= cursor.lastSequence) return
  if (event.sequence !== cursor.lastSequence + 1) {
    beginRecovery('sequence-gap', event, options.reportRecovery)
    return
  }
  if (event.type === 'preparationFailed') {
    if (event.requestGeneration <= cursor.requestGeneration) return
    cursor.lastSequence = event.sequence
    cursor.latestRequestGeneration = event.requestGeneration
    logicalRevision += 1
    publishReducedEvent(event, options.notify)
    return
  }
  if (event.requestGeneration > cursor.requestGeneration && event.type !== 'started') {
    beginRecovery('sequence-gap', event, options.reportRecovery)
    return
  }
  if (
    event.requestGeneration === cursor.requestGeneration &&
    (event.requestId !== publishedSnapshot.requestId ||
      event.actionId !== publishedSnapshot.actionId)
  ) {
    beginRecovery('sequence-gap', event, options.reportRecovery)
    return
  }
  if (
    publishedSnapshot.status !== 'streaming' &&
    event.requestGeneration === cursor.requestGeneration
  ) return

  if (event.type === 'started') {
    if (event.requestGeneration <= cursor.requestGeneration) {
      beginRecovery('sequence-gap', event, options.reportRecovery)
      return
    }
    acceptInitialStarted(event, options.notify)
    return
  }
  if (event.type === 'delta') {
    acceptDelta(event, options.notify, options.schedule)
    return
  }
  if (event.type === 'thinkingDelta') {
    acceptThinkingDelta(event, options.notify)
    return
  }
  if (event.type === 'notice') {
    cursor.lastSequence = event.sequence
    logicalRevision += 1
    publishReducedEvent(event, options.notify)
    return
  }
  acceptTerminal(event, options.notify, options.reportRecovery)
}

function acceptActionEvent(event: ActionStreamEvent): void {
  if (activeSessionId && event.sessionId !== activeSessionId) return
  if (phase !== 'live') {
    bufferedEvents.push(event)
    return
  }
  processLiveEvent(event, { notify: true, reportRecovery: true, schedule: true })
}

function handleVisibilityChange(): void {
  if (document.visibilityState === 'visible') flushPendingActionEvents('visibility')
}

function handlePageShow(): void {
  flushPendingActionEvents('pageshow')
}

function isRecovering(): boolean {
  return phase === 'recovering'
}

/**
 * Installs the bridge listener before React mounts. A session-bound store starts
 * in hydration mode so native events remain ordered behind the ready snapshot.
 */
export function startActionEventStore(
  sessionId?: string | null,
  recoveryCallback?: (reason: ActionEventRecoveryReason) => void
): Unsubscribe {
  activeSessionId = sessionId || null
  onRecoveryRequired = recoveryCallback ?? null
  if (!sessionId && !cursor) phase = 'live'

  if (!stopBridgeListener) {
    const stopActionEvents = window.textLens.onActionEvent(acceptActionEvent)
    window.addEventListener('pageshow', handlePageShow)
    document.addEventListener('visibilitychange', handleVisibilityChange)
    stopBridgeListener = () => {
      stopActionEvents()
      window.removeEventListener('pageshow', handlePageShow)
      document.removeEventListener('visibilitychange', handleVisibilityChange)
    }
  }

  return () => {
    stopBridgeListener?.()
    stopBridgeListener = null
    activeSessionId = null
    onRecoveryRequired = null
  }
}

export function getActionEventLogicalRevision(): number {
  return logicalRevision
}

/** Compatibility alias retained until Task 4 removes the old call site. */
export function getActionEventStoreRevision(): number {
  return getActionEventLogicalRevision()
}

export function hydrateActionEventStore(
  sessionSnapshot: ResultSessionSnapshot,
  _expectedEventRevision?: number
): ResultHydrationOutcome {
  if (activeSessionId && sessionSnapshot.sessionId !== activeSessionId) {
    throw new Error('Result session snapshot does not match the active session')
  }
  if (countUnicodeScalars(sessionSnapshot.content) !== sessionSnapshot.contentScalarCount) {
    throw new Error('Result session snapshot contentScalarCount is invalid')
  }

  const ack: ResultReadyAck = {
    sessionId: sessionSnapshot.sessionId,
    sessionGeneration: sessionSnapshot.sessionGeneration,
    requestGeneration: sessionSnapshot.requestGeneration,
    lastSequence: sessionSnapshot.lastSequence,
    handshakeGeneration: sessionSnapshot.handshakeGeneration
  }
  const eventsWaitingForSnapshot = bufferedEvents

  publishedSnapshot = resultStateFromSnapshot(sessionSnapshot)
  cursor = {
    sessionGeneration: sessionSnapshot.sessionGeneration,
    requestGeneration: sessionSnapshot.requestGeneration,
    latestRequestGeneration: Math.max(
      sessionSnapshot.requestGeneration,
      sessionSnapshot.preparationFailure?.requestGeneration ?? 0
    ),
    lastSequence: sessionSnapshot.lastSequence,
    lastContentSequence: sessionSnapshot.lastContentSequence,
    contentScalarCount: sessionSnapshot.contentScalarCount
  }
  bufferedEvents = []
  phase = 'live'
  hasPublishedNonEmptyContent = sessionSnapshot.contentScalarCount > 0
  logicalRevision += 1
  notifySubscribers()

  const replayCandidates = eventsWaitingForSnapshot.filter((event) => {
    if (event.sessionId !== sessionSnapshot.sessionId) return false
    if (event.sessionGeneration < sessionSnapshot.sessionGeneration) return false
    if (event.sessionGeneration > sessionSnapshot.sessionGeneration) return true
    if (
      event.sessionGeneration === sessionSnapshot.sessionGeneration &&
      event.requestGeneration < sessionSnapshot.requestGeneration
    ) return false
    return event.sequence > sessionSnapshot.lastSequence
  })
  const stateBeforeReplay = publishedSnapshot

  for (let index = 0; index < replayCandidates.length; index += 1) {
    const event = replayCandidates[index]!
    processLiveEvent(event, {
      notify: false,
      reportRecovery: false,
      schedule: false
    })
    if (isRecovering()) {
      bufferedEvents.push(...replayCandidates.slice(index + 1))
      break
    }
  }

  if (publishedSnapshot !== stateBeforeReplay) notifySubscribers()

  return { ack, recoveryNeeded: isRecovering() }
}

export function subscribeToActionEvents(listener: () => void): Unsubscribe {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

export function getActionEventSnapshot(): ResultState {
  return publishedSnapshot
}
