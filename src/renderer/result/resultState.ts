import { countUnicodeScalars, type ActionStreamEvent, type PreparationFailure, type ResultSessionSnapshot } from '../../shared'
import type { TranscriptTurn } from './conversationTranscript'

export type ResultStatus = 'idle' | 'streaming' | 'completed' | 'cancelled' | 'error'

export interface ResultState {
  sessionGeneration: number | null
  requestGeneration: number | null
  requestId: string | null
  actionId: string | null
  providerId: string | null
  modelId: string | null
  preparationFailure: PreparationFailure | null
  /** Authoritative turns at the last hydration/Started, patched by the Ask view. */
  transcript: TranscriptTurn[] | null
  status: ResultStatus
  content: string
  /** Live chain-of-thought for the current request (not answer history). */
  thinkingContent: string
  contentScalarCount: number
  contentRevision: number
  generationNotice: string
  errorMessage: string
  retryable: boolean
}

export interface ResultDeltaBatch {
  requestId: string
  sessionGeneration: number
  requestGeneration: number
  delta: string
  contentScalarCount: number
}

export interface ResultThinkingBatch {
  requestId: string
  sessionGeneration: number
  requestGeneration: number
  delta: string
}

export const INITIAL_RESULT_STATE: ResultState = {
  sessionGeneration: null,
  requestGeneration: null,
  requestId: null,
  actionId: null,
  providerId: null,
  modelId: null,
  preparationFailure: null,
  transcript: null,
  status: 'idle',
  content: '',
  thinkingContent: '',
  contentScalarCount: 0,
  contentRevision: 0,
  generationNotice: '',
  errorMessage: '',
  retryable: false
}

export function resultStateFromSnapshot(snapshot: ResultSessionSnapshot): ResultState {
  return {
    sessionGeneration: snapshot.sessionGeneration,
    requestGeneration: snapshot.requestGeneration,
    requestId: snapshot.requestId,
    actionId: snapshot.actionId,
    providerId: snapshot.providerId ?? null,
    modelId: snapshot.modelId ?? null,
    preparationFailure: snapshot.preparationFailure ?? null,
    transcript: snapshot.transcript ?? null,
    status: snapshot.status,
    content: snapshot.content,
    thinkingContent: snapshot.thinkingContent ?? '',
    contentScalarCount: snapshot.contentScalarCount,
    contentRevision: snapshot.content ? 1 : 0,
    generationNotice: snapshot.generationNotice?.message ?? '',
    errorMessage: snapshot.errorMessage,
    retryable: snapshot.retryable
  }
}

function startedStateFrom(event: Extract<ActionStreamEvent, { type: 'started' }>, previous: ResultState): ResultState {
  return {
    sessionGeneration: event.sessionGeneration,
    requestGeneration: event.requestGeneration,
    requestId: event.requestId,
    actionId: event.actionId,
    providerId: event.providerId ?? previous.providerId,
    modelId: event.modelId ?? previous.modelId,
    preparationFailure: null,
    transcript: event.transcript ?? previous.transcript,
    status: 'streaming',
    content: '',
    thinkingContent: '',
    contentScalarCount: 0,
    contentRevision: 0,
    generationNotice: '',
    errorMessage: '',
    retryable: false
  }
}

export function appendResultDeltaBatch(
  state: ResultState,
  batch: ResultDeltaBatch
): ResultState {
  if (
    state.status !== 'streaming' ||
    state.sessionGeneration !== batch.sessionGeneration ||
    state.requestGeneration !== batch.requestGeneration ||
    state.requestId !== batch.requestId
  ) return state

  return {
    ...state,
    content: state.content + batch.delta,
    contentScalarCount: batch.contentScalarCount,
    contentRevision: state.contentRevision + 1
  }
}

export function appendResultThinkingBatch(
  state: ResultState,
  batch: ResultThinkingBatch
): ResultState {
  if (
    state.status !== 'streaming' ||
    state.sessionGeneration !== batch.sessionGeneration ||
    state.requestGeneration !== batch.requestGeneration ||
    state.requestId !== batch.requestId ||
    !batch.delta
  ) return state

  return {
    ...state,
    thinkingContent: state.thinkingContent + batch.delta
  }
}

/** Events from a superseded or terminal request are intentionally ignored. */
export function reduceActionEvent(state: ResultState, event: ActionStreamEvent): ResultState {
  if (event.type === 'started') {
    const firstRequest = state.sessionGeneration === null
    const sameSession = state.sessionGeneration === event.sessionGeneration
    const newerRequest = state.requestGeneration === null ||
      event.requestGeneration > state.requestGeneration
    if (!firstRequest && (!sameSession || !newerRequest)) return state
    return startedStateFrom(event, state)
  }

  if (event.type === 'preparationFailed') {
    if (
      state.sessionGeneration !== event.sessionGeneration ||
      (state.requestGeneration !== null && event.requestGeneration <= state.requestGeneration) ||
      (state.preparationFailure && event.requestGeneration < state.preparationFailure.requestGeneration)
    ) return state
    return {
      ...state,
      preparationFailure: {
        requestId: event.requestId,
        requestGeneration: event.requestGeneration,
        code: event.code,
        message: event.message,
        retryable: event.retryable,
        question: event.question
      }
    }
  }

  if (
    state.sessionGeneration !== event.sessionGeneration ||
    state.requestGeneration !== event.requestGeneration ||
    state.requestId !== event.requestId ||
    state.status !== 'streaming'
  ) return state

  if (event.type === 'delta') {
    return appendResultDeltaBatch(state, {
      requestId: event.requestId,
      sessionGeneration: event.sessionGeneration,
      requestGeneration: event.requestGeneration,
      delta: event.delta,
      contentScalarCount: state.contentScalarCount + countUnicodeScalars(event.delta)
    })
  }
  if (event.type === 'thinkingDelta') {
    return appendResultThinkingBatch(state, {
      requestId: event.requestId,
      sessionGeneration: event.sessionGeneration,
      requestGeneration: event.requestGeneration,
      delta: event.delta
    })
  }
  if (event.type === 'completed') {
    return { ...state, status: 'completed', errorMessage: '', retryable: false }
  }
  if (event.type === 'cancelled') {
    return { ...state, status: 'cancelled', retryable: true }
  }
  if (event.type === 'notice') {
    return event.message === state.generationNotice
      ? state
      : { ...state, generationNotice: event.message }
  }
  if (event.type === 'resyncRequired') return state
  return {
    ...state,
    status: 'error',
    errorMessage: event.message,
    retryable: event.retryable
  }
}
