/** UI / AI output locale (settings interface + {{language}} for summary/explain). */
export const UI_LOCALES = ['zh-CN', 'en-US'] as const
export type UiLocale = (typeof UI_LOCALES)[number]

/** Target languages available for the translate action and result-box switcher. */
export const TRANSLATION_LANGUAGES = [
  'zh-CN',
  'en-US',
  'ja-JP',
  'ko-KR',
  'ru-RU',
  'de-DE',
  'fr-FR'
] as const
export type TranslationLanguage = (typeof TRANSLATION_LANGUAGES)[number]

export const TRANSLATION_LANGUAGE_NAMES: Readonly<Record<TranslationLanguage, string>> = {
  'zh-CN': '简体中文',
  'en-US': 'English',
  'ja-JP': '日本語',
  'ko-KR': '한국어',
  'ru-RU': 'Русский',
  'de-DE': 'Deutsch',
  'fr-FR': 'Français'
}

/** Short codes shown in the result-window translation route. */
export const TRANSLATION_LANGUAGE_CODES: Readonly<Record<TranslationLanguage, string>> = {
  'zh-CN': 'CN',
  'en-US': 'EN',
  'ja-JP': 'JA',
  'ko-KR': 'KO',
  'ru-RU': 'RU',
  'de-DE': 'DE',
  'fr-FR': 'FR'
}

/** English names used inside the translation prompt's {{target_language}} slot. */
export const TRANSLATION_TARGET_ENGLISH_NAMES: Readonly<Record<TranslationLanguage, string>> = {
  'zh-CN': 'Chinese (Simplified)',
  'en-US': 'English',
  'ja-JP': 'Japanese',
  'ko-KR': 'Korean',
  'ru-RU': 'Russian',
  'de-DE': 'German',
  'fr-FR': 'French'
}

export function isTranslationLanguage(value: string): value is TranslationLanguage {
  return (TRANSLATION_LANGUAGES as readonly string[]).includes(value)
}

const HAN = /[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]/u
const KANA = /[\u3040-\u309f\u30a0-\u30ff]/u
const HANGUL = /[\uac00-\ud7af\u1100-\u11ff]/u
const CYRILLIC = /[\u0400-\u04ff]/u
const LETTER = /\p{Alphabetic}/u

/**
 * Detect the likely source language of selected text by script share.
 *
 * A single foreign character must not hijack a whole paragraph, so each
 * script has to own a meaningful share of the letters. Kana uses a lower
 * threshold because Japanese prose mixes it with a large Han share.
 * Mirrors `detect_translation_language` in `src-tauri/src/actions.rs`.
 */
export function detectTranslationLanguage(text: string): TranslationLanguage {
  let han = 0
  let kana = 0
  let hangul = 0
  let cyrillic = 0
  let other = 0
  for (const character of text) {
    if (HAN.test(character)) han += 1
    else if (KANA.test(character)) kana += 1
    else if (HANGUL.test(character)) hangul += 1
    else if (CYRILLIC.test(character)) cyrillic += 1
    else if (LETTER.test(character)) other += 1
  }
  const total = han + kana + hangul + cyrillic + other
  if (total === 0) return 'en-US'
  if (kana * 10 >= total) return 'ja-JP'
  if (hangul * 10 >= total * 3) return 'ko-KR'
  if (han * 10 >= total * 3) return 'zh-CN'
  if (cyrillic * 10 >= total * 3) return 'ru-RU'
  return 'en-US'
}

export interface TranslationLanguageSettings {
  defaultLanguage: TranslationLanguage
  targetLanguage: TranslationLanguage
}

/**
 * Resolve the default target: translate into `targetLanguage`, except when
 * the text already is in it, in which case `defaultLanguage` is the reverse
 * direction. Mirrors `default_translation_target` in the Rust executor.
 */
export function defaultTranslationTarget(
  source: TranslationLanguage,
  settings: TranslationLanguageSettings
): TranslationLanguage {
  return source === settings.targetLanguage
    ? settings.defaultLanguage
    : settings.targetLanguage
}

export function translationTargetEnglishName(locale: TranslationLanguage): string {
  return TRANSLATION_TARGET_ENGLISH_NAMES[locale]
}
