import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import '../styles.css'
import '../themes.css'
import './toolbar.css'
import { installTauriBridge } from '../lib/tauriBridge'
import { WindowErrorBoundary } from '../lib/ErrorBoundary'
import { applyTheme, storedTheme } from '../lib/theme'
import { ToolbarApp } from './ToolbarApp'

installTauriBridge()
applyTheme(storedTheme())

const root = document.getElementById('root')

if (!root) {
  throw new Error('Toolbar root element is missing')
}

createRoot(root).render(
  <StrictMode>
    <WindowErrorBoundary>
      <ToolbarApp />
    </WindowErrorBoundary>
  </StrictMode>
)
