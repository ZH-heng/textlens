import { Component, type ErrorInfo, type ReactNode } from 'react'

interface WindowErrorBoundaryProps {
  children: ReactNode
}

interface WindowErrorBoundaryState {
  failed: boolean
}

/**
 * A render throw in any window otherwise blanks the WebView (and the result
 * window may never reveal). Keep a minimal, theme-aware fallback instead.
 */
export class WindowErrorBoundary extends Component<
  WindowErrorBoundaryProps,
  WindowErrorBoundaryState
> {
  state: WindowErrorBoundaryState = { failed: false }

  static getDerivedStateFromError(): WindowErrorBoundaryState {
    return { failed: true }
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error('TextLens renderer failed', error, info.componentStack)
  }

  render(): ReactNode {
    if (!this.state.failed) return this.props.children
    return (
      <div
        role="alert"
        style={{
          boxSizing: 'border-box',
          display: 'flex',
          minHeight: '100vh',
          alignItems: 'center',
          justifyContent: 'center',
          padding: 24,
          color: 'var(--text, #1f2937)',
          background: 'var(--surface, transparent)',
          font: '13px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif',
          textAlign: 'center'
        }}
      >
        界面加载失败，请关闭窗口后重试。
      </div>
    )
  }
}
