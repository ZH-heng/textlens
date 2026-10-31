import { readFileSync } from 'node:fs'

import { THEME_OPTIONS } from '../shared/constants'

const readCss = (relativePath: string): string =>
  readFileSync(new URL(relativePath, import.meta.url), 'utf8')

const resultCss = readCss('./result/result.css')
const settingsCss = readCss('./settings/settings.css')
const stylesCss = readCss('./styles.css')
const themesCss = readCss('./themes.css')
const toolbarCss = readCss('./toolbar/toolbar.css')

// Tokens every theme block must set explicitly. Shared tokens (danger,
// shadows, radii) may inherit from the base theme, but the identity of a theme
// lives in these.
const IDENTITY_TOKENS = [
  '--bg',
  '--surface',
  '--surface-solid',
  '--surface-muted',
  '--surface-inset',
  '--text',
  '--text-secondary',
  '--text-tertiary',
  '--accent',
  '--accent-hover',
  '--accent-soft'
] as const

const CSS_FILES = [stylesCss, themesCss, resultCss, settingsCss, toolbarCss]

function tokenNames(block: string): Set<string> {
  const names = new Set<string>()
  for (const match of block.matchAll(/(--[a-z0-9-]+)\s*:/g)) {
    const name = match[1]
    if (name) names.add(name)
  }
  return names
}

const themeBlocks = Array.from(
  themesCss.matchAll(/\[data-theme='([a-z-]+)'\][\s\S]*?\{([\s\S]*?)\}/g),
  (match) => ({ id: match[1] ?? '', block: match[2] ?? '' })
)

describe('appearance themes', () => {
  it('defines a block for every selectable non-base theme', () => {
    const ids = new Set(themeBlocks.map((entry) => entry.id))
    const known = new Set<string>(THEME_OPTIONS.map((option) => option.id))
    for (const option of THEME_OPTIONS) {
      if (option.id === 'warm') continue
      expect(ids.has(option.id), `missing theme block: ${option.id}`).toBe(true)
    }
    for (const id of ids) {
      expect(known.has(id), `unknown theme block: ${id}`).toBe(true)
    }
  })

  it('keeps every theme block complete for its identity tokens', () => {
    expect(themeBlocks.length).toBeGreaterThan(0)
    for (const { id, block } of themeBlocks) {
      const tokens = tokenNames(block)
      for (const token of IDENTITY_TOKENS) {
        expect(tokens.has(token), `${id} missing ${token}`).toBe(true)
      }
    }
  })

  it('defines every custom property referenced without a fallback', () => {
    const defined = new Set<string>()
    for (const css of CSS_FILES) {
      for (const name of tokenNames(css)) defined.add(name)
    }
    for (const css of CSS_FILES) {
      for (const match of css.matchAll(/var\(\s*(--[a-z0-9-]+)\s*\)/g)) {
        const name = match[1]
        if (!name) continue
        expect(defined.has(name), `undefined custom property: ${name}`).toBe(true)
      }
    }
  })

  it('routes primary button text through the on-accent token', () => {
    expect(stylesCss).toMatch(
      /\.button--primary\s*\{[^}]*color:\s*var\(--on-accent\)/
    )
    expect(stylesCss.match(/--on-accent\s*:/g)?.length ?? 0).toBeGreaterThanOrEqual(2)
  })
})
