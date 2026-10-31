import type { Theme } from '../../shared'

export const THEME_STORAGE_KEY = 'textlens.theme'

/**
 * Applies an appearance theme by attribute on the document element. The warm
 * theme is the base token set in styles.css, so it clears the attribute
 * instead of adding a no-op override.
 *
 * The applied theme is mirrored into localStorage so each webview can paint
 * the right tokens before its asynchronous settings read resolves (a result
 * window can otherwise reveal in warm and flip). Pass `persist = false` for
 * unsaved settings previews.
 */
export function applyTheme(
  theme: Theme | string | null | undefined,
  persist = true
): void {
  const root = document.documentElement
  if (!theme || theme === 'warm') {
    delete root.dataset.theme
  } else {
    root.dataset.theme = theme
  }
  if (persist && theme) {
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, theme)
    } catch {
      // Storage can be unavailable in hardened webviews; the attribute still applies.
    }
  }
}

/** Last theme applied by any TextLens webview in this app origin. */
export function storedTheme(): string | null {
  try {
    return window.localStorage.getItem(THEME_STORAGE_KEY)
  } catch {
    return null
  }
}
