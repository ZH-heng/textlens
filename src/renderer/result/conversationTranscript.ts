import { countUnicodeScalars } from '../../shared'

export type TranscriptRole = 'user' | 'assistant'

export interface TranscriptTurn {
  id: string
  role: TranscriptRole
  content: string
  streaming: boolean
  /** Unicode scalar count of `content`. Streaming patches supply it from the
      typewriter controller so per-frame renders stay O(1); absent values fall
      back to a one-shot scan at render time. */
  scalarCount?: number
}

function nextTurnId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID()
  }
  return `turn-${Date.now()}-${Math.random().toString(36).slice(2, 10)}`
}

export function isAskAction(kind: string | undefined): boolean {
  return kind === 'ask'
}

export function appendUserTurn(turns: TranscriptTurn[], question: string): TranscriptTurn[] {
  const content = question.trim()
  if (!content) return turns
  return [
    ...turns,
    {
      id: nextTurnId(),
      role: 'user',
      content,
      streaming: false
    }
  ]
}

export function beginAssistantTurn(turns: TranscriptTurn[], turnId: string): TranscriptTurn[] {
  return [
    ...turns,
    {
      id: turnId,
      role: 'assistant',
      content: '',
      streaming: true
    }
  ]
}

export function patchStreamingAssistant(
  turns: TranscriptTurn[],
  content: string,
  streaming: boolean,
  scalarCount?: number
): TranscriptTurn[] {
  if (turns.length === 0) return turns
  let lastAssistantIndex = -1
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    if (turns[index]?.role === 'assistant') {
      lastAssistantIndex = index
      break
    }
  }
  if (lastAssistantIndex < 0) return turns
  const current = turns[lastAssistantIndex]
  if (!current) return turns
  if (current.content === content && current.streaming === streaming) return turns
  const next = turns.slice()
  next[lastAssistantIndex] = {
    ...current,
    content,
    streaming,
    scalarCount: scalarCount ?? countUnicodeScalars(content)
  }
  return next
}
