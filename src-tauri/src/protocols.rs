//! Protocol-specific request shaping, auth, model listing and event decoding.
//!
//! The transport layer stays protocol-neutral: it only moves bytes, enforces
//! deadlines and cancellation. Everything that differs between OpenAI Chat
//! Completions, OpenAI Responses, Anthropic Messages and Gemini lives here.

use reqwest::RequestBuilder;
use serde_json::{json, Value};
use url::Url;

use crate::{
    actions::{ChatMessage, ChatRole},
    models::{ProviderConfig, ProviderModel, ProviderProtocol, ThinkingLevel},
    openai_protocol::SseCodec,
    thinking::{self, AppliedThinkingControl, ThinkingRequestPlan},
};

const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4096;
const ANTHROPIC_THINKING_MAX_TOKENS: u64 = 8192;
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_REMOTE_MODELS: usize = 200;

fn anthropic_max_tokens(model: &str) -> u64 {
    let id = model.trim().to_ascii_lowercase();
    if [
        "claude-3-7",
        "claude-3.7",
        "claude-sonnet-4",
        "claude-opus-4",
        "claude-haiku-4",
        "claude-4",
    ]
    .iter()
    .any(|family| id.contains(family))
    {
        ANTHROPIC_THINKING_MAX_TOKENS
    } else {
        // Older Claude 3/3.5 models cap output at 4096 tokens.
        ANTHROPIC_DEFAULT_MAX_TOKENS
    }
}

pub(crate) fn sse_codec(protocol: ProviderProtocol) -> SseCodec {
    match protocol {
        ProviderProtocol::OpenAiChat => SseCodec::OpenAiChat,
        ProviderProtocol::OpenAiResponses => SseCodec::OpenAiResponses,
        ProviderProtocol::AnthropicMessages => SseCodec::AnthropicMessages,
        ProviderProtocol::Gemini => SseCodec::Gemini,
    }
}

pub(crate) fn endpoint(provider: &ProviderConfig, model: &str) -> Result<String, String> {
    let base = provider.base_url.trim_end_matches('/');
    match provider.protocol {
        ProviderProtocol::OpenAiChat => Ok(format!("{base}/chat/completions")),
        ProviderProtocol::OpenAiResponses => Ok(format!("{base}/responses")),
        ProviderProtocol::AnthropicMessages => Ok(format!("{base}/messages")),
        ProviderProtocol::Gemini => gemini_url(base, model, true),
    }
}

pub(crate) fn models_endpoint(provider: &ProviderConfig) -> Result<String, String> {
    let base = provider.base_url.trim_end_matches('/');
    match provider.protocol {
        ProviderProtocol::OpenAiChat | ProviderProtocol::OpenAiResponses => {
            Ok(format!("{base}/models"))
        }
        ProviderProtocol::AnthropicMessages => Ok(format!("{base}/models?limit=1000")),
        ProviderProtocol::Gemini => {
            let mut url = Url::parse(base).map_err(|_| "API 地址无效".to_owned())?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| "API 地址无效".to_owned())?;
                segments.push("models");
            }
            url.query_pairs_mut().append_pair("pageSize", "1000");
            Ok(url.into())
        }
    }
}

fn gemini_url(base: &str, model: &str, stream: bool) -> Result<String, String> {
    let mut url = Url::parse(base).map_err(|_| "API 地址无效".to_owned())?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "API 地址无效".to_owned())?;
        segments.push("models");
        if stream {
            segments.push(&format!("{model}:streamGenerateContent"));
        } else {
            segments.push(&format!("{model}:generateContent"));
        }
    }
    if stream {
        url.query_pairs_mut().append_pair("alt", "sse");
    }
    Ok(url.into())
}

pub(crate) fn apply_auth(
    builder: RequestBuilder,
    protocol: ProviderProtocol,
    api_key: &str,
) -> RequestBuilder {
    match protocol {
        ProviderProtocol::OpenAiChat | ProviderProtocol::OpenAiResponses => {
            builder.bearer_auth(api_key)
        }
        ProviderProtocol::AnthropicMessages => builder
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION),
        ProviderProtocol::Gemini => builder.header("x-goog-api-key", api_key),
    }
}

pub(crate) fn build_base_body(
    protocol: ProviderProtocol,
    model: &str,
    messages: &[ChatMessage],
) -> Value {
    match protocol {
        ProviderProtocol::OpenAiChat => json!({
            "model": model,
            "stream": true,
            "messages": messages,
        }),
        ProviderProtocol::OpenAiResponses => {
            let input = messages
                .iter()
                .map(|message| {
                    json!({
                        "role": role_str(message.role),
                        "content": message.content,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "model": model,
                "stream": true,
                // Responses defaults to server-side storage; local desktop
                // requests must not leave a stored copy behind.
                "store": false,
                "input": input,
            })
        }
        ProviderProtocol::AnthropicMessages => {
            let mut system = Vec::new();
            let mut turns = Vec::new();
            for message in messages {
                match message.role {
                    ChatRole::System => system.push(message.content.as_str()),
                    ChatRole::User => turns.push(json!({
                        "role": "user",
                        "content": message.content,
                    })),
                    ChatRole::Assistant => turns.push(json!({
                        "role": "assistant",
                        "content": message.content,
                    })),
                }
            }
            let mut body = json!({
                "model": model,
                "max_tokens": anthropic_max_tokens(model),
                "stream": true,
                "messages": turns,
            });
            if !system.is_empty() {
                body["system"] = json!(system.join("\n\n"));
            }
            body
        }
        ProviderProtocol::Gemini => {
            let mut system = Vec::new();
            let mut contents = Vec::new();
            for message in messages {
                match message.role {
                    ChatRole::System => system.push(message.content.as_str()),
                    ChatRole::User => contents.push(json!({
                        "role": "user",
                        "parts": [{"text": message.content}],
                    })),
                    ChatRole::Assistant => contents.push(json!({
                        "role": "model",
                        "parts": [{"text": message.content}],
                    })),
                }
            }
            let mut body = json!({
                "contents": contents,
            });
            if !system.is_empty() {
                body["systemInstruction"] = json!({
                    "parts": [{"text": system.join("\n\n")}],
                });
            }
            body
        }
    }
}

pub(crate) fn apply_thinking(
    protocol: ProviderProtocol,
    body: &mut Value,
    plan: &ThinkingRequestPlan,
) -> Option<AppliedThinkingControl> {
    if protocol == ProviderProtocol::AnthropicMessages
        && body.get("max_tokens").is_none()
        && plan.mode != crate::models::ThinkingMode::Off
    {
        // Anthropic rejects thinking without max_tokens; keep the invariant
        // even when a caller built a partial body.
        body["max_tokens"] = json!(ANTHROPIC_THINKING_MAX_TOKENS);
    }
    thinking::apply_thinking_to_body(protocol, body, plan)
}

fn role_str(role: ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    }
}

pub(crate) fn parse_models(
    protocol: ProviderProtocol,
    payload: &Value,
) -> Result<Vec<ProviderModel>, String> {
    let items = match protocol {
        ProviderProtocol::Gemini => payload
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| "模型列表响应缺少 models 数组".to_owned())?,
        _ => payload
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| "模型列表响应缺少 data 数组".to_owned())?,
    };
    let mut models = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for item in items.iter().take(MAX_REMOTE_MODELS) {
        if protocol == ProviderProtocol::Gemini
            && item
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .is_some_and(|methods| {
                    !methods
                        .iter()
                        .any(|method| method.as_str() == Some("generateContent"))
                })
        {
            continue;
        }
        let raw_id = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let id = raw_id.strip_prefix("models/").unwrap_or(raw_id).trim();
        if id.is_empty()
            || id.chars().count() > 256
            || id.chars().any(char::is_control)
            || !ids.insert(id.to_owned())
        {
            continue;
        }
        let name = item
            .get("display_name")
            .or_else(|| item.get("displayName"))
            .or_else(|| {
                // Gemini's `name` is the resource path, not a display name.
                (protocol != ProviderProtocol::Gemini)
                    .then(|| item.get("name"))
                    .flatten()
            })
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && name.chars().count() <= 256)
            .unwrap_or(id);
        let detected = if matches!(
            protocol,
            ProviderProtocol::OpenAiChat | ProviderProtocol::OpenAiResponses
        ) {
            thinking::extract_thinking_capability_from_api_item(item)
                .or_else(|| thinking::infer_protocol_thinking_capability(protocol, id))
        } else {
            thinking::infer_protocol_thinking_capability(protocol, id)
        };
        let (thinking_levels, thinking_capability) = detected
            .map(|value| (value.levels, Some(value.capability)))
            .unwrap_or_default();
        let thinking_default = default_thinking_level(&thinking_levels);
        models.push(ProviderModel {
            id: id.to_owned(),
            name: name.to_owned(),
            thinking_levels,
            thinking_capability,
            thinking_enabled: true,
            thinking_default,
        });
    }
    Ok(models)
}

fn default_thinking_level(levels: &[ThinkingLevel]) -> Option<ThinkingLevel> {
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

/// Preserve per-model user choices across a remote model refresh.
pub(crate) fn merge_remote_models(
    existing: &[ProviderModel],
    remote: Vec<ProviderModel>,
) -> Vec<ProviderModel> {
    use std::collections::HashSet;

    let remote_ids: HashSet<&str> = remote.iter().map(|model| model.id.as_str()).collect();
    let manual_stale = existing
        .iter()
        .filter(|model| {
            !remote_ids.contains(model.id.as_str())
                && model
                    .thinking_capability
                    .as_ref()
                    .is_some_and(|capability| {
                        capability.source == crate::models::ThinkingCapabilitySource::Manual
                    })
        })
        .count();
    // Remote entries always win seats; manual-only entries fill the remainder.
    let keep_manual_stale =
        manual_stale.min(crate::models::MAX_PROVIDER_MODELS.saturating_sub(remote.len()));
    let mut kept_manual_stale = 0usize;
    let mut merged: Vec<ProviderModel> = existing
        .iter()
        .filter(|model| {
            if remote_ids.contains(model.id.as_str()) {
                return true;
            }
            if kept_manual_stale < keep_manual_stale
                && model
                    .thinking_capability
                    .as_ref()
                    .is_some_and(|capability| {
                        capability.source == crate::models::ThinkingCapabilitySource::Manual
                    })
            {
                kept_manual_stale += 1;
                return true;
            }
            false
        })
        .cloned()
        .collect();
    for incoming in remote {
        if let Some(current) = merged.iter_mut().find(|model| model.id == incoming.id) {
            current.name = incoming.name;
            let manual = current
                .thinking_capability
                .as_ref()
                .is_some_and(|capability| {
                    capability.source == crate::models::ThinkingCapabilitySource::Manual
                });
            if !manual {
                let preserved_default = current
                    .thinking_default
                    .filter(|level| incoming.thinking_levels.contains(level));
                current.thinking_levels = incoming.thinking_levels;
                current.thinking_capability = incoming.thinking_capability;
                current.thinking_default = preserved_default.or(incoming.thinking_default);
            }
        } else {
            merged.push(incoming);
        }
    }
    merged.truncate(crate::models::MAX_PROVIDER_MODELS);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ThinkingCapability, ThinkingCapabilitySource, ThinkingMode};

    fn provider(protocol: ProviderProtocol, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            id: "provider".to_owned(),
            name: "Provider".to_owned(),
            enabled: true,
            protocol,
            base_url: base_url.to_owned(),
            models: Vec::new(),
        }
    }

    #[test]
    fn endpoints_follow_each_protocol() {
        assert_eq!(
            endpoint(
                &provider(ProviderProtocol::OpenAiChat, "https://api.example.com/v1"),
                "m"
            )
            .unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint(
                &provider(
                    ProviderProtocol::OpenAiResponses,
                    "https://api.example.com/v1"
                ),
                "m"
            )
            .unwrap(),
            "https://api.example.com/v1/responses"
        );
        assert_eq!(
            endpoint(
                &provider(
                    ProviderProtocol::AnthropicMessages,
                    "https://api.anthropic.com/v1"
                ),
                "m"
            )
            .unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            endpoint(
                &provider(
                    ProviderProtocol::Gemini,
                    "https://generativelanguage.googleapis.com/v1beta"
                ),
                "gemini-2.5-flash"
            )
            .unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn model_lists_parse_each_protocol_shape() {
        let openai = parse_models(
            ProviderProtocol::OpenAiChat,
            &json!({"data": [{"id": "gpt-5", "name": "GPT-5"}]}),
        )
        .unwrap();
        assert_eq!(openai[0].id, "gpt-5");
        assert_eq!(openai[0].name, "GPT-5");
        assert_eq!(
            openai[0].thinking_capability.as_ref().unwrap().source,
            ThinkingCapabilitySource::Heuristic
        );

        let anthropic = parse_models(
            ProviderProtocol::AnthropicMessages,
            &json!({"data": [{"id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5"}]}),
        )
        .unwrap();
        assert_eq!(anthropic[0].id, "claude-sonnet-4-5");
        assert_eq!(anthropic[0].name, "Claude Sonnet 4.5");
        assert_eq!(anthropic[0].thinking_levels.len(), 3);

        let gemini = parse_models(
            ProviderProtocol::Gemini,
            &json!({"models": [
                {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/text-embedding-004", "supportedGenerationMethods": ["embedContent"]}
            ]}),
        )
        .unwrap();
        assert_eq!(gemini.len(), 1);
        assert_eq!(gemini[0].id, "gemini-2.5-pro");
        assert_eq!(gemini[0].name, "Gemini 2.5 Pro");
        assert!(!gemini[0].thinking_capability.as_ref().unwrap().supports_off);
    }

    #[test]
    fn request_bodies_match_each_protocol() {
        let messages = vec![
            ChatMessage {
                role: ChatRole::System,
                content: "system".to_owned(),
            },
            ChatMessage {
                role: ChatRole::User,
                content: "hello".to_owned(),
            },
        ];
        let chat = build_base_body(ProviderProtocol::OpenAiChat, "m", &messages);
        assert_eq!(chat["messages"][1]["content"], "hello");

        let responses = build_base_body(ProviderProtocol::OpenAiResponses, "m", &messages);
        assert_eq!(responses["store"], false);
        assert_eq!(responses["input"][0]["role"], "system");

        let anthropic = build_base_body(
            ProviderProtocol::AnthropicMessages,
            "claude-sonnet-4-5",
            &messages,
        );
        assert_eq!(anthropic["system"], "system");
        assert_eq!(anthropic["messages"][0]["role"], "user");
        assert_eq!(anthropic["max_tokens"], ANTHROPIC_THINKING_MAX_TOKENS);

        let legacy = build_base_body(
            ProviderProtocol::AnthropicMessages,
            "claude-3-5-sonnet",
            &messages,
        );
        assert_eq!(legacy["max_tokens"], ANTHROPIC_DEFAULT_MAX_TOKENS);

        let gemini = build_base_body(ProviderProtocol::Gemini, "m", &messages);
        assert_eq!(gemini["systemInstruction"]["parts"][0]["text"], "system");
        assert_eq!(gemini["contents"][0]["role"], "user");
    }

    #[test]
    fn thinking_mapping_targets_the_right_field() {
        let capability = ThinkingCapability {
            source: ThinkingCapabilitySource::Heuristic,
            dialect: None,
            supports_off: true,
        };
        let plan = |mode| ThinkingRequestPlan {
            capability: Some(capability.clone()),
            levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            mode,
        };

        let mut body = build_base_body(ProviderProtocol::OpenAiResponses, "m", &[]);
        let applied = apply_thinking(
            ProviderProtocol::OpenAiResponses,
            &mut body,
            &plan(ThinkingMode::Medium),
        )
        .unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(applied.field_path, "reasoning.effort");

        let mut body = build_base_body(
            ProviderProtocol::AnthropicMessages,
            "claude-sonnet-4-5",
            &[],
        );
        let applied = apply_thinking(
            ProviderProtocol::AnthropicMessages,
            &mut body,
            &plan(ThinkingMode::High),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], ANTHROPIC_THINKING_MAX_TOKENS);
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        assert_eq!(applied.field_path, "thinking");

        let mut body = build_base_body(
            ProviderProtocol::AnthropicMessages,
            "claude-3-5-sonnet",
            &[],
        );
        apply_thinking(
            ProviderProtocol::AnthropicMessages,
            &mut body,
            &plan(ThinkingMode::High),
        )
        .unwrap();
        assert_eq!(body["max_tokens"], ANTHROPIC_DEFAULT_MAX_TOKENS);
        // Older Claude caps output at 4096, so the budget leaves 1024 for text.
        assert_eq!(body["thinking"]["budget_tokens"], 3072);

        let mut body = build_base_body(ProviderProtocol::Gemini, "m", &[]);
        let applied = apply_thinking(
            ProviderProtocol::Gemini,
            &mut body,
            &plan(ThinkingMode::Low),
        )
        .unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            1024
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
        assert_eq!(
            applied.field_path,
            "generationConfig.thinkingConfig.thinkingBudget"
        );
    }

    #[test]
    fn remote_merge_keeps_manual_thinking_levels() {
        let existing = vec![ProviderModel {
            id: "m".to_owned(),
            name: "old".to_owned(),
            thinking_levels: vec![ThinkingLevel::High],
            thinking_capability: Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Manual,
                dialect: None,
                supports_off: true,
            }),
            thinking_enabled: false,
            thinking_default: Some(ThinkingLevel::High),
        }];
        let remote = vec![ProviderModel {
            id: "m".to_owned(),
            name: "new".to_owned(),
            thinking_levels: vec![ThinkingLevel::Low, ThinkingLevel::Medium],
            thinking_capability: None,
            thinking_enabled: true,
            thinking_default: Some(ThinkingLevel::Medium),
        }];
        let merged = merge_remote_models(&existing, remote);
        assert_eq!(merged[0].name, "new");
        assert_eq!(merged[0].thinking_levels, vec![ThinkingLevel::High]);
        assert_eq!(merged[0].thinking_default, Some(ThinkingLevel::High));
        assert!(!merged[0].thinking_enabled);
    }

    fn model(
        id: &str,
        levels: Vec<ThinkingLevel>,
        default: Option<ThinkingLevel>,
        source: ThinkingCapabilitySource,
    ) -> ProviderModel {
        ProviderModel {
            id: id.to_owned(),
            name: id.to_owned(),
            thinking_levels: levels,
            thinking_capability: Some(ThinkingCapability {
                source,
                dialect: None,
                supports_off: true,
            }),
            thinking_enabled: true,
            thinking_default: default,
        }
    }

    #[test]
    fn remote_merge_keeps_valid_default_and_replaces_invalid_one() {
        let existing = vec![
            model(
                "keep",
                vec![ThinkingLevel::Low, ThinkingLevel::High],
                Some(ThinkingLevel::High),
                ThinkingCapabilitySource::Heuristic,
            ),
            model(
                "reset",
                vec![ThinkingLevel::Low, ThinkingLevel::High],
                Some(ThinkingLevel::High),
                ThinkingCapabilitySource::Heuristic,
            ),
        ];
        let remote = vec![
            model(
                "keep",
                vec![ThinkingLevel::Low, ThinkingLevel::High],
                Some(ThinkingLevel::Low),
                ThinkingCapabilitySource::Heuristic,
            ),
            model(
                "reset",
                vec![ThinkingLevel::Medium],
                Some(ThinkingLevel::Medium),
                ThinkingCapabilitySource::Heuristic,
            ),
        ];
        let merged = merge_remote_models(&existing, remote);
        assert_eq!(merged[0].id, "keep");
        assert_eq!(merged[0].thinking_default, Some(ThinkingLevel::High));
        assert_eq!(merged[1].id, "reset");
        assert_eq!(merged[1].thinking_default, Some(ThinkingLevel::Medium));
    }

    #[test]
    fn remote_merge_drops_stale_models_unless_manual() {
        let existing = vec![
            model(
                "stale-heuristic",
                vec![ThinkingLevel::Low],
                Some(ThinkingLevel::Low),
                ThinkingCapabilitySource::Heuristic,
            ),
            model(
                "stale-manual",
                vec![ThinkingLevel::High],
                Some(ThinkingLevel::High),
                ThinkingCapabilitySource::Manual,
            ),
        ];
        let remote = vec![model(
            "fresh",
            vec![ThinkingLevel::Medium],
            Some(ThinkingLevel::Medium),
            ThinkingCapabilitySource::Heuristic,
        )];
        let merged = merge_remote_models(&existing, remote);
        let ids = merged
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["stale-manual", "fresh"]);
    }

    #[test]
    fn remote_merge_caps_the_combined_list() {
        let existing = (0..5)
            .map(|index| {
                model(
                    &format!("manual-{index}"),
                    vec![ThinkingLevel::High],
                    Some(ThinkingLevel::High),
                    ThinkingCapabilitySource::Manual,
                )
            })
            .collect::<Vec<_>>();
        let remote = (0..crate::models::MAX_PROVIDER_MODELS - 1)
            .map(|index| {
                model(
                    &format!("remote-{index}"),
                    vec![ThinkingLevel::Medium],
                    Some(ThinkingLevel::Medium),
                    ThinkingCapabilitySource::Heuristic,
                )
            })
            .collect::<Vec<_>>();
        let merged = merge_remote_models(&existing, remote);
        assert_eq!(merged.len(), crate::models::MAX_PROVIDER_MODELS);
        // Remote entries own the list; only one spare seat remains for manual rows.
        assert_eq!(merged[0].id, "manual-0");
        assert_eq!(merged[1].id, "remote-0");
        assert!(merged.iter().all(|model| model.id != "manual-1"));
    }
}
