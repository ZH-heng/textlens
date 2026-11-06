import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

import {
  TRANSLATION_LANGUAGE_CODES,
  TRANSLATION_LANGUAGES,
  TRANSLATION_TARGET_ENGLISH_NAMES,
  UI_LOCALES,
  defaultTranslationTarget,
  detectTranslationLanguage,
  translationTargetEnglishName
} from '../languages'
import detectionFixtures from './fixtures/language-detection.json'

const rustModelsSource = readFileSync(
  resolve(process.cwd(), 'src-tauri/src/models.rs'),
  'utf8'
)

function enumSerdeCodes(name: string): string[] {
  const block = new RegExp(`pub enum ${name} \\{([\\s\\S]*?)\\n\\}`, 'u').exec(
    rustModelsSource
  )?.[1] ?? ''
  return [...block.matchAll(/#\[serde\(rename = "([^"]+)"\)\]/gu)].map((match) => match[1]!)
}

describe('translation languages', () => {
  it('exposes JA/KO/RU/DE/FR alongside zh/en with short result codes', () => {
    expect(TRANSLATION_LANGUAGES).toEqual([
      'zh-CN',
      'en-US',
      'ja-JP',
      'ko-KR',
      'ru-RU',
      'de-DE',
      'fr-FR'
    ])
    expect(TRANSLATION_LANGUAGE_CODES['ja-JP']).toBe('JA')
    expect(TRANSLATION_LANGUAGE_CODES['ko-KR']).toBe('KO')
    expect(TRANSLATION_LANGUAGE_CODES['ru-RU']).toBe('RU')
    expect(TRANSLATION_LANGUAGE_CODES['de-DE']).toBe('DE')
    expect(TRANSLATION_LANGUAGE_CODES['fr-FR']).toBe('FR')
  })

  it('keeps Rust locale/language enums and prompt names in lockstep', () => {
    expect(enumSerdeCodes('Locale')).toEqual([...UI_LOCALES])
    expect(enumSerdeCodes('TranslationLanguage')).toEqual([...TRANSLATION_LANGUAGES])
    const englishNames = /pub fn english_name\(self\) -> &'static str \{([\s\S]*?)\n    \}/u
      .exec(rustModelsSource)?.[1] ?? ''
    expect(
      [...englishNames.matchAll(/Self::\w+ => "([^"]+)"/gu)].map((match) => match[1])
    ).toEqual(
      TRANSLATION_LANGUAGES.map((code) => TRANSLATION_TARGET_ENGLISH_NAMES[code])
    )
  })

  it('detects CJK / Hangul / Cyrillic source languages', () => {
    expect(detectTranslationLanguage('这是中文')).toBe('zh-CN')
    expect(detectTranslationLanguage('これは日本語')).toBe('ja-JP')
    expect(detectTranslationLanguage('안녕하세요')).toBe('ko-KR')
    expect(detectTranslationLanguage('Привет')).toBe('ru-RU')
    expect(detectTranslationLanguage('Hello world')).toBe('en-US')
  })

  it('uses script share so one foreign character cannot hijack a paragraph', () => {
    expect(
      detectTranslationLanguage('The quick brown fox jumps over the lazy dog near 重 5G.')
    ).toBe('en-US')
    expect(detectTranslationLanguage('Hello の world')).toBe('en-US')
    expect(detectTranslationLanguage('Hello 한 world')).toBe('en-US')
    expect(detectTranslationLanguage('Hello Ж world')).toBe('en-US')
    expect(detectTranslationLanguage('Mixed English 中文中文中文')).toBe('zh-CN')
    expect(detectTranslationLanguage('日本語能力試験に合格した')).toBe('ja-JP')
    expect(detectTranslationLanguage('12345 — !')).toBe('en-US')
  })

  it.each(detectionFixtures)('detects $language with shared Rust fixtures: $text', ({ text, language }) => {
    expect(detectTranslationLanguage(text)).toBe(language)
  })

  it.each(['de-DE', 'fr-FR'] as const)('reverses %s text to the default language', (language) => {
    const text = detectionFixtures.find((fixture) => fixture.language === language)!.text
    expect(defaultTranslationTarget(detectTranslationLanguage(text), {
      defaultLanguage: 'zh-CN', targetLanguage: language
    })).toBe('zh-CN')
  })

  it('translates into the target unless the text already uses it', () => {
    const settings = { defaultLanguage: 'en-US' as const, targetLanguage: 'zh-CN' as const }
    expect(defaultTranslationTarget('en-US', settings)).toBe('zh-CN')
    expect(defaultTranslationTarget('zh-CN', settings)).toBe('en-US')
    expect(defaultTranslationTarget('ja-JP', settings)).toBe('zh-CN')
    expect(defaultTranslationTarget('fr-FR', settings)).toBe('zh-CN')
  })

  it('maps prompt slot names in English', () => {
    expect(translationTargetEnglishName('de-DE')).toBe('German')
    expect(translationTargetEnglishName('zh-CN')).toBe('Chinese (Simplified)')
  })
})
