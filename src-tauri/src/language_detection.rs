//! Offline language evidence and filters shared with the renderer.
//! Filtering never changes the source text sent to the model.

use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

use regex::{Regex, RegexBuilder, RegexSet};
use serde::Deserialize;

use crate::models::TranslationLanguage;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleData {
    line_patterns: Vec<String>,
    inline_patterns: Vec<InlineRule>,
    latin: Vec<LatinRule>,
    latin_folds: Vec<FoldRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InlineRule {
    pattern: String,
    replacement: String,
    #[serde(default)]
    case_sensitive: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LatinRule {
    language: TranslationLanguage,
    words: Vec<String>,
    single_words: Vec<String>,
    accents: String,
}

#[derive(Deserialize)]
struct FoldRule {
    letters: String,
    replacement: String,
}

struct LatinProfile {
    language: TranslationLanguage,
    words: HashSet<String>,
    single_words: HashSet<String>,
    accents: HashSet<char>,
}

struct DetectionRules {
    lines: Vec<Regex>,
    inline: Vec<(Regex, String)>,
    folds: HashMap<char, String>,
    latin: Vec<LatinProfile>,
    scripts: RegexSet,
}

fn fold_word(word: &str, folds: &HashMap<char, String>) -> String {
    let mut folded = String::new();
    for character in word.chars() {
        if matches!(character as u32, 0x0300..=0x036f) {
            continue;
        }
        if let Some(value) = folds.get(&character) {
            folded.push_str(value);
        } else {
            folded.push(character);
        }
    }
    folded
}

fn compile_pattern(pattern: &str, case_sensitive: bool) -> Regex {
    // JavaScript word boundaries are ASCII-based even with the Unicode flag.
    RegexBuilder::new(&pattern.replace(r"\b", r"(?-u:\b)"))
        .case_insensitive(!case_sensitive)
        .build()
        .expect("valid shared language-detection expression")
}

fn rules() -> &'static DetectionRules {
    static RULES: OnceLock<DetectionRules> = OnceLock::new();
    RULES.get_or_init(|| {
        let data: RuleData =
            serde_json::from_str(include_str!("../../src/shared/languageDetectionRules.json"))
                .expect("valid shared language-detection rules");
        let folds: HashMap<char, String> = data
            .latin_folds
            .into_iter()
            .flat_map(|rule| {
                rule.letters
                    .chars()
                    .map(|character| (character, rule.replacement.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let latin = data
            .latin
            .into_iter()
            .map(|profile| LatinProfile {
                language: profile.language,
                words: profile
                    .words
                    .iter()
                    .map(|word| fold_word(word, &folds))
                    .collect(),
                single_words: profile
                    .single_words
                    .iter()
                    .map(|word| fold_word(word, &folds))
                    .collect(),
                accents: profile.accents.chars().collect(),
            })
            .collect();
        DetectionRules {
            lines: data
                .line_patterns
                .iter()
                .map(|pattern| compile_pattern(pattern, false))
                .collect(),
            inline: data
                .inline_patterns
                .into_iter()
                .map(|rule| {
                    (
                        compile_pattern(&rule.pattern, rule.case_sensitive),
                        rule.replacement,
                    )
                })
                .collect(),
            folds,
            latin,
            scripts: RegexSet::new([
                r"\p{Script=Han}",
                r"[\p{Script=Hiragana}\p{Script=Katakana}]",
                r"\p{Script=Hangul}",
                r"\p{Script=Cyrillic}",
            ])
            .expect("valid script expressions"),
        }
    })
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if !matches!(first, '`' | '~') {
        return None;
    }
    let width = line
        .chars()
        .take_while(|&character| character == first)
        .count();
    (width >= 3).then_some((first, width))
}

fn strip_code_spans(text: &str) -> String {
    let mut cursor = 0;
    let mut result = String::new();
    while cursor < text.len() {
        let Some(offset) = text[cursor..].find('`') else {
            result.push_str(&text[cursor..]);
            return result;
        };
        let start = cursor + offset;
        let width = text.as_bytes()[start..]
            .iter()
            .take_while(|&&value| value == b'`')
            .count();
        let mut search = start + width;
        let mut end = None;
        while search < text.len() {
            let Some(offset) = text[search..].find('`') else {
                break;
            };
            let next = search + offset;
            let next_width = text.as_bytes()[next..]
                .iter()
                .take_while(|&&value| value == b'`')
                .count();
            if next_width == width {
                end = Some(next);
                break;
            }
            search = next + next_width;
        }
        if let Some(end) = end {
            result.push_str(&text[cursor..start]);
            result.push(' ');
            cursor = end + width;
        } else {
            result.push_str(&text[cursor..start + width]);
            cursor = start + width;
        }
    }
    result
}

fn detection_text(text: &str, rules: &DetectionRules) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut fence: Option<(char, usize)> = None;
    let mut lines = Vec::new();
    for line in normalized.split('\n') {
        let trimmed = line.trim_start();
        let marker = fence_marker(trimmed);
        if let Some((character, width)) = fence {
            if marker.is_some_and(|(next, count)| {
                next == character && count >= width && trimmed[count..].trim().is_empty()
            }) {
                fence = None;
            }
            continue;
        }
        // Backticks in the info string make this an inline span, not a fence.
        if let Some(marker) = marker
            .filter(|(character, width)| *character != '`' || !trimmed[*width..].contains('`'))
        {
            fence = Some(marker);
            continue;
        }
        if rules.lines.iter().any(|pattern| pattern.is_match(line)) {
            continue;
        }
        lines.push(line);
    }
    let mut natural = strip_code_spans(&lines.join("\n"));
    for (pattern, replacement) in &rules.inline {
        natural = pattern
            .replace_all(&natural, replacement.as_str())
            .into_owned();
    }
    natural
}

fn is_latin(character: char) -> bool {
    character.is_ascii_lowercase()
        || matches!(character as u32,
        0x00c0..=0x00d6 | 0x00d8..=0x00f6 | 0x00f8..=0x024f | 0x1e00..=0x1eff)
}

fn latin_words(text: &str) -> Vec<&str> {
    text.split(|character| !is_latin(character) && !matches!(character as u32, 0x0300..=0x036f))
        .map(|word| {
            word.trim_start_matches(|character| matches!(character as u32, 0x0300..=0x036f))
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn detect_latin(words: &[&str], rules: &DetectionRules) -> TranslationLanguage {
    let normalized: Vec<String> = words
        .iter()
        .map(|word| fold_word(word, &rules.folds))
        .collect();
    if normalized.len() == 1 {
        let single: Vec<_> = rules
            .latin
            .iter()
            .filter(|profile| profile.single_words.contains(&normalized[0]))
            .collect();
        if single.len() == 1 {
            return single[0].language;
        }
    }
    let unique: HashSet<_> = normalized.iter().collect();
    let mut scores: Vec<_> = rules
        .latin
        .iter()
        .map(|profile| {
            let hits = unique
                .iter()
                .filter(|word| profile.words.contains(word.as_str()))
                .count();
            let accents = words
                .iter()
                .filter(|word| {
                    word.chars()
                        .any(|character| profile.accents.contains(&character))
                })
                .count();
            (
                profile.language,
                hits,
                hits * 3 + if hits > 0 { accents.min(2) } else { 0 },
            )
        })
        .collect();
    scores.sort_by(|a, b| b.2.cmp(&a.2));
    let best = scores[0];
    if best.1 >= 2 && best.2 > scores[1].2 {
        best.0
    } else {
        TranslationLanguage::EnUs
    }
}

pub(crate) fn detect_source_language(text: &str) -> TranslationLanguage {
    use TranslationLanguage::{EnUs, JaJp, KoKr, RuRu, ZhCn};
    let rules = rules();
    let natural = detection_text(text, rules).to_lowercase();
    let words = latin_words(&natural);
    let (mut han, mut kana, mut hangul, mut cyrillic, mut latin, mut other) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut encoded = [0; 4];
    for character in natural.chars() {
        let script = rules.scripts.matches(character.encode_utf8(&mut encoded));
        if script.matched(0) {
            han += 1;
        } else if script.matched(1) {
            kana += 1;
        } else if script.matched(2) {
            hangul += 1;
        } else if script.matched(3) {
            cyrillic += 1;
        } else if is_latin(character) {
            latin += 1;
        } else if !matches!(character as u32, 0x0300..=0x036f) && character.is_alphabetic() {
            other += 1;
        }
    }
    let latin_weight = latin.min(
        words
            .iter()
            .map(|word| {
                let folded = fold_word(word, &rules.folds);
                if rules
                    .latin
                    .iter()
                    .any(|profile| profile.words.contains(&folded))
                {
                    word.chars().count().min(3)
                } else {
                    1
                }
            })
            .sum(),
    );
    let total = han + kana + hangul + cyrillic + latin_weight + other;
    if total == 0 {
        return EnUs;
    }
    let japanese = kana > 0 && (kana >= 2 || han + kana == total) && kana * 10 >= total;
    let mut candidates = [
        (
            if japanese { JaJp } else { ZhCn },
            if japanese { han + kana } else { han },
        ),
        (KoKr, hangul),
        (RuRu, cyrillic),
    ];
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    let (language, count) = candidates[0];
    if (count >= 2 || count == total)
        && count >= latin_weight
        && count >= other
        && count * 10 >= total * 4
    {
        return language;
    }
    detect_latin(&words, rules)
}
