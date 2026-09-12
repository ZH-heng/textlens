use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProtocolLimits {
    pub max_event_bytes: usize,
    pub max_stream_bytes: usize,
}

impl ProtocolLimits {
    pub(crate) const PRODUCTION: Self = Self {
        max_event_bytes: 4 * 1024 * 1024,
        max_stream_bytes: 256 * 1024 * 1024,
    };

    #[cfg(test)]
    pub(crate) const fn test() -> Self {
        Self {
            max_event_bytes: 1024 * 1024,
            max_stream_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseState {
    Open,
    FinishSeen,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedSseData {
    pub content: Option<String>,
    /// Model chain-of-thought / reasoning delta (DeepSeek R1 `reasoning_content`, etc.).
    /// Never merged into answer `content`.
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    pub metadata_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodedSseEvent {
    Ignored,
    Data(DecodedSseData),
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseTermination {
    Done,
    FinishReason,
    CleanEofUnverified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderErrorEnvelope {
    pub code: Option<String>,
    pub kind: Option<String>,
    pub param: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProtocolError {
    InvalidUtf8,
    MalformedJson,
    PartialEvent,
    EventTooLarge,
    StreamTooLarge,
    Provider(ProviderErrorEnvelope),
    ContentAfterFinish,
    DataAfterDone,
}

pub(crate) struct SseDecoder {
    limits: ProtocolLimits,
    buffer: Vec<u8>,
    consumed: usize,
    scan_from: usize,
    raw_bytes: usize,
    state: SseState,
}

impl SseDecoder {
    pub(crate) fn new(limits: ProtocolLimits) -> Self {
        Self {
            limits,
            buffer: Vec::new(),
            consumed: 0,
            scan_from: 0,
            raw_bytes: 0,
            state: SseState::Open,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), ProtocolError> {
        let total = self.raw_bytes.saturating_add(chunk.len());
        if total > self.limits.max_stream_bytes {
            return self.fail(ProtocolError::StreamTooLarge);
        }

        self.raw_bytes = total;
        self.buffer.extend_from_slice(chunk);
        if self.has_oversized_unconsumed_event() {
            return self.fail(ProtocolError::EventTooLarge);
        }
        Ok(())
    }

    pub(crate) fn next_event(&mut self) -> Result<Option<DecodedSseEvent>, ProtocolError> {
        let Some((event_end, separator_len)) = self.next_boundary() else {
            return Ok(None);
        };

        let event_start = self.consumed;
        self.consumed = event_end + separator_len;
        self.scan_from = self.consumed;

        if matches!(self.state, SseState::Done) && event_end > event_start {
            return self.fail(ProtocolError::DataAfterDone);
        }

        let event = match std::str::from_utf8(&self.buffer[event_start..event_end]) {
            Ok(event) => event,
            Err(_) => return self.fail(ProtocolError::InvalidUtf8),
        };

        let mut data_lines = Vec::new();
        for raw_line in event.split('\n') {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            if line.starts_with(':') {
                continue;
            }
            if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.strip_prefix(' ').unwrap_or(value));
            }
        }

        let result = if data_lines.is_empty() {
            Ok(DecodedSseEvent::Ignored)
        } else {
            let data = data_lines.join("\n");
            if data.is_empty() {
                Err(ProtocolError::MalformedJson)
            } else if data == "[DONE]" {
                self.state = SseState::Done;
                Ok(DecodedSseEvent::Done)
            } else {
                self.decode_data(&data)
            }
        };

        self.compact_if_needed();
        match result {
            Ok(event) => Ok(Some(event)),
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn finish(self) -> Result<SseTermination, ProtocolError> {
        if self.consumed != self.buffer.len() {
            return Err(ProtocolError::PartialEvent);
        }

        match self.state {
            SseState::Done => Ok(SseTermination::Done),
            SseState::FinishSeen => Ok(SseTermination::FinishReason),
            SseState::Open => Ok(SseTermination::CleanEofUnverified),
            SseState::Error => Err(ProtocolError::MalformedJson),
        }
    }

    #[cfg(test)]
    pub(crate) fn raw_bytes(&self) -> usize {
        self.raw_bytes
    }

    fn decode_data(&mut self, data: &str) -> Result<DecodedSseEvent, ProtocolError> {
        let value: Value = serde_json::from_str(data).map_err(|_| ProtocolError::MalformedJson)?;
        if let Some(error) = provider_error_from_value(&value) {
            return Err(ProtocolError::Provider(error));
        }

        let content = value
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        // Prefer dedicated reasoning_content; some gateways use delta.reasoning.
        let reasoning = value
            .pointer("/choices/0/delta/reasoning_content")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .pointer("/choices/0/delta/reasoning")
                    .and_then(Value::as_str)
            })
            .map(ToOwned::to_owned);
        let finish_reason = value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
            .map(ToOwned::to_owned);

        if matches!(self.state, SseState::FinishSeen)
            && content.as_deref().is_some_and(|value| !value.is_empty())
        {
            return Err(ProtocolError::ContentAfterFinish);
        }
        if finish_reason.is_some() && matches!(self.state, SseState::Open) {
            self.state = SseState::FinishSeen;
        }

        Ok(DecodedSseEvent::Data(DecodedSseData {
            metadata_only: content.is_none() && reasoning.is_none() && finish_reason.is_none(),
            content,
            reasoning,
            finish_reason,
        }))
    }

    fn next_boundary(&mut self) -> Option<(usize, usize)> {
        let mut index = self.scan_from.max(self.consumed);
        while index < self.buffer.len() {
            if self.buffer[index] == b'\n'
                && self
                    .buffer
                    .get(index + 1)
                    .is_some_and(|byte| *byte == b'\n')
            {
                return Some((index, 2));
            }
            if self.buffer[index] == b'\r'
                && self
                    .buffer
                    .get(index + 1)
                    .is_some_and(|byte| *byte == b'\n')
                && self
                    .buffer
                    .get(index + 2)
                    .is_some_and(|byte| *byte == b'\r')
                && self
                    .buffer
                    .get(index + 3)
                    .is_some_and(|byte| *byte == b'\n')
            {
                return Some((index, 4));
            }
            index += 1;
        }

        self.scan_from = self.buffer.len().saturating_sub(3).max(self.consumed);
        None
    }

    fn has_oversized_unconsumed_event(&self) -> bool {
        let mut event_start = self.consumed;
        let mut index = event_start;
        while index < self.buffer.len() {
            let separator_len = if self.buffer[index] == b'\n'
                && self
                    .buffer
                    .get(index + 1)
                    .is_some_and(|byte| *byte == b'\n')
            {
                Some(2)
            } else if self.buffer[index] == b'\r'
                && self
                    .buffer
                    .get(index + 1)
                    .is_some_and(|byte| *byte == b'\n')
                && self
                    .buffer
                    .get(index + 2)
                    .is_some_and(|byte| *byte == b'\r')
                && self
                    .buffer
                    .get(index + 3)
                    .is_some_and(|byte| *byte == b'\n')
            {
                Some(4)
            } else {
                None
            };

            if let Some(separator_len) = separator_len {
                if index - event_start > self.limits.max_event_bytes {
                    return true;
                }
                event_start = index + separator_len;
                index = event_start;
            } else {
                index += 1;
            }
        }
        self.buffer.len() - event_start > self.limits.max_event_bytes
    }

    fn compact_if_needed(&mut self) {
        if self.consumed >= 64 * 1024 && self.consumed.saturating_mul(2) >= self.buffer.len() {
            let remaining = self.buffer.len() - self.consumed;
            self.buffer.copy_within(self.consumed.., 0);
            self.buffer.truncate(remaining);
            self.consumed = 0;
            self.scan_from = 0;
        }
    }

    fn fail<T>(&mut self, error: ProtocolError) -> Result<T, ProtocolError> {
        self.state = SseState::Error;
        Err(error)
    }
}

pub(crate) fn provider_error_from_value(value: &Value) -> Option<ProviderErrorEnvelope> {
    let object = value.as_object()?;
    match object.get("error") {
        Some(Value::Object(error)) => Some(ProviderErrorEnvelope {
            code: field(error, "code").or_else(|| field(object, "code")),
            kind: field(error, "type")
                .or_else(|| field(error, "kind"))
                .or_else(|| field(object, "type"))
                .or_else(|| field(object, "kind")),
            param: field(error, "param").or_else(|| field(object, "param")),
            message: field(error, "message")
                .or_else(|| field(object, "message"))
                .unwrap_or_else(|| "provider returned an error".to_owned()),
        }),
        Some(Value::String(message)) => Some(ProviderErrorEnvelope {
            code: field(object, "code"),
            kind: field(object, "type").or_else(|| field(object, "kind")),
            param: field(object, "param"),
            message: bounded(message),
        }),
        Some(Value::Null) | None => {
            if object.contains_key("choices") {
                return None;
            }
            Some(ProviderErrorEnvelope {
                code: Some(field(object, "code")?),
                kind: field(object, "type").or_else(|| field(object, "kind")),
                param: field(object, "param"),
                message: field(object, "message")?,
            })
        }
        _ => None,
    }
}

fn field(object: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    object.get(name).and_then(Value::as_str).map(bounded)
}

fn bounded(value: &str) -> String {
    const MAX_RAW_FIELD_CHARS: usize = 1024;
    value.chars().take(MAX_RAW_FIELD_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_decodes_every_split_of_crlf_unicode_and_multiline_data() {
        let wire = concat!(
            ": heartbeat\r\n\r\n",
            "data: {\"choices\":[\r\n",
            "data: {\"delta\":{\"content\":\"你🙂\"},\"finish_reason\":null}]}\r\n\r\n",
            "data: [DONE]\r\n\r\n"
        )
        .as_bytes();

        for split in 0..=wire.len() {
            let mut decoder = SseDecoder::new(ProtocolLimits::test());
            decoder.push(&wire[..split]).unwrap();
            decoder.push(&wire[split..]).unwrap();
            let events = drain(&mut decoder).unwrap();
            assert_eq!(
                events,
                vec![
                    DecodedSseEvent::Ignored,
                    DecodedSseEvent::Data(DecodedSseData {
                        content: Some("你🙂".to_owned()),
                        reasoning: None,
                        finish_reason: None,
                        metadata_only: false,
                    }),
                    DecodedSseEvent::Done,
                ]
            );
            assert_eq!(decoder.finish().unwrap(), SseTermination::Done);
        }
    }

    #[test]
    fn protocol_parses_reasoning_content_without_merging_into_answer() {
        let events = decode_all(
            concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step1\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"reasoning\":\"step2\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"final\"}}]}\n\n",
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            events.events,
            vec![
                DecodedSseEvent::Data(DecodedSseData {
                    content: None,
                    reasoning: Some("step1".to_owned()),
                    finish_reason: None,
                    metadata_only: false,
                }),
                DecodedSseEvent::Data(DecodedSseData {
                    content: None,
                    reasoning: Some("step2".to_owned()),
                    finish_reason: None,
                    metadata_only: false,
                }),
                DecodedSseEvent::Data(DecodedSseData {
                    content: Some("final".to_owned()),
                    reasoning: None,
                    finish_reason: None,
                    metadata_only: false,
                }),
            ]
        );
    }

    #[test]
    fn protocol_rejects_invalid_utf8_malformed_json_and_partial_eof() {
        for bytes in [
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\xFF\"}}]}\n\n".as_slice(),
            b"data: {not-json}\n\n".as_slice(),
        ] {
            let mut decoder = SseDecoder::new(ProtocolLimits::test());
            decoder.push(bytes).unwrap();
            assert!(decoder.next_event().is_err());
        }

        let mut decoder = SseDecoder::new(ProtocolLimits::test());
        decoder
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}")
            .unwrap();
        assert_eq!(decoder.finish(), Err(ProtocolError::PartialEvent));
    }

    #[test]
    fn protocol_accepts_done_finish_reason_and_clean_boundary_eof() {
        let done = decode_all(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\n",
        )
        .unwrap();
        assert_eq!(done.termination, SseTermination::Done);

        let finish = decode_all(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"},\"finish_reason\":\"stop\"}]}\n\n",
        )
        .unwrap();
        assert_eq!(finish.termination, SseTermination::FinishReason);

        let eof = decode_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n").unwrap();
        assert_eq!(eof.termination, SseTermination::CleanEofUnverified);
    }

    #[test]
    fn protocol_rejects_content_after_finish_and_buffered_data_after_done() {
        for wire in [
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n".as_slice(),
            b"data: [DONE]\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n".as_slice(),
        ] {
            let mut decoder = SseDecoder::new(ProtocolLimits::test());
            decoder.push(wire).unwrap();
            let mut error = None;
            loop {
                match decoder.next_event() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(value) => {
                        error = Some(value);
                        break;
                    }
                }
            }
            assert!(error.is_some());
        }
    }

    #[test]
    fn protocol_detects_standard_and_compatible_error_envelopes() {
        let fixtures = [
            serde_json::json!({"error":{"code":"unsupported_parameter","param":"reasoning_effort","message":"unknown field"}}),
            serde_json::json!({"error":"gateway rejected request","code":"unknown_field","param":"enable_thinking"}),
            serde_json::json!({"code":"bad_request","message":"request failed"}),
        ];
        for value in fixtures {
            let error = provider_error_from_value(&value).expect("provider error");
            assert!(!error.message.is_empty());
        }
        assert!(provider_error_from_value(&serde_json::json!({
            "choices":[{"message":{"content":"ok"}}]
        }))
        .is_none());
    }

    #[test]
    fn protocol_enforces_event_and_stream_limits_independently() {
        let limits = ProtocolLimits {
            max_event_bytes: 64,
            max_stream_bytes: 256,
        };
        let mut oversized_event = SseDecoder::new(limits);
        assert_eq!(
            oversized_event.push(&vec![b'x'; 65]),
            Err(ProtocolError::EventTooLarge),
        );

        let mut oversized_stream = SseDecoder::new(limits);
        let tiny_event = b": ok\n\n";
        for _ in 0..42 {
            oversized_stream.push(tiny_event).unwrap();
            let _ = drain(&mut oversized_stream).unwrap();
        }
        assert_eq!(oversized_stream.raw_bytes(), 252);
        assert_eq!(
            oversized_stream.push(tiny_event),
            Err(ProtocolError::StreamTooLarge),
        );
    }

    fn drain(decoder: &mut SseDecoder) -> Result<Vec<DecodedSseEvent>, ProtocolError> {
        let mut events = Vec::new();
        while let Some(event) = decoder.next_event()? {
            events.push(event);
        }
        Ok(events)
    }

    struct DecodedFixture {
        #[allow(dead_code)]
        events: Vec<DecodedSseEvent>,
        termination: SseTermination,
    }

    fn decode_all(bytes: &[u8]) -> Result<DecodedFixture, ProtocolError> {
        let mut decoder = SseDecoder::new(ProtocolLimits::test());
        decoder.push(bytes)?;
        let events = drain(&mut decoder)?;
        let termination = decoder.finish()?;
        Ok(DecodedFixture {
            events,
            termination,
        })
    }
}
