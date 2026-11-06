import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { ActionStreamEvent, ResultSessionSnapshot } from '../../shared'

let receiveEvent: ((event: ActionStreamEvent) => void) | null = null
let frameCallbacks = new Map<number, FrameRequestCallback>()
let nextFrameId = 1

const eventBase = {
  sessionId: 'session-1',
  sessionGeneration: 9,
  requestId: 'request-1',
  requestGeneration: 1,
  actionId: 'summary'
} as const

type EventOverrides = Partial<Pick<
  ActionStreamEvent,
  | 'sessionId'
  | 'sessionGeneration'
  | 'requestId'
  | 'requestGeneration'
  | 'actionId'
>>

const started = (sequence = 1, overrides: EventOverrides = {}): ActionStreamEvent => ({
  ...eventBase,
  ...overrides,
  type: 'started',
  sequence
})

const delta = (
  sequence: number,
  value: string,
  overrides: EventOverrides = {}
): ActionStreamEvent => ({
  ...eventBase,
  ...overrides,
  type: 'delta',
  sequence,
  delta: value
})

function snapshot(
  overrides: Partial<ResultSessionSnapshot> = {}
): ResultSessionSnapshot {
  return {
    sessionId: 'session-1',
    sessionGeneration: 9,
    requestId: 'request-1',
    requestGeneration: 1,
    actionId: 'summary',
    providerId: 'openai-compatible',
    modelId: 'model-1',
    selection: {
      selectionId: 'selection-1',
      text: 'hello',
      sourceApp: { name: 'Test', bundleId: null },
      anchor: { kind: 'cursor', x: 10, y: 20 },
      direction: 'unknown',
      isFullscreen: false
    },
    status: 'streaming',
    content: '',
    thinkingContent: '',
    contentScalarCount: 0,
    lastContentSequence: 0,
    lastSequence: 0,
    handshakeGeneration: 4,
    errorMessage: '',
    retryable: false,
    pinned: false,
    ...overrides
  }
}

function emit(event: ActionStreamEvent): void {
  if (!receiveEvent) throw new Error('action event bridge is not listening')
  receiveEvent(event)
}

const thinkingDelta = (
  sequence: number,
  value: string,
  overrides: EventOverrides = {}
): ActionStreamEvent => ({
  ...eventBase,
  ...overrides,
  type: 'thinkingDelta',
  sequence,
  delta: value
})

function runNextFrame(now = 16): void {
  const [id, callback] = frameCallbacks.entries().next().value ?? []
  if (id === undefined || !callback) throw new Error('No frame was scheduled')
  frameCallbacks.delete(id)
  callback(now)
}

beforeEach(() => {
  vi.useFakeTimers()
  vi.resetModules()
  receiveEvent = null
  frameCallbacks = new Map()
  nextFrameId = 1
  vi.spyOn(window, 'requestAnimationFrame').mockImplementation((callback) => {
    const id = nextFrameId++
    frameCallbacks.set(id, callback)
    return id
  })
  vi.spyOn(window, 'cancelAnimationFrame').mockImplementation((id) => {
    frameCallbacks.delete(id)
  })
  Object.defineProperty(window, 'textLens', {
    configurable: true,
    value: {
      onActionEvent: vi.fn((listener: (event: ActionStreamEvent) => void) => {
        receiveEvent = listener
        return () => {
          receiveEvent = null
        }
      })
    }
  })
})

afterEach(() => {
  vi.restoreAllMocks()
  vi.useRealTimers()
  Reflect.deleteProperty(window, 'textLens')
})

describe('action event store publication pacing', () => {
  it('applies every delta synchronously without waiting for rAF', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore()
    const listener = vi.fn()
    const unsubscribe = store.subscribeToActionEvents(listener)

    emit(started())
    expect(listener).toHaveBeenCalledTimes(1)
    emit(delta(2, '一'))
    expect(listener).toHaveBeenCalledTimes(2)
    expect(store.getActionEventSnapshot().content).toBe('一')
    emit(delta(3, '二'))
    expect(listener).toHaveBeenCalledTimes(3)
    expect(store.getActionEventSnapshot().content).toBe('一二')
    // must not require flushPendingActionEvents('frame')
    expect(frameCallbacks.size).toBe(0)

    unsubscribe()
    stop()
  })

  it('publishes many subsequent deltas immediately without frame batching', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore()
    const listener = vi.fn()
    store.subscribeToActionEvents(listener)

    emit(started())
    emit(delta(2, 'A'))
    for (let index = 0; index < 100; index += 1) {
      emit(delta(index + 3, 'x'))
    }
    expect(listener).toHaveBeenCalledTimes(102)
    expect(store.getActionEventSnapshot().content).toBe(`A${'x'.repeat(100)}`)
    expect(frameCallbacks.size).toBe(0)
    stop()
  })

  it('streams thinking deltas immediately without changing answer integrity counters', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore()
    const listener = vi.fn()
    store.subscribeToActionEvents(listener)

    emit(started())
    emit(thinkingDelta(2, 'think'))
    emit(thinkingDelta(3, '-more'))
    emit(delta(4, 'ans'))

    expect(store.getActionEventSnapshot()).toMatchObject({
      thinkingContent: 'think-more',
      content: 'ans',
      contentScalarCount: 3,
      status: 'streaming'
    })
    // started + 2 thinking + 1 answer
    expect(listener).toHaveBeenCalledTimes(4)
    stop()
  })

  it('publishes a valid terminal once with full content already applied', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore()
    const listener = vi.fn()
    store.subscribeToActionEvents(listener)

    emit(started())
    emit(delta(2, 'A'))
    emit(delta(3, 'B'))
    emit({
      ...eventBase,
      type: 'completed',
      sequence: 4,
      lastContentSequence: 3,
      contentScalarCount: 2
    })

    expect(store.getActionEventSnapshot()).toMatchObject({
      status: 'completed',
      content: 'AB',
      contentScalarCount: 2
    })
    // started + 2 deltas + completed
    expect(listener).toHaveBeenCalledTimes(4)
    expect(frameCallbacks.size).toBe(0)
    stop()
  })

  it('keeps flushPendingActionEvents as a safe no-op after sync publish', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore()

    emit(started())
    emit(delta(2, 'A'))
    emit(delta(3, 'B'))
    expect(store.getActionEventSnapshot().content).toBe('AB')

    window.dispatchEvent(new PageTransitionEvent('pageshow'))
    store.flushPendingActionEvents('native-reveal')
    expect(store.getActionEventSnapshot().content).toBe('AB')
    expect(frameCallbacks.size).toBe(0)
    stop()
  })
})

describe('action event store hydration and recovery', () => {
  it('atomically hydrates, replays a continuous buffered suffix, and ACKs the snapshot watermark', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    const listener = vi.fn()
    store.subscribeToActionEvents(listener)

    emit(delta(12, 'A'))
    emit(delta(13, 'B'))
    const outcome = store.hydrateActionEventStore(snapshot({
      content: 'base',
      contentScalarCount: 4,
      lastContentSequence: 11,
      lastSequence: 11
    }))

    expect(outcome).toEqual({
      ack: {
        sessionId: 'session-1',
        sessionGeneration: 9,
        requestGeneration: 1,
        lastSequence: 11,
        handshakeGeneration: 4
      },
      recoveryNeeded: false
    })
    expect(store.getActionEventSnapshot().content).toBe('baseAB')
    expect(listener).toHaveBeenCalledTimes(2)
    expect(recovery).not.toHaveBeenCalled()
    stop()
  })

  it('discards buffered duplicates at or below the snapshot watermark', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')

    emit(delta(10, 'old-10'))
    emit(delta(11, 'old-11'))
    emit(delta(12, 'new'))
    const outcome = store.hydrateActionEventStore(snapshot({
      content: 'base',
      contentScalarCount: 4,
      lastContentSequence: 11,
      lastSequence: 11
    }))

    expect(outcome.recoveryNeeded).toBe(false)
    expect(store.getActionEventSnapshot().content).toBe('basenew')
    stop()
  })

  it('starts one recovery for a live sequence gap and buffers later events', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    store.hydrateActionEventStore(snapshot({ lastSequence: 13 }))

    emit(delta(15, 'gap'))
    emit(delta(14, 'later-arrival'))

    expect(store.getActionEventSnapshot().content).toBe('')
    expect(recovery).toHaveBeenCalledTimes(1)
    expect(recovery).toHaveBeenCalledWith('sequence-gap')
    stop()
  })

  it('starts resync recovery and buffers subsequent live events', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    store.hydrateActionEventStore(snapshot({ lastSequence: 11 }))

    emit({
      ...eventBase,
      type: 'resyncRequired',
      sequence: 12,
      snapshotLastSequence: 11
    })
    emit(delta(13, 'buffered'))

    expect(recovery).toHaveBeenCalledTimes(1)
    expect(recovery).toHaveBeenCalledWith('resync-required')
    expect(store.getActionEventSnapshot().content).toBe('')
    stop()
  })

  it('retains a higher session generation for recovery even when its sequence is lower', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)

    emit(started(1, { sessionGeneration: 10 }))
    const outcome = store.hydrateActionEventStore(snapshot({
      sessionGeneration: 9,
      lastSequence: 11
    }))

    expect(outcome.recoveryNeeded).toBe(true)
    expect(recovery).not.toHaveBeenCalled()
    stop()
  })

  it('ignores an older request generation even when its sequence is newer', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    store.hydrateActionEventStore(snapshot({ lastSequence: 10 }))

    emit(started(11, { requestId: 'request-2', requestGeneration: 2 }))
    const current = store.getActionEventSnapshot()
    emit({
      ...eventBase,
      requestId: 'request-1',
      requestGeneration: 1,
      type: 'notice',
      sequence: 12,
      code: 'old',
      message: 'old notice'
    })

    expect(store.getActionEventSnapshot()).toBe(current)
    stop()
  })

  it.each([
    { lastContentSequence: 1, contentScalarCount: 1 },
    { lastContentSequence: 2, contentScalarCount: 2 }
  ])('recovers instead of publishing a terminal with mismatched watermarks', async (watermark) => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore(undefined, recovery)

    emit(started())
    emit(delta(2, 'A'))
    emit({
      ...eventBase,
      type: 'completed',
      sequence: 3,
      ...watermark
    })

    expect(store.getActionEventSnapshot().status).toBe('streaming')
    expect(recovery).toHaveBeenCalledTimes(1)
    expect(recovery).toHaveBeenCalledWith('sequence-gap')
    stop()
  })

  it('returns replay recovery intent only after installing a continuous prefix', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)

    emit(delta(12, 'A'))
    emit(delta(14, 'C'))
    const outcome = store.hydrateActionEventStore(snapshot({ lastSequence: 11 }))

    expect(outcome.recoveryNeeded).toBe(true)
    expect(store.getActionEventSnapshot().content).toBe('A')
    expect(recovery).not.toHaveBeenCalled()

    const second = store.hydrateActionEventStore(snapshot({
      content: 'AB',
      contentScalarCount: 2,
      lastContentSequence: 13,
      lastSequence: 13,
      handshakeGeneration: 5
    }))
    expect(second.recoveryNeeded).toBe(false)
    expect(store.getActionEventSnapshot().content).toBe('ABC')
    stop()
  })

  it('validates snapshot scalar counts before publishing any hydration', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    const listener = vi.fn()
    store.subscribeToActionEvents(listener)

    expect(() => store.hydrateActionEventStore(snapshot({
      content: '😀𠮷',
      contentScalarCount: 2,
      lastContentSequence: 2,
      lastSequence: 2
    }))).not.toThrow()
    const valid = store.getActionEventSnapshot()

    expect(() => store.hydrateActionEventStore(snapshot({
      content: '😀𠮷',
      contentScalarCount: 3,
      lastContentSequence: 2,
      lastSequence: 2
    }))).toThrow()
    const understated = snapshot({
      content: `# heading\n${'😀'.repeat(16_384)}`,
      contentScalarCount: 16_384,
      lastContentSequence: 2,
      lastSequence: 2
    }) as ResultSessionSnapshot
    expect(() => store.hydrateActionEventStore(understated)).toThrow()
    expect(store.getActionEventSnapshot()).toBe(valid)
    expect(listener).toHaveBeenCalledTimes(1)
    stop()
  })

  it('rejects a snapshot for a different active session', async () => {
    const store = await import('./actionEventStore')
    const stop = store.startActionEventStore('session-1')
    expect(() => store.hydrateActionEventStore(snapshot({ sessionId: 'session-2' })))
      .toThrow(/session/i)
    stop()
  })
})

describe('action event store ordered notices', () => {
  it('advances only notice sequence state and recovers gaps through hydration', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore(undefined, recovery)
    const listener = vi.fn()
    store.subscribeToActionEvents(listener)

    emit(started(20))
    emit(delta(21, 'A'))
    emit(delta(22, 'B'))
    const beforeNotice = store.getActionEventSnapshot()
    const logicalBeforeNotice = store.getActionEventLogicalRevision()
    expect(beforeNotice.content).toBe('AB')
    expect(frameCallbacks.size).toBe(0)

    emit({
      ...eventBase,
      type: 'notice',
      sequence: 23,
      code: 'fallback',
      message: 'fallback applied'
    })
    const noticed = store.getActionEventSnapshot()
    expect(noticed).toMatchObject({
      content: 'AB',
      contentScalarCount: 2,
      status: 'streaming',
      generationNotice: 'fallback applied'
    })
    expect(noticed).not.toBe(beforeNotice)
    expect(store.getActionEventLogicalRevision()).toBe(logicalBeforeNotice + 1)

    const callsAfterNotice = listener.mock.calls.length
    emit({
      ...eventBase,
      type: 'notice',
      sequence: 23,
      code: 'fallback',
      message: 'fallback applied'
    })
    expect(store.getActionEventSnapshot()).toBe(noticed)
    expect(listener).toHaveBeenCalledTimes(callsAfterNotice)

    emit({
      ...eventBase,
      type: 'notice',
      sequence: 25,
      code: 'gap',
      message: 'gap notice'
    })
    expect(recovery).toHaveBeenCalledWith('sequence-gap')

    const outcome = store.hydrateActionEventStore(snapshot({
      content: 'AB',
      contentScalarCount: 2,
      lastContentSequence: 22,
      lastSequence: 24,
      generationNotice: { code: 'snapshot', message: 'snapshot notice' },
      handshakeGeneration: 5
    }))
    expect(outcome.recoveryNeeded).toBe(false)
    expect(store.getActionEventSnapshot()).toMatchObject({
      content: 'AB',
      contentScalarCount: 2,
      status: 'streaming',
      generationNotice: 'gap notice'
    })

    const afterReplay = store.getActionEventSnapshot()
    emit({
      ...eventBase,
      requestId: 'old-request',
      requestGeneration: 0,
      type: 'notice',
      sequence: 26,
      code: 'old',
      message: 'old notice'
    })
    expect(store.getActionEventSnapshot()).toBe(afterReplay)

    emit(started(26, { requestId: 'request-2', requestGeneration: 2 }))
    expect(store.getActionEventSnapshot()).toMatchObject({
      requestId: 'request-2',
      generationNotice: '',
      content: ''
    })
    stop()
  })
})

describe('action event store replacement preparation contract', () => {
  const oldTranscript = [
    { id: 'session-1:turn:0', role: 'user', content: 'old question', streaming: false, scalarCount: 12 },
    { id: 'session-1:turn:1', role: 'assistant', content: 'old answer', streaming: false, scalarCount: 10 }
  ] satisfies NonNullable<ResultSessionSnapshot['transcript']>

  const nextTranscript = [
    ...oldTranscript,
    { id: 'session-1:turn:2', role: 'user', content: 'next question', streaming: false, scalarCount: 13 },
    { id: 'session-1:turn:3', role: 'assistant', content: '', streaming: true, scalarCount: 0 }
  ] satisfies NonNullable<ResultSessionSnapshot['transcript']>

  const completedSnapshot = (overrides: Partial<ResultSessionSnapshot> = {}) => snapshot({
    status: 'completed',
    content: 'old answer',
    contentScalarCount: 10,
    lastContentSequence: 9,
    lastSequence: 10,
    transcript: oldTranscript,
    ...overrides
  })

  const preparationFailed = (sequence = 11, generation = 2): ActionStreamEvent => ({
    ...eventBase,
    requestId: `request-${generation}`,
    requestGeneration: generation,
    type: 'preparationFailed',
    sequence,
    code: 'PREPARATION_FAILED',
    message: 'provider disabled',
    retryable: false,
    question: 'next question'
  })

  const nextStarted = (sequence = 12, generation = 3): ActionStreamEvent => ({
    ...eventBase,
    requestId: `request-${generation}`,
    requestGeneration: generation,
    type: 'started',
    sequence,
    providerId: 'provider-next',
    modelId: 'model-next',
    question: 'next question',
    transcript: nextTranscript
  })

  const nextCompleted = (sequence = 14, generation = 3): ActionStreamEvent => ({
    ...eventBase,
    requestId: `request-${generation}`,
    requestGeneration: generation,
    type: 'completed',
    sequence,
    lastContentSequence: sequence - 1,
    contentScalarCount: 1
  })

  it('consumes preparation failure sequence while preserving the committed answer and accepts a following request', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    store.hydrateActionEventStore(completedSnapshot())
    const before = store.getActionEventSnapshot()
    const revision = store.getActionEventLogicalRevision()

    emit(preparationFailed())
    const failed = store.getActionEventSnapshot()
    expect(failed).toEqual({
      ...before,
      preparationFailure: {
        requestId: 'request-2', requestGeneration: 2, code: 'PREPARATION_FAILED',
        message: 'provider disabled', retryable: false, question: 'next question'
      }
    })
    expect(failed.transcript).toBe(before.transcript)
    expect(store.getActionEventLogicalRevision()).toBe(revision + 1)
    emit(preparationFailed())
    expect(store.getActionEventSnapshot()).toBe(failed)
    expect(store.getActionEventLogicalRevision()).toBe(revision + 1)

    emit(nextStarted())
    emit(delta(13, '😀', { requestId: 'request-3', requestGeneration: 3 }))
    emit(nextCompleted())
    expect(store.getActionEventSnapshot()).toMatchObject({
      requestId: 'request-3', requestGeneration: 3, providerId: 'provider-next', modelId: 'model-next',
      preparationFailure: null, transcript: nextTranscript, content: '😀', contentScalarCount: 1, status: 'completed'
    })
    expect(recovery).not.toHaveBeenCalled()
    expect(frameCallbacks.size).toBe(0)
    stop()
  })

  it('replays a same-batch failure and success after hydration without creating a sequence gap', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    emit(preparationFailed())
    emit(nextStarted())
    emit(delta(13, '😀', { requestId: 'request-3', requestGeneration: 3 }))
    emit(nextCompleted())

    const outcome = store.hydrateActionEventStore(completedSnapshot())
    expect(outcome).toEqual({
      ack: { sessionId: 'session-1', sessionGeneration: 9, requestGeneration: 1, lastSequence: 10, handshakeGeneration: 4 },
      recoveryNeeded: false
    })
    expect(store.getActionEventSnapshot()).toMatchObject({
      requestId: 'request-3', requestGeneration: 3, status: 'completed', content: '😀', preparationFailure: null, transcript: nextTranscript
    })
    expect(recovery).not.toHaveBeenCalled()
    stop()
  })

  it('acks the committed generation and failure watermark on snapshot recovery while replaying later failures and Started', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    emit(preparationFailed())
    emit(preparationFailed(12, 3))
    emit(nextStarted(13, 4))
    emit(delta(14, '😀', { requestId: 'request-4', requestGeneration: 4 }))
    emit(nextCompleted(15, 4))
    const outcome = store.hydrateActionEventStore(completedSnapshot({
      lastSequence: 11,
      handshakeGeneration: 5,
      preparationFailure: {
        requestId: 'request-2', requestGeneration: 2, code: 'PREPARATION_FAILED',
        message: 'provider disabled', retryable: false, question: 'next question'
      }
    }))
    expect(outcome.ack).toMatchObject({ requestGeneration: 1, lastSequence: 11, handshakeGeneration: 5 })
    expect(outcome.recoveryNeeded).toBe(false)
    expect(store.getActionEventSnapshot()).toMatchObject({
      requestId: 'request-4', requestGeneration: 4, status: 'completed', content: '😀', preparationFailure: null
    })
    expect(recovery).not.toHaveBeenCalled()
    stop()
  })

  it('recovers a real gap after a failure and replays the continuous tail with the new request watermarks', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    store.hydrateActionEventStore(completedSnapshot())
    emit(preparationFailed())
    emit(nextStarted(13))
    emit(delta(14, '😀', { requestId: 'request-3', requestGeneration: 3 }))
    emit(nextCompleted(15))
    expect(recovery).toHaveBeenCalledTimes(1)
    expect(recovery).toHaveBeenCalledWith('sequence-gap')
    expect(store.getActionEventSnapshot().requestId).toBe('request-1')

    const outcome = store.hydrateActionEventStore(snapshot({
      requestId: 'request-3', requestGeneration: 3, providerId: 'provider-next', modelId: 'model-next',
      lastSequence: 13, transcript: nextTranscript, handshakeGeneration: 5
    }))
    expect(outcome.ack).toMatchObject({ requestGeneration: 3, lastSequence: 13, handshakeGeneration: 5 })
    expect(outcome.recoveryNeeded).toBe(false)
    expect(store.getActionEventSnapshot()).toMatchObject({ requestId: 'request-3', status: 'completed', content: '😀' })
    expect(recovery).toHaveBeenCalledTimes(1)
    stop()
  })

  it.each([false, true])('honors committed-generation resync after a terminal answer (preparation failure received: %s)', async (hasFailure) => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    store.hydrateActionEventStore(completedSnapshot())
    if (hasFailure) emit(preparationFailed())
    const resyncSequence = hasFailure ? 12 : 11
    emit({ ...eventBase, type: 'resyncRequired', sequence: resyncSequence, snapshotLastSequence: resyncSequence - 1 })
    expect(recovery).toHaveBeenCalledTimes(1)
    expect(recovery).toHaveBeenCalledWith('resync-required')
    expect(store.getActionEventSnapshot().content).toBe('old answer')

    emit(nextStarted(resyncSequence + 1))
    emit(delta(resyncSequence + 2, '😀', { requestId: 'request-3', requestGeneration: 3 }))
    emit(nextCompleted(resyncSequence + 3))
    const outcome = store.hydrateActionEventStore(completedSnapshot({
      lastSequence: resyncSequence,
      handshakeGeneration: 5,
      ...(hasFailure ? { preparationFailure: {
        requestId: 'request-2', requestGeneration: 2, code: 'PREPARATION_FAILED',
        message: 'provider disabled', retryable: false, question: 'next question'
      } } : {})
    }))
    expect(outcome.ack).toMatchObject({ requestGeneration: 1, lastSequence: resyncSequence })
    expect(outcome.recoveryNeeded).toBe(false)
    expect(store.getActionEventSnapshot()).toMatchObject({ requestId: 'request-3', status: 'completed', content: '😀' })
    expect(recovery).toHaveBeenCalledTimes(1)
    stop()
  })
})

describe('action event store compacted preparation recovery', () => {
  it('honors a resync marker that skips discarded failed-attempt events and restores their snapshot metadata', async () => {
    const store = await import('./actionEventStore')
    const recovery = vi.fn()
    const stop = store.startActionEventStore('session-1', recovery)
    const completed = snapshot({ status: 'completed', content: 'old', contentScalarCount: 3, lastContentSequence: 9, lastSequence: 10 })
    store.hydrateActionEventStore(completed)
    // Failed generations 2 and 3 occupied sequences 11 and 12, but the native
    // pending queue compacted them into its authoritative snapshot.
    emit({ ...eventBase, type: 'resyncRequired', sequence: 13, snapshotLastSequence: 12 })
    expect(recovery).toHaveBeenCalledTimes(1)
    expect(recovery).toHaveBeenCalledWith('resync-required')
    const preparationFailure = {
      requestId: 'request-3', requestGeneration: 3, code: 'PREPARATION_FAILED',
      message: 'provider disabled', retryable: false, question: 'restore this input'
    }
    const outcome = store.hydrateActionEventStore({ ...completed, lastSequence: 13, handshakeGeneration: 5, preparationFailure })
    expect(outcome).toEqual({
      ack: { sessionId: 'session-1', sessionGeneration: 9, requestGeneration: 1, lastSequence: 13, handshakeGeneration: 5 },
      recoveryNeeded: false
    })
    expect(store.getActionEventSnapshot()).toMatchObject({ requestId: 'request-1', requestGeneration: 1, status: 'completed', content: 'old', preparationFailure })
    emit(started(14, { requestId: 'request-4', requestGeneration: 4 }))
    expect(store.getActionEventSnapshot()).toMatchObject({ requestId: 'request-4', preparationFailure: null, status: 'streaming' })
    expect(recovery).toHaveBeenCalledTimes(1)
    stop()
  })
})
