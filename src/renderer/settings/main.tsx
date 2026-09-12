import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'

import '../styles.css'
import '../themes.css'
import './settings.css'
import { installTauriBridge } from '../lib/tauriBridge'
import { SettingsApp } from './SettingsApp'

installTauriBridge()

const root = document.getElementById('root')

if (!root) {
  throw new Error('Settings root element is missing')
}

createRoot(root).render(
  <StrictMode>
    <SettingsApp />
  </StrictMode>
)
