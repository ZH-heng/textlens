use std::{future::Future, ops::ControlFlow, time::Duration};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::Response;
use serde::Serialize;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    models::AI_OUTPUT_LIMIT,
    openai_protocol::{
        decode_json_content, is_incomplete_finish_reason, json_response_is_incomplete,
        provider_error_from_value, DecodedSseEvent, ProtocolError, ProtocolLimits,
        ProviderErrorEnvelope, SseCodec, SseDecoder, SseTermination,
    },
};

const CANCELLATION_CHECKPOINT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportConfig {
    pub headers: Duration,
    pub first_event: Duration,
    pub first_content: Duration,
    pub stream_idle: Duration,
    pub error_body: Duration,
    pub total: Duration,
    pub json_bytes: usize,
    pub error_bytes: usize,
    pub protocol: ProtocolLimits,
    pub output_scalars: usize,
}

impl TransportConfig {
    pub(crate) const fn production() -> Self {
        Self {
            headers: Duration::from_secs(120),
            first_event: Duration::from_secs(120),
            first_content: Duration::from_secs(120),
            stream_idle: Duration::from_secs(60),
            error_body: Duration::from_millis(500),
            total: Duration::from_secs(600),
            json_bytes: 16 * 1024 * 1024,
            error_bytes: 8 * 1024,
            protocol: ProtocolLimits::PRODUCTION,
            output_scalars: AI_OUTPUT_LIMIT,
        }
    }

    #[cfg(test)]
    pub(crate) const fn test() -> Self {
        Self {
            headers: Duration::from_millis(100),
            first_event: Duration::from_millis(100),
            first_content: Duration::from_millis(200),
            stream_idle: Duration::from_millis(50),
            error_body: Duration::from_millis(500),
            total: Duration::from_secs(10),
            json_bytes: 1024 * 1024,
            error_bytes: 8192,
            protocol: ProtocolLimits::test(),
            output_scalars: 1_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GenerationBudget {
    pub started_at: Instant,
    pub total_deadline: Instant,
}

impl GenerationBudget {
    pub(crate) fn new(started_at: Instant, total: Duration) -> Self {
        Self {
            started_at,
            total_deadline: started_at + total,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportMarker {
    RequestSend,
    Headers,
    FirstBodyByte,
    FirstValidEvent,
    FirstContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseTransport {
    Sse,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseTermination {
    Done,
    FinishReason,
    CleanEofUnverified,
    Json,
}

#[derive(Debug)]
pub(crate) struct TransportSuccess {
    pub content: String,
    pub transport: ResponseTransport,
    pub termination: ResponseTermination,
}

#[derive(Debug)]
pub(crate) struct TransportFailure {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub status: Option<u16>,
    pub provider_error: Option<ProviderErrorEnvelope>,
    pub content_seen: bool,
}

#[derive(Clone, Copy)]
enum DeadlineKind {
    Total,
    FirstEvent,
    FirstContent,
    StreamIdle,
}

#[derive(Clone, Copy)]
struct Deadline {
    kind: DeadlineKind,
    instant: Instant,
}

pub(crate) async fn await_headers<F>(
    send: F,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    config: TransportConfig,
) -> Result<Response, TransportFailure>
where
    F: Future<Output = Result<Response, reqwest::Error>>,
{
    let deadline = if budget.started_at + config.headers <= budget.total_deadline {
        Deadline {
            kind: DeadlineKind::FirstEvent,
            instant: budget.started_at + config.headers,
        }
    } else {
        Deadline {
            kind: DeadlineKind::Total,
            instant: budget.total_deadline,
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancelled_failure(false)),
        _ = tokio::time::sleep_until(deadline.instant) => Err(match deadline.kind {
            DeadlineKind::Total => timeout_failure(DeadlineKind::Total, false),
            DeadlineKind::FirstEvent => headers_timeout_failure(),
            DeadlineKind::FirstContent | DeadlineKind::StreamIdle => unreachable!("header wait has no stream deadline"),
        }),
        response = send => response.map_err(reqwest_failure),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn consume_sse_response_for<D, T, M>(
    codec: SseCodec,
    response: Response,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    headers_at: Instant,
    config: TransportConfig,
    on_delta: D,
    on_thinking: T,
    on_marker: M,
) -> Result<TransportSuccess, TransportFailure>
where
    D: FnMut(String) -> ControlFlow<()>,
    T: FnMut(String) -> ControlFlow<()>,
    M: FnMut(TransportMarker, Instant),
{
    consume_sse_chunks_for(
        codec,
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(reqwest_failure)),
        cancellation,
        budget,
        headers_at,
        config,
        on_delta,
        on_thinking,
        on_marker,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn consume_json_response_for<D, T, M>(
    codec: SseCodec,
    response: Response,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    headers_at: Instant,
    config: TransportConfig,
    on_delta: D,
    on_thinking: T,
    on_marker: M,
) -> Result<TransportSuccess, TransportFailure>
where
    D: FnMut(String) -> ControlFlow<()>,
    T: FnMut(String) -> ControlFlow<()>,
    M: FnMut(TransportMarker, Instant),
{
    let _ = on_thinking;
    consume_json_chunks_for(
        codec,
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(reqwest_failure)),
        cancellation,
        budget,
        headers_at,
        config,
        on_delta,
        on_marker,
    )
    .await
}

pub(crate) async fn read_http_error(
    response: Response,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    config: TransportConfig,
) -> Result<Vec<u8>, TransportFailure> {
    read_limited_error_chunks(
        response
            .bytes_stream()
            .map(|chunk| chunk.map_err(reqwest_failure)),
        cancellation,
        budget,
        config,
    )
    .await
}

#[cfg(test)]
async fn consume_sse_chunks<S, D, T, M>(
    chunks: S,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    headers_at: Instant,
    config: TransportConfig,
    on_delta: D,
    on_thinking: T,
    on_marker: M,
) -> Result<TransportSuccess, TransportFailure>
where
    S: Stream<Item = Result<Bytes, TransportFailure>>,
    D: FnMut(String) -> ControlFlow<()>,
    T: FnMut(String) -> ControlFlow<()>,
    M: FnMut(TransportMarker, Instant),
{
    consume_sse_chunks_for(
        SseCodec::OpenAiChat,
        chunks,
        cancellation,
        budget,
        headers_at,
        config,
        on_delta,
        on_thinking,
        on_marker,
    )
    .await
}

async fn consume_sse_chunks_for<S, D, T, M>(
    codec: SseCodec,
    chunks: S,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    headers_at: Instant,
    config: TransportConfig,
    mut on_delta: D,
    mut on_thinking: T,
    mut on_marker: M,
) -> Result<TransportSuccess, TransportFailure>
where
    S: Stream<Item = Result<Bytes, TransportFailure>>,
    D: FnMut(String) -> ControlFlow<()>,
    T: FnMut(String) -> ControlFlow<()>,
    M: FnMut(TransportMarker, Instant),
{
    futures_util::pin_mut!(chunks);
    let mut decoder = SseDecoder::with_codec(config.protocol, codec);
    let mut content = String::new();
    let mut output_scalars = 0usize;
    let mut thinking_scalars = 0usize;
    let mut content_seen = false;
    let mut first_body_seen = false;
    let mut first_event_deadline = Some(headers_at + config.first_event);
    let mut first_content_deadline = Some(headers_at + config.first_content);
    let mut idle_deadline = None;
    let mut bytes_until_checkpoint = CANCELLATION_CHECKPOINT_BYTES;
    let mut done_seen = false;

    loop {
        let next_deadline = earliest_active_deadline(
            budget.total_deadline,
            first_event_deadline,
            first_content_deadline,
            idle_deadline,
        );
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled_failure(content_seen)),
            _ = tokio::time::sleep_until(next_deadline.instant) => {
                return Err(timeout_failure(next_deadline.kind, content_seen));
            }
            next = chunks.next() => next,
        };
        let Some(chunk) = next else {
            let termination = decoder
                .finish()
                .map_err(|error| protocol_failure(error, content_seen))?;
            if content.trim().is_empty() {
                return Err(empty_response_failure(content_seen));
            }
            return Ok(TransportSuccess {
                content,
                transport: ResponseTransport::Sse,
                termination: response_termination(termination),
            });
        };
        let chunk = chunk?;
        if !first_body_seen && !chunk.is_empty() {
            first_body_seen = true;
            on_marker(TransportMarker::FirstBodyByte, Instant::now());
        }

        let mut offset = 0;
        while offset < chunk.len() {
            if cancellation.is_cancelled() {
                return Err(cancelled_failure(content_seen));
            }
            let take = (chunk.len() - offset).min(bytes_until_checkpoint);
            decoder
                .push(&chunk[offset..offset + take])
                .map_err(|error| protocol_failure(error, content_seen))?;
            offset += take;
            bytes_until_checkpoint -= take;

            loop {
                let event = decoder
                    .next_event()
                    .map_err(|error| protocol_failure(error, content_seen))?;
                let Some(event) = event else {
                    if done_seen {
                        if content.trim().is_empty() {
                            return Err(empty_response_failure(content_seen));
                        }
                        return Ok(TransportSuccess {
                            content,
                            transport: ResponseTransport::Sse,
                            termination: ResponseTermination::Done,
                        });
                    }
                    break;
                };

                match event {
                    DecodedSseEvent::Ignored => {}
                    DecodedSseEvent::Done => done_seen = true,
                    DecodedSseEvent::Data(data) => {
                        let incomplete = data
                            .finish_reason
                            .as_deref()
                            .is_some_and(is_incomplete_finish_reason);
                        let now = Instant::now();
                        if first_event_deadline.take().is_some() {
                            on_marker(TransportMarker::FirstValidEvent, now);
                        }
                        if content_seen {
                            idle_deadline = Some(now + config.stream_idle);
                        }
                        // Thinking/reasoning is stream activity: credit first-content
                        // and idle timers so long CoT does not look hung, but never
                        // merge into answer content or answer scalar limits.
                        if let Some(thinking) = data.reasoning.filter(|delta| !delta.is_empty()) {
                            let scalar_count = thinking.chars().count();
                            if thinking_scalars.saturating_add(scalar_count)
                                <= config.output_scalars
                            {
                                thinking_scalars += scalar_count;
                                if matches!(on_thinking(thinking), ControlFlow::Break(())) {
                                    return Err(stale_ticket_failure(content_seen));
                                }
                            }
                            let marked_at = Instant::now();
                            if first_content_deadline.take().is_some() {
                                on_marker(TransportMarker::FirstContent, marked_at);
                            }
                            idle_deadline = Some(marked_at + config.stream_idle);
                        }
                        if let Some(delta) = data.content.filter(|delta| !delta.is_empty()) {
                            let scalar_count = delta.chars().count();
                            if output_scalars.saturating_add(scalar_count) > config.output_scalars {
                                return Err(output_limit_failure(content_seen));
                            }
                            output_scalars += scalar_count;
                            content.push_str(&delta);
                            content_seen = true;
                            if matches!(on_delta(delta), ControlFlow::Break(())) {
                                return Err(stale_ticket_failure(content_seen));
                            }
                            let marked_at = Instant::now();
                            if first_content_deadline.take().is_some() {
                                on_marker(TransportMarker::FirstContent, marked_at);
                            }
                            idle_deadline = Some(marked_at + config.stream_idle);
                        }
                        if incomplete {
                            return Err(incomplete_response_failure(content_seen));
                        }
                    }
                }

                if cancellation.is_cancelled() {
                    return Err(cancelled_failure(content_seen));
                }
                if Instant::now() >= budget.total_deadline {
                    return Err(timeout_failure(DeadlineKind::Total, content_seen));
                }
            }

            if bytes_until_checkpoint == 0 {
                tokio::task::yield_now().await;
                if cancellation.is_cancelled() {
                    return Err(cancelled_failure(content_seen));
                }
                bytes_until_checkpoint = CANCELLATION_CHECKPOINT_BYTES;
            }
        }
    }
}

#[cfg(test)]
async fn consume_json_chunks<S, D, M>(
    chunks: S,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    headers_at: Instant,
    config: TransportConfig,
    on_delta: D,
    on_marker: M,
) -> Result<TransportSuccess, TransportFailure>
where
    S: Stream<Item = Result<Bytes, TransportFailure>>,
    D: FnMut(String) -> ControlFlow<()>,
    M: FnMut(TransportMarker, Instant),
{
    consume_json_chunks_for(
        SseCodec::OpenAiChat,
        chunks,
        cancellation,
        budget,
        headers_at,
        config,
        on_delta,
        on_marker,
    )
    .await
}

async fn consume_json_chunks_for<S, D, M>(
    codec: SseCodec,
    chunks: S,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    headers_at: Instant,
    config: TransportConfig,
    mut on_delta: D,
    mut on_marker: M,
) -> Result<TransportSuccess, TransportFailure>
where
    S: Stream<Item = Result<Bytes, TransportFailure>>,
    D: FnMut(String) -> ControlFlow<()>,
    M: FnMut(TransportMarker, Instant),
{
    futures_util::pin_mut!(chunks);
    let mut body = Vec::new();
    let mut first_body_seen = false;
    let mut first_byte_deadline = Some(headers_at + config.first_event);
    let mut idle_deadline = None;
    loop {
        let deadline = earliest_active_deadline(
            budget.total_deadline,
            first_byte_deadline,
            None,
            idle_deadline,
        );
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled_failure(false)),
            _ = tokio::time::sleep_until(deadline.instant) => {
                return Err(timeout_failure(deadline.kind, false));
            }
            next = chunks.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk?;
        if !first_body_seen && !chunk.is_empty() {
            first_body_seen = true;
            first_byte_deadline = None;
            on_marker(TransportMarker::FirstBodyByte, Instant::now());
        }
        if !chunk.is_empty() {
            idle_deadline = Some(Instant::now() + config.stream_idle);
        }
        if body.len().saturating_add(chunk.len()) > config.json_bytes {
            return Err(response_too_large_failure(false));
        }
        body.extend_from_slice(&chunk);
    }

    let value: Value =
        serde_json::from_slice(&body).map_err(|_| invalid_response_failure(false))?;
    if let Some(envelope) = provider_error_from_value(&value) {
        return Err(provider_failure(envelope, false));
    }
    on_marker(TransportMarker::FirstValidEvent, Instant::now());
    let content = decode_json_content(codec, &value).unwrap_or_default();
    let incomplete = json_response_is_incomplete(codec, &value);
    let scalar_count = content.chars().count();
    if scalar_count > config.output_scalars {
        return Err(output_limit_failure(false));
    }
    if content.trim().is_empty() {
        if incomplete {
            return Err(incomplete_response_failure(false));
        }
        return Err(empty_response_failure(false));
    }
    if cancellation.is_cancelled() {
        return Err(cancelled_failure(false));
    }
    if matches!(on_delta(content.clone()), ControlFlow::Break(())) {
        return Err(stale_ticket_failure(true));
    }
    on_marker(TransportMarker::FirstContent, Instant::now());
    if incomplete {
        return Err(incomplete_response_failure(true));
    }
    Ok(TransportSuccess {
        content,
        transport: ResponseTransport::Json,
        termination: ResponseTermination::Json,
    })
}

async fn read_limited_error_chunks<S>(
    chunks: S,
    cancellation: &CancellationToken,
    budget: &GenerationBudget,
    config: TransportConfig,
) -> Result<Vec<u8>, TransportFailure>
where
    S: Stream<Item = Result<Bytes, TransportFailure>>,
{
    futures_util::pin_mut!(chunks);
    let deadline = (Instant::now() + config.error_body).min(budget.total_deadline);
    let mut body = Vec::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled_failure(false)),
            _ = tokio::time::sleep_until(deadline) => return Err(error_body_timeout_failure()),
            next = chunks.next() => next,
        };
        let Some(chunk) = next else {
            return Ok(body);
        };
        let chunk = chunk?;
        let remaining = config.error_bytes.saturating_sub(body.len());
        if remaining == 0 {
            return Ok(body);
        }
        let take = chunk.len().min(remaining);
        body.extend_from_slice(&chunk[..take]);
        if take < chunk.len() || body.len() == config.error_bytes {
            return Ok(body);
        }
    }
}

fn earliest_active_deadline(
    total_deadline: Instant,
    first_event_deadline: Option<Instant>,
    first_content_deadline: Option<Instant>,
    idle_deadline: Option<Instant>,
) -> Deadline {
    let mut earliest = Deadline {
        kind: DeadlineKind::Total,
        instant: total_deadline,
    };
    for candidate in [
        first_event_deadline.map(|instant| Deadline {
            kind: DeadlineKind::FirstEvent,
            instant,
        }),
        first_content_deadline.map(|instant| Deadline {
            kind: DeadlineKind::FirstContent,
            instant,
        }),
        idle_deadline.map(|instant| Deadline {
            kind: DeadlineKind::StreamIdle,
            instant,
        }),
    ] {
        if let Some(candidate) = candidate.filter(|candidate| candidate.instant < earliest.instant)
        {
            earliest = candidate;
        }
    }
    earliest
}

fn response_termination(termination: SseTermination) -> ResponseTermination {
    match termination {
        SseTermination::Done => ResponseTermination::Done,
        SseTermination::FinishReason => ResponseTermination::FinishReason,
        SseTermination::CleanEofUnverified => ResponseTermination::CleanEofUnverified,
    }
}

fn reqwest_failure(_error: reqwest::Error) -> TransportFailure {
    failure(
        "NETWORK_ERROR",
        "网络连接失败，请检查网络、代理与服务地址",
        true,
        false,
    )
}

fn protocol_failure(error: ProtocolError, content_seen: bool) -> TransportFailure {
    match error {
        ProtocolError::Provider(envelope) => provider_failure(envelope, content_seen),
        ProtocolError::EventTooLarge | ProtocolError::StreamTooLarge => {
            response_too_large_failure(content_seen)
        }
        ProtocolError::DataAfterDone => failure(
            "DATA_AFTER_DONE",
            "模型服务在流结束后继续发送数据",
            false,
            content_seen,
        ),
        ProtocolError::ContentAfterFinish => failure(
            "CONTENT_AFTER_FINISH",
            "模型服务在结束标记后继续发送内容",
            false,
            content_seen,
        ),
        ProtocolError::PartialEvent => failure(
            "INCOMPLETE_STREAM",
            "模型流在完整事件之前结束",
            true,
            content_seen,
        ),
        ProtocolError::InvalidUtf8 | ProtocolError::MalformedJson => {
            invalid_response_failure(content_seen)
        }
    }
}

fn provider_failure(envelope: ProviderErrorEnvelope, content_seen: bool) -> TransportFailure {
    TransportFailure {
        code: "PROVIDER_ERROR",
        message: envelope.message.clone(),
        retryable: false,
        status: None,
        provider_error: Some(envelope),
        content_seen,
    }
}

fn headers_timeout_failure() -> TransportFailure {
    failure(
        "HEADERS_TIMEOUT",
        "模型服务未在规定时间内返回响应头",
        true,
        false,
    )
}

fn error_body_timeout_failure() -> TransportFailure {
    failure(
        "ERROR_BODY_TIMEOUT",
        "模型服务错误详情读取超时",
        true,
        false,
    )
}

fn timeout_failure(kind: DeadlineKind, content_seen: bool) -> TransportFailure {
    let (code, message) = match kind {
        DeadlineKind::Total => ("TOTAL_TIMEOUT", "模型请求超过允许时间，已自动停止"),
        DeadlineKind::FirstEvent => ("FIRST_EVENT_TIMEOUT", "模型流未及时返回有效事件"),
        DeadlineKind::FirstContent => ("FIRST_CONTENT_TIMEOUT", "模型流未及时返回文本内容"),
        DeadlineKind::StreamIdle => ("STREAM_IDLE_TIMEOUT", "模型流长时间未返回有效内容"),
    };
    failure(code, message, true, content_seen)
}

fn cancelled_failure(content_seen: bool) -> TransportFailure {
    failure("CANCELLED", "请求已取消", false, content_seen)
}

fn stale_ticket_failure(content_seen: bool) -> TransportFailure {
    failure(
        "STALE_TICKET",
        "generation ticket is stale",
        false,
        content_seen,
    )
}

fn empty_response_failure(content_seen: bool) -> TransportFailure {
    failure(
        "EMPTY_RESPONSE",
        "模型服务未返回文本内容",
        true,
        content_seen,
    )
}

fn incomplete_response_failure(content_seen: bool) -> TransportFailure {
    failure(
        "INCOMPLETE_RESPONSE",
        if content_seen {
            "模型回复未完整结束，已保留收到的内容，可重试当前请求"
        } else {
            "模型回复未完整结束，可重试当前请求"
        },
        true,
        content_seen,
    )
}

fn output_limit_failure(content_seen: bool) -> TransportFailure {
    failure(
        "OUTPUT_TOO_LARGE",
        "模型输出超过允许长度，已停止读取",
        true,
        content_seen,
    )
}

fn response_too_large_failure(content_seen: bool) -> TransportFailure {
    failure(
        "RESPONSE_TOO_LARGE",
        "模型服务响应过大，已停止读取",
        true,
        content_seen,
    )
}

fn invalid_response_failure(content_seen: bool) -> TransportFailure {
    failure(
        "INVALID_RESPONSE",
        "模型服务返回了无效响应",
        true,
        content_seen,
    )
}

fn failure(
    code: &'static str,
    message: impl Into<String>,
    retryable: bool,
    content_seen: bool,
) -> TransportFailure {
    TransportFailure {
        code,
        message: message.into(),
        retryable,
        status: None,
        provider_error: None,
        content_seen,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        ops::ControlFlow,
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use bytes::Bytes;
    use futures_util::{Stream, StreamExt};
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn transport_headers_timeout_is_stage_specific() {
        let token = CancellationToken::new();
        let config = TransportConfig::test();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            let budget = GenerationBudget::new(tokio::time::Instant::now(), config.total);
            let future = std::future::pending::<Result<reqwest::Response, reqwest::Error>>();
            await_headers(future, &task_token, &budget, config).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(config.headers + Duration::from_millis(1)).await;
        assert_eq!(task.await.unwrap().unwrap_err().code, "HEADERS_TIMEOUT");
    }

    #[tokio::test(start_paused = true)]
    async fn transport_first_content_is_delivered_without_waiting_for_second_chunk() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let stream = tokio_stream_from_receiver(receiver);
        let token = CancellationToken::new();
        let config = TransportConfig::test();
        let delivered = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = delivered.clone();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            let budget = GenerationBudget::new(tokio::time::Instant::now(), config.total);
            consume_sse_chunks(
                stream,
                &task_token,
                &budget,
                tokio::time::Instant::now(),
                config,
                move |delta| {
                    captured.lock().unwrap().push(delta);
                    ControlFlow::Continue(())
                },
                |_| ControlFlow::Continue(()),
                |_, _| {},
            )
            .await
        });

        sender
            .send(Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
            )))
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(&*delivered.lock().unwrap(), &["first"]);
        sender
            .send(Ok(Bytes::from_static(b"data: [DONE]\n\n")))
            .unwrap();
        drop(sender);
        assert_eq!(task.await.unwrap().unwrap().content, "first");
    }

    #[tokio::test]
    async fn transport_done_drains_only_buffered_tail_and_never_polls_pending_network() {
        let stream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\ndata: [DONE]\n\n",
        ))])
        .chain(futures_util::stream::pending());
        let success = tokio::time::timeout(
            Duration::from_millis(100),
            consume_sse_chunks(
                stream,
                &CancellationToken::new(),
                &GenerationBudget::new(tokio::time::Instant::now(), Duration::from_secs(10)),
                tokio::time::Instant::now(),
                TransportConfig::test(),
                |_| ControlFlow::Continue(()),
                |_| ControlFlow::Continue(()),
                |_, _| {},
            ),
        )
        .await
        .expect("[DONE] must not poll the permanently pending tail")
        .unwrap();
        assert_eq!(success.content, "first");
        assert_eq!(success.termination, ResponseTermination::Done);
    }

    #[tokio::test]
    async fn transport_done_rejects_data_already_buffered_after_done() {
        let wire = Bytes::from_static(
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
                "data: [DONE]\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n\n"
            )
            .as_bytes(),
        );
        let failure = consume_sse_chunks(
            futures_util::stream::iter(vec![Ok(wire)]),
            &CancellationToken::new(),
            &GenerationBudget::new(tokio::time::Instant::now(), Duration::from_secs(10)),
            tokio::time::Instant::now(),
            TransportConfig::test(),
            |_| ControlFlow::Continue(()),
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code, "DATA_AFTER_DONE");
    }

    #[tokio::test(start_paused = true)]
    async fn transport_comments_do_not_satisfy_first_event_or_first_content() {
        let stream = futures_util::stream::iter(vec![Ok(Bytes::from_static(b": ping\n\n"))])
            .chain(futures_util::stream::pending());
        let failure = run_sse_until_timeout(stream, TransportConfig::test()).await;
        assert_eq!(failure.code, "FIRST_EVENT_TIMEOUT");
    }

    #[tokio::test(start_paused = true)]
    async fn transport_metadata_satisfies_first_event_but_not_first_content() {
        let stream = futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        ))])
        .chain(futures_util::stream::pending());
        let failure = run_sse_until_timeout(stream, TransportConfig::test()).await;
        assert_eq!(failure.code, "FIRST_CONTENT_TIMEOUT");
    }

    #[tokio::test(start_paused = true)]
    async fn transport_idle_deadline_resets_only_on_valid_protocol_events() {
        let config = TransportConfig::test();
        let stream = scripted_timed_stream(vec![
            (
                Duration::ZERO,
                b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".to_vec(),
            ),
            (
                config.stream_idle / 2,
                b"data: {\"choices\":[],\"usage\":{\"total_tokens\":1}}\n\n".to_vec(),
            ),
            (config.stream_idle / 2, b": tcp-heartbeat\n\n".to_vec()),
        ]);
        let failure = run_sse_until_timeout(stream, config).await;
        assert_eq!(failure.code, "STREAM_IDLE_TIMEOUT");
    }

    #[tokio::test]
    async fn transport_cancellation_interrupts_one_large_chunk() {
        let event = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        let chunk = Bytes::from(event.repeat(10_000));
        let token = CancellationToken::new();
        let cancel = token.clone();
        let delivered = Arc::new(AtomicUsize::new(0));
        let count = delivered.clone();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            let budget =
                GenerationBudget::new(tokio::time::Instant::now(), Duration::from_secs(600));
            consume_sse_chunks(
                futures_util::stream::iter(vec![Ok(chunk)]),
                &task_token,
                &budget,
                tokio::time::Instant::now(),
                TransportConfig::production(),
                move |_| {
                    count.fetch_add(1, Ordering::Relaxed);
                    ControlFlow::Continue(())
                },
                |_| ControlFlow::Continue(()),
                |_, _| {},
            )
            .await
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        let failure = task.await.unwrap().unwrap_err();
        assert_eq!(failure.code, "CANCELLED");
        assert!(delivered.load(Ordering::Relaxed) < 10_000);
    }

    #[tokio::test]
    async fn transport_json_error_envelope_wins_before_content_extraction() {
        let bytes = Bytes::from_static(
            br#"{"error":{"code":"bad_request","message":"failed"},"choices":[{"message":{"content":"must-not-display"}}]}"#,
        );
        let result = consume_json_chunks(
            futures_util::stream::iter(vec![Ok(bytes)]),
            &CancellationToken::new(),
            &GenerationBudget::new(tokio::time::Instant::now(), Duration::from_secs(600)),
            tokio::time::Instant::now(),
            TransportConfig::production(),
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await;
        assert!(result.unwrap_err().provider_error.is_some());
    }

    #[tokio::test]
    async fn transport_empty_done_and_whitespace_answers_never_complete() {
        for wire in [
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"   \"}}]}\n\ndata: [DONE]\n\n",
        ] {
            let failure = consume_sse_chunks(
                futures_util::stream::iter(vec![Ok(Bytes::from(wire.to_owned()))]),
                &CancellationToken::new(),
                &GenerationBudget::new(Instant::now(), Duration::from_secs(10)),
                Instant::now(),
                TransportConfig::test(),
                |_| ControlFlow::Continue(()),
                |_| ControlFlow::Continue(()),
                |_, _| {},
            )
            .await
            .unwrap_err();
            assert_eq!(failure.code, "EMPTY_RESPONSE");
        }
    }

    #[tokio::test]
    async fn transport_truncated_sse_preserves_final_content_before_reporting_failure() {
        for (codec, wire) in [
            (SseCodec::OpenAiChat, "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n\n"),
            (SseCodec::OpenAiResponses, "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\nevent: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}\n\n"),
        ] {
            let mut deltas = Vec::new();
            let failure = consume_sse_chunks_for(codec,
                futures_util::stream::iter(vec![Ok(Bytes::from(wire.to_owned()))]),
                &CancellationToken::new(), &GenerationBudget::new(Instant::now(), Duration::from_secs(10)),
                Instant::now(), TransportConfig::test(), |delta| { deltas.push(delta); ControlFlow::Continue(()) }, |_| ControlFlow::Continue(()), |_, _| {},
            ).await.unwrap_err();
            assert_eq!(failure.code, "INCOMPLETE_RESPONSE");
            assert_eq!(deltas.concat(), "partial");
            assert!(failure.content_seen);
        }
    }

    #[tokio::test]
    async fn transport_json_incomplete_providers_preserve_content_and_successful_stop_completes() {
        for (codec, body) in [
            (
                SseCodec::OpenAiChat,
                serde_json::json!({"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}),
            ),
            (
                SseCodec::OpenAiResponses,
                serde_json::json!({"output_text":"partial","status":"incomplete"}),
            ),
            (
                SseCodec::AnthropicMessages,
                serde_json::json!({"content":[{"type":"text","text":"partial"}],"stop_reason":"max_tokens"}),
            ),
            (
                SseCodec::Gemini,
                serde_json::json!({"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MAX_TOKENS"}]}),
            ),
        ] {
            let mut deltas = Vec::new();
            let failure = consume_json_chunks_for(
                codec,
                futures_util::stream::iter(vec![Ok(Bytes::from(
                    serde_json::to_vec(&body).unwrap(),
                ))]),
                &CancellationToken::new(),
                &GenerationBudget::new(Instant::now(), Duration::from_secs(10)),
                Instant::now(),
                TransportConfig::test(),
                |delta| {
                    deltas.push(delta);
                    ControlFlow::Continue(())
                },
                |_, _| {},
            )
            .await
            .unwrap_err();
            assert_eq!(failure.code, "INCOMPLETE_RESPONSE", "{codec:?}");
            assert!(failure.content_seen);
            assert_eq!(deltas.concat(), "partial");
        }
        let success = consume_json_chunks(
            futures_util::stream::iter(vec![Ok(Bytes::from_static(
                br#"{"choices":[{"message":{"content":"complete"},"finish_reason":"stop"}]}"#,
            ))]),
            &CancellationToken::new(),
            &GenerationBudget::new(Instant::now(), Duration::from_secs(10)),
            Instant::now(),
            TransportConfig::test(),
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(success.content, "complete");
    }

    #[tokio::test(start_paused = true)]
    async fn transport_json_without_body_times_out_at_first_byte_deadline() {
        let start = Instant::now();
        let config = TransportConfig::test();
        let failure = consume_json_chunks(
            futures_util::stream::pending(),
            &CancellationToken::new(),
            &GenerationBudget::new(start, config.total),
            start,
            config,
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code, "FIRST_EVENT_TIMEOUT");
        assert_eq!(Instant::now() - start, config.first_event);
    }

    #[tokio::test(start_paused = true)]
    async fn transport_json_partial_body_times_out_when_reading_stalls() {
        let start = Instant::now();
        let config = TransportConfig::test();
        let stream = futures_util::stream::iter(vec![Ok(Bytes::from_static(br#"{"choices":["#))])
            .chain(futures_util::stream::pending());
        let failure = consume_json_chunks(
            stream,
            &CancellationToken::new(),
            &GenerationBudget::new(start, config.total),
            start,
            config,
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code, "STREAM_IDLE_TIMEOUT");
        assert_eq!(Instant::now() - start, config.stream_idle);
    }

    #[tokio::test(start_paused = true)]
    async fn transport_json_continuous_slow_body_resets_idle_budget() {
        let config = TransportConfig::test();
        let stream = futures_util::stream::unfold(
            VecDeque::from(vec![
                (
                    Duration::ZERO,
                    br#"{"choices":[{"message":{"content":"#.to_vec(),
                ),
                (config.stream_idle / 2, b"\"continuous".to_vec()),
                (config.stream_idle / 2, b" slow".to_vec()),
                (config.stream_idle / 2, b" body\"}}]}".to_vec()),
            ]),
            |mut chunks| async move {
                let (delay, bytes) = chunks.pop_front()?;
                tokio::time::sleep(delay).await;
                Some((Ok(Bytes::from(bytes)), chunks))
            },
        );
        let success = consume_json_chunks(
            stream,
            &CancellationToken::new(),
            &GenerationBudget::new(Instant::now(), config.total),
            Instant::now(),
            config,
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(success.content, "continuous slow body");
    }

    #[tokio::test(start_paused = true)]
    async fn transport_error_body_stops_after_five_hundred_milliseconds() {
        let config = TransportConfig::production();
        let stream = futures_util::stream::pending();
        let task = tokio::spawn(async move {
            let budget = GenerationBudget::new(tokio::time::Instant::now(), config.total);
            read_limited_error_chunks(stream, &CancellationToken::new(), &budget, config).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(501)).await;
        assert_eq!(task.await.unwrap().unwrap_err().code, "ERROR_BODY_TIMEOUT");
    }

    #[tokio::test]
    async fn transport_delta_break_stops_old_stream_immediately() {
        let wire = Bytes::from(
            [
                "data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n",
            ]
            .concat(),
        );
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let seen = delivered.clone();
        let result = consume_sse_chunks(
            futures_util::stream::iter(vec![Ok(wire)]),
            &CancellationToken::new(),
            &GenerationBudget::new(tokio::time::Instant::now(), Duration::from_secs(10)),
            tokio::time::Instant::now(),
            TransportConfig::test(),
            move |delta| {
                seen.lock().unwrap().push(delta);
                ControlFlow::Break(())
            },
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await;
        assert_eq!(&*delivered.lock().unwrap(), &["first"]);
        assert_eq!(result.unwrap_err().code, "STALE_TICKET");
    }

    type TestChunkStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportFailure>> + Send>>;

    fn tokio_stream_from_receiver(
        receiver: tokio::sync::mpsc::UnboundedReceiver<Result<Bytes, TransportFailure>>,
    ) -> TestChunkStream {
        Box::pin(futures_util::stream::unfold(
            receiver,
            |mut receiver| async move { receiver.recv().await.map(|item| (item, receiver)) },
        ))
    }

    fn scripted_timed_stream(chunks: Vec<(Duration, Vec<u8>)>) -> TestChunkStream {
        let scheduled =
            futures_util::stream::unfold(VecDeque::from(chunks), |mut chunks| async move {
                let (delay, bytes) = chunks.pop_front()?;
                tokio::time::sleep(delay).await;
                Some((Ok(Bytes::from(bytes)), chunks))
            });
        Box::pin(scheduled.chain(futures_util::stream::pending()))
    }

    async fn run_sse_until_timeout<S>(stream: S, config: TransportConfig) -> TransportFailure
    where
        S: Stream<Item = Result<Bytes, TransportFailure>>,
    {
        let token = CancellationToken::new();
        let budget = GenerationBudget::new(tokio::time::Instant::now(), config.total);
        consume_sse_chunks(
            stream,
            &token,
            &budget,
            tokio::time::Instant::now(),
            config,
            |_| ControlFlow::Continue(()),
            |_| ControlFlow::Continue(()),
            |_, _| {},
        )
        .await
        .unwrap_err()
    }

    #[tokio::test]
    async fn transport_reasoning_delta_invokes_thinking_callback_without_answer_content() {
        let wire = Bytes::from_static(
            concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"ans\"}}]}\n\n",
                "data: [DONE]\n\n"
            )
            .as_bytes(),
        );
        let thinking = Arc::new(Mutex::new(Vec::new()));
        let answer = Arc::new(Mutex::new(Vec::new()));
        let think_cap = thinking.clone();
        let ans_cap = answer.clone();
        let success = consume_sse_chunks(
            futures_util::stream::iter(vec![Ok(wire)]),
            &CancellationToken::new(),
            &GenerationBudget::new(tokio::time::Instant::now(), Duration::from_secs(10)),
            tokio::time::Instant::now(),
            TransportConfig::test(),
            move |delta| {
                ans_cap.lock().unwrap().push(delta);
                ControlFlow::Continue(())
            },
            move |delta| {
                think_cap.lock().unwrap().push(delta);
                ControlFlow::Continue(())
            },
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(&*thinking.lock().unwrap(), &["think"]);
        assert_eq!(&*answer.lock().unwrap(), &["ans"]);
        assert_eq!(success.content, "ans");
    }
}
