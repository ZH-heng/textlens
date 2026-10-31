import { describe, expect, it } from 'vitest'

import {
  TRANSLATION_LANGUAGE_CODES,
  TRANSLATION_LANGUAGES,
  defaultTranslationTarget,
  detectTranslationLanguage,
  translationTargetEnglishName
} from '../languages'

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
