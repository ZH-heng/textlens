import type { ActionStreamEvent, ResultSessionSnapshot } from '../../shared'
import {
  appendResultDeltaBatch,
  INITIAL_RESULT_STATE,
  reduceActionEvent,
  resultStateFromSnapshot
} from './resultState'

describe('result action stream reducer', () => {
  const started = (requestGeneration: number, sequence: number): ActionStreamEvent => ({
    type: 'started',
    sessionId: 'session-1',
    sessionGeneration: 3,
    requestId: `request-${requestGeneration}`,
    requestGeneration,
    sequence,
    actionId: 'summary'
  })

  const terminalEvent = (
    type: 'completed' | 'cancelled' | 'error'
  ): ActionStreamEvent => {
    const base = {
      sessionId: 'session-1',
      sessionGeneration: 3,
      requestId: 'request-1',
      requestGeneration: 1,
      sequence: 3,
      actionId: 'summary'
    } as const
    if (type === 'completed') {
      return { ...base, type, lastContentSequence: 2, contentScalarCount: 7 }
    }
    if (type === 'cancelled') return { ...base, type }
    return { ...base, type, code: 'network', message: 'failed', retryable: true }
  }

  const lateDelta: ActionStreamEvent = {
    sessionId: 'session-1',
    sessionGeneration: 3,
    requestId: 'request-1',
    requestGeneration: 1,
    sequence: 4,
    actionId: 'summary',
    type: 'delta',
    delta: 'late'
  }

  const differentTerminal: ActionStreamEvent = {
    sessionId: 'session-1',
    sessionGeneration: 3,
    requestId: 'request-1',
    requestGeneration: 1,
    sequence: 5,
    actionId: 'summary',
    type: 'error',
    code: 'late-error',
    message: 'late error',
    retryable: false
  }

  it.each(['completed', 'cancelled', 'error'] as const)(
    'keeps %s terminal monotonic',
    (terminalType) => {
      let state = reduceActionEvent(INITIAL_RESULT_STATE, started(1, 1))
      state = appendResultDeltaBatch(state, {
        requestId: 'request-1',
        sessionGeneration: 3,
        requestGeneration: 1,
        delta: 'partial',
        contentScalarCount: 7
      })
      const terminal = reduceActionEvent(state, terminalEvent(terminalType))
      expect(reduceActionEvent(terminal, lateDelta)).toBe(terminal)
      expect(reduceActionEvent(terminal, differentTerminal)).toBe(terminal)
    }
  )

  it('ignores same and lower request generations then resets for a newer request', () => {
    let state = reduceActionEvent(INITIAL_RESULT_STATE, started(2, 1))
    state = appendResultDeltaBatch(state, {
      requestId: 'request-2',
      sessionGeneration: 3,
      requestGeneration: 2,
      delta: 'partial',
      contentScalarCount: 7
    })

    expect(reduceActionEvent(state, started(2, 2))).toBe(state)
    expect(reduceActionEvent(state, started(1, 3))).toBe(state)

    const next = reduceActionEvent(state, started(3, 4))
    expect(next).toMatchObject({
      requestId: 'request-3',
      requestGeneration: 3,
      status: 'streaming',
      content: '',
      thinkingContent: '',
      contentScalarCount: 0,
      contentRevision: 0,
      generationNotice: ''
    })
  })

  it('appends thinking deltas without touching answer content counters', () => {
    let state = reduceActionEvent(INITIAL_RESULT_STATE, started(1, 1))
    state = reduceActionEvent(state, {
      type: 'thinkingDelta',
      sessionId: 'session-1',
      sessionGeneration: 3,
      requestId: 'request-1',
      requestGeneration: 1,
      sequence: 2,
      actionId: 'summary',
      delta: 'reason'
    })
    expect(state).toMatchObject({
      thinkingContent: 'reason',
      content: '',
      contentScalarCount: 0,
      contentRevision: 0,
      status: 'streaming'
    })

    state = reduceActionEvent(state, {
      type: 'delta',
      sessionId: 'session-1',
      sessionGeneration: 3,
      requestId: 'request-1',
      requestGeneration: 1,
      sequence: 3,
      actionId: 'summary',
      delta: 'ans'
    })
    expect(state).toMatchObject({
      thinkingContent: 'reason',
      content: 'ans',
      contentScalarCount: 3,
      contentRevision: 1
    })
  })

  it('only appends a matching streaming delta batch once', () => {
    const state = reduceActionEvent(INITIAL_RESULT_STATE, started(1, 1))
    const appended = appendResultDeltaBatch(state, {
      requestId: 'request-1',
      sessionGeneration: 3,
      requestGeneration: 1,
      delta: '😀',
      contentScalarCount: 1
    })

    expect(appended).toMatchObject({
      content: '😀',
      contentScalarCount: 1,
      contentRevision: 1
    })
    expect(appendResultDeltaBatch(appended, {
      requestId: 'other',
      sessionGeneration: 3,
      requestGeneration: 1,
      delta: 'ignored',
      contentScalarCount: 8
    })).toBe(appended)
  })

  it('preserves partial content for cancelled and error terminal states', () => {
    const streaming = appendResultDeltaBatch(
      reduceActionEvent(INITIAL_RESULT_STATE, started(1, 1)),
      {
        requestId: 'request-1',
        sessionGeneration: 3,
        requestGeneration: 1,
        delta: 'partial',
        contentScalarCount: 7
      }
    )

    expect(reduceActionEvent(streaming, terminalEvent('cancelled'))).toMatchObject({
      status: 'cancelled',
      content: 'partial',
      retryable: true
    })
    expect(reduceActionEvent(streaming, terminalEvent('error'))).toMatchObject({
      status: 'error',
      content: 'partial',
      errorMessage: 'failed',
      retryable: true
    })
  })

  it('stores a matching notice without changing body state', () => {
    const streaming = appendResultDeltaBatch(
      reduceActionEvent(INITIAL_RESULT_STATE, started(1, 1)),
      {
        requestId: 'request-1',
        sessionGeneration: 3,
        requestGeneration: 1,
        delta: 'partial',
        contentScalarCount: 7
      }
    )
    const notice: ActionStreamEvent = {
      sessionId: 'session-1',
      sessionGeneration: 3,
      requestId: 'request-1',
      requestGeneration: 1,
      sequence: 3,
      actionId: 'summary',
      type: 'notice',
      code: 'THINKING_CONTROL_FALLBACK',
      message: 'fallback applied'
    }

    const noticed = reduceActionEvent(streaming, notice)
    expect(noticed).toMatchObject({
      status: 'streaming',
      content: 'partial',
      contentScalarCount: 7,
      contentRevision: 1,
      generationNotice: 'fallback applied'
    })
    expect(reduceActionEvent(noticed, { ...notice, requestId: 'old', requestGeneration: 0 }))
      .toBe(noticed)
  })

  it('maps snapshot generations, scalar count, revision, and notice', () => {
    const state = resultStateFromSnapshot({
      sessionId: 'session-1',
      sessionGeneration: 3,
      requestId: 'request-1',
      requestGeneration: 1,
      actionId: 'summary',
      selection: {
        selectionId: 'selection-1',
        text: 'selected',
        sourceApp: { name: 'Test', bundleId: null },
        anchor: { kind: 'cursor', x: 1, y: 2 },
        direction: 'unknown',
        isFullscreen: false
      },
      status: 'streaming',
      content: '😀',
      thinkingContent: '',
      contentScalarCount: 1,
      lastContentSequence: 2,
      lastSequence: 2,
      handshakeGeneration: 1,
      generationNotice: { code: 'fallback', message: 'fallback applied' },
      errorMessage: '',
      retryable: false,
      pinned: false
    })

    expect(state).toMatchObject({
      sessionGeneration: 3,
      requestGeneration: 1,
      contentScalarCount: 1,
      contentRevision: 1,
      generationNotice: 'fallback applied'
    })
  })
})

describe('result replacement preparation and transcript contract', () => {
  const oldTranscript = [
    { id: 'session-1:turn:0', role: 'user', content: 'question', streaming: false, scalarCount: 8 },
    { id: 'session-1:turn:1', role: 'assistant', content: 'old answer', streaming: false, scalarCount: 10 }
  ] satisfies NonNullable<ResultSessionSnapshot['transcript']>

  const eventBase = { sessionId: 'session-1', sessionGeneration: 3, actionId: 'ask-ai' } as const
  const failure = (generation = 2): ActionStreamEvent => ({
    ...eventBase, type: 'preparationFailed', requestId: `request-${generation}`, requestGeneration: generation,
    sequence: generation + 9, code: 'PREPARATION_FAILED', message: 'model removed', retryable: false, question: 'retry question'
  })
  const readySnapshot = (overrides: Partial<ResultSessionSnapshot> = {}): ResultSessionSnapshot => ({
    ...eventBase,
    requestId: 'request-1', requestGeneration: 1, providerId: 'provider-old', modelId: 'model-old',
    selection: {
      selectionId: 'selection-1', text: 'source', sourceApp: { name: 'Test', bundleId: null },
      anchor: { kind: 'cursor', x: 1, y: 2 }, direction: 'unknown', isFullscreen: false
    },
    status: 'completed', content: 'old answer', thinkingContent: 'old reason',
    lastContentSequence: 9, lastSequence: 10, contentScalarCount: 10, handshakeGeneration: 4,
    generationNotice: { code: 'notice', message: 'old notice' }, transcript: oldTranscript,
    errorMessage: '', retryable: false, pinned: false, ...overrides
  })

  it('keeps the committed request, route, answer and transcript unchanged when preparation fails', () => {
    const before = resultStateFromSnapshot(readySnapshot())
    const failed = reduceActionEvent(before, failure())
    expect(failed).toEqual({
      ...before,
      preparationFailure: {
        requestId: 'request-2', requestGeneration: 2, code: 'PREPARATION_FAILED',
        message: 'model removed', retryable: false, question: 'retry question'
      }
    })
    expect(failed.transcript).toBe(before.transcript)
    const failedAgain = reduceActionEvent(failed, failure(3))
    expect(failedAgain.preparationFailure?.requestId).toBe('request-3')
    expect(reduceActionEvent(failedAgain, failure(2))).toBe(failedAgain)
    expect(reduceActionEvent(failedAgain, { ...failure(4), sessionGeneration: 2 })).toBe(failedAgain)
  })

  it('restores failure metadata with the old committed identity and complete history from a snapshot', () => {
    const preparationFailure = {
      requestId: 'request-2', requestGeneration: 2, code: 'PREPARATION_FAILED',
      message: 'model removed', retryable: false, question: 'retry question'
    }
    const restored = resultStateFromSnapshot(readySnapshot({ lastSequence: 11, preparationFailure }))
    expect(restored).toMatchObject({
      requestId: 'request-1', requestGeneration: 1, providerId: 'provider-old', modelId: 'model-old',
      status: 'completed', content: 'old answer', contentScalarCount: 10, contentRevision: 1,
      preparationFailure, transcript: oldTranscript
    })
    expect(restored.transcript).toBe(oldTranscript)
  })

  it('replaces the transcript base and failed attempt when a newer Started commits a route', () => {
    const failed = reduceActionEvent(resultStateFromSnapshot(readySnapshot()), failure())
    const transcript = [
      ...oldTranscript,
      { id: 'session-1:turn:2', role: 'user', content: 'question', streaming: false, scalarCount: 8 },
      { id: 'session-1:turn:3', role: 'assistant', content: '', streaming: true, scalarCount: 0 }
    ] satisfies NonNullable<ResultSessionSnapshot['transcript']>
    const started: ActionStreamEvent = {
      ...eventBase, type: 'started', requestId: 'request-3', requestGeneration: 3, sequence: 12,
      providerId: 'provider-next', modelId: 'model-next', question: 'question', transcript
    }
    let state = reduceActionEvent(failed, started)
    expect(state).toMatchObject({
      requestId: 'request-3', requestGeneration: 3, providerId: 'provider-next', modelId: 'model-next',
      preparationFailure: null, transcript, status: 'streaming', content: '', thinkingContent: '',
      contentScalarCount: 0, contentRevision: 0, generationNotice: '', errorMessage: '', retryable: false
    })
    expect(state.transcript).toBe(transcript)
    expect(state.transcript?.[2]?.id).not.toBe(state.transcript?.[0]?.id)
    state = reduceActionEvent(state, { ...eventBase, type: 'delta', requestId: 'request-3', requestGeneration: 3, sequence: 13, delta: '😀' })
    state = reduceActionEvent(state, { ...eventBase, type: 'completed', requestId: 'request-3', requestGeneration: 3, sequence: 14, lastContentSequence: 13, contentScalarCount: 1 })
    expect(state).toMatchObject({ status: 'completed', content: '😀', contentScalarCount: 1, transcript })
    expect(reduceActionEvent(state, failure(2))).toBe(state)
  })

  it('accepts an explicit empty transcript for an empty Ask route switch', () => {
    const failed = reduceActionEvent(resultStateFromSnapshot(readySnapshot()), failure())
    const next = reduceActionEvent(failed, {
      ...eventBase, type: 'started', requestId: 'request-3', requestGeneration: 3, sequence: 12,
      providerId: 'provider-next', modelId: 'model-next', transcript: []
    })
    expect(next).toMatchObject({ requestId: 'request-3', preparationFailure: null, content: '', transcript: [] })
    expect(next.transcript).not.toBe(oldTranscript)
  })
})
