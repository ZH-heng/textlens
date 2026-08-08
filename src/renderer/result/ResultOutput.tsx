import {
  Component,
  Suspense,
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ErrorInfo,
  type JSX,
  type ReactNode
} from 'react'

import {
  safeMarkdownLoader,
  type MarkdownGeneration,
  type RetryableMarkdownLoader
} from './LazySafeMarkdown'
import { shouldRenderRichMarkdown } from './markdownPolicy'
import type { ResultStatus } from './resultState'

export type ResultOutputMilestone =
  | 'plain-layout'
  | 'presentation-opportunity'
  | 'markdown-task'
  | 'rich-commit'

export interface MarkdownTaskScheduler {
  requestFrame(callback: FrameRequestCallback): number
  cancelFrame(handle: number): void
  requestIdle(callback: () => void): number
  cancelIdle(handle: number): void
}

export const browserMarkdownScheduler: MarkdownTaskScheduler = {
  requestFrame: (callback) => window.requestAnimationFrame(callback),
  cancelFrame: (handle) => window.cancelAnimationFrame(handle),
  requestIdle: (callback) => {
    if (typeof window.requestIdleCallback === 'function') {
      return window.requestIdleCallback(() => callback(), { timeout: 1_000 })
    }
    return window.setTimeout(callback, 50)
  },
  cancelIdle: (handle) => {
    if (typeof window.cancelIdleCallback === 'function') {
      window.cancelIdleCallback(handle)
    } else {
      window.clearTimeout(handle)
    }
  }
}

export interface DeferredRichMarkdownState {
  generation: MarkdownGeneration | null
  loading: boolean
  failed: boolean
  retry(): void
}

export function useDeferredRichMarkdown(
  requestKey: string,
  status: ResultStatus,
  eligible: boolean,
  revealCommitted: boolean,
  loader: RetryableMarkdownLoader = safeMarkdownLoader,
  scheduler: MarkdownTaskScheduler = browserMarkdownScheduler,
  onMilestone?: (milestone: ResultOutputMilestone, at: number) => void
): DeferredRichMarkdownState {
  const [generation, setGeneration] = useState<MarkdownGeneration | null>(null)
  const [loading, setLoading] = useState(false)
  const [failed, setFailed] = useState(false)
  const [attempt, setAttempt] = useState(0)
  const milestoneRef = useRef(onMilestone)
  milestoneRef.current = onMilestone
  const plainLayoutRecorded = useRef(false)
  // Rich Markdown now upgrades live while a reply streams in (Cherry-style),
  // so the deferred module load starts as soon as content looks Markdown-like.
  const active = (status === 'completed' || status === 'streaming') && eligible

  const record = useCallback((milestone: ResultOutputMilestone): void => {
    milestoneRef.current?.(milestone, performance.now())
  }, [])

  useLayoutEffect(() => {
    if (!active || plainLayoutRecorded.current) return
    plainLayoutRecorded.current = true
    record('plain-layout')
  }, [active, record, requestKey])

  useEffect(() => {
    if (!active || !revealCommitted) return

    let cancelled = false
    let frameHandle: number | null = scheduler.requestFrame(() => {
      frameHandle = null
      if (cancelled) return
      record('presentation-opportunity')
      idleHandle = scheduler.requestIdle(() => {
        idleHandle = null
        if (cancelled) return
        record('markdown-task')
        const nextGeneration = loader.nextGeneration()
        setGeneration(nextGeneration)
        setLoading(true)
        void loader.preload().then(
          () => {
            if (!cancelled) setLoading(false)
          },
          () => {
            if (!cancelled) {
              setLoading(false)
              setFailed(true)
            }
          }
        )
      })
    })
    let idleHandle: number | null = null

    return () => {
      cancelled = true
      if (frameHandle !== null) scheduler.cancelFrame(frameHandle)
      if (idleHandle !== null) scheduler.cancelIdle(idleHandle)
    }
  }, [active, attempt, loader, record, requestKey, revealCommitted, scheduler])

  const retry = useCallback((): void => {
    if (!active) return
    setGeneration(null)
    setLoading(false)
    setFailed(false)
    setAttempt((current) => current + 1)
  }, [active])

  return { generation, loading, failed, retry }
}

interface MarkdownErrorBoundaryProps {
  fallback: ReactNode
  children: ReactNode
}

interface MarkdownErrorBoundaryState {
  failed: boolean
}

class MarkdownErrorBoundary extends Component<
  MarkdownErrorBoundaryProps,
  MarkdownErrorBoundaryState
> {
  state: MarkdownErrorBoundaryState = { failed: false }

  static getDerivedStateFromError(): MarkdownErrorBoundaryState {
    return { failed: true }
  }

  componentDidCatch(_error: Error, _info: ErrorInfo): void {
    // The fallback keeps the complete plain body visible. A user-triggered
    // retry remounts this boundary with a fresh loader generation.
  }

  render(): ReactNode {
    return this.state.failed ? this.props.fallback : this.props.children
  }
}

export interface ResultOutputProps {
  requestKey: string
  status: ResultStatus
  content: string
  contentScalarCount: number
  contentRevision: number
  revealCommitted: boolean
  onOpenExternal(url: string): void
  loader?: RetryableMarkdownLoader
  scheduler?: MarkdownTaskScheduler
  onMilestone?: (milestone: ResultOutputMilestone, at: number) => void
}

function PlainOutput({
  content,
  status,
  failed,
  retry
}: {
  content: string
  status: ResultStatus
  failed: boolean
  retry(): void
}): JSX.Element {
  return (
    <>
      <span className="stream-plain-text">{content}</span>
      {status === 'streaming' && <span className="stream-caret" aria-hidden="true" />}
      {failed && (
        <button className="result-footer-button" type="button" onClick={retry}>
          重试富文本渲染
        </button>
      )}
    </>
  )
}

function RichCommitMarker({
  children,
  onCommit
}: {
  children: ReactNode
  onCommit(): void
}): JSX.Element {
  useLayoutEffect(onCommit, [onCommit])
  return <>{children}</>
}

function ResultOutputInstance({
  requestKey,
  status,
  content,
  contentScalarCount,
  revealCommitted,
  onOpenExternal,
  loader = safeMarkdownLoader,
  scheduler = browserMarkdownScheduler,
  onMilestone
}: ResultOutputProps): JSX.Element {
  const eligible =
    (status === 'completed' || status === 'streaming') &&
    shouldRenderRichMarkdown(content, contentScalarCount)
  const deferred = useDeferredRichMarkdown(
    requestKey,
    status,
    eligible,
    revealCommitted,
    loader,
    scheduler,
    onMilestone
  )
  const recordRichCommit = useCallback(() => {
    onMilestone?.('rich-commit', performance.now())
  }, [onMilestone])
  const fallback = (
    <PlainOutput content={content} status={status} failed={deferred.failed} retry={deferred.retry} />
  )

  if (!eligible || deferred.loading || deferred.failed || !deferred.generation) return fallback

  const RichMarkdown = deferred.generation.Component
  return (
    <MarkdownErrorBoundary
      key={deferred.generation.id}
      fallback={(
        <PlainOutput content={content} status={status} failed retry={deferred.retry} />
      )}
    >
      <Suspense fallback={fallback}>
        <RichCommitMarker onCommit={recordRichCommit}>
          <RichMarkdown content={content} onOpenExternal={onOpenExternal} />
        </RichCommitMarker>
      </Suspense>
    </MarkdownErrorBoundary>
  )
}

function ResultOutputView(props: ResultOutputProps): JSX.Element {
  return <ResultOutputInstance key={props.requestKey} {...props} />
}

export const ResultOutput = memo(ResultOutputView)
