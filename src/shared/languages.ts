import { detectSourceLanguage } from './languageDetection'

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

/** Offline source detection; filters technical content before comparing language evidence. */
export function detectTranslationLanguage(text: string): TranslationLanguage {
  return detectSourceLanguage(text)
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
