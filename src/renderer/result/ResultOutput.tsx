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
import {
  RICH_MARKDOWN_SCALAR_LIMIT,
  shouldRenderRichMarkdown
} from './markdownPolicy'
import type { ResultStatus } from './resultState'

const STREAMING_MARKDOWN_UPDATE_INTERVAL_MS = 48

export type ResultOutputMilestone =
  | 'plain-layout'
  | 'presentation-opportunity'
  | 'markdown-task'
  | 'rich-commit'

export interface MarkdownTaskScheduler {
  requestFrame(callback: FrameRequestCallback): number
  cancelFrame(handle: number): void
}

export const browserMarkdownScheduler: MarkdownTaskScheduler = {
  requestFrame: (callback) => window.requestAnimationFrame(callback),
  cancelFrame: (handle) => window.cancelAnimationFrame(handle)
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
  const active = (status === 'streaming' || status === 'completed') && eligible

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

    return () => {
      cancelled = true
      if (frameHandle !== null) scheduler.cancelFrame(frameHandle)
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
  onOpenExternal: (url: string) => void
  loader?: RetryableMarkdownLoader
  scheduler?: MarkdownTaskScheduler
  onMilestone?: (milestone: ResultOutputMilestone, at: number) => void
}

/**
 * Reasoning models very often emit blank lines before the first answer line.
 * The plain stream view renders `pre-wrap`, so those lines show up as a tall
 * blank gap between the collapsed thinking bar and the answer (rich Markdown
 * silently swallows them, which makes the plain→rich handoff jump too).
 * Stripping only the whitespace-only lines before the first content line
 * keeps indented first lines intact; the raw content stays untouched for
 * copy and revision accounting.
 */
export function stripLeadingBlankLines(content: string): string {
  return content.replace(/^(?:[^\S\n]*\n)+/, '')
}

function PlainOutput({
  content,
  showCaret,
  failed,
  retry
}: {
  content: string
  showCaret: boolean
  failed: boolean
  retry(): void
}): JSX.Element {
  return (
    <>
      <span className="stream-plain-text">{stripLeadingBlankLines(content)}</span>
      {showCaret && <span className="stream-caret" aria-hidden="true" />}
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

/**
 * react-markdown reparses its complete source on every update. Keep the
 * renderer stable while limiting that work to roughly 20 Hz; terminal content
 * still commits synchronously so the final token is never left behind.
 */
function useStreamingMarkdownContent(content: string, streaming: boolean): string {
  const [rendered, setRendered] = useState(content)
  const latestRef = useRef(content)
  const timerRef = useRef<number | null>(null)
  const lastCommitRef = useRef(performance.now())
  latestRef.current = content

  useEffect(() => {
    if (!streaming) {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current)
        timerRef.current = null
      }
      lastCommitRef.current = performance.now()
      setRendered(content)
      return
    }

    const elapsed = performance.now() - lastCommitRef.current
    if (elapsed >= STREAMING_MARKDOWN_UPDATE_INTERVAL_MS) {
      lastCommitRef.current = performance.now()
      setRendered(content)
      return
    }
    if (timerRef.current !== null) return

    timerRef.current = window.setTimeout(() => {
      timerRef.current = null
      lastCommitRef.current = performance.now()
      setRendered(latestRef.current)
    }, Math.max(0, STREAMING_MARKDOWN_UPDATE_INTERVAL_MS - elapsed))
  }, [content, streaming])

  useEffect(() => () => {
    if (timerRef.current !== null) window.clearTimeout(timerRef.current)
  }, [])

  return streaming ? rendered : content
}

function RichMarkdownOutput({
  component: RichMarkdown,
  content,
  streaming,
  onOpenExternal
}: {
  component: MarkdownGeneration['Component']
  content: string
  streaming: boolean
  onOpenExternal(url: string): void
}): JSX.Element {
  const renderedContent = useStreamingMarkdownContent(content, streaming)
  return <RichMarkdown content={renderedContent} onOpenExternal={onOpenExternal} />
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
  // Match Cherry Studio's stable streaming renderer: once an append-only
  // request reveals Markdown syntax, keep that renderer through completion so
  // the terminal event does not replace the whole plain-text DOM.
  const richModeRef = useRef(false)
  if (shouldRenderRichMarkdown(content, contentScalarCount)) {
    richModeRef.current = true
  }
  const eligible = richModeRef.current &&
    content.length > 0 &&
    contentScalarCount <= RICH_MARKDOWN_SCALAR_LIMIT
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
  const outputIsStreaming = status === 'streaming'
  const fallback = (
    <PlainOutput
      content={content}
      showCaret={status === 'streaming'}
      failed={deferred.failed}
      retry={deferred.retry}
    />
  )

  if (!eligible || deferred.loading || deferred.failed || !deferred.generation) return fallback

  const RichMarkdown = deferred.generation.Component
  return (
    <MarkdownErrorBoundary
      key={deferred.generation.id}
      fallback={(
        <PlainOutput
          content={content}
          showCaret={status === 'streaming'}
          failed
          retry={deferred.retry}
        />
      )}
    >
      <Suspense fallback={fallback}>
        <RichCommitMarker onCommit={recordRichCommit}>
          <RichMarkdownOutput
            component={RichMarkdown}
            content={content}
            streaming={outputIsStreaming}
            onOpenExternal={onOpenExternal}
          />
        </RichCommitMarker>
      </Suspense>
    </MarkdownErrorBoundary>
  )
}

function ResultOutputView(props: ResultOutputProps): JSX.Element {
  return <ResultOutputInstance key={props.requestKey} {...props} />
}

export const ResultOutput = memo(ResultOutputView)
