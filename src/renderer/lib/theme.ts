import type { Theme } from '../../shared'

/**
 * Applies an appearance theme by attribute on the document element. The warm
 * theme is the base token set in styles.css, so it clears the attribute
 * instead of adding a no-op override.
 */
export function applyTheme(theme: Theme | string | null | undefined): void {
  const root = document.documentElement
  if (!theme || theme === 'warm') {
    delete root.dataset.theme
    return
  }
  root.dataset.theme = theme
}
