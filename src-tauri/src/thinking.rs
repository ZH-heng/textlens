//! Thinking-level heuristics and OpenAI-compatible request body injection.

use serde_json::{json, Value};

use crate::models::{
    clamp_thinking_mode, ProviderProtocol, ThinkingCapability, ThinkingCapabilitySource,
    ThinkingDialect, ThinkingLevel, ThinkingMode,
};
use crate::openai_protocol::ProviderErrorEnvelope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedThinkingCapability {
    pub levels: Vec<ThinkingLevel>,
    pub capability: ThinkingCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingRequestPlan {
    pub capability: Option<ThinkingCapability>,
    pub levels: Vec<ThinkingLevel>,
    pub mode: ThinkingMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingBodyControl {
    ReasoningEffort(String),
    EnableThinking(bool),
    ChatTemplateKwargs(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedThinkingControl {
    pub field_path: &'static str,
    pub source: ThinkingCapabilitySource,
}

/// Authorize the one narrowly-scoped no-field retry.
///
/// HTTP status is diagnostic only: providers may return a structured rejection
/// in a successful envelope or with a 400/422 response.  The caller must still
/// pass the original attempt number and content/cancellation state.
pub fn should_fallback_without_control(
    applied: &AppliedThinkingControl,
    mode: ThinkingMode,
    attempt: u8,
    _status: Option<u16>,
    provider_error: Option<&ProviderErrorEnvelope>,
    content_seen: bool,
    cancelled: bool,
) -> bool {
    applied.source == ThinkingCapabilitySource::Heuristic
        && mode == ThinkingMode::Off
        && attempt == 1
        && !content_seen
        && !cancelled
        && provider_error.is_some_and(|error| {
            matches!(
                error
                    .code
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some(
                    "unsupported_parameter"
                        | "unknown_field"
                        | "unsupported_value"
                        | "invalid_value"
                )
            ) && error_names_field(error, applied.field_path)
        })
}

fn error_names_field(error: &ProviderErrorEnvelope, field_path: &str) -> bool {
    if error
        .param
        .as_deref()
        .is_some_and(|param| normalize_field_path(param) == field_path)
    {
        return true;
    }
    message_contains_field(&error.message, field_path)
}

fn normalize_field_path(value: &str) -> &str {
    value
        .trim()
        .strip_prefix("$.")
        .unwrap_or_else(|| value.trim())
}

fn message_contains_field(message: &str, field_path: &str) -> bool {
    let bytes = message.as_bytes();
    let needle = field_path.as_bytes();
    if needle.is_empty() || needle.len() > bytes.len() {
        return false;
    }
    bytes
        .windows(needle.len())
        .enumerate()
        .any(|(index, window)| {
            if window != needle {
                return false;
            }
            let before_index = index.checked_sub(1);
            let before = before_index.and_then(|i| bytes.get(i)).copied();
            let after = bytes.get(index + needle.len()).copied();
            let json_path_prefix = before == Some(b'.')
                && before_index
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| bytes.get(i))
                    == Some(&b'$');
            (!before.is_some_and(is_field_identifier_char) || json_path_prefix)
                && !after.is_some_and(is_field_identifier_char)
        })
}

fn is_field_identifier_char(value: u8) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, b'_' | b'.')
}

/// Parse a free-form effort string from provider API metadata.
pub fn parse_level_str(raw: &str) -> Option<ThinkingLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" | "x-high" | "extra_high" | "extra-high" | "max" => Some(ThinkingLevel::Xhigh),
        _ => None,
    }
}

/// Extract explicit thinking evidence from an OpenAI-compatible `/models` item.
/// The presence of relevant metadata is authoritative even when it is empty or ambiguous.
pub fn extract_thinking_capability_from_api_item(
    item: &Value,
) -> Option<DetectedThinkingCapability> {
    let supported_parameters = item.get("supported_parameters");
    let nested_options = item.pointer("/reasoning/effort_options");
    let root_options = item.get("reasoning_effort_options");
    let has_support_flag = has_explicit_support_flag(item);
    if supported_parameters.is_none()
        && nested_options.is_none()
        && root_options.is_none()
        && !has_support_flag
    {
        return None;
    }

    let parameters = supported_parameters
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();

    let mut dialects = Vec::new();
    let mut add_dialect = |dialect| {
        if !dialects.contains(&dialect) {
            dialects.push(dialect);
        }
    };
    let options = nested_options
        .and_then(Value::as_array)
        .or_else(|| root_options.and_then(Value::as_array));
    if parameters.iter().any(|value| value == "reasoning_effort") || options.is_some() {
        add_dialect(ThinkingDialect::ReasoningEffort);
    }
    if parameters.iter().any(|value| value == "enable_thinking") {
        add_dialect(ThinkingDialect::EnableThinking);
    }
    if parameters.iter().any(|value| {
        matches!(
            value.as_str(),
            "chat_template_kwargs" | "chat_template_kwargs.enable_thinking"
        )
    }) {
        add_dialect(ThinkingDialect::ChatTemplateKwargs);
    }

    let mut levels = Vec::new();
    let mut supports_off = false;
    if let Some(options) = options {
        for raw in options.iter().filter_map(Value::as_str) {
            let normalized = raw.trim().to_ascii_lowercase();
            if normalized == "none" {
                supports_off = true;
            } else if let Some(level) = parse_level_str(&normalized) {
                if !levels.contains(&level) {
                    levels.push(level);
                }
            }
        }
    } else if dialects == [ThinkingDialect::ReasoningEffort] {
        levels = standard_levels();
    }

    let explicitly_unsupported = explicit_support_flag(item) == Some(false);
    if explicitly_unsupported {
        levels.clear();
        supports_off = false;
    }
    let dialect = (!explicitly_unsupported && dialects.len() == 1).then(|| dialects[0]);
    if matches!(
        dialect,
        Some(ThinkingDialect::EnableThinking | ThinkingDialect::ChatTemplateKwargs)
    ) {
        supports_off = true;
        if levels.is_empty() {
            levels = standard_levels();
        }
    }

    Some(DetectedThinkingCapability {
        levels,
        capability: ThinkingCapability {
            source: ThinkingCapabilitySource::Explicit,
            dialect,
            supports_off,
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingFamily {
    OSeries,
    GptOss,
    Gpt,
    Claude,
    Gemini,
    DeepSeekMax,
    DeepSeekReasoner,
    Qwen,
    Glm,
    Kimi,
    Grok,
    Magistral,
    Generic,
}

impl ThinkingFamily {
    fn levels(self) -> Vec<ThinkingLevel> {
        match self {
            ThinkingFamily::Gpt => openai_full_levels(),
            ThinkingFamily::DeepSeekMax => deepseek_v4_levels(),
            // R1/reasoner models reason unconditionally and expose no
            // reasoning-effort parameter, so there is no gear to offer.
            ThinkingFamily::DeepSeekReasoner => Vec::new(),
            _ => standard_levels(),
        }
    }

    fn openai_dialect(self) -> ThinkingDialect {
        match self {
            ThinkingFamily::DeepSeekMax => ThinkingDialect::ReasoningEffortMax,
            ThinkingFamily::Qwen => ThinkingDialect::EnableThinking,
            _ => ThinkingDialect::ReasoningEffort,
        }
    }
}

fn detected_from_family(
    family: ThinkingFamily,
    dialect: Option<ThinkingDialect>,
    supports_off: bool,
) -> DetectedThinkingCapability {
    DetectedThinkingCapability {
        levels: family.levels(),
        capability: ThinkingCapability {
            source: ThinkingCapabilitySource::Heuristic,
            dialect,
            supports_off,
        },
    }
}

/// First 1-2 digit version after a vendor anchor (gpt6, gpt-5.1, claude-3-7,
/// gemini-2.5). Longer numbers such as dates or parameter counts are ignored.
fn family_version(id: &str, anchor: &str) -> Option<(u32, u32)> {
    let bytes = id.as_bytes();
    let needle = anchor.as_bytes();
    let mut from = 0;
    while from + needle.len() <= bytes.len() {
        let Some(offset) = find_bytes(bytes, needle, from) else {
            return None;
        };
        from = offset + 1;
        let before_ok = offset == 0 || !bytes[offset - 1].is_ascii_alphanumeric();
        let after = offset + needle.len();
        let after_ok = bytes
            .get(after)
            .is_none_or(|value| !value.is_ascii_alphabetic());
        if before_ok && after_ok {
            if let Some(version) = parse_version_at(bytes, after) {
                return Some(version);
            }
        }
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

fn parse_version_at(bytes: &[u8], start: usize) -> Option<(u32, u32)> {
    let mut index = start;
    let mut skipped = 0;
    while index < bytes.len() && !bytes[index].is_ascii_digit() {
        index += 1;
        skipped += 1;
        if skipped > 16 {
            return None;
        }
    }
    if index >= bytes.len() {
        return None;
    }
    let major_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() && index - major_start < 3 {
        index += 1;
    }
    let major_len = index - major_start;
    if major_len == 0 || major_len > 2 || bytes.get(index).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let major = parse_digits(&bytes[major_start..index]);
    let mut minor = 0;
    if matches!(bytes.get(index), Some(b'.' | b'-')) {
        let minor_start = index + 1;
        let mut minor_end = minor_start;
        while minor_end < bytes.len()
            && bytes[minor_end].is_ascii_digit()
            && minor_end - minor_start < 3
        {
            minor_end += 1;
        }
        let minor_len = minor_end - minor_start;
        if (1..=2).contains(&minor_len) && !bytes.get(minor_end).is_some_and(u8::is_ascii_digit) {
            minor = parse_digits(&bytes[minor_start..minor_end]);
        }
    }
    Some((major, minor))
}

fn parse_digits(digits: &[u8]) -> u32 {
    digits
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(digit - b'0'))
}

fn is_reasoning_claude(id: &str) -> bool {
    id.contains("claude")
        && (id.contains("thinking")
            || family_version(id, "claude")
                .is_some_and(|(major, minor)| major > 3 || (major == 3 && minor >= 7)))
}

fn is_reasoning_gemini(id: &str) -> bool {
    if !id.contains("gemini")
        || ["embedding", "image", "tts", "audio"]
            .iter()
            .any(|kind| id.contains(kind))
    {
        return false;
    }
    id.contains("thinking")
        || family_version(id, "gemini")
            .is_some_and(|(major, minor)| major > 2 || (major == 2 && minor >= 5))
}

fn is_reasoning_qwen(id: &str) -> bool {
    id.contains("qwq")
        || family_version(id, "qwen").is_some_and(|(major, _)| {
            major >= 3
                && (id.contains("think") || id.contains("reasoning") || !id.contains("instruct"))
        })
}

fn is_reasoning_glm(id: &str) -> bool {
    id.contains("glm-z1")
        || family_version(id, "glm")
            .is_some_and(|(major, minor)| major > 4 || (major == 4 && minor >= 5))
}

fn is_reasoning_kimi(id: &str) -> bool {
    family_version(id, "kimi").is_some_and(|(major, _)| major >= 3)
}

fn is_reasoning_grok(id: &str) -> bool {
    family_version(id, "grok")
        .is_some_and(|(major, _)| major >= 4 || (major == 3 && id.contains("mini")))
}

fn detect_openai_family(id: &str) -> Option<ThinkingFamily> {
    if o_series_match(id) {
        return Some(ThinkingFamily::OSeries);
    }
    if id.contains("gpt-oss") {
        return Some(ThinkingFamily::GptOss);
    }
    if family_version(id, "gpt").is_some_and(|(major, _)| major >= 5) {
        return Some(ThinkingFamily::Gpt);
    }
    if is_reasoning_claude(id) {
        return Some(ThinkingFamily::Claude);
    }
    if is_reasoning_gemini(id) {
        return Some(ThinkingFamily::Gemini);
    }
    if family_version(id, "deepseek").is_some_and(|(major, _)| major >= 4) {
        return Some(ThinkingFamily::DeepSeekMax);
    }
    if id.contains("deepseek") && (id.contains("r1") || id.contains("reasoner")) {
        return Some(ThinkingFamily::DeepSeekReasoner);
    }
    if is_reasoning_qwen(id) {
        return Some(ThinkingFamily::Qwen);
    }
    if is_reasoning_glm(id) {
        return Some(ThinkingFamily::Glm);
    }
    if is_reasoning_kimi(id) {
        return Some(ThinkingFamily::Kimi);
    }
    if is_reasoning_grok(id) {
        return Some(ThinkingFamily::Grok);
    }
    if id.contains("magistral") {
        return Some(ThinkingFamily::Magistral);
    }
    if id.contains("thinking") || id.contains("reasoner") || id.contains("reasoning") {
        return Some(ThinkingFamily::Generic);
    }
    None
}

/// Infer capability from model id only when provider metadata is silent.
pub fn infer_thinking_capability(model_id: &str) -> Option<DetectedThinkingCapability> {
    let id = model_id.trim().to_ascii_lowercase();
    let family = detect_openai_family(&id)?;
    if family == ThinkingFamily::DeepSeekReasoner {
        return Some(detected_from_family(family, None, false));
    }
    Some(detected_from_family(
        family,
        Some(family.openai_dialect()),
        true,
    ))
}

/// Protocol-aware inference used when the provider model list carries no
/// OpenAI-style thinking metadata.
pub fn infer_protocol_thinking_capability(
    protocol: ProviderProtocol,
    model_id: &str,
) -> Option<DetectedThinkingCapability> {
    let id = model_id.trim().to_ascii_lowercase();
    if id.is_empty() {
        return None;
    }
    match protocol {
        ProviderProtocol::OpenAiChat | ProviderProtocol::OpenAiResponses => {
            infer_thinking_capability(&id)
        }
        ProviderProtocol::AnthropicMessages => {
            is_reasoning_claude(&id).then(|| {
                // Omitting the thinking block keeps Claude in its default
                // non-thinking mode, so off is always representable.
                detected_from_family(ThinkingFamily::Claude, None, true)
            })
        }
        ProviderProtocol::Gemini => {
            is_reasoning_gemini(&id).then(|| {
                // Gemini Pro has a minimum thinking budget and cannot truly
                // disable reasoning; Flash accepts a zero budget.
                detected_from_family(
                    ThinkingFamily::Gemini,
                    None,
                    !(id.contains("pro") && !id.contains("flash")),
                )
            })
        }
    }
}

fn openai_full_levels() -> Vec<ThinkingLevel> {
    vec![
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
    ]
}

fn standard_levels() -> Vec<ThinkingLevel> {
    vec![
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
    ]
}

fn deepseek_v4_levels() -> Vec<ThinkingLevel> {
    vec![
        ThinkingLevel::Low,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
    ]
}

fn explicit_support_flag(item: &Value) -> Option<bool> {
    thinking_support_flag_keys()
        .into_iter()
        .find_map(|key| item.get(key).and_then(Value::as_bool))
}

fn has_explicit_support_flag(item: &Value) -> bool {
    thinking_support_flag_keys()
        .into_iter()
        .any(|key| item.get(key).is_some())
}

fn thinking_support_flag_keys() -> [&'static str; 6] {
    [
        "supports_thinking",
        "supports_reasoning",
        "thinking_supported",
        "reasoning_supported",
        "supportsThinking",
        "supportsReasoning",
    ]
}

fn o_series_match(id: &str) -> bool {
    // Mirrors TS: /(^|[/:_-])o[1-9]([/:_-]|$)/
    let bytes = id.as_bytes();
    (0..bytes.len()).any(|index| {
        bytes[index] == b'o'
            && bytes
                .get(index + 1)
                .is_some_and(|digit| (b'1'..=b'9').contains(digit))
            && (index == 0 || matches!(bytes[index - 1], b'/' | b':' | b'_' | b'-'))
            && bytes
                .get(index + 2)
                .is_none_or(|value| matches!(*value, b'/' | b':' | b'_' | b'-'))
    })
}

fn level_api_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
    }
}

fn level_api_str_for(dialect: ThinkingDialect, level: ThinkingLevel) -> &'static str {
    match dialect {
        ThinkingDialect::ReasoningEffortMax => match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium | ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "max",
        },
        ThinkingDialect::ReasoningEffort
        | ThinkingDialect::EnableThinking
        | ThinkingDialect::ChatTemplateKwargs => level_api_str(level),
    }
}

pub fn resolve_body_control(plan: &ThinkingRequestPlan) -> Option<ThinkingBodyControl> {
    let capability = plan.capability.as_ref()?;
    // Manual level edits may leave the dialect unset; Chat Completions still
    // needs a concrete field, and reasoning_effort is the safest default.
    let dialect = capability
        .dialect
        .unwrap_or(ThinkingDialect::ReasoningEffort);
    let mode = plan.mode;
    if mode == ThinkingMode::Auto {
        return None;
    }
    if mode == ThinkingMode::Off {
        if !capability.supports_off {
            return None;
        }
        return Some(match dialect {
            ThinkingDialect::ReasoningEffort | ThinkingDialect::ReasoningEffortMax => {
                ThinkingBodyControl::ReasoningEffort("none".to_owned())
            }
            ThinkingDialect::EnableThinking => ThinkingBodyControl::EnableThinking(false),
            ThinkingDialect::ChatTemplateKwargs => ThinkingBodyControl::ChatTemplateKwargs(false),
        });
    }

    Some(match dialect {
        ThinkingDialect::ReasoningEffort | ThinkingDialect::ReasoningEffortMax => {
            ThinkingBodyControl::ReasoningEffort(
                level_api_str_for(dialect, mode.as_level()?).to_owned(),
            )
        }
        ThinkingDialect::EnableThinking => ThinkingBodyControl::EnableThinking(true),
        ThinkingDialect::ChatTemplateKwargs => ThinkingBodyControl::ChatTemplateKwargs(true),
    })
}

pub fn apply_thinking_to_body(
    protocol: ProviderProtocol,
    body: &mut Value,
    plan: &ThinkingRequestPlan,
) -> Option<AppliedThinkingControl> {
    let capability = plan.capability.as_ref()?;
    if plan.mode == ThinkingMode::Auto {
        return None;
    }
    let source = capability.source;
    let field_path = match protocol {
        ProviderProtocol::OpenAiChat => {
            let field_path = match resolve_body_control(plan)? {
                ThinkingBodyControl::ReasoningEffort(value) => {
                    body.as_object_mut()?
                        .insert("reasoning_effort".to_owned(), json!(value));
                    "reasoning_effort"
                }
                ThinkingBodyControl::EnableThinking(value) => {
                    body.as_object_mut()?
                        .insert("enable_thinking".to_owned(), json!(value));
                    "enable_thinking"
                }
                ThinkingBodyControl::ChatTemplateKwargs(value) => {
                    body.as_object_mut()?.insert(
                        "chat_template_kwargs".to_owned(),
                        json!({"enable_thinking": value}),
                    );
                    "chat_template_kwargs.enable_thinking"
                }
            };
            field_path
        }
        ProviderProtocol::OpenAiResponses => {
            let reasoning = if plan.mode == ThinkingMode::Off {
                if !capability.supports_off {
                    return None;
                }
                json!({"effort": "none"})
            } else {
                let dialect = capability
                    .dialect
                    .unwrap_or(ThinkingDialect::ReasoningEffort);
                json!({
                    "effort": level_api_str_for(dialect, plan.mode.as_level()?),
                    "summary": "auto",
                })
            };
            body.as_object_mut()?
                .insert("reasoning".to_owned(), reasoning);
            "reasoning.effort"
        }
        ProviderProtocol::AnthropicMessages => {
            if plan.mode == ThinkingMode::Off {
                // Omitting the block is Anthropic's way of disabling thinking.
                return None;
            }
            let max_tokens = body
                .get("max_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(8192);
            let budget = anthropic_budget(plan.mode.as_level()?, max_tokens);
            body.as_object_mut()?.insert(
                "thinking".to_owned(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
            "thinking"
        }
        ProviderProtocol::Gemini => {
            let (budget, include_thoughts) = if plan.mode == ThinkingMode::Off {
                if capability.supports_off {
                    (0, false)
                } else {
                    // Gemini Pro cannot disable thinking; its minimum budget is
                    // the closest representable "off".
                    (GEMINI_MIN_THINKING_BUDGET, false)
                }
            } else {
                (gemini_budget(plan.mode.as_level()?), true)
            };
            let mut thinking = json!({"thinkingBudget": budget});
            if include_thoughts {
                // Without this flag Gemini runs thinking but omits thought parts.
                thinking["includeThoughts"] = json!(true);
            }
            let generation = body
                .as_object_mut()?
                .entry("generationConfig".to_owned())
                .or_insert_with(|| json!({}));
            generation
                .as_object_mut()?
                .insert("thinkingConfig".to_owned(), thinking);
            "generationConfig.thinkingConfig.thinkingBudget"
        }
    };
    Some(AppliedThinkingControl { field_path, source })
}

fn anthropic_budget(level: ThinkingLevel, max_tokens: u64) -> u64 {
    // Budget must stay below max_tokens; leave at least 1024 tokens for output.
    let cap = max_tokens.saturating_sub(1024).max(1024);
    let requested = match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => 1024,
        ThinkingLevel::Medium => 2048,
        ThinkingLevel::High => 4096,
        ThinkingLevel::Xhigh => 7168,
    };
    requested.min(cap)
}

const GEMINI_MIN_THINKING_BUDGET: u64 = 128;

fn gemini_budget(level: ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Minimal => 512,
        ThinkingLevel::Low => 1024,
        ThinkingLevel::Medium => 4096,
        ThinkingLevel::High => 16384,
        ThinkingLevel::Xhigh => 24576,
    }
}

pub fn resolve_thinking_for_request(
    protocol: ProviderProtocol,
    model_id: &str,
    thinking_enabled: bool,
    thinking_default: Option<ThinkingLevel>,
    model_levels: &[ThinkingLevel],
    capability: Option<&ThinkingCapability>,
    mode: ThinkingMode,
) -> ThinkingRequestPlan {
    let inferred = if capability.is_none() {
        infer_protocol_thinking_capability(protocol, model_id)
    } else {
        None
    };
    let levels = if model_levels.is_empty() {
        inferred
            .as_ref()
            .map(|value| value.levels.clone())
            .unwrap_or_default()
    } else {
        model_levels.to_vec()
    };
    let capability = capability
        .cloned()
        .or_else(|| inferred.map(|value| value.capability))
        .or_else(|| manual_capability(protocol, model_id, &levels));
    let mode = if !thinking_enabled {
        ThinkingMode::Off
    } else {
        match mode {
            ThinkingMode::Auto => thinking_default
                .filter(|level| levels.contains(level))
                .or_else(|| default_level(&levels))
                .map(ThinkingMode::from_level)
                .unwrap_or(ThinkingMode::Off),
            mode => clamp_thinking_mode(mode, &levels),
        }
    };
    ThinkingRequestPlan {
        capability,
        levels,
        mode,
    }
}

fn default_level(levels: &[ThinkingLevel]) -> Option<ThinkingLevel> {
    levels
        .contains(&ThinkingLevel::Medium)
        .then_some(ThinkingLevel::Medium)
        .or_else(|| {
            levels
                .contains(&ThinkingLevel::High)
                .then_some(ThinkingLevel::High)
        })
        .or_else(|| levels.first().copied())
}

fn manual_capability(
    protocol: ProviderProtocol,
    model_id: &str,
    levels: &[ThinkingLevel],
) -> Option<ThinkingCapability> {
    if levels.is_empty() {
        return None;
    }
    let supports_off = match protocol {
        ProviderProtocol::Gemini => {
            let id = model_id.trim().to_ascii_lowercase();
            !(id.contains("pro") && !id.contains("flash"))
        }
        _ => true,
    };
    Some(ThinkingCapability {
        source: ThinkingCapabilitySource::Manual,
        dialect: matches!(
            protocol,
            ProviderProtocol::OpenAiChat | ProviderProtocol::OpenAiResponses
        )
        .then_some(ThinkingDialect::ReasoningEffort),
        supports_off,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorized(
        applied: &AppliedThinkingControl,
        mode: ThinkingMode,
        attempt: u8,
        error: Option<&ProviderErrorEnvelope>,
        content_seen: bool,
        cancelled: bool,
    ) -> bool {
        should_fallback_without_control(
            applied,
            mode,
            attempt,
            Some(422),
            error,
            content_seen,
            cancelled,
        )
    }

    #[test]
    fn fallback_allows_only_heuristic_off_exact_field_rejection_before_content() {
        let applied = AppliedThinkingControl {
            field_path: "reasoning_effort",
            source: ThinkingCapabilitySource::Heuristic,
        };
        let error = ProviderErrorEnvelope {
            code: Some("unsupported_parameter".to_owned()),
            kind: Some("invalid_request_error".to_owned()),
            param: Some("reasoning_effort".to_owned()),
            message: "Unsupported parameter: reasoning_effort".to_owned(),
        };
        assert!(should_fallback_without_control(
            &applied,
            ThinkingMode::Off,
            1,
            Some(400),
            Some(&error),
            false,
            false,
        ));
        let mut invalid_value = error;
        invalid_value.code = Some("invalid_value".to_owned());
        assert!(should_fallback_without_control(
            &applied,
            ThinkingMode::Off,
            1,
            Some(400),
            Some(&invalid_value),
            false,
            false,
        ));
    }

    #[test]
    fn fallback_rejects_every_disallowed_context() {
        let heuristic = AppliedThinkingControl {
            field_path: "enable_thinking",
            source: ThinkingCapabilitySource::Heuristic,
        };
        let explicit = AppliedThinkingControl {
            field_path: "enable_thinking",
            source: ThinkingCapabilitySource::Explicit,
        };
        let matching = ProviderErrorEnvelope {
            code: Some("unknown_field".to_owned()),
            kind: None,
            param: Some("enable_thinking".to_owned()),
            message: "unknown field enable_thinking".to_owned(),
        };
        let unrelated = ProviderErrorEnvelope {
            code: Some("unknown_field".to_owned()),
            kind: None,
            param: Some("temperature".to_owned()),
            message: "unknown field temperature".to_owned(),
        };
        let generic = ProviderErrorEnvelope {
            code: Some("bad_request".to_owned()),
            kind: None,
            param: Some("enable_thinking".to_owned()),
            message: "bad request".to_owned(),
        };

        assert!(!authorized(
            &explicit,
            ThinkingMode::Off,
            1,
            Some(&matching),
            false,
            false
        ));
        assert!(!authorized(
            &heuristic,
            ThinkingMode::High,
            1,
            Some(&matching),
            false,
            false
        ));
        assert!(!authorized(
            &heuristic,
            ThinkingMode::Off,
            2,
            Some(&matching),
            false,
            false
        ));
        assert!(!authorized(
            &heuristic,
            ThinkingMode::Off,
            1,
            Some(&matching),
            true,
            false
        ));
        assert!(!authorized(
            &heuristic,
            ThinkingMode::Off,
            1,
            Some(&matching),
            false,
            true
        ));
        assert!(!authorized(
            &heuristic,
            ThinkingMode::Off,
            1,
            Some(&unrelated),
            false,
            false
        ));
        assert!(!authorized(
            &heuristic,
            ThinkingMode::Off,
            1,
            Some(&generic),
            false,
            false
        ));
        assert!(!authorized(
            &heuristic,
            ThinkingMode::Off,
            1,
            None,
            false,
            false
        ));
    }

    #[test]
    fn fallback_message_match_requires_field_boundaries() {
        let applied = AppliedThinkingControl {
            field_path: "reasoning_effort",
            source: ThinkingCapabilitySource::Heuristic,
        };
        for message in [
            "unknown field reasoning_effort",
            "unknown field `reasoning_effort`",
            "unknown field $.reasoning_effort",
        ] {
            let error = ProviderErrorEnvelope {
                code: Some("unknown_field".to_owned()),
                kind: None,
                param: None,
                message: message.to_owned(),
            };
            assert!(authorized(
                &applied,
                ThinkingMode::Off,
                1,
                Some(&error),
                false,
                false
            ));
        }
        let false_positive = ProviderErrorEnvelope {
            code: Some("unknown_field".to_owned()),
            kind: None,
            param: None,
            message: "unknown field not_reasoning_effort_backup".to_owned(),
        };
        assert!(!authorized(
            &applied,
            ThinkingMode::Off,
            1,
            Some(&false_positive),
            false,
            false,
        ));
    }

    #[test]
    fn thinking_openai_dialect_never_contains_qwen_fields() {
        let plan = ThinkingRequestPlan {
            capability: Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Explicit,
                dialect: Some(ThinkingDialect::ReasoningEffort),
                supports_off: true,
            }),
            levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            mode: ThinkingMode::Off,
        };
        let mut body = serde_json::json!({"model":"o3-mini","stream":true});
        let applied =
            apply_thinking_to_body(ProviderProtocol::OpenAiChat, &mut body, &plan).unwrap();
        assert_eq!(body["reasoning_effort"], "none");
        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
        assert_eq!(applied.field_path, "reasoning_effort");
    }

    #[test]
    fn thinking_qwen_dialects_never_contain_reasoning_effort() {
        for dialect in [
            ThinkingDialect::EnableThinking,
            ThinkingDialect::ChatTemplateKwargs,
        ] {
            let plan = ThinkingRequestPlan {
                capability: Some(ThinkingCapability {
                    source: ThinkingCapabilitySource::Explicit,
                    dialect: Some(dialect),
                    supports_off: true,
                }),
                levels: vec![
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                ],
                mode: ThinkingMode::Off,
            };
            let mut body = serde_json::json!({"model":"qwen3","stream":true});
            apply_thinking_to_body(ProviderProtocol::OpenAiChat, &mut body, &plan).unwrap();
            assert!(body.get("reasoning_effort").is_none());
            match dialect {
                ThinkingDialect::EnableThinking => assert_eq!(body["enable_thinking"], false),
                ThinkingDialect::ChatTemplateKwargs => {
                    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false)
                }
                ThinkingDialect::ReasoningEffort | ThinkingDialect::ReasoningEffortMax => {
                    unreachable!()
                }
            }
        }
    }

    #[test]
    fn thinking_off_omits_none_when_explicit_levels_do_not_support_off() {
        let plan = ThinkingRequestPlan {
            capability: Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Explicit,
                dialect: Some(ThinkingDialect::ReasoningEffort),
                supports_off: false,
            }),
            levels: vec![ThinkingLevel::Low, ThinkingLevel::Medium],
            mode: ThinkingMode::Off,
        };
        let mut body = serde_json::json!({"model":"vendor/model","stream":true});
        assert!(apply_thinking_to_body(ProviderProtocol::OpenAiChat, &mut body, &plan).is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn thinking_explicit_empty_metadata_blocks_name_inference() {
        let item = serde_json::json!({"id":"o3-mini","supported_parameters":[]});
        let detected = extract_thinking_capability_from_api_item(&item).unwrap();
        assert!(detected.levels.is_empty());
        assert_eq!(
            detected.capability.source,
            ThinkingCapabilitySource::Explicit
        );
        assert_eq!(detected.capability.dialect, None);

        let unsupported = extract_thinking_capability_from_api_item(&serde_json::json!({
            "id": "o3-mini",
            "supports_thinking": false
        }))
        .unwrap();
        assert!(unsupported.levels.is_empty());
        assert_eq!(unsupported.capability.dialect, None);

        let malformed_options = extract_thinking_capability_from_api_item(&serde_json::json!({
            "id": "o3-mini",
            "reasoning_effort_options": "low"
        }))
        .unwrap();
        assert!(malformed_options.levels.is_empty());
        assert_eq!(malformed_options.capability.dialect, None);
    }

    #[test]
    fn thinking_silent_metadata_uses_family_specific_heuristics() {
        assert_eq!(
            infer_thinking_capability("o3-mini")
                .unwrap()
                .capability
                .dialect,
            Some(ThinkingDialect::ReasoningEffort)
        );
        assert_eq!(
            infer_thinking_capability("qwen3-235b-a22b")
                .unwrap()
                .capability
                .dialect,
            Some(ThinkingDialect::EnableThinking)
        );
        assert!(infer_thinking_capability("gpt-4o-mini").is_none());
    }

    #[test]
    fn thinking_family_versions_generalize_beyond_known_models() {
        for id in [
            "gpt6-luna",
            "gpt-6-luna",
            "openai/gpt-6-luna",
            "gpt-5.1-codex",
        ] {
            let detected =
                infer_protocol_thinking_capability(ProviderProtocol::OpenAiChat, id).unwrap();
            assert_eq!(detected.levels, openai_full_levels(), "{id}");
            assert_eq!(
                detected.capability.dialect,
                Some(ThinkingDialect::ReasoningEffort),
                "{id}"
            );
            assert!(detected.capability.supports_off, "{id}");
        }

        for id in [
            "claude-sonnet-4-5",
            "anthropic/claude-3.7-sonnet",
            "gemini-2.5-pro",
            "google/gemini-3-pro",
            "glm-4.6",
            "glm-z1-air",
            "kimi-k3",
            "grok-4-fast",
            "magistral-medium-2506",
            "gpt-oss-120b",
        ] {
            let detected =
                infer_protocol_thinking_capability(ProviderProtocol::OpenAiChat, id).unwrap();
            assert_eq!(detected.levels, standard_levels(), "{id}");
            assert_eq!(
                detected.capability.dialect,
                Some(ThinkingDialect::ReasoningEffort),
                "{id}"
            );
        }

        // DeepSeek R1/reasoner always reasons and accepts no effort field;
        // exposing gears would make every level request fail.
        let deepseek_reasoner =
            infer_protocol_thinking_capability(ProviderProtocol::OpenAiChat, "deepseek-r1")
                .unwrap();
        assert!(deepseek_reasoner.levels.is_empty());
        assert_eq!(deepseek_reasoner.capability.dialect, None);
        assert!(!deepseek_reasoner.capability.supports_off);

        for id in [
            "gpt-4o",
            "gpt-4.1",
            "chatgpt-4o-latest",
            "qwen-plus-2025",
            "qwen2.5-72b-instruct",
            "glm-4-plus",
            "grok-3",
            "deepseek-chat",
            "deepseek-v3",
        ] {
            assert!(
                infer_protocol_thinking_capability(ProviderProtocol::OpenAiChat, id).is_none(),
                "{id}"
            );
        }

        for (protocol, id) in [
            (ProviderProtocol::AnthropicMessages, "gpt6-luna"),
            (ProviderProtocol::AnthropicMessages, "glm-4.6"),
            (ProviderProtocol::Gemini, "glm-4.6"),
            (ProviderProtocol::Gemini, "gpt6-luna"),
        ] {
            assert!(
                infer_protocol_thinking_capability(protocol, id).is_none(),
                "{id}"
            );
        }
    }

    #[test]
    fn thinking_explicit_metadata_selects_one_safe_dialect() {
        let effort = extract_thinking_capability_from_api_item(&serde_json::json!({
            "supported_parameters": ["reasoning_effort"],
            "reasoning": {"effort_options": ["none", "minimal", "high", "x-high"]}
        }))
        .unwrap();
        assert_eq!(
            effort.levels,
            vec![
                ThinkingLevel::Minimal,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh
            ]
        );
        assert_eq!(
            effort.capability.dialect,
            Some(ThinkingDialect::ReasoningEffort)
        );
        assert!(effort.capability.supports_off);

        let boolean = extract_thinking_capability_from_api_item(&serde_json::json!({
            "supported_parameters": ["enable_thinking"]
        }))
        .unwrap();
        assert_eq!(
            boolean.capability.dialect,
            Some(ThinkingDialect::EnableThinking)
        );

        let nested = extract_thinking_capability_from_api_item(&serde_json::json!({
            "supported_parameters": ["chat_template_kwargs.enable_thinking"]
        }))
        .unwrap();
        assert_eq!(
            nested.capability.dialect,
            Some(ThinkingDialect::ChatTemplateKwargs)
        );
    }

    #[test]
    fn thinking_ambiguous_explicit_metadata_never_guesses_a_dialect() {
        for parameters in [
            serde_json::json!(["reasoning"]),
            serde_json::json!(["include_reasoning"]),
            serde_json::json!(["reasoning_effort", "enable_thinking"]),
        ] {
            let detected = extract_thinking_capability_from_api_item(&serde_json::json!({
                "supported_parameters": parameters
            }))
            .unwrap();
            assert_eq!(
                detected.capability.source,
                ThinkingCapabilitySource::Explicit
            );
            assert_eq!(detected.capability.dialect, None);
        }
    }

    #[test]
    fn thinking_protocol_inference_distinguishes_anthropic_and_gemini() {
        let anthropic = infer_protocol_thinking_capability(
            ProviderProtocol::AnthropicMessages,
            "claude-sonnet-4-5",
        )
        .unwrap();
        assert_eq!(anthropic.levels, standard_levels());
        assert!(anthropic.capability.supports_off);
        assert!(infer_protocol_thinking_capability(
            ProviderProtocol::AnthropicMessages,
            "claude-3-5-sonnet"
        )
        .is_none());

        let flash =
            infer_protocol_thinking_capability(ProviderProtocol::Gemini, "gemini-2.5-flash")
                .unwrap();
        assert!(flash.capability.supports_off);
        let pro =
            infer_protocol_thinking_capability(ProviderProtocol::Gemini, "gemini-2.5-pro").unwrap();
        assert!(!pro.capability.supports_off);
        assert!(
            infer_protocol_thinking_capability(ProviderProtocol::Gemini, "gemini-1.5-pro-003")
                .is_none()
        );
        assert!(infer_protocol_thinking_capability(
            ProviderProtocol::Gemini,
            "gemini-2.5-flash-image-preview"
        )
        .is_none());
    }

    #[test]
    fn thinking_deepseek_v4_maps_low_high_max() {
        let detected = infer_protocol_thinking_capability(
            ProviderProtocol::OpenAiChat,
            "deepseek-ai/deepseek-v4.1-flash-1017",
        )
        .unwrap();
        assert_eq!(detected.levels, deepseek_v4_levels());
        assert_eq!(
            detected.capability.dialect,
            Some(ThinkingDialect::ReasoningEffortMax)
        );
        assert!(detected.capability.supports_off);
        assert_eq!(parse_level_str("max"), Some(ThinkingLevel::Xhigh));

        for (mode, expected) in [
            (ThinkingMode::Low, "low"),
            (ThinkingMode::High, "high"),
            (ThinkingMode::Xhigh, "max"),
        ] {
            let plan = ThinkingRequestPlan {
                capability: Some(detected.capability.clone()),
                levels: detected.levels.clone(),
                mode,
            };
            let mut body = serde_json::json!({"model": "m"});
            apply_thinking_to_body(ProviderProtocol::OpenAiChat, &mut body, &plan).unwrap();
            assert_eq!(body["reasoning_effort"], expected);
        }

        let plan = ThinkingRequestPlan {
            capability: Some(detected.capability),
            levels: detected.levels.clone(),
            mode: ThinkingMode::Xhigh,
        };
        let mut body = serde_json::json!({"model": "m"});
        apply_thinking_to_body(ProviderProtocol::OpenAiResponses, &mut body, &plan).unwrap();
        assert_eq!(body["reasoning"]["effort"], "max");

        let auto = resolve_thinking_for_request(
            ProviderProtocol::OpenAiChat,
            "deepseek-ai/deepseek-v4.1-flash-1017",
            true,
            None,
            &deepseek_v4_levels(),
            None,
            ThinkingMode::Auto,
        );
        assert_eq!(auto.mode, ThinkingMode::High);
    }

    #[test]
    fn thinking_auto_mode_uses_model_default_and_model_disable_wins() {
        let auto = resolve_thinking_for_request(
            ProviderProtocol::AnthropicMessages,
            "claude-sonnet-4-5",
            true,
            Some(ThinkingLevel::High),
            &[
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            None,
            ThinkingMode::Auto,
        );
        assert_eq!(auto.mode, ThinkingMode::High);

        let disabled = resolve_thinking_for_request(
            ProviderProtocol::AnthropicMessages,
            "claude-sonnet-4-5",
            false,
            Some(ThinkingLevel::High),
            &[
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            None,
            ThinkingMode::Auto,
        );
        assert_eq!(disabled.mode, ThinkingMode::Off);

        let no_default = resolve_thinking_for_request(
            ProviderProtocol::OpenAiChat,
            "gpt-5",
            true,
            None,
            &[ThinkingLevel::Low],
            None,
            ThinkingMode::Auto,
        );
        assert_eq!(no_default.mode, ThinkingMode::Low);
    }

    #[test]
    fn thinking_off_uses_gemini_minimum_budget_when_it_cannot_disable() {
        let plan = ThinkingRequestPlan {
            capability: Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Heuristic,
                dialect: None,
                supports_off: false,
            }),
            levels: standard_levels(),
            mode: ThinkingMode::Off,
        };
        let mut body = serde_json::json!({"contents": []});
        let applied = apply_thinking_to_body(ProviderProtocol::Gemini, &mut body, &plan).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            GEMINI_MIN_THINKING_BUDGET
        );
        assert_eq!(
            applied.field_path,
            "generationConfig.thinkingConfig.thinkingBudget"
        );
    }

    #[test]
    fn thinking_off_uses_zero_budget_when_gemini_can_disable() {
        let plan = ThinkingRequestPlan {
            capability: Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Heuristic,
                dialect: None,
                supports_off: true,
            }),
            levels: standard_levels(),
            mode: ThinkingMode::Off,
        };
        let mut body = serde_json::json!({"contents": []});
        apply_thinking_to_body(ProviderProtocol::Gemini, &mut body, &plan).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
    }

    #[test]
    fn thinking_resolution_prefers_persisted_evidence_and_legacy_levels() {
        let persisted = ThinkingCapability {
            source: ThinkingCapabilitySource::Explicit,
            dialect: None,
            supports_off: false,
        };
        let plan = resolve_thinking_for_request(
            ProviderProtocol::OpenAiChat,
            "o3-mini",
            true,
            None,
            &[ThinkingLevel::Low],
            Some(&persisted),
            ThinkingMode::High,
        );
        assert_eq!(plan.capability, Some(persisted));
        assert_eq!(plan.levels, vec![ThinkingLevel::Low]);
        assert_eq!(plan.mode, ThinkingMode::Off);

        let legacy = resolve_thinking_for_request(
            ProviderProtocol::OpenAiChat,
            "gpt-5",
            true,
            None,
            &[ThinkingLevel::High],
            None,
            ThinkingMode::High,
        );
        assert_eq!(
            legacy.capability.unwrap().source,
            ThinkingCapabilitySource::Heuristic
        );
        assert_eq!(legacy.levels, vec![ThinkingLevel::High]);
        assert_eq!(legacy.mode, ThinkingMode::High);
    }

    #[test]
    fn thinking_non_off_boolean_dialect_sends_true_only() {
        let plan = ThinkingRequestPlan {
            capability: Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Heuristic,
                dialect: Some(ThinkingDialect::EnableThinking),
                supports_off: true,
            }),
            levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            mode: ThinkingMode::Medium,
        };
        let mut body = serde_json::json!({"model":"qwen3"});
        let applied =
            apply_thinking_to_body(ProviderProtocol::OpenAiChat, &mut body, &plan).unwrap();
        assert_eq!(body["enable_thinking"], true);
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(applied.field_path, "enable_thinking");
    }
}
