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
  '--border',
  '--border-strong',
  '--accent',
  '--accent-hover',
  '--accent-soft',
  '--on-accent'
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
  it('defines a block for every selectable theme, including nested warm previews', () => {
    const ids = new Set(themeBlocks.map((entry) => entry.id))
    const known = new Set<string>(THEME_OPTIONS.map((option) => option.id))
    for (const option of THEME_OPTIONS) {
      expect(ids.has(option.id), `missing theme block: ${option.id}`).toBe(true)
    }
    for (const id of ids) {
      expect(known.has(id), `unknown theme block: ${id}`).toBe(true)
    }
  })

  it('keeps mixed custom-property values behind feature detection', () => {
    for (const css of [resultCss, stylesCss]) {
      const enhancement = css.indexOf('@supports (color: color-mix(')
      expect(enhancement).toBeGreaterThan(0)
      expect(css.slice(0, enhancement)).not.toMatch(/--[a-z0-9-]+\s*:[^;]*color-mix\(/)
    }
    const base = resultCss.match(/\.result-window\s*\{([^}]*)\}/s)?.[1] ?? ''
    for (const token of [
      '--result-header-surface', '--result-content-surface', '--result-card-surface',
      '--result-model-control-surface', '--result-model-menu-surface'
    ]) {
      expect(base).toMatch(new RegExp(`${token}:\\s*var\\(--surface-`))
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

  it('defines the shared spacing, type and radius scales', () => {
    const tokens = tokenNames(stylesCss)
    for (let step = 1; step <= 8; step += 1) {
      expect(tokens.has(`--space-${step}`), `missing --space-${step}`).toBe(true)
    }
    for (const token of [
      '--text-xs',
      '--text-sm',
      '--text-md',
      '--text-lg',
      '--text-xl',
      '--weight-regular',
      '--weight-medium',
      '--weight-semibold',
      '--weight-bold',
      '--radius-xs',
      '--radius-sm',
      '--radius-md',
      '--radius-lg',
      '--radius-pill',
      '--border-subtle',
      '--scrim'
    ]) {
      expect(tokens.has(token), `missing ${token}`).toBe(true)
    }
  })

  it('keeps font weights and radii on tokens outside the token block', () => {
    for (const css of CSS_FILES) {
      expect(css).not.toMatch(/font-weight:\s*[0-9]/)
      expect(css).not.toMatch(/border-radius:[^;]*[0-9]+px/)
    }
  })
})
