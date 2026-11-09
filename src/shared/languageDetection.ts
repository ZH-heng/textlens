import rules from './languageDetectionRules.json'
import type { TranslationLanguage } from './languages'

const linePatterns = rules.linePatterns.map((pattern) => new RegExp(pattern, 'iu'))
const inlinePatterns = rules.inlinePatterns.map((rule) => ({
  pattern: new RegExp(rule.pattern, 'caseSensitive' in rule && rule.caseSensitive ? 'gu' : 'giu'),
  replacement: rule.replacement
}))
const folds = new Map(rules.latinFolds.flatMap(({ letters, replacement }) =>
  Array.from(letters, (letter) => [letter, replacement] as const)))
const LATIN_WORD = /[a-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u024f\u1e00-\u1eff][a-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u024f\u1e00-\u1eff\u0300-\u036f]*/gu
const LATIN_CHARACTER = /[a-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u024f\u1e00-\u1eff]/u
const foldWord = (word: string): string => Array.from(word, (character) =>
  /[\u0300-\u036f]/u.test(character) ? '' : folds.get(character) ?? character).join('')
const latinProfiles = rules.latin.map((profile) => ({
  language: profile.language as TranslationLanguage,
  words: new Set(profile.words.map(foldWord)),
  singleWords: new Set(profile.singleWords.map(foldWord)),
  accents: new Set(profile.accents)
}))

function stripCodeSpans(text: string): string {
  let cursor = 0
  let result = ''
  while (cursor < text.length) {
    const start = text.indexOf('`', cursor)
    if (start < 0) return result + text.slice(cursor)
    let width = 1
    while (text[start + width] === '`') width += 1
    let search = start + width
    let end = -1
    while (search < text.length) {
      const next = text.indexOf('`', search)
      if (next < 0) break
      let nextWidth = 1
      while (text[next + nextWidth] === '`') nextWidth += 1
      if (nextWidth === width) { end = next; break }
      search = next + nextWidth
    }
    if (end < 0) {
      result += text.slice(cursor, start + width)
      cursor = start + width
    } else {
      result += text.slice(cursor, start) + ' '
      cursor = end + width
    }
  }
  return result
}

/** Filtering affects detection only; the original selection is always sent unchanged. */
export function translationDetectionText(text: string): string {
  let fence: { marker: string; width: number } | null = null
  const lines: string[] = []
  for (const line of text.replace(/\r\n?/gu, '\n').split('\n')) {
    const trimmed = line.trimStart()
    const marker = /^(`{3,}|~{3,})/u.exec(trimmed)?.[0]
    if (fence) {
      if (marker?.[0] === fence.marker && marker.length >= fence.width
        && trimmed.slice(marker.length).trim() === '') fence = null
      continue
    }
    // A backtick fence cannot have backticks in its info string. A complete
    // same-line code span must not hide the natural-language lines after it.
    if (marker && !(marker[0] === '`' && trimmed.slice(marker.length).includes('`'))) {
      fence = { marker: marker[0]!, width: marker.length }
      continue
    }
    if (linePatterns.some((pattern) => pattern.test(line))) continue
    lines.push(line)
  }
  let natural = stripCodeSpans(lines.join('\n'))
  for (const { pattern, replacement } of inlinePatterns) natural = natural.replace(pattern, replacement)
  return natural
}

function detectLatinLanguage(words: string[]): TranslationLanguage {
  const normalized = words.map(foldWord)
  if (normalized.length === 1) {
    const single = latinProfiles.filter((profile) => profile.singleWords.has(normalized[0]!))
    if (single.length === 1) return single[0]!.language
  }
  const unique = new Set(normalized)
  const scores = latinProfiles.map((profile) => {
    const hits = [...unique].filter((word) => profile.words.has(word)).length
    const accents = words.filter((word) => [...word].some((character) => profile.accents.has(character))).length
    return { language: profile.language, hits, score: hits * 3 + (hits ? Math.min(accents, 2) : 0) }
  }).sort((a, b) => b.score - a.score)
  const best = scores[0]!
  return best.hits >= 2 && best.score > scores[1]!.score ? best.language : 'en-US'
}

/** Shared evidence and filters mirror language_detection.rs, without network or model calls. */
export function detectSourceLanguage(text: string): TranslationLanguage {
  const natural = translationDetectionText(text).toLowerCase()
  const words = natural.match(LATIN_WORD) ?? []
  let han = 0
  let kana = 0
  let hangul = 0
  let cyrillic = 0
  let latin = 0
  let other = 0
  for (const character of natural) {
    const code = character.codePointAt(0)!
    if (/\p{Script=Han}/u.test(character)) han += 1
    else if (/\p{Script=Hiragana}|\p{Script=Katakana}/u.test(character)) kana += 1
    else if (/\p{Script=Hangul}/u.test(character)) hangul += 1
    else if (/\p{Script=Cyrillic}/u.test(character)) cyrillic += 1
    else if (LATIN_CHARACTER.test(character)) latin += 1
    else if (code < 0x0300 || code > 0x036f) {
      if (/\p{Alphabetic}/u.test(character)) other += 1
    }
  }
  // Compare CJK characters to words, so long English identifiers cannot drown out Chinese prose.
  const latinWeight = Math.min(latin, words.reduce((weight, word) => {
    const knownWord = latinProfiles.some((profile) => profile.words.has(foldWord(word)))
    return weight + (knownWord ? Math.min([...word].length, 3) : 1)
  }, 0))
  const total = han + kana + hangul + cyrillic + latinWeight + other
  if (!total) return 'en-US'
  const japanese = kana > 0 && (kana >= 2 || han + kana === total) && kana * 10 >= total
  const candidates: { language: TranslationLanguage; count: number }[] = [
    { language: japanese ? 'ja-JP' : 'zh-CN', count: japanese ? han + kana : han },
    { language: 'ko-KR', count: hangul },
    { language: 'ru-RU', count: cyrillic }
  ]
  candidates.sort((a, b) => b.count - a.count)
  const best = candidates[0]!
  if ((best.count >= 2 || best.count === total) && best.count >= latinWeight
    && best.count >= other && best.count * 10 >= total * 4) return best.language
  return detectLatinLanguage(words)
}
