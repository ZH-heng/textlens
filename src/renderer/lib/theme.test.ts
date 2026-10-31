import { applyTheme, storedTheme, THEME_STORAGE_KEY } from './theme'

describe('applyTheme', () => {
  beforeEach(() => {
    window.localStorage.clear()
    delete document.documentElement.dataset.theme
  })

  it('applies and remembers a non-default theme', () => {
    applyTheme('ocean')
    expect(document.documentElement.dataset.theme).toBe('ocean')
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe('ocean')
    expect(storedTheme()).toBe('ocean')
  })

  it('clears the attribute for the base warm theme but still remembers it', () => {
    applyTheme('ocean')
    applyTheme('warm')
    expect(document.documentElement.dataset.theme).toBeUndefined()
    expect(storedTheme()).toBe('warm')
  })

  it('does not persist unsaved previews', () => {
    applyTheme('sage', false)
    expect(document.documentElement.dataset.theme).toBe('sage')
    expect(storedTheme()).toBeNull()
  })

  it('leaves the stored theme untouched when no theme is known', () => {
    applyTheme('sand')
    applyTheme(null)
    expect(document.documentElement.dataset.theme).toBeUndefined()
    expect(storedTheme()).toBe('sand')
  })
})
