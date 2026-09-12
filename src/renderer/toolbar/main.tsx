import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import '../styles.css'
import '../themes.css'
import './toolbar.css'
import { installTauriBridge } from '../lib/tauriBridge'
import { ToolbarApp } from './ToolbarApp'

installTauriBridge()

const root = document.getElementById('root')

if (!root) {
  throw new Error('Toolbar root element is missing')
}

createRoot(root).render(
  <StrictMode>
    <ToolbarApp />
  </StrictMode>
)
