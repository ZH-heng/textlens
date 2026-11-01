import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import '../styles.css'
import '../themes.css'
import './result.css'
import { installTauriBridge } from '../lib/tauriBridge'
import { runDetached } from '../lib/asyncEffects'
import { WindowErrorBoundary } from '../lib/ErrorBoundary'
import { applyTheme, storedTheme } from '../lib/theme'
import { startActionEventStore } from './actionEventStore'
import { ResultApp } from './ResultApp'
import {
  createResultSessionBootstrap,
  reportBootstrapError
} from './resultSessionBootstrap'

installTauriBridge()
applyTheme(storedTheme())

const root = document.getElementById('root')

if (!root) {
  throw new Error('Result root element is missing')
}

const resultSessionId = new URLSearchParams(window.location.search).get('sessionId')?.trim()
const bootstrap = resultSessionId
  ? createResultSessionBootstrap(resultSessionId)
  : null
const stopActionEvents = resultSessionId
  ? startActionEventStore(resultSessionId, () => {
      runDetached(bootstrap?.recover(), {
        scope: 'result',
        operation: 'result-ready-recovery',
        onError: (error) => {
          reportBootstrapError(error, 'result-ready-recovery')
        }
      })
    })
  : () => undefined
if (import.meta.hot) import.meta.hot.dispose(stopActionEvents)
runDetached(bootstrap?.start(), {
  scope: 'result',
  operation: 'result-ready-start',
  onError: (error) => {
    reportBootstrapError(error, 'result-ready-start')
  }
})

createRoot(root).render(
  <StrictMode>
    <WindowErrorBoundary>
      <ResultApp bootstrap={bootstrap} />
    </WindowErrorBoundary>
  </StrictMode>
)
