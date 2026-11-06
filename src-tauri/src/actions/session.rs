use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use tokio_util::sync::CancellationToken;

use crate::models::{
    ActionNotice, ActionSnapshot, ActionSnapshotStatus, ActionStreamEvent, ActionStreamPayload,
    EventSequence, HandshakeGeneration, PreparationFailure, RequestGeneration, ResultReadyAck,
    SessionGeneration, MAX_WIRE_COUNTER,
};

// Content deltas are emitted immediately (no coalesce window / byte batching)
// so translate/explain/summary TTFB and tail fluency stay as low-latency as possible.
const MAX_PENDING_EVENTS: usize = 256;
/// Closed sessions only need to absorb late renderer commands for the most
/// recent result windows. Keeping every tombstone forever slowly grows the
/// tray process, so retain a bounded tail.
const MAX_CLOSED_SESSIONS: usize = 64;

pub(super) struct SessionTable {
    entries: HashMap<String, SessionEntry>,
    closed_order: VecDeque<String>,
    last_session_generation: SessionGeneration,
}

pub(super) enum SessionEntry {
    Open(SessionState),
    Closed(SessionTombstone),
}

pub(super) struct SessionTombstone {
    pub session_generation: SessionGeneration,
    pub last_request_generation: RequestGeneration,
}

pub(super) struct SessionState {
    pub session_generation: SessionGeneration,
    pub last_request_generation: RequestGeneration,
    pub request_slot: RequestSlot,
    pub request_stream: Option<RequestStreamState>,
    pub window_label: String,
    pub snapshot: ActionSnapshot,
    pub next_sequence: EventSequence,
    pub pending_events: VecDeque<ActionStreamEvent>,
    pub ready: bool,
    pub next_handshake_generation: HandshakeGeneration,
    pub pending_handshake: Option<ResultReadyAck>,
    pub flusher_running: bool,
    pub delivery_epoch: DeliveryEpoch,
    pub snapshot_absorbed_through: EventSequence,
    /// Ask sessions open as Completed with empty content; allow the first continue.
    pub allow_continue_without_content: bool,
    active_flusher_epoch: Option<DeliveryEpoch>,
    active_flusher_token: Option<Arc<()>>,
    in_flight: Option<InFlightEmit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DeliveryEpoch(u64);

impl DeliveryEpoch {
    const NONE: Self = Self(0);

    fn checked_next(self) -> Option<Self> {
        (self.0 < MAX_WIRE_COUNTER).then(|| Self(self.0 + 1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InFlightEmit {
    sequence: EventSequence,
    delivery_epoch: DeliveryEpoch,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RequestStreamState {
    first_content_sent: bool,
    pending: String,
    pending_scalar_count: u64,
    last_emit_at: Option<tokio::time::Instant>,
    deadline: Option<tokio::time::Instant>,
    deadline_generation: DeadlineGeneration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DeadlineGeneration(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimerDirective {
    None,
    // Reserved for the coalesced-delta protocol. Streaming currently emits
    // every delta immediately (see the module header), so nothing constructs
    // this yet; the flush machinery stays for a future batching mode.
    #[allow(dead_code)]
    Schedule {
        deadline: tokio::time::Instant,
        generation: DeadlineGeneration,
    },
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeltaTransition {
    Ignored,
    Stale,
    // Reserved alongside TimerDirective::Schedule; matched defensively by the
    // submit paths but not produced while deltas are emitted immediately.
    #[allow(dead_code)]
    Buffered {
        deadline: tokio::time::Instant,
        generation: DeadlineGeneration,
        timer: TimerDirective,
    },
    Emitted {
        event: ActionStreamEvent,
        timer: TimerDirective,
    },
}

pub(super) enum RequestSlot {
    Vacant,
    Preparing {
        ticket: RequestTicket,
        kind: ReservationKind,
        cancellation: CancellationToken,
    },
    Running {
        ticket: RequestTicket,
        cancellation: CancellationToken,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RequestTicket {
    pub session_id: String,
    pub session_generation: SessionGeneration,
    pub request_generation: RequestGeneration,
    pub request_id: String,
    pub action_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReservationKind {
    Initial,
    Retry,
    Continue,
}

#[derive(Debug)]
pub(super) struct Reservation {
    pub ticket: RequestTicket,
    pub cancellation: CancellationToken,
    #[cfg_attr(not(test), allow(dead_code))]
    pub kind: ReservationKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InitialReservationInput {
    pub session_id: String,
    pub window_label: String,
    pub request_id: String,
    pub action_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub question: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionError {
    InvalidInput,
    NotFound,
    Ended,
    Busy,
    Ineligible,
    CounterExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionResult {
    Applied,
    Rejected,
}

#[derive(Debug)]
pub(super) struct CancelTransition {
    pub cancellation: Option<CancellationToken>,
    pub timer: TimerDirective,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) struct NoticeTransition {
    pub result: TransitionResult,
    pub events: Vec<ActionStreamEvent>,
    pub timer: TimerDirective,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TerminalKind {
    Completed,
    Cancelled,
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalTransition {
    pub result: TransitionResult,
    pub timer: TimerDirective,
}

#[derive(Debug)]
pub(super) struct CloseTransition {
    pub cancellation: Option<CancellationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BeginReadySnapshot {
    pub snapshot: ActionSnapshot,
    pub ack: ResultReadyAck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AckTransition {
    pub result: TransitionResult,
    pub start_flusher: bool,
}

pub(super) struct FlusherLease {
    pub(super) session_id: String,
    pub(super) session_generation: SessionGeneration,
    pub(super) delivery_epoch: DeliveryEpoch,
    owner_token: Arc<()>,
}

pub(super) struct EmitLease {
    pub(super) event: ActionStreamEvent,
    pub(super) window_label: String,
    pub(super) delivery_epoch: DeliveryEpoch,
    owner_token: Arc<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EmitOutcome {
    Sent,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FlushTransition {
    pub(super) continue_now: bool,
}

impl Default for SessionTable {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            closed_order: VecDeque::new(),
            last_session_generation: SessionGeneration(0),
        }
    }
}

impl SessionTable {
    pub(super) fn authoritative_snapshot(&self, session_id: &str) -> Option<&ActionSnapshot> {
        match self.entries.get(session_id) {
            Some(SessionEntry::Open(state)) => Some(&state.snapshot),
            Some(SessionEntry::Closed(tombstone)) => {
                let _ = (
                    tombstone.session_generation,
                    tombstone.last_request_generation,
                );
                None
            }
            None => None,
        }
    }

    pub(super) fn completed_content_for(&self, ticket: &RequestTicket) -> Option<&str> {
        let SessionEntry::Open(state) = self.entries.get(&ticket.session_id)? else {
            return None;
        };
        (state.snapshot.session_generation == ticket.session_generation
            && state.snapshot.request_generation == ticket.request_generation
            && state.snapshot.status == ActionSnapshotStatus::Completed)
            .then_some(state.snapshot.content.as_str())
    }

    pub(super) fn reserve_initial(
        &mut self,
        input: InitialReservationInput,
    ) -> Result<Reservation, SessionError> {
        if [
            input.session_id.as_str(),
            input.window_label.as_str(),
            input.request_id.as_str(),
            input.action_id.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            return Err(SessionError::InvalidInput);
        }

        match self.entries.get(&input.session_id) {
            Some(SessionEntry::Closed(_)) => return Err(SessionError::Ended),
            Some(SessionEntry::Open(_)) => return Err(SessionError::Busy),
            None => {}
        }

        let session_generation = self
            .last_session_generation
            .checked_next()
            .ok_or(SessionError::CounterExhausted)?;
        let request_generation = RequestGeneration(0)
            .checked_next()
            .ok_or(SessionError::CounterExhausted)?;
        let ticket = RequestTicket {
            session_id: input.session_id.clone(),
            session_generation,
            request_generation,
            request_id: input.request_id,
            action_id: input.action_id,
        };
        let cancellation = CancellationToken::new();
        let snapshot = running_snapshot(&ticket, EventSequence::FIRST);
        let mut state = SessionState {
            session_generation,
            last_request_generation: request_generation,
            request_slot: RequestSlot::Preparing {
                ticket: ticket.clone(),
                kind: ReservationKind::Initial,
                cancellation: cancellation.clone(),
            },
            request_stream: None,
            window_label: input.window_label,
            snapshot,
            next_sequence: EventSequence::FIRST,
            pending_events: VecDeque::new(),
            ready: false,
            next_handshake_generation: HandshakeGeneration(0),
            pending_handshake: None,
            flusher_running: false,
            delivery_epoch: DeliveryEpoch::NONE,
            snapshot_absorbed_through: EventSequence::NONE,
            allow_continue_without_content: false,
            active_flusher_epoch: None,
            active_flusher_token: None,
            in_flight: None,
        };
        enqueue_payload_at(
            &mut state,
            &ticket,
            EventSequence::FIRST,
            ActionStreamPayload::Started {
                provider_id: None,
                model_id: None,
                question: None,
                transcript: None,
            },
        );

        self.last_session_generation = session_generation;
        self.entries
            .insert(ticket.session_id.clone(), SessionEntry::Open(state));
        Ok(Reservation {
            ticket,
            cancellation,
            kind: ReservationKind::Initial,
        })
    }

    /// Open a session already Completed with empty content (Ask: wait for first question).
    /// No network reservation, no Started event — renderer hydrates via begin_ready.
    pub(super) fn open_completed_without_generation(
        &mut self,
        input: InitialReservationInput,
    ) -> Result<RequestTicket, SessionError> {
        if [
            input.session_id.as_str(),
            input.window_label.as_str(),
            input.request_id.as_str(),
            input.action_id.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            return Err(SessionError::InvalidInput);
        }

        match self.entries.get(&input.session_id) {
            Some(SessionEntry::Closed(_)) => return Err(SessionError::Ended),
            Some(SessionEntry::Open(_)) => return Err(SessionError::Busy),
            None => {}
        }

        let session_generation = self
            .last_session_generation
            .checked_next()
            .ok_or(SessionError::CounterExhausted)?;
        let request_generation = RequestGeneration(0)
            .checked_next()
            .ok_or(SessionError::CounterExhausted)?;
        let ticket = RequestTicket {
            session_id: input.session_id.clone(),
            session_generation,
            request_generation,
            request_id: input.request_id,
            action_id: input.action_id,
        };
        let state = SessionState {
            session_generation,
            last_request_generation: request_generation,
            request_slot: RequestSlot::Vacant,
            request_stream: None,
            window_label: input.window_label,
            snapshot: ActionSnapshot {
                session_id: ticket.session_id.clone(),
                session_generation,
                request_id: ticket.request_id.clone(),
                request_generation,
                action_id: ticket.action_id.clone(),
                status: ActionSnapshotStatus::Completed,
                content: String::new(),
                thinking_content: String::new(),
                last_sequence: EventSequence::NONE,
                last_content_sequence: EventSequence::NONE,
                content_scalar_count: 0,
                generation_notice: None,
                error_code: None,
                error_message: None,
                retryable: false,
                preparation_failure: None,
                transcript: Vec::new(),
            },
            next_sequence: EventSequence::NONE,
            pending_events: VecDeque::new(),
            ready: false,
            next_handshake_generation: HandshakeGeneration(0),
            pending_handshake: None,
            flusher_running: false,
            delivery_epoch: DeliveryEpoch::NONE,
            snapshot_absorbed_through: EventSequence::NONE,
            allow_continue_without_content: true,
            active_flusher_epoch: None,
            active_flusher_token: None,
            in_flight: None,
        };

        self.last_session_generation = session_generation;
        self.entries
            .insert(ticket.session_id.clone(), SessionEntry::Open(state));
        Ok(ticket)
    }

    pub(super) fn reserve_retry(
        &mut self,
        session_id: &str,
        request_id: String,
    ) -> Result<Reservation, SessionError> {
        self.reserve_replacement(session_id, request_id, ReservationKind::Retry)
    }

    pub(super) fn reserve_continue(
        &mut self,
        session_id: &str,
        request_id: String,
    ) -> Result<Reservation, SessionError> {
        self.reserve_replacement(session_id, request_id, ReservationKind::Continue)
    }

    fn reserve_replacement(
        &mut self,
        session_id: &str,
        request_id: String,
        kind: ReservationKind,
    ) -> Result<Reservation, SessionError> {
        if session_id.trim().is_empty() || request_id.trim().is_empty() {
            return Err(SessionError::InvalidInput);
        }
        let state = match self.entries.get_mut(session_id) {
            Some(SessionEntry::Open(state)) => state,
            Some(SessionEntry::Closed(_)) => return Err(SessionError::Ended),
            None => return Err(SessionError::NotFound),
        };
        if !matches!(state.request_slot, RequestSlot::Vacant) {
            return Err(SessionError::Busy);
        }
        let eligible = match kind {
            ReservationKind::Retry => matches!(
                state.snapshot.status,
                ActionSnapshotStatus::Completed
                    | ActionSnapshotStatus::Cancelled
                    | ActionSnapshotStatus::Error
            ),
            ReservationKind::Continue => {
                state.snapshot.status == ActionSnapshotStatus::Completed
                    && (!state.snapshot.content.is_empty() || state.allow_continue_without_content)
            }
            ReservationKind::Initial => false,
        };
        if !eligible {
            return Err(SessionError::Ineligible);
        }
        let request_generation = state
            .last_request_generation
            .checked_next()
            .ok_or(SessionError::CounterExhausted)?;
        let ticket = RequestTicket {
            session_id: session_id.to_owned(),
            session_generation: state.session_generation,
            request_generation,
            request_id,
            action_id: state.snapshot.action_id.clone(),
        };
        let cancellation = CancellationToken::new();
        state.last_request_generation = request_generation;
        state.request_stream = None;
        state.request_slot = RequestSlot::Preparing {
            ticket: ticket.clone(),
            kind,
            cancellation: cancellation.clone(),
        };
        Ok(Reservation {
            ticket,
            cancellation,
            kind,
        })
    }

    #[cfg(test)]
    pub(super) fn commit_prepare_success(&mut self, ticket: &RequestTicket) -> TransitionResult {
        self.commit_prepare_success_with_metadata(ticket, None, None, None, None)
    }

    pub(super) fn commit_prepare_success_with_metadata(
        &mut self,
        ticket: &RequestTicket,
        provider_id: Option<String>,
        model_id: Option<String>,
        question: Option<String>,
        transcript: Option<Vec<crate::models::TranscriptTurn>>,
    ) -> TransitionResult {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ticket.session_id) else {
            return TransitionResult::Rejected;
        };
        let kind = match &state.request_slot {
            RequestSlot::Preparing {
                ticket: active,
                kind,
                ..
            } if active == ticket => *kind,
            _ => return TransitionResult::Rejected,
        };
        let replacement_sequence = if kind == ReservationKind::Initial {
            None
        } else {
            let Some(sequence) = state.next_sequence.checked_next() else {
                return TransitionResult::Rejected;
            };
            Some(sequence)
        };
        let pending_limit = if let Some(sequence) = replacement_sequence {
            match preflight_pending_limit(state, 1, sequence) {
                Ok(plan) => plan,
                Err(_) => return TransitionResult::Rejected,
            }
        } else {
            PendingLimitPlan::None
        };
        let RequestSlot::Preparing { cancellation, .. } =
            std::mem::replace(&mut state.request_slot, RequestSlot::Vacant)
        else {
            unreachable!("matching slot was checked above");
        };

        if let Some(sequence) = replacement_sequence {
            enqueue_payload_at(
                state,
                ticket,
                sequence,
                ActionStreamPayload::Started {
                    provider_id,
                    model_id,
                    question,
                    transcript,
                },
            );
            apply_pending_limit_plan(state, pending_limit);
        }
        state.request_slot = RequestSlot::Running {
            ticket: ticket.clone(),
            cancellation,
        };
        state.request_stream = Some(RequestStreamState::default());
        TransitionResult::Applied
    }

    pub(super) fn commit_prepare_failure(
        &mut self,
        ticket: &RequestTicket,
        failure: SessionFailure,
    ) -> TransitionResult {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ticket.session_id) else {
            return TransitionResult::Rejected;
        };
        let kind = match &state.request_slot {
            RequestSlot::Preparing {
                ticket: active,
                kind,
                ..
            } if active == ticket => *kind,
            _ => return TransitionResult::Rejected,
        };
        let Some(error_sequence) = state.next_sequence.checked_next() else {
            return TransitionResult::Rejected;
        };
        let pending_limit = match preflight_pending_limit(state, 1, error_sequence) {
            Ok(plan) => plan,
            Err(_) => return TransitionResult::Rejected,
        };
        state.request_slot = RequestSlot::Vacant;
        state.request_stream = None;

        let payload = if kind == ReservationKind::Initial {
            ActionStreamPayload::Error {
                code: failure.code,
                message: failure.message,
                retryable: failure.retryable,
            }
        } else {
            ActionStreamPayload::PreparationFailed {
                code: failure.code,
                message: failure.message,
                retryable: failure.retryable,
                question: failure.question,
            }
        };
        enqueue_payload_at(state, ticket, error_sequence, payload);
        apply_pending_limit_plan(state, pending_limit);
        TransitionResult::Applied
    }

    pub(super) fn accept_delta(
        &mut self,
        ticket: &RequestTicket,
        delta: String,
        now: tokio::time::Instant,
    ) -> Result<DeltaTransition, SessionError> {
        if delta.is_empty() {
            return Ok(DeltaTransition::Ignored);
        }
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ticket.session_id) else {
            return Ok(DeltaTransition::Stale);
        };
        if !matches!(
            &state.request_slot,
            RequestSlot::Running { ticket: active, .. } if active == ticket
        ) {
            return Ok(DeltaTransition::Stale);
        }
        if state.request_stream.is_none() {
            return Ok(DeltaTransition::Stale);
        }

        // Always emit content deltas immediately (first token and tail).
        // If an older code path left pending text, prepend it so nothing is lost.
        let (sequence, _) = preflight_event_sequences(state.next_sequence, 1)?;
        let pending_limit = preflight_pending_limit(state, 1, sequence)?;
        let stream = state
            .request_stream
            .as_mut()
            .expect("matching Running request must own a stream");
        stream.first_content_sent = true;
        stream.last_emit_at = Some(now);
        let mut full = take_pending(stream);
        full.push_str(&delta);
        let event = enqueue_payload_at(
            state,
            ticket,
            sequence,
            ActionStreamPayload::Delta { delta: full },
        );
        apply_pending_limit_plan(state, pending_limit);
        Ok(DeltaTransition::Emitted {
            event,
            timer: TimerDirective::None,
        })
    }

    /// Emit reasoning/CoT text immediately (same delivery path as answer deltas).
    pub(super) fn accept_thinking_delta(
        &mut self,
        ticket: &RequestTicket,
        delta: String,
        now: tokio::time::Instant,
    ) -> Result<DeltaTransition, SessionError> {
        if delta.is_empty() {
            return Ok(DeltaTransition::Ignored);
        }
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ticket.session_id) else {
            return Ok(DeltaTransition::Stale);
        };
        if !matches!(
            &state.request_slot,
            RequestSlot::Running { ticket: active, .. } if active == ticket
        ) {
            return Ok(DeltaTransition::Stale);
        }
        if state.request_stream.is_none() {
            return Ok(DeltaTransition::Stale);
        }

        let (sequence, _) = preflight_event_sequences(state.next_sequence, 1)?;
        let pending_limit = preflight_pending_limit(state, 1, sequence)?;
        let stream = state
            .request_stream
            .as_mut()
            .expect("matching Running request must own a stream");
        stream.last_emit_at = Some(now);
        let event = enqueue_payload_at(
            state,
            ticket,
            sequence,
            ActionStreamPayload::ThinkingDelta { delta },
        );
        apply_pending_limit_plan(state, pending_limit);
        Ok(DeltaTransition::Emitted {
            event,
            timer: TimerDirective::None,
        })
    }

    pub(super) fn flush_due_delta(
        &mut self,
        ticket: &RequestTicket,
        generation: DeadlineGeneration,
        now: tokio::time::Instant,
    ) -> Result<Option<ActionStreamEvent>, SessionError> {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ticket.session_id) else {
            return Ok(None);
        };
        if !matches!(
            &state.request_slot,
            RequestSlot::Running { ticket: active, .. } if active == ticket
        ) {
            return Ok(None);
        }
        let Some(stream) = state.request_stream.as_ref() else {
            return Ok(None);
        };
        let Some(deadline) = stream.deadline else {
            return Ok(None);
        };
        if stream.deadline_generation != generation || now < deadline || stream.pending.is_empty() {
            return Ok(None);
        }

        let (sequence, _) = preflight_event_sequences(state.next_sequence, 1)?;
        let pending_limit = preflight_pending_limit(state, 1, sequence)?;
        let delta = {
            let stream = state
                .request_stream
                .as_mut()
                .expect("matching Running request must own a stream");
            let delta = take_pending(stream);
            stream.last_emit_at = Some(now);
            delta
        };
        let event = enqueue_payload_at(
            state,
            ticket,
            sequence,
            ActionStreamPayload::Delta { delta },
        );
        apply_pending_limit_plan(state, pending_limit);
        Ok(Some(event))
    }

    pub(super) fn enqueue_notice(
        &mut self,
        ticket: &RequestTicket,
        code: &str,
        message: &str,
    ) -> Result<NoticeTransition, SessionError> {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ticket.session_id) else {
            return Ok(rejected_notice_transition());
        };
        if !matches!(
            &state.request_slot,
            RequestSlot::Running { ticket: active, .. } if active == ticket
        ) || state.request_stream.is_none()
        {
            return Ok(rejected_notice_transition());
        }
        let has_pending = state
            .request_stream
            .as_ref()
            .is_some_and(|stream| !stream.pending.is_empty());
        let needed = if has_pending { 2 } else { 1 };
        let (first, second) = preflight_event_sequences(state.next_sequence, needed)?;
        let final_sequence = second.unwrap_or(first);
        let pending_limit = preflight_pending_limit(state, needed, final_sequence)?;

        let mut events = Vec::with_capacity(needed);
        if has_pending {
            events.push(enqueue_pending_delta_at(state, ticket, first));
        }
        let notice_sequence = final_sequence;
        events.push(enqueue_payload_at(
            state,
            ticket,
            notice_sequence,
            ActionStreamPayload::Notice {
                code: code.to_owned(),
                message: message.to_owned(),
            },
        ));
        apply_pending_limit_plan(state, pending_limit);
        Ok(NoticeTransition {
            result: TransitionResult::Applied,
            events,
            timer: if has_pending {
                TimerDirective::Cancel
            } else {
                TimerDirective::None
            },
        })
    }

    pub(super) fn commit_terminal(
        &mut self,
        ticket: &RequestTicket,
        terminal: TerminalKind,
    ) -> Result<TerminalTransition, SessionError> {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ticket.session_id) else {
            return Ok(rejected_terminal_transition());
        };
        if !matches!(
            &state.request_slot,
            RequestSlot::Running { ticket: active, .. } if active == ticket
        ) || state.request_stream.is_none()
        {
            return Ok(rejected_terminal_transition());
        }
        let has_pending = state
            .request_stream
            .as_ref()
            .is_some_and(|stream| !stream.pending.is_empty());
        let needed = if has_pending { 2 } else { 1 };
        let sequences = preflight_event_sequences(state.next_sequence, needed)?;
        let final_sequence = sequences.1.unwrap_or(sequences.0);
        let pending_limit = preflight_pending_limit(state, needed, final_sequence)?;

        enqueue_terminal_with_sequences(state, ticket, terminal, sequences, has_pending);
        apply_pending_limit_plan(state, pending_limit);
        state.request_slot = RequestSlot::Vacant;
        Ok(TerminalTransition {
            result: TransitionResult::Applied,
            timer: TimerDirective::Cancel,
        })
    }

    pub(super) fn cancel(&mut self, session_id: &str) -> Result<CancelTransition, SessionError> {
        let state = match self.entries.get_mut(session_id) {
            Some(SessionEntry::Open(state)) => state,
            Some(SessionEntry::Closed(_)) => return Err(SessionError::Ended),
            None => return Err(SessionError::NotFound),
        };
        enum CancelKind {
            PreserveReplacement(RequestTicket),
            CommitPreparing(RequestTicket),
            CommitRunning {
                ticket: RequestTicket,
                has_pending: bool,
            },
        }
        let cancel_kind = match &state.request_slot {
            RequestSlot::Vacant => return Err(SessionError::Ineligible),
            RequestSlot::Preparing { ticket, kind, .. } if *kind == ReservationKind::Initial => {
                CancelKind::CommitPreparing(ticket.clone())
            }
            RequestSlot::Preparing { ticket, .. } => {
                CancelKind::PreserveReplacement(ticket.clone())
            }
            RequestSlot::Running { ticket, .. } => CancelKind::CommitRunning {
                ticket: ticket.clone(),
                has_pending: state
                    .request_stream
                    .as_ref()
                    .is_some_and(|stream| !stream.pending.is_empty()),
            },
        };
        let sequences = match &cancel_kind {
            CancelKind::PreserveReplacement(_) => {
                Some(preflight_event_sequences(state.next_sequence, 1)?)
            }
            CancelKind::CommitPreparing(_) => {
                Some(preflight_event_sequences(state.next_sequence, 1)?)
            }
            CancelKind::CommitRunning { has_pending, .. } => Some(preflight_event_sequences(
                state.next_sequence,
                if *has_pending { 2 } else { 1 },
            )?),
        };
        let pending_limit = match sequences {
            Some((first, second)) => {
                let needed = usize::from(second.is_some()) + 1;
                preflight_pending_limit(state, needed, second.unwrap_or(first))?
            }
            None => PendingLimitPlan::None,
        };

        let timer = match (&cancel_kind, sequences) {
            (CancelKind::PreserveReplacement(ticket), Some((sequence, _))) => {
                enqueue_payload_at(
                    state,
                    ticket,
                    sequence,
                    ActionStreamPayload::PreparationFailed {
                        code: "PREPARATION_CANCELLED".to_owned(),
                        message: "请求准备已取消，已保留上一轮内容".to_owned(),
                        retryable: true,
                        question: None,
                    },
                );
                apply_pending_limit_plan(state, pending_limit);
                state.request_stream = None;
                TimerDirective::None
            }
            (CancelKind::CommitPreparing(ticket), Some(sequences)) => {
                enqueue_terminal_with_sequences(
                    state,
                    ticket,
                    TerminalKind::Cancelled,
                    sequences,
                    false,
                );
                apply_pending_limit_plan(state, pending_limit);
                TimerDirective::None
            }
            (
                CancelKind::CommitRunning {
                    ticket,
                    has_pending,
                },
                Some(sequences),
            ) => {
                enqueue_terminal_with_sequences(
                    state,
                    ticket,
                    TerminalKind::Cancelled,
                    sequences,
                    *has_pending,
                );
                apply_pending_limit_plan(state, pending_limit);
                TimerDirective::Cancel
            }
            _ => unreachable!("cancel sequence reservation must match its request kind"),
        };
        let cancellation = match std::mem::replace(&mut state.request_slot, RequestSlot::Vacant) {
            RequestSlot::Preparing { cancellation, .. }
            | RequestSlot::Running { cancellation, .. } => cancellation,
            RequestSlot::Vacant => unreachable!("vacant slot returned above"),
        };
        Ok(CancelTransition {
            cancellation: Some(cancellation),
            timer,
        })
    }

    pub(super) fn begin_ready(
        &mut self,
        session_id: &str,
        window_label: &str,
    ) -> Result<BeginReadySnapshot, SessionError> {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(session_id) else {
            return Err(SessionError::Ended);
        };
        let handshake_generation = state
            .next_handshake_generation
            .checked_next()
            .ok_or(SessionError::CounterExhausted)?;
        let delivery_epoch = state
            .delivery_epoch
            .checked_next()
            .ok_or(SessionError::CounterExhausted)?;
        let snapshot = state.snapshot.clone();
        let ack = ResultReadyAck {
            session_id: session_id.to_owned(),
            session_generation: snapshot.session_generation,
            request_generation: snapshot.request_generation,
            last_sequence: snapshot.last_sequence,
            handshake_generation,
        };

        state.ready = false;
        state.next_handshake_generation = handshake_generation;
        state.pending_handshake = Some(ack.clone());
        state.delivery_epoch = delivery_epoch;
        state.snapshot_absorbed_through = snapshot.last_sequence;
        state.window_label = window_label.to_owned();
        let preserved_front = state.in_flight.map(|in_flight| in_flight.sequence);
        state.pending_events.retain(|event| {
            Some(event.sequence) == preserved_front || event.sequence > snapshot.last_sequence
        });
        debug_assert_flusher_identity(state);

        Ok(BeginReadySnapshot { snapshot, ack })
    }

    pub(super) fn ack_ready(&mut self, ack: ResultReadyAck) -> AckTransition {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&ack.session_id) else {
            return rejected_ack_transition();
        };
        if state.pending_handshake.as_ref() != Some(&ack) {
            return rejected_ack_transition();
        }

        state.pending_handshake = None;
        state.ready = true;
        let start_flusher = !state.flusher_running && !state.pending_events.is_empty();
        debug_assert_flusher_identity(state);
        AckTransition {
            result: TransitionResult::Applied,
            start_flusher,
        }
    }

    pub(super) fn acquire_flusher(&mut self, session_id: &str) -> Option<FlusherLease> {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(session_id) else {
            return None;
        };
        debug_assert_flusher_identity(state);
        if !state.ready || state.flusher_running || state.pending_events.is_empty() {
            return None;
        }

        let owner_token = Arc::new(());
        state.flusher_running = true;
        state.active_flusher_epoch = Some(state.delivery_epoch);
        state.active_flusher_token = Some(Arc::clone(&owner_token));
        Some(FlusherLease {
            session_id: session_id.to_owned(),
            session_generation: state.session_generation,
            delivery_epoch: state.delivery_epoch,
            owner_token,
        })
    }

    pub(super) fn next_emit(&mut self, lease: &FlusherLease) -> Option<EmitLease> {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&lease.session_id) else {
            return None;
        };
        if state.session_generation != lease.session_generation
            || !active_flusher_matches(state, lease)
        {
            return None;
        }
        if state.in_flight.is_some() {
            return None;
        }
        if !state.ready || state.pending_events.is_empty() {
            release_flusher(state);
            return None;
        }

        let event = state
            .pending_events
            .front()
            .cloned()
            .expect("non-empty queue was checked above");
        state.in_flight = Some(InFlightEmit {
            sequence: event.sequence,
            delivery_epoch: state.delivery_epoch,
        });
        Some(EmitLease {
            event,
            window_label: state.window_label.clone(),
            delivery_epoch: state.delivery_epoch,
            owner_token: Arc::clone(&lease.owner_token),
        })
    }

    pub(super) fn finish_emit(
        &mut self,
        lease: &FlusherLease,
        event: &EmitLease,
        outcome: EmitOutcome,
    ) -> FlushTransition {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(&lease.session_id) else {
            return FlushTransition {
                continue_now: false,
            };
        };
        if state.session_generation != lease.session_generation
            || !active_flusher_matches(state, lease)
            || !Arc::ptr_eq(&event.owner_token, &lease.owner_token)
        {
            return FlushTransition {
                continue_now: false,
            };
        }

        let matching_emit = state.in_flight
            == Some(InFlightEmit {
                sequence: event.event.sequence,
                delivery_epoch: event.delivery_epoch,
            });
        let matching_front = matching_emit
            && state
                .pending_events
                .front()
                .is_some_and(|front| front.sequence == event.event.sequence);
        if !matching_front {
            return FlushTransition {
                continue_now: state.ready && !state.pending_events.is_empty(),
            };
        }
        state.in_flight = None;
        let epoch_changed = event.delivery_epoch != state.delivery_epoch;
        if matching_front
            && epoch_changed
            && event.event.sequence <= state.snapshot_absorbed_through
        {
            state.pending_events.pop_front();
        } else if matching_front {
            match outcome {
                EmitOutcome::Sent => {
                    state.pending_events.pop_front();
                }
                EmitOutcome::Failed if !epoch_changed => {
                    state.ready = false;
                }
                EmitOutcome::Failed => {}
            }
        }

        if !state.ready || state.pending_events.is_empty() {
            release_flusher(state);
            FlushTransition {
                continue_now: false,
            }
        } else {
            debug_assert_flusher_identity(state);
            FlushTransition { continue_now: true }
        }
    }

    #[cfg(test)]
    fn enforce_pending_limit(&mut self, session_id: &str) -> Result<(), SessionError> {
        let Some(SessionEntry::Open(state)) = self.entries.get_mut(session_id) else {
            return Ok(());
        };
        let plan = preflight_pending_limit(state, 0, state.next_sequence)?;
        apply_pending_limit_plan(state, plan);
        Ok(())
    }

    pub(super) fn close(&mut self, session_id: &str) -> CloseTransition {
        let Some(entry) = self.entries.get_mut(session_id) else {
            return CloseTransition { cancellation: None };
        };
        let (session_generation, last_request_generation) = match entry {
            SessionEntry::Open(state) => (state.session_generation, state.last_request_generation),
            SessionEntry::Closed(_) => return CloseTransition { cancellation: None },
        };
        let old_entry = std::mem::replace(
            entry,
            SessionEntry::Closed(SessionTombstone {
                session_generation,
                last_request_generation,
            }),
        );
        let SessionEntry::Open(state) = old_entry else {
            unreachable!("open entry was checked above");
        };
        let cancellation = match state.request_slot {
            RequestSlot::Vacant => None,
            RequestSlot::Preparing { cancellation, .. }
            | RequestSlot::Running { cancellation, .. } => Some(cancellation),
        };
        self.closed_order.push_back(session_id.to_owned());
        while self.closed_order.len() > MAX_CLOSED_SESSIONS {
            let Some(oldest) = self.closed_order.pop_front() else {
                break;
            };
            if matches!(self.entries.get(&oldest), Some(SessionEntry::Closed(_))) {
                self.entries.remove(&oldest);
            }
        }
        CloseTransition { cancellation }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingLimitPlan {
    None,
    NotReady,
    Ready {
        resync_sequence: EventSequence,
        delivery_epoch: DeliveryEpoch,
    },
}

fn preflight_pending_limit(
    state: &SessionState,
    added_events: usize,
    sequence_after_added: EventSequence,
) -> Result<PendingLimitPlan, SessionError> {
    if state.pending_events.len().saturating_add(added_events) <= MAX_PENDING_EVENTS {
        return Ok(PendingLimitPlan::None);
    }
    if !state.ready {
        return Ok(PendingLimitPlan::NotReady);
    }

    let resync_sequence = sequence_after_added
        .checked_next()
        .ok_or(SessionError::CounterExhausted)?;
    let delivery_epoch = state
        .delivery_epoch
        .checked_next()
        .ok_or(SessionError::CounterExhausted)?;
    Ok(PendingLimitPlan::Ready {
        resync_sequence,
        delivery_epoch,
    })
}

fn apply_pending_limit_plan(state: &mut SessionState, plan: PendingLimitPlan) {
    match plan {
        PendingLimitPlan::None => {}
        PendingLimitPlan::NotReady => {
            let preserved_front = take_in_flight_front(state);
            state.pending_events.clear();
            state.pending_events.extend(preserved_front);
            state.pending_handshake = None;
            state.snapshot_absorbed_through = state.snapshot.last_sequence;
        }
        PendingLimitPlan::Ready {
            resync_sequence,
            delivery_epoch,
        } => {
            let snapshot_last_sequence = state.snapshot.last_sequence;
            let preserved_front = take_in_flight_front(state);
            state.pending_events.clear();
            state.pending_events.extend(preserved_front);
            let ticket = RequestTicket {
                session_id: state.snapshot.session_id.clone(),
                session_generation: state.snapshot.session_generation,
                request_id: state.snapshot.request_id.clone(),
                request_generation: state.snapshot.request_generation,
                action_id: state.snapshot.action_id.clone(),
            };
            enqueue_payload_at(
                state,
                &ticket,
                resync_sequence,
                ActionStreamPayload::ResyncRequired {
                    snapshot_last_sequence,
                },
            );
            state.delivery_epoch = delivery_epoch;
            state.snapshot_absorbed_through = snapshot_last_sequence;
        }
    }
    debug_assert!(state.pending_events.len() <= MAX_PENDING_EVENTS);
    debug_assert_flusher_identity(state);
}

fn active_flusher_matches(state: &SessionState, lease: &FlusherLease) -> bool {
    state.flusher_running
        && state.active_flusher_epoch == Some(lease.delivery_epoch)
        && state
            .active_flusher_token
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &lease.owner_token))
}

fn release_flusher(state: &mut SessionState) {
    state.flusher_running = false;
    state.active_flusher_epoch = None;
    state.active_flusher_token = None;
    state.in_flight = None;
    debug_assert_flusher_identity(state);
}

fn take_in_flight_front(state: &mut SessionState) -> Option<ActionStreamEvent> {
    let in_flight = state.in_flight?;
    state
        .pending_events
        .front()
        .is_some_and(|front| front.sequence == in_flight.sequence)
        .then(|| state.pending_events.pop_front())
        .flatten()
}

fn debug_assert_flusher_identity(state: &SessionState) {
    debug_assert_eq!(
        state.flusher_running,
        state.active_flusher_epoch.is_some() && state.active_flusher_token.is_some()
    );
    debug_assert!(state.flusher_running || state.in_flight.is_none());
}

fn rejected_ack_transition() -> AckTransition {
    AckTransition {
        result: TransitionResult::Rejected,
        start_flusher: false,
    }
}

fn preflight_event_sequences(
    current: EventSequence,
    needed: usize,
) -> Result<(EventSequence, Option<EventSequence>), SessionError> {
    debug_assert!((1..=2).contains(&needed));
    let first = current
        .checked_next()
        .ok_or(SessionError::CounterExhausted)?;
    let second = if needed == 2 {
        Some(first.checked_next().ok_or(SessionError::CounterExhausted)?)
    } else {
        None
    };
    Ok((first, second))
}

fn enqueue_payload_at(
    state: &mut SessionState,
    ticket: &RequestTicket,
    sequence: EventSequence,
    payload: ActionStreamPayload,
) -> ActionStreamEvent {
    let event = stream_event(ticket, sequence, payload);
    apply_event_to_snapshot(&mut state.snapshot, &event);
    state.pending_events.push_back(event.clone());
    state.next_sequence = sequence;
    event
}

fn apply_event_to_snapshot(snapshot: &mut ActionSnapshot, event: &ActionStreamEvent) {
    match &event.payload {
        ActionStreamPayload::Started { transcript, .. } => {
            snapshot.session_id.clone_from(&event.session_id);
            snapshot.session_generation = event.session_generation;
            snapshot.request_id.clone_from(&event.request_id);
            snapshot.request_generation = event.request_generation;
            snapshot.action_id.clone_from(&event.action_id);
            snapshot.status = ActionSnapshotStatus::Running;
            snapshot.content.clear();
            snapshot.thinking_content.clear();
            snapshot.last_sequence = event.sequence;
            snapshot.last_content_sequence = EventSequence::NONE;
            snapshot.content_scalar_count = 0;
            snapshot.generation_notice = None;
            snapshot.error_code = None;
            snapshot.error_message = None;
            snapshot.retryable = false;
            snapshot.preparation_failure = None;
            snapshot.transcript = transcript.clone().unwrap_or_default();
        }
        ActionStreamPayload::PreparationFailed {
            code,
            message,
            retryable,
            question,
        } => {
            snapshot.last_sequence = event.sequence;
            snapshot.preparation_failure = Some(PreparationFailure {
                request_id: event.request_id.clone(),
                request_generation: event.request_generation,
                code: code.clone(),
                message: message.clone(),
                retryable: *retryable,
                question: question.clone(),
            });
        }
        ActionStreamPayload::Delta { delta } => {
            snapshot.content.push_str(delta);
            snapshot.last_sequence = event.sequence;
            snapshot.last_content_sequence = event.sequence;
            snapshot.content_scalar_count += delta.chars().count() as u64;
        }
        ActionStreamPayload::ThinkingDelta { delta } => {
            snapshot.thinking_content.push_str(delta);
            snapshot.last_sequence = event.sequence;
        }
        ActionStreamPayload::Notice { code, message } => {
            snapshot.last_sequence = event.sequence;
            snapshot.generation_notice = Some(ActionNotice {
                code: code.clone(),
                message: message.clone(),
            });
        }
        ActionStreamPayload::ResyncRequired { .. } => {
            snapshot.last_sequence = event.sequence;
        }
        ActionStreamPayload::Completed {
            last_content_sequence,
            content_scalar_count,
        } => {
            debug_assert_eq!(*last_content_sequence, snapshot.last_content_sequence);
            debug_assert_eq!(*content_scalar_count, snapshot.content_scalar_count);
            snapshot.status = ActionSnapshotStatus::Completed;
            snapshot.last_sequence = event.sequence;
            snapshot.error_code = None;
            snapshot.error_message = None;
            snapshot.retryable = false;
        }
        ActionStreamPayload::Cancelled => {
            snapshot.status = ActionSnapshotStatus::Cancelled;
            snapshot.last_sequence = event.sequence;
            snapshot.error_code = None;
            snapshot.error_message = None;
            snapshot.retryable = false;
        }
        ActionStreamPayload::Error {
            code,
            message,
            retryable,
        } => {
            snapshot.status = ActionSnapshotStatus::Error;
            snapshot.last_sequence = event.sequence;
            snapshot.error_code = Some(code.clone());
            snapshot.error_message = Some(message.clone());
            snapshot.retryable = *retryable;
        }
    }
}

fn take_pending(stream: &mut RequestStreamState) -> String {
    stream.pending_scalar_count = 0;
    stream.deadline = None;
    std::mem::take(&mut stream.pending)
}

fn enqueue_pending_delta_at(
    state: &mut SessionState,
    ticket: &RequestTicket,
    sequence: EventSequence,
) -> ActionStreamEvent {
    let delta = take_pending(
        state
            .request_stream
            .as_mut()
            .expect("matching Running request must own a stream"),
    );
    debug_assert!(!delta.is_empty());
    enqueue_payload_at(
        state,
        ticket,
        sequence,
        ActionStreamPayload::Delta { delta },
    )
}

fn enqueue_terminal_with_sequences(
    state: &mut SessionState,
    ticket: &RequestTicket,
    terminal: TerminalKind,
    sequences: (EventSequence, Option<EventSequence>),
    has_pending: bool,
) {
    let (first, second) = sequences;
    if has_pending {
        enqueue_pending_delta_at(state, ticket, first);
    } else if let Some(stream) = state.request_stream.as_mut() {
        stream.deadline = None;
    }
    let terminal_sequence = second.unwrap_or(first);
    let payload = match terminal {
        TerminalKind::Completed => ActionStreamPayload::Completed {
            last_content_sequence: state.snapshot.last_content_sequence,
            content_scalar_count: state.snapshot.content_scalar_count,
        },
        TerminalKind::Cancelled => ActionStreamPayload::Cancelled,
        TerminalKind::Error {
            code,
            message,
            retryable,
        } => ActionStreamPayload::Error {
            code,
            message,
            retryable,
        },
    };
    enqueue_payload_at(state, ticket, terminal_sequence, payload);
    state.request_stream = None;
}

#[allow(dead_code)]
fn rejected_notice_transition() -> NoticeTransition {
    NoticeTransition {
        result: TransitionResult::Rejected,
        events: Vec::new(),
        timer: TimerDirective::None,
    }
}

fn rejected_terminal_transition() -> TerminalTransition {
    TerminalTransition {
        result: TransitionResult::Rejected,
        timer: TimerDirective::None,
    }
}

fn running_snapshot(ticket: &RequestTicket, sequence: EventSequence) -> ActionSnapshot {
    ActionSnapshot {
        session_id: ticket.session_id.clone(),
        session_generation: ticket.session_generation,
        request_id: ticket.request_id.clone(),
        request_generation: ticket.request_generation,
        action_id: ticket.action_id.clone(),
        status: ActionSnapshotStatus::Running,
        content: String::new(),
        thinking_content: String::new(),
        last_sequence: sequence,
        last_content_sequence: EventSequence::NONE,
        content_scalar_count: 0,
        generation_notice: None,
        error_code: None,
        error_message: None,
        retryable: false,
        preparation_failure: None,
        transcript: Vec::new(),
    }
}

fn stream_event(
    ticket: &RequestTicket,
    sequence: EventSequence,
    payload: ActionStreamPayload,
) -> ActionStreamEvent {
    ActionStreamEvent {
        session_id: ticket.session_id.clone(),
        session_generation: ticket.session_generation,
        request_id: ticket.request_id.clone(),
        request_generation: ticket.request_generation,
        sequence,
        action_id: ticket.action_id.clone(),
        payload,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier, Mutex},
        time::Duration,
    };

    use super::*;
    use crate::models::{
        ActionNotice, ActionSnapshotStatus, ActionStreamPayload, EventSequence,
        HandshakeGeneration, RequestGeneration, ResultReadyAck, SessionGeneration,
        MAX_WIRE_COUNTER,
    };

    fn initial_input(session_id: &str) -> InitialReservationInput {
        InitialReservationInput {
            session_id: session_id.to_owned(),
            window_label: "main".to_owned(),
            request_id: "request-1".to_owned(),
            action_id: "translate".to_owned(),
        }
    }

    fn terminal_table(
        session_id: &str,
        status: ActionSnapshotStatus,
        content: &str,
    ) -> SessionTable {
        let mut table = SessionTable::default();
        let reservation = table.reserve_initial(initial_input(session_id)).unwrap();
        assert_eq!(
            table.commit_prepare_success(&reservation.ticket),
            TransitionResult::Applied
        );
        let SessionEntry::Open(state) = table.entries.get_mut(session_id).unwrap() else {
            panic!("fixture session must be open");
        };
        state.request_slot = RequestSlot::Vacant;
        state.request_stream = None;
        state.snapshot.status = status;
        state.snapshot.content = content.to_owned();
        state.snapshot.content_scalar_count = content.chars().count() as u64;
        state.snapshot.error_code =
            (status == ActionSnapshotStatus::Error).then(|| "FIXTURE".to_owned());
        state.snapshot.error_message =
            (status == ActionSnapshotStatus::Error).then(|| "fixture error".to_owned());
        state.snapshot.retryable = status == ActionSnapshotStatus::Error;
        table
    }

    fn open_state<'a>(table: &'a SessionTable, session_id: &str) -> &'a SessionState {
        match table.entries.get(session_id) {
            Some(SessionEntry::Open(state)) => state,
            _ => panic!("expected open session"),
        }
    }

    fn open_state_mut<'a>(table: &'a mut SessionTable, session_id: &str) -> &'a mut SessionState {
        match table.entries.get_mut(session_id) {
            Some(SessionEntry::Open(state)) => state,
            _ => panic!("expected open session"),
        }
    }

    fn running_table(session_id: &str) -> (SessionTable, RequestTicket) {
        let mut table = SessionTable::default();
        let reservation = table.reserve_initial(initial_input(session_id)).unwrap();
        assert_eq!(
            table.commit_prepare_success(&reservation.ticket),
            TransitionResult::Applied
        );
        (table, reservation.ticket)
    }

    fn running_ready_table(session_id: &str) -> (SessionTable, RequestTicket) {
        let (mut table, ticket) = running_table(session_id);
        let ready = table.begin_ready(session_id, "result/window").unwrap();
        assert_eq!(table.ack_ready(ready.ack).result, TransitionResult::Applied);
        (table, ticket)
    }

    fn pause_flusher(table: &mut SessionTable, session_id: &str) {
        let state = open_state_mut(table, session_id);
        let owner_token = Arc::new(());
        state.flusher_running = true;
        state.active_flusher_epoch = Some(state.delivery_epoch);
        state.active_flusher_token = Some(owner_token);
        state.in_flight = None;
    }

    fn drain_with_single_lease(table: &mut SessionTable, session_id: &str) -> Vec<EventSequence> {
        let lease = if open_state(table, session_id).flusher_running {
            let state = open_state(table, session_id);
            FlusherLease {
                session_id: session_id.to_owned(),
                session_generation: state.session_generation,
                delivery_epoch: state
                    .active_flusher_epoch
                    .expect("running flusher must have acquire epoch"),
                owner_token: Arc::clone(
                    state
                        .active_flusher_token
                        .as_ref()
                        .expect("running flusher must have owner token"),
                ),
            }
        } else {
            table
                .acquire_flusher(session_id)
                .expect("queued ready session must grant one lease")
        };
        let mut sequences = Vec::new();
        while let Some(event) = table.next_emit(&lease) {
            sequences.push(event.event.sequence);
            table.finish_emit(&lease, &event, EmitOutcome::Sent);
        }
        sequences
    }

    #[derive(Debug, Clone, Copy)]
    enum ReadyCounter {
        Handshake,
        DeliveryEpoch,
    }

    #[derive(Debug, Clone, Copy)]
    enum TwoEventOverflowOperation {
        Notice,
        Terminal,
        Cancel,
    }

    fn set_ready_counter_to_max(
        table: &mut SessionTable,
        session_id: &str,
        exhausted: ReadyCounter,
    ) {
        let state = open_state_mut(table, session_id);
        match exhausted {
            ReadyCounter::Handshake => {
                state.next_handshake_generation = HandshakeGeneration(MAX_WIRE_COUNTER)
            }
            ReadyCounter::DeliveryEpoch => state.delivery_epoch = DeliveryEpoch(MAX_WIRE_COUNTER),
        }
    }

    fn fill_queue_past_limit_for_test(table: &mut SessionTable, ticket: &RequestTicket) {
        for index in 0..=MAX_PENDING_EVENTS {
            let state = open_state_mut(table, &ticket.session_id);
            let sequence = state.next_sequence.checked_next().unwrap();
            enqueue_payload_at(
                state,
                ticket,
                sequence,
                ActionStreamPayload::Delta {
                    delta: index.to_string(),
                },
            );
        }
    }

    fn fill_queue_to_limit_for_test(table: &mut SessionTable, ticket: &RequestTicket, len: usize) {
        for index in 0..len {
            let state = open_state_mut(table, &ticket.session_id);
            let sequence = state.next_sequence.checked_next().unwrap();
            enqueue_payload_at(
                state,
                ticket,
                sequence,
                ActionStreamPayload::Delta {
                    delta: index.to_string(),
                },
            );
        }
    }

    fn install_pending_delta(table: &mut SessionTable, session_id: &str, delta: &str) {
        let stream = open_state_mut(table, session_id)
            .request_stream
            .as_mut()
            .expect("running fixture must have request stream");
        stream.first_content_sent = true;
        stream.pending = delta.to_owned();
        stream.pending_scalar_count = delta.chars().count() as u64;
        stream.deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
        stream.deadline_generation = DeadlineGeneration(1);
    }

    impl SessionTable {
        fn enqueue_test_delta(
            &mut self,
            ticket: &RequestTicket,
            delta: String,
        ) -> Result<(), SessionError> {
            let state = open_state_mut(self, &ticket.session_id);
            let sequence = state
                .next_sequence
                .checked_next()
                .ok_or(SessionError::CounterExhausted)?;
            enqueue_payload_at(
                state,
                ticket,
                sequence,
                ActionStreamPayload::Delta { delta },
            );
            self.enforce_pending_limit(&ticket.session_id)
        }
    }

    fn completed_table(session_id: &str, content: &str) -> SessionTable {
        terminal_table(session_id, ActionSnapshotStatus::Completed, content)
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CounterOperation {
        Terminal,
        Cancel,
        Notice,
    }

    #[derive(Debug, Clone)]
    enum ComparableRequestSlot {
        Vacant,
        Preparing {
            ticket: RequestTicket,
            kind: ReservationKind,
            cancellation: CancellationToken,
        },
        Running {
            ticket: RequestTicket,
            cancellation: CancellationToken,
        },
    }

    #[derive(Debug, Clone)]
    struct ComparableOpenState {
        session_generation: SessionGeneration,
        last_request_generation: RequestGeneration,
        request_slot: ComparableRequestSlot,
        request_stream: Option<RequestStreamState>,
        window_label: String,
        snapshot: ActionSnapshot,
        next_sequence: EventSequence,
        pending_events: VecDeque<ActionStreamEvent>,
        ready: bool,
        next_handshake_generation: HandshakeGeneration,
        pending_handshake: Option<ResultReadyAck>,
        flusher_running: bool,
        delivery_epoch: DeliveryEpoch,
        snapshot_absorbed_through: EventSequence,
        active_flusher_epoch: Option<DeliveryEpoch>,
        active_flusher_token: Option<Arc<()>>,
        in_flight: Option<InFlightEmit>,
    }

    fn comparable_open_state(table: &SessionTable, session_id: &str) -> ComparableOpenState {
        let state = open_state(table, session_id);
        let request_slot = match &state.request_slot {
            RequestSlot::Vacant => ComparableRequestSlot::Vacant,
            RequestSlot::Preparing {
                ticket,
                kind,
                cancellation,
            } => ComparableRequestSlot::Preparing {
                ticket: ticket.clone(),
                kind: *kind,
                cancellation: cancellation.clone(),
            },
            RequestSlot::Running {
                ticket,
                cancellation,
            } => ComparableRequestSlot::Running {
                ticket: ticket.clone(),
                cancellation: cancellation.clone(),
            },
        };
        ComparableOpenState {
            session_generation: state.session_generation,
            last_request_generation: state.last_request_generation,
            request_slot,
            request_stream: state.request_stream.clone(),
            window_label: state.window_label.clone(),
            snapshot: state.snapshot.clone(),
            next_sequence: state.next_sequence,
            pending_events: state.pending_events.clone(),
            ready: state.ready,
            next_handshake_generation: state.next_handshake_generation,
            pending_handshake: state.pending_handshake.clone(),
            flusher_running: state.flusher_running,
            delivery_epoch: state.delivery_epoch,
            snapshot_absorbed_through: state.snapshot_absorbed_through,
            active_flusher_epoch: state.active_flusher_epoch,
            active_flusher_token: state.active_flusher_token.clone(),
            in_flight: state.in_flight,
        }
    }

    fn assert_open_state_exact(
        table: &SessionTable,
        session_id: &str,
        before: &ComparableOpenState,
    ) {
        let after = comparable_open_state(table, session_id);
        assert_eq!(after.session_generation, before.session_generation);
        assert_eq!(
            after.last_request_generation,
            before.last_request_generation
        );
        assert_eq!(after.request_stream, before.request_stream);
        assert_eq!(after.window_label, before.window_label);
        assert_eq!(after.snapshot, before.snapshot);
        assert_eq!(after.next_sequence, before.next_sequence);
        assert_eq!(after.pending_events, before.pending_events);
        assert_eq!(after.ready, before.ready);
        assert_eq!(
            after.next_handshake_generation,
            before.next_handshake_generation
        );
        assert_eq!(after.pending_handshake, before.pending_handshake);
        assert_eq!(after.flusher_running, before.flusher_running);
        assert_eq!(after.delivery_epoch, before.delivery_epoch);
        assert_eq!(
            after.snapshot_absorbed_through,
            before.snapshot_absorbed_through
        );
        assert_eq!(after.active_flusher_epoch, before.active_flusher_epoch);
        match (
            before.active_flusher_token.as_ref(),
            after.active_flusher_token.as_ref(),
        ) {
            (None, None) => {}
            (Some(before_token), Some(after_token)) => {
                assert!(Arc::ptr_eq(after_token, before_token));
            }
            _ => panic!("active flusher token presence changed"),
        }
        assert_eq!(after.in_flight, before.in_flight);

        let (before_cancellation, after_cancellation) =
            match (&before.request_slot, &after.request_slot) {
                (ComparableRequestSlot::Vacant, ComparableRequestSlot::Vacant) => (None, None),
                (
                    ComparableRequestSlot::Preparing {
                        ticket: before_ticket,
                        kind: before_kind,
                        cancellation: before_cancellation,
                    },
                    ComparableRequestSlot::Preparing {
                        ticket: after_ticket,
                        kind: after_kind,
                        cancellation: after_cancellation,
                    },
                ) => {
                    assert_eq!(after_ticket, before_ticket);
                    assert_eq!(after_kind, before_kind);
                    (
                        Some(before_cancellation.clone()),
                        Some(after_cancellation.clone()),
                    )
                }
                (
                    ComparableRequestSlot::Running {
                        ticket: before_ticket,
                        cancellation: before_cancellation,
                    },
                    ComparableRequestSlot::Running {
                        ticket: after_ticket,
                        cancellation: after_cancellation,
                    },
                ) => {
                    assert_eq!(after_ticket, before_ticket);
                    (
                        Some(before_cancellation.clone()),
                        Some(after_cancellation.clone()),
                    )
                }
                (before_slot, after_slot) => {
                    panic!("request slot changed: before {before_slot:?}, after {after_slot:?}")
                }
            };
        if let (Some(before_cancellation), Some(after_cancellation)) =
            (before_cancellation, after_cancellation)
        {
            assert!(!before_cancellation.is_cancelled());
            assert!(!after_cancellation.is_cancelled());
            before_cancellation.cancel();
            assert!(after_cancellation.is_cancelled());
        }
    }

    fn set_sequence_watermark(table: &mut SessionTable, session_id: &str, sequence: EventSequence) {
        open_state_mut(table, session_id).next_sequence = sequence;
    }

    fn set_deadline_generation(
        table: &mut SessionTable,
        session_id: &str,
        generation: DeadlineGeneration,
    ) {
        open_state_mut(table, session_id)
            .request_stream
            .as_mut()
            .expect("running fixture must have request stream")
            .deadline_generation = generation;
    }

    #[test]
    fn begin_ready_never_creates_and_only_latest_handshake_can_ack() {
        let mut table = SessionTable::default();
        assert_eq!(
            table.begin_ready("missing", "result/missing"),
            Err(SessionError::Ended)
        );
        assert!(!table.entries.contains_key("missing"));
        let (mut table, ticket) = running_table("s");
        table
            .accept_delta(&ticket, "A".into(), tokio::time::Instant::now())
            .unwrap();
        let first = table.begin_ready("s", "result/s").unwrap();
        let second = table.begin_ready("s", "result/s").unwrap();
        assert!(second.ack.handshake_generation > first.ack.handshake_generation);
        assert_eq!(
            table.ack_ready(first.ack).result,
            TransitionResult::Rejected
        );
        let accepted = table.ack_ready(second.ack);
        assert_eq!(accepted.result, TransitionResult::Applied);
        assert!(!accepted.start_flusher);
    }

    #[test]
    fn events_after_snapshot_watermark_wait_until_ack() {
        let (mut table, ticket) = running_table("s");
        table
            .accept_delta(&ticket, "A".into(), tokio::time::Instant::now())
            .unwrap();
        let ready = table.begin_ready("s", "result/s").unwrap();
        assert_eq!(ready.snapshot.content, "A");
        let watermark = ready.snapshot.last_sequence;
        table
            .accept_delta(
                &ticket,
                "B".into(),
                tokio::time::Instant::now() + Duration::from_millis(9),
            )
            .unwrap();
        assert!(table.acquire_flusher("s").is_none());
        assert!(open_state(&table, "s")
            .pending_events
            .iter()
            .all(|event| event.sequence > watermark));
        let acknowledged = table.ack_ready(ready.ack);
        assert_eq!(acknowledged.result, TransitionResult::Applied);
        assert!(acknowledged.start_flusher);
    }

    #[test]
    fn concurrent_ready_uses_one_flusher_and_preserves_event_order() {
        let (mut table, ticket) = running_table("s");
        let first = table.begin_ready("s", "result/s").unwrap();
        assert_eq!(table.ack_ready(first.ack).result, TransitionResult::Applied);
        table
            .accept_delta(&ticket, "A".into(), tokio::time::Instant::now())
            .unwrap();
        let lease = table.acquire_flusher("s").expect("one owner");
        assert!(table.acquire_flusher("s").is_none());
        let in_flight = table.next_emit(&lease).unwrap();
        let second = table.begin_ready("s", "result/s").unwrap();
        table
            .accept_delta(
                &ticket,
                "B".into(),
                tokio::time::Instant::now() + Duration::from_millis(9),
            )
            .unwrap();
        assert_eq!(
            table.ack_ready(second.ack).result,
            TransitionResult::Applied
        );
        table.finish_emit(&lease, &in_flight, EmitOutcome::Sent);
        let sequences = drain_with_single_lease(&mut table, "s");
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!open_state(&table, "s").flusher_running);
    }

    #[test]
    fn begin_between_emits_drops_absorbed_unsent_front_and_owner_stops_cleanly() {
        let (mut table, ticket) = running_ready_table("s");
        let started_at = tokio::time::Instant::now();
        table.accept_delta(&ticket, "A".into(), started_at).unwrap();
        table
            .accept_delta(&ticket, "B".into(), started_at + Duration::from_millis(9))
            .unwrap();
        let lease = table.acquire_flusher("s").unwrap();
        let first = table.next_emit(&lease).unwrap();
        table.finish_emit(&lease, &first, EmitOutcome::Sent);

        let ready = table.begin_ready("s", "result/s").unwrap();
        assert!(open_state(&table, "s").pending_events.is_empty());
        assert_eq!(table.ack_ready(ready.ack).result, TransitionResult::Applied);
        assert!(table.next_emit(&lease).is_none());
        assert!(!open_state(&table, "s").flusher_running);
    }

    #[test]
    fn emit_failure_keeps_front_event_and_requires_new_ready() {
        let (mut table, ticket) = running_ready_table("s");
        table
            .accept_delta(&ticket, "A".into(), tokio::time::Instant::now())
            .unwrap();
        let lease = table.acquire_flusher("s").unwrap();
        let event = table.next_emit(&lease).unwrap();
        let transition = table.finish_emit(&lease, &event, EmitOutcome::Failed);
        assert!(!transition.continue_now);
        let state = open_state(&table, "s");
        assert!(!state.ready);
        assert_eq!(
            state.pending_events.front().unwrap().sequence,
            event.event.sequence
        );
    }

    #[test]
    fn released_lease_cannot_operate_reacquired_owner_in_same_epoch() {
        let (mut table, ticket) = running_ready_table("s");
        table
            .accept_delta(&ticket, "A".into(), tokio::time::Instant::now())
            .unwrap();
        let old_lease = table.acquire_flusher("s").unwrap();
        let first = table.next_emit(&old_lease).unwrap();
        table.finish_emit(&old_lease, &first, EmitOutcome::Sent);
        assert!(!open_state(&table, "s").flusher_running);

        table
            .accept_delta(
                &ticket,
                "B".into(),
                tokio::time::Instant::now() + Duration::from_millis(9),
            )
            .unwrap();
        let new_lease = table.acquire_flusher("s").unwrap();
        assert!(table.next_emit(&old_lease).is_none());
        assert!(table.next_emit(&new_lease).is_some());
    }

    #[test]
    fn stale_or_out_of_order_finish_never_removes_the_current_front() {
        let (mut table, ticket) = running_ready_table("s");
        let started_at = tokio::time::Instant::now();
        table.accept_delta(&ticket, "A".into(), started_at).unwrap();
        table
            .accept_delta(&ticket, "B".into(), started_at + Duration::from_millis(9))
            .unwrap();
        let lease = table.acquire_flusher("s").unwrap();
        let first = table.next_emit(&lease).unwrap();
        let mut wrong_event = first.event.clone();
        wrong_event.sequence = open_state(&table, "s").pending_events[1].sequence;
        let wrong = EmitLease {
            event: wrong_event,
            window_label: first.window_label.clone(),
            delivery_epoch: first.delivery_epoch,
            owner_token: Arc::clone(&first.owner_token),
        };
        let before = open_state(&table, "s")
            .pending_events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>();
        table.finish_emit(&lease, &wrong, EmitOutcome::Sent);
        assert_eq!(
            open_state(&table, "s")
                .pending_events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            before
        );

        table.finish_emit(&lease, &first, EmitOutcome::Sent);
        let remaining = open_state(&table, "s")
            .pending_events
            .front()
            .unwrap()
            .sequence;
        table.finish_emit(&lease, &first, EmitOutcome::Sent);
        assert_eq!(
            open_state(&table, "s")
                .pending_events
                .front()
                .unwrap()
                .sequence,
            remaining
        );
    }

    #[test]
    fn absorbed_old_epoch_failure_does_not_revoke_new_ready() {
        let (mut table, ticket) = running_ready_table("s");
        table
            .accept_delta(&ticket, "A".into(), tokio::time::Instant::now())
            .unwrap();
        let lease = table.acquire_flusher("s").unwrap();
        let event = table.next_emit(&lease).unwrap();
        let ready = table.begin_ready("s", "result/s").unwrap();
        assert_eq!(table.ack_ready(ready.ack).result, TransitionResult::Applied);
        table.finish_emit(&lease, &event, EmitOutcome::Failed);
        assert!(open_state(&table, "s").ready);
        assert!(open_state(&table, "s").pending_events.is_empty());
    }

    #[test]
    fn queue_overflow_produces_resync_instead_of_silent_delta_loss() {
        let (mut table, ticket) = running_ready_table("s");
        pause_flusher(&mut table, "s");
        for index in 0..257 {
            table
                .enqueue_test_delta(&ticket, index.to_string())
                .unwrap();
        }
        let queue = &open_state(&table, "s").pending_events;
        assert!(queue.len() <= MAX_PENDING_EVENTS);
        assert!(queue
            .iter()
            .any(|event| matches!(event.payload, ActionStreamPayload::ResyncRequired { .. })));
    }

    #[test]
    fn not_ready_overflow_invalidates_pending_handshake_without_resync() {
        let (mut table, ticket) = running_table("s");
        let ready = table.begin_ready("s", "result/s").unwrap();
        for index in 0..=MAX_PENDING_EVENTS {
            table
                .enqueue_test_delta(&ticket, index.to_string())
                .unwrap();
        }
        let state = open_state(&table, "s");
        assert!(state.pending_events.len() <= MAX_PENDING_EVENTS);
        assert!(state
            .pending_events
            .iter()
            .all(|event| !matches!(event.payload, ActionStreamPayload::ResyncRequired { .. })));
        assert!(state.pending_handshake.is_none());
        assert_eq!(
            table.ack_ready(ready.ack).result,
            TransitionResult::Rejected
        );
    }

    #[test]
    fn production_delta_enqueue_enforces_pending_limit() {
        let (mut table, ticket) = running_ready_table("s");
        pause_flusher(&mut table, "s");
        let started_at = tokio::time::Instant::now();
        let mut expected = String::new();
        let mut last_domain_sequence = EventSequence::NONE;
        for index in 0..257 {
            let delta = index.to_string();
            expected.push_str(&delta);
            last_domain_sequence = match table
                .accept_delta(
                    &ticket,
                    delta,
                    started_at + Duration::from_millis(index * 9),
                )
                .unwrap()
            {
                DeltaTransition::Emitted { event, .. } => event.sequence,
                other => panic!("spaced production delta must emit, got {other:?}"),
            };
        }
        let state = open_state(&table, "s");
        assert_eq!(state.snapshot.content, expected);
        assert!(state.pending_events.len() <= MAX_PENDING_EVENTS);
        let resync = state
            .pending_events
            .iter()
            .find(|event| matches!(event.payload, ActionStreamPayload::ResyncRequired { .. }))
            .expect("ready production overflow must enqueue ResyncRequired");
        let ActionStreamPayload::ResyncRequired {
            snapshot_last_sequence,
        } = &resync.payload
        else {
            unreachable!("matching ResyncRequired was selected above")
        };
        assert_eq!(*snapshot_last_sequence, last_domain_sequence);
        assert_eq!(
            resync.sequence,
            snapshot_last_sequence.checked_next().unwrap()
        );
        assert_eq!(state.snapshot.last_sequence, resync.sequence);
    }

    #[test]
    fn production_two_event_batches_preflight_and_resync_at_255_pending() {
        for operation in [
            TwoEventOverflowOperation::Notice,
            TwoEventOverflowOperation::Terminal,
            TwoEventOverflowOperation::Cancel,
        ] {
            let session_id = format!("two-event-{operation:?}");
            let (mut table, ticket) = running_ready_table(&session_id);
            pause_flusher(&mut table, &session_id);
            fill_queue_to_limit_for_test(&mut table, &ticket, MAX_PENDING_EVENTS - 1);
            install_pending_delta(&mut table, &session_id, "tail");

            match operation {
                TwoEventOverflowOperation::Notice => {
                    table.enqueue_notice(&ticket, "N", "notice").unwrap();
                }
                TwoEventOverflowOperation::Terminal => {
                    table
                        .commit_terminal(&ticket, TerminalKind::Completed)
                        .unwrap();
                }
                TwoEventOverflowOperation::Cancel => {
                    table.cancel(&session_id).unwrap();
                }
            }

            let state = open_state(&table, &session_id);
            assert!(state.pending_events.len() <= MAX_PENDING_EVENTS);
            assert!(matches!(
                state.pending_events.back().unwrap().payload,
                ActionStreamPayload::ResyncRequired { .. }
            ));
            assert!(state.snapshot.content.ends_with("tail"));
        }
    }

    #[test]
    fn production_overflow_resync_sequence_exhaustion_is_failure_atomic() {
        let (mut table, ticket) = running_ready_table("overflow-production-max");
        pause_flusher(&mut table, "overflow-production-max");
        fill_queue_to_limit_for_test(&mut table, &ticket, MAX_PENDING_EVENTS);
        set_sequence_watermark(
            &mut table,
            "overflow-production-max",
            EventSequence(MAX_WIRE_COUNTER - 1),
        );
        let before = comparable_open_state(&table, "overflow-production-max");
        assert_eq!(
            table.accept_delta(&ticket, "last".into(), tokio::time::Instant::now()),
            Err(SessionError::CounterExhausted)
        );
        assert_open_state_exact(&table, "overflow-production-max", &before);
    }

    #[test]
    fn task4_handshake_and_overflow_counter_exhaustion_are_failure_atomic() {
        for exhausted in [ReadyCounter::Handshake, ReadyCounter::DeliveryEpoch] {
            let (mut table, _) = running_table(&format!("ready-{exhausted:?}"));
            set_ready_counter_to_max(&mut table, &format!("ready-{exhausted:?}"), exhausted);
            let before = comparable_open_state(&table, &format!("ready-{exhausted:?}"));
            assert_eq!(
                table.begin_ready(&format!("ready-{exhausted:?}"), "result/window"),
                Err(SessionError::CounterExhausted),
            );
            assert_open_state_exact(&table, &format!("ready-{exhausted:?}"), &before);
        }

        let (mut table, ticket) = running_ready_table("overflow-max");
        pause_flusher(&mut table, "overflow-max");
        fill_queue_past_limit_for_test(&mut table, &ticket);
        set_sequence_watermark(&mut table, "overflow-max", EventSequence(MAX_WIRE_COUNTER));
        let before = comparable_open_state(&table, "overflow-max");
        assert_eq!(
            table.enforce_pending_limit("overflow-max"),
            Err(SessionError::CounterExhausted)
        );
        assert_open_state_exact(&table, "overflow-max", &before);
    }

    #[test]
    fn accept_delta_emits_every_content_chunk_immediately_after_first() {
        let (mut table, ticket) = running_table("s");
        let t0 = tokio::time::Instant::now();
        match table.accept_delta(&ticket, "首".into(), t0).unwrap() {
            DeltaTransition::Emitted { .. } => {}
            other => panic!("first must emit, got {other:?}"),
        }
        match table
            .accept_delta(&ticket, "字".into(), t0 + Duration::from_millis(1))
            .unwrap()
        {
            DeltaTransition::Emitted { event, timer } => {
                assert!(matches!(
                    event.payload,
                    ActionStreamPayload::Delta { ref delta } if delta == "字"
                ));
                assert_eq!(timer, TimerDirective::None);
            }
            other => panic!("second must emit immediately, got {other:?}"),
        }
    }

    #[test]
    fn first_content_and_tail_are_both_immediate() {
        let (mut table, ticket) = running_table("s");
        let t0 = tokio::time::Instant::now();
        assert_eq!(
            table.accept_delta(&ticket, "".into(), t0).unwrap(),
            DeltaTransition::Ignored
        );
        let first = match table.accept_delta(&ticket, "首".into(), t0).unwrap() {
            DeltaTransition::Emitted {
                event,
                timer: TimerDirective::None,
            } => event,
            other => panic!("expected immediate first Delta, got {other:?}"),
        };
        assert!(matches!(&first.payload,
            ActionStreamPayload::Delta { delta } if delta == "首"));
        assert_eq!(first.sequence, EventSequence(2));
        let second = match table
            .accept_delta(&ticket, "a".into(), t0 + Duration::from_millis(1))
            .unwrap()
        {
            DeltaTransition::Emitted {
                event,
                timer: TimerDirective::None,
            } => event,
            other => panic!("expected immediate tail Delta, got {other:?}"),
        };
        assert!(matches!(&second.payload,
            ActionStreamPayload::Delta { delta } if delta == "a"));
        let large = "x".repeat(4096);
        let event = match table
            .accept_delta(&ticket, large.clone(), t0 + Duration::from_millis(2))
            .unwrap()
        {
            DeltaTransition::Emitted {
                event,
                timer: TimerDirective::None,
            } => event,
            other => panic!("expected immediate large Delta, got {other:?}"),
        };
        assert!(matches!(&event.payload,
            ActionStreamPayload::Delta { delta } if delta.as_str() == large.as_str()));
    }

    #[test]
    fn dense_single_character_deltas_emit_each_chunk_without_loss() {
        // Stay under MAX_PENDING_EVENTS so overflow resync does not hide emit behavior.
        const FRAGMENT_COUNT: usize = 200;

        let (mut table, ticket) = running_table("dense");
        let now = tokio::time::Instant::now();
        let mut accept_emissions = 0;

        for _ in 0..FRAGMENT_COUNT {
            match table.accept_delta(&ticket, "x".to_owned(), now).unwrap() {
                DeltaTransition::Emitted { event, timer } => {
                    assert_eq!(event.sequence, EventSequence(accept_emissions as u64 + 2));
                    assert_eq!(timer, TimerDirective::None);
                    accept_emissions += 1;
                }
                other => panic!("dense non-empty matching delta must emit, got {other:?}"),
            }
        }
        assert_eq!(accept_emissions, FRAGMENT_COUNT);

        assert_eq!(
            table
                .commit_terminal(&ticket, TerminalKind::Completed)
                .unwrap()
                .result,
            TransitionResult::Applied
        );
        let state = open_state(&table, "dense");
        let delta_events = state
            .pending_events
            .iter()
            .filter_map(|event| match &event.payload {
                ActionStreamPayload::Delta { delta } => Some((event.sequence, delta)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(delta_events.len(), FRAGMENT_COUNT);
        assert!(delta_events.iter().all(|(_, delta)| delta.len() == 1));
        assert_eq!(
            delta_events
                .iter()
                .map(|(sequence, _)| *sequence)
                .collect::<Vec<_>>(),
            (2..=delta_events.len() as u64 + 1)
                .map(EventSequence)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            delta_events
                .iter()
                .map(|(_, delta)| delta.as_str())
                .collect::<String>(),
            "x".repeat(FRAGMENT_COUNT)
        );
        assert_eq!(state.snapshot.content, "x".repeat(FRAGMENT_COUNT));
        assert_eq!(state.snapshot.content_scalar_count, FRAGMENT_COUNT as u64);
        assert_eq!(
            state.snapshot.last_content_sequence,
            EventSequence(delta_events.len() as u64 + 1)
        );
        assert_eq!(
            state
                .pending_events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=state.pending_events.len() as u64)
                .map(EventSequence)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            state.pending_events.back().unwrap().payload,
            ActionStreamPayload::Completed {
                last_content_sequence,
                content_scalar_count,
            } if last_content_sequence == state.snapshot.last_content_sequence
                && content_scalar_count == FRAGMENT_COUNT as u64
        ));
    }

    #[test]
    fn sequence_is_session_local_and_monotonic_across_requests() {
        let (mut table, first) = running_table("s");
        table
            .accept_delta(&first, "one".into(), tokio::time::Instant::now())
            .unwrap();
        assert_eq!(
            table
                .commit_terminal(&first, TerminalKind::Completed)
                .unwrap()
                .result,
            TransitionResult::Applied,
        );
        let second = table.reserve_retry("s", "retry-2".to_owned()).unwrap();
        assert_eq!(
            table.commit_prepare_success(&second.ticket),
            TransitionResult::Applied,
        );
        let started = open_state(&table, "s").pending_events.back().unwrap();
        assert_eq!(started.request_generation, second.ticket.request_generation);
        assert_eq!(started.sequence, EventSequence(4));
    }

    #[test]
    fn matching_prepare_success_installs_a_fresh_request_stream() {
        let mut table = SessionTable::default();
        let initial = table.reserve_initial(initial_input("s")).unwrap();
        assert_eq!(open_state(&table, "s").request_stream, None);
        assert_eq!(
            table.commit_prepare_success(&initial.ticket),
            TransitionResult::Applied
        );
        assert_eq!(
            open_state(&table, "s").request_stream,
            Some(RequestStreamState::default())
        );
        open_state_mut(&mut table, "s")
            .request_stream
            .as_mut()
            .unwrap()
            .first_content_sent = true;
        table
            .commit_terminal(&initial.ticket, TerminalKind::Completed)
            .unwrap();
        assert_eq!(open_state(&table, "s").request_stream, None);
        let retry = table.reserve_retry("s", "retry".to_owned()).unwrap();
        assert_eq!(open_state(&table, "s").request_stream, None);
        assert_eq!(
            table.commit_prepare_success(&retry.ticket),
            TransitionResult::Applied
        );
        assert_eq!(
            open_state(&table, "s").request_stream,
            Some(RequestStreamState::default())
        );
    }

    #[test]
    fn terminal_after_immediate_deltas_completes_with_full_content() {
        let (mut table, ticket) = running_table("s");
        let now = tokio::time::Instant::now();
        table.accept_delta(&ticket, "A".into(), now).unwrap();
        assert!(matches!(
            table
                .accept_delta(&ticket, "B".into(), now + Duration::from_millis(1))
                .unwrap(),
            DeltaTransition::Emitted {
                timer: TimerDirective::None,
                ..
            }
        ));
        assert_eq!(
            table
                .commit_terminal(&ticket, TerminalKind::Completed)
                .unwrap()
                .result,
            TransitionResult::Applied,
        );
        let state = open_state(&table, "s");
        assert_eq!(state.snapshot.content, "AB");
        assert_eq!(state.snapshot.content_scalar_count, 2);
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        let events = state.pending_events.iter().collect::<Vec<_>>();
        assert!(matches!(
            events[events.len() - 2].payload,
            ActionStreamPayload::Delta { .. }
        ));
        assert!(matches!(
            events.last().unwrap().payload,
            ActionStreamPayload::Completed {
                last_content_sequence: EventSequence(_),
                content_scalar_count: 2
            }
        ));
    }

    #[test]
    fn first_terminal_wins_and_late_delta_is_rejected() {
        let (mut table, ticket) = running_table("s");
        assert_eq!(
            table
                .commit_terminal(&ticket, TerminalKind::Cancelled)
                .unwrap()
                .result,
            TransitionResult::Applied,
        );
        assert_eq!(
            table
                .commit_terminal(&ticket, TerminalKind::Completed)
                .unwrap()
                .result,
            TransitionResult::Rejected,
        );
        assert_eq!(
            table
                .accept_delta(&ticket, "late".into(), tokio::time::Instant::now())
                .unwrap(),
            DeltaTransition::Stale
        );
        assert_eq!(
            open_state(&table, "s").snapshot.status,
            ActionSnapshotStatus::Cancelled
        );
    }

    #[test]
    fn ordered_notice_advances_only_last_sequence_and_started_clears_it() {
        let (mut table, ticket) = running_table("s");
        let before = open_state(&table, "s").snapshot.clone();
        let transition = table
            .enqueue_notice(
                &ticket,
                "THINKING_CONTROL_DOWNGRADED",
                "Provider default is in use",
            )
            .unwrap();
        assert_eq!(transition.result, TransitionResult::Applied);
        let notice = transition.events.last().unwrap();
        let after = &open_state(&table, "s").snapshot;
        assert!(notice.sequence > before.last_sequence);
        assert_eq!(after.last_sequence, notice.sequence);
        assert_eq!(after.last_content_sequence, EventSequence(0));
        assert_eq!(after.content_scalar_count, 0);
        assert_eq!(after.status, before.status);
        assert_eq!(
            after
                .generation_notice
                .as_ref()
                .map(|notice| notice.code.as_str()),
            Some("THINKING_CONTROL_DOWNGRADED")
        );
        assert_eq!(
            table
                .commit_terminal(
                    &ticket,
                    TerminalKind::Error {
                        code: "FAILED".into(),
                        message: "failed".into(),
                        retryable: true,
                    },
                )
                .unwrap()
                .result,
            TransitionResult::Applied
        );
        let retry = table.reserve_retry("s", "retry-2".to_owned()).unwrap();
        assert_eq!(
            table.commit_prepare_success(&retry.ticket),
            TransitionResult::Applied,
        );
        assert!(open_state(&table, "s").snapshot.generation_notice.is_none());
    }

    #[test]
    fn notice_after_emitted_deltas_does_not_flush_content() {
        let (mut table, ticket) = running_table("s");
        let now = tokio::time::Instant::now();
        table.accept_delta(&ticket, "A".into(), now).unwrap();
        assert!(matches!(
            table
                .accept_delta(&ticket, "B".into(), now + Duration::from_millis(1))
                .unwrap(),
            DeltaTransition::Emitted { .. }
        ));
        let transition = table.enqueue_notice(&ticket, "N", "notice").unwrap();
        assert_eq!(transition.result, TransitionResult::Applied);
        assert_eq!(transition.timer, TimerDirective::None);
        assert_eq!(transition.events.len(), 1);
        assert!(matches!(&transition.events[0].payload,
            ActionStreamPayload::Notice { code, message }
                if code == "N" && message == "notice"));
    }

    #[test]
    fn cancel_running_after_emitted_deltas_returns_unsignalled_token() {
        let (mut table, ticket) = running_table("s");
        let now = tokio::time::Instant::now();
        table.accept_delta(&ticket, "A".into(), now).unwrap();
        assert!(matches!(
            table
                .accept_delta(&ticket, "B".into(), now + Duration::from_millis(1))
                .unwrap(),
            DeltaTransition::Emitted { .. }
        ));
        let active_token = match &open_state(&table, "s").request_slot {
            RequestSlot::Running { cancellation, .. } => cancellation.clone(),
            _ => panic!("fixture must be Running"),
        };
        let transition = table.cancel("s").unwrap();
        // Running cancel always cancels any stream timer (even when no pending).
        assert_eq!(transition.timer, TimerDirective::Cancel);
        let events = &open_state(&table, "s").pending_events;
        assert!(matches!(
            &events[events.len() - 1].payload,
            ActionStreamPayload::Cancelled
        ));
        // B was already emitted as its own delta before cancel.
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            ActionStreamPayload::Delta { delta } if delta == "B"
        )));
        assert!(matches!(
            open_state(&table, "s").request_slot,
            RequestSlot::Vacant
        ));
        let returned = transition.cancellation.unwrap();
        assert!(!active_token.is_cancelled());
        assert!(!returned.is_cancelled());
        returned.cancel();
        assert!(active_token.is_cancelled());
    }

    #[test]
    fn cancel_replacement_preparing_preserves_old_snapshot_and_has_no_stream() {
        let mut table = completed_table("s", "old answer");
        let before = open_state(&table, "s").snapshot.clone();
        let retry = table.reserve_retry("s", "retry".to_owned()).unwrap();
        assert_eq!(open_state(&table, "s").request_stream, None);
        let transition = table.cancel("s").unwrap();
        assert_eq!(transition.timer, TimerDirective::None);
        let snapshot = &open_state(&table, "s").snapshot;
        assert_eq!(snapshot.content, before.content);
        assert_eq!(snapshot.status, before.status);
        assert_eq!(snapshot.request_id, before.request_id);
        assert_eq!(
            snapshot.preparation_failure.as_ref().unwrap().request_id,
            retry.ticket.request_id
        );
        assert!(snapshot.last_sequence > before.last_sequence);
        assert_eq!(open_state(&table, "s").request_stream, None);
        assert_eq!(
            table.commit_prepare_success(&retry.ticket),
            TransitionResult::Rejected
        );
        assert!(!transition.cancellation.unwrap().is_cancelled());
    }

    #[test]
    fn task3_counter_exhaustion_is_failure_atomic() {
        let now = tokio::time::Instant::now();

        let (mut accept, ticket) = running_table("accept-max");
        set_sequence_watermark(&mut accept, "accept-max", EventSequence(MAX_WIRE_COUNTER));
        let before = comparable_open_state(&accept, "accept-max");
        assert_eq!(
            accept.accept_delta(&ticket, "A".into(), now),
            Err(SessionError::CounterExhausted)
        );
        assert_open_state_exact(&accept, "accept-max", &before);

        // accept_delta no longer advances deadline_generation; second delta emits.
        let (mut deadline, ticket) = running_table("deadline-max");
        deadline.accept_delta(&ticket, "A".into(), now).unwrap();
        set_deadline_generation(
            &mut deadline,
            "deadline-max",
            DeadlineGeneration(MAX_WIRE_COUNTER),
        );
        assert!(matches!(
            deadline
                .accept_delta(&ticket, "B".into(), now + Duration::from_millis(1))
                .unwrap(),
            DeltaTransition::Emitted { .. }
        ));

        for operation in [
            CounterOperation::Terminal,
            CounterOperation::Cancel,
            CounterOperation::Notice,
        ] {
            let session_id = format!("two-sequences-{operation:?}");
            let (mut table, ticket) = running_table(&session_id);
            table.accept_delta(&ticket, "A".into(), now).unwrap();
            table
                .accept_delta(&ticket, "B".into(), now + Duration::from_millis(1))
                .unwrap();
            set_sequence_watermark(&mut table, &session_id, EventSequence(MAX_WIRE_COUNTER));
            let before = comparable_open_state(&table, &session_id);
            let error = match operation {
                CounterOperation::Terminal => table
                    .commit_terminal(&ticket, TerminalKind::Completed)
                    .unwrap_err(),
                CounterOperation::Cancel => table.cancel(&session_id).unwrap_err(),
                CounterOperation::Notice => {
                    table.enqueue_notice(&ticket, "N", "notice").unwrap_err()
                }
            };
            assert_eq!(error, SessionError::CounterExhausted);
            assert_open_state_exact(&table, &session_id, &before);
        }
    }

    #[test]
    fn initial_reservation_rejects_each_blank_identifier_without_mutation() {
        let mut cases = Vec::new();

        let mut blank_session = initial_input("valid-session");
        blank_session.session_id = String::new();
        cases.push(blank_session);

        let mut blank_window = initial_input("valid-session");
        blank_window.window_label = " \t".to_owned();
        cases.push(blank_window);

        let mut blank_request = initial_input("valid-session");
        blank_request.request_id = "\n".to_owned();
        cases.push(blank_request);

        let mut blank_action = initial_input("valid-session");
        blank_action.action_id = "   ".to_owned();
        cases.push(blank_action);

        for input in cases {
            let mut table = SessionTable::default();
            assert_eq!(
                table.reserve_initial(input).unwrap_err(),
                SessionError::InvalidInput
            );
            assert!(table.entries.is_empty());
            assert_eq!(table.last_session_generation, SessionGeneration(0));
        }
    }

    #[test]
    fn initial_reservation_is_atomic_and_session_id_is_never_reused() {
        let mut table = SessionTable::default();
        let reservation = table.reserve_initial(initial_input("session-a")).unwrap();

        assert_eq!(reservation.kind, ReservationKind::Initial);
        assert_eq!(reservation.ticket.session_generation, SessionGeneration(1));
        assert_eq!(reservation.ticket.request_generation, RequestGeneration(1));
        let state = open_state(&table, "session-a");
        match &state.request_slot {
            RequestSlot::Preparing { ticket, kind, .. } => {
                assert_eq!(ticket, &reservation.ticket);
                assert_eq!(*kind, ReservationKind::Initial);
            }
            _ => panic!("initial reservation must atomically install Preparing"),
        }
        assert_eq!(state.snapshot.session_id, "session-a");
        assert_eq!(state.snapshot.session_generation, SessionGeneration(1));
        assert_eq!(state.snapshot.request_id, "request-1");
        assert_eq!(state.snapshot.request_generation, RequestGeneration(1));
        assert_eq!(state.snapshot.action_id, "translate");
        assert_eq!(state.snapshot.status, ActionSnapshotStatus::Running);
        assert_eq!(state.snapshot.last_sequence, EventSequence::FIRST);
        assert_eq!(state.pending_events.len(), 1);
        let started = state.pending_events.front().unwrap();
        assert_eq!(started.session_id, state.snapshot.session_id);
        assert_eq!(
            started.session_generation,
            state.snapshot.session_generation
        );
        assert_eq!(started.request_id, state.snapshot.request_id);
        assert_eq!(
            started.request_generation,
            state.snapshot.request_generation
        );
        assert_eq!(started.action_id, state.snapshot.action_id);
        assert_eq!(started.sequence, EventSequence::FIRST);
        assert!(matches!(
            started.payload,
            ActionStreamPayload::Started { .. }
        ));

        let closed = table.close("session-a");
        assert!(closed.cancellation.is_some());
        assert!(matches!(
            table.entries.get("session-a"),
            Some(SessionEntry::Closed(SessionTombstone {
                session_generation: SessionGeneration(1),
                last_request_generation: RequestGeneration(1),
            }))
        ));
        assert_eq!(
            table
                .reserve_initial(initial_input("session-a"))
                .unwrap_err(),
            SessionError::Ended
        );
        assert!(matches!(
            table.entries.get("session-a"),
            Some(SessionEntry::Closed(_))
        ));
    }

    #[test]
    fn concurrent_continue_reservations_have_one_winner() {
        let table = Arc::new(Mutex::new(terminal_table(
            "session-race",
            ActionSnapshotStatus::Completed,
            "answer",
        )));
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for request_id in ["continue-a", "continue-b"] {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                table
                    .lock()
                    .unwrap()
                    .reserve_continue("session-race", request_id.to_owned())
            }));
        }
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(SessionError::Busy)))
                .count(),
            1
        );
    }

    #[test]
    fn replacement_prepare_success_installs_fresh_running_state_for_retry_and_continue() {
        for kind in [ReservationKind::Retry, ReservationKind::Continue] {
            let session_id = format!("replacement-success-{kind:?}");
            let request_id = format!("replacement-request-{kind:?}");
            let mut table = terminal_table(
                &session_id,
                ActionSnapshotStatus::Completed,
                "old terminal answer",
            );
            let old_snapshot = open_state(&table, &session_id).snapshot.clone();
            let old_pending_len = open_state(&table, &session_id).pending_events.len();
            let expected_sequence = open_state(&table, &session_id)
                .next_sequence
                .checked_next()
                .unwrap();

            let reservation = match kind {
                ReservationKind::Retry => table.reserve_retry(&session_id, request_id.clone()),
                ReservationKind::Continue => {
                    table.reserve_continue(&session_id, request_id.clone())
                }
                ReservationKind::Initial => unreachable!(),
            }
            .unwrap();

            let preparing = open_state(&table, &session_id);
            assert_eq!(preparing.snapshot, old_snapshot);
            assert_eq!(preparing.pending_events.len(), old_pending_len);
            match &preparing.request_slot {
                RequestSlot::Preparing {
                    ticket,
                    kind: preparing_kind,
                    ..
                } => {
                    assert_eq!(ticket, &reservation.ticket);
                    assert_eq!(*preparing_kind, kind);
                }
                _ => panic!("replacement reservation must remain Preparing"),
            }

            assert_eq!(
                table.commit_prepare_success(&reservation.ticket),
                TransitionResult::Applied
            );
            let running = open_state(&table, &session_id);
            assert_eq!(running.snapshot.session_id, session_id);
            assert_eq!(running.snapshot.session_generation, SessionGeneration(1));
            assert_eq!(running.snapshot.request_id, request_id);
            assert_eq!(running.snapshot.request_generation, RequestGeneration(2));
            assert_eq!(running.snapshot.action_id, "translate");
            assert_eq!(running.snapshot.status, ActionSnapshotStatus::Running);
            assert!(running.snapshot.content.is_empty());
            assert_eq!(running.snapshot.last_sequence, expected_sequence);
            assert_eq!(running.snapshot.last_content_sequence, EventSequence::NONE);
            assert_eq!(running.snapshot.content_scalar_count, 0);
            assert_eq!(running.snapshot.generation_notice, None);
            assert_eq!(running.snapshot.error_code, None);
            assert_eq!(running.snapshot.error_message, None);
            assert!(!running.snapshot.retryable);
            assert_eq!(running.next_sequence, expected_sequence);
            assert_eq!(running.pending_events.len(), old_pending_len + 1);
            let started = running.pending_events.back().unwrap();
            assert_eq!(started.session_id, running.snapshot.session_id);
            assert_eq!(
                started.session_generation,
                running.snapshot.session_generation
            );
            assert_eq!(started.request_id, running.snapshot.request_id);
            assert_eq!(
                started.request_generation,
                running.snapshot.request_generation
            );
            assert_eq!(started.action_id, running.snapshot.action_id);
            assert_eq!(started.sequence, expected_sequence);
            assert!(matches!(
                started.payload,
                ActionStreamPayload::Started { .. }
            ));
            match &running.request_slot {
                RequestSlot::Running { ticket, .. } => assert_eq!(ticket, &reservation.ticket),
                _ => panic!("matching prepare success must transition to Running"),
            }
        }
    }

    #[test]
    fn cancel_initial_preparing_commits_cancelled_before_caller_signals_token() {
        let mut table = SessionTable::default();
        let reservation = table
            .reserve_initial(initial_input("cancel-initial-preparing"))
            .unwrap();

        let transition = table.cancel("cancel-initial-preparing").unwrap();
        let state = open_state(&table, "cancel-initial-preparing");
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        assert_eq!(state.snapshot.session_id, reservation.ticket.session_id);
        assert_eq!(
            state.snapshot.session_generation,
            reservation.ticket.session_generation
        );
        assert_eq!(state.snapshot.request_id, reservation.ticket.request_id);
        assert_eq!(
            state.snapshot.request_generation,
            reservation.ticket.request_generation
        );
        assert_eq!(state.snapshot.action_id, reservation.ticket.action_id);
        assert_eq!(state.snapshot.status, ActionSnapshotStatus::Cancelled);
        assert!(state.snapshot.content.is_empty());
        assert_eq!(state.snapshot.last_sequence, EventSequence(2));
        assert_eq!(state.snapshot.last_content_sequence, EventSequence::NONE);
        assert_eq!(state.snapshot.content_scalar_count, 0);
        assert_eq!(state.pending_events.len(), 2);
        let cancelled = state.pending_events.back().unwrap();
        assert_eq!(cancelled.session_id, reservation.ticket.session_id);
        assert_eq!(
            cancelled.session_generation,
            reservation.ticket.session_generation
        );
        assert_eq!(cancelled.request_id, reservation.ticket.request_id);
        assert_eq!(
            cancelled.request_generation,
            reservation.ticket.request_generation
        );
        assert_eq!(cancelled.action_id, reservation.ticket.action_id);
        assert_eq!(cancelled.sequence, EventSequence(2));
        assert!(matches!(cancelled.payload, ActionStreamPayload::Cancelled));

        let cancellation = transition.cancellation.expect("token must be returned");
        assert!(!cancellation.is_cancelled());
        assert!(!reservation.cancellation.is_cancelled());
        cancellation.cancel();
        assert!(reservation.cancellation.is_cancelled());
    }

    #[test]
    fn cancel_running_preserves_content_watermarks_and_signals_only_after_return() {
        let mut table = SessionTable::default();
        let reservation = table
            .reserve_initial(initial_input("cancel-running"))
            .unwrap();
        assert_eq!(
            table.commit_prepare_success(&reservation.ticket),
            TransitionResult::Applied
        );
        let state = match table.entries.get_mut("cancel-running").unwrap() {
            SessionEntry::Open(state) => state,
            SessionEntry::Closed(_) => unreachable!(),
        };
        state.snapshot.content = "正文🙂".to_owned();
        state.snapshot.last_sequence = EventSequence(4);
        state.snapshot.last_content_sequence = EventSequence(4);
        state.snapshot.content_scalar_count = 3;
        state.next_sequence = EventSequence(4);
        let old_pending_len = state.pending_events.len();

        let transition = table.cancel("cancel-running").unwrap();
        let state = open_state(&table, "cancel-running");
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        assert_eq!(state.snapshot.session_id, reservation.ticket.session_id);
        assert_eq!(
            state.snapshot.session_generation,
            reservation.ticket.session_generation
        );
        assert_eq!(state.snapshot.request_id, reservation.ticket.request_id);
        assert_eq!(
            state.snapshot.request_generation,
            reservation.ticket.request_generation
        );
        assert_eq!(state.snapshot.action_id, reservation.ticket.action_id);
        assert_eq!(state.snapshot.status, ActionSnapshotStatus::Cancelled);
        assert_eq!(state.snapshot.content, "正文🙂");
        assert_eq!(state.snapshot.last_sequence, EventSequence(5));
        assert_eq!(state.snapshot.last_content_sequence, EventSequence(4));
        assert_eq!(state.snapshot.content_scalar_count, 3);
        assert_eq!(state.next_sequence, EventSequence(5));
        assert_eq!(state.pending_events.len(), old_pending_len + 1);
        let cancelled = state.pending_events.back().unwrap();
        assert_eq!(cancelled.session_id, reservation.ticket.session_id);
        assert_eq!(
            cancelled.session_generation,
            reservation.ticket.session_generation
        );
        assert_eq!(cancelled.request_id, reservation.ticket.request_id);
        assert_eq!(
            cancelled.request_generation,
            reservation.ticket.request_generation
        );
        assert_eq!(cancelled.action_id, reservation.ticket.action_id);
        assert_eq!(cancelled.sequence, EventSequence(5));
        assert!(matches!(cancelled.payload, ActionStreamPayload::Cancelled));

        let cancellation = transition.cancellation.expect("token must be returned");
        assert!(!cancellation.is_cancelled());
        assert!(!reservation.cancellation.is_cancelled());
        cancellation.cancel();
        assert!(reservation.cancellation.is_cancelled());
    }

    #[test]
    fn cancel_preparing_invalidates_ticket_before_token_signal() {
        let mut table = terminal_table(
            "session-cancel",
            ActionSnapshotStatus::Completed,
            "old answer",
        );
        let old_snapshot = open_state(&table, "session-cancel").snapshot.clone();
        let reservation = table
            .reserve_retry("session-cancel", "retry-1".to_owned())
            .unwrap();

        let transition = table.cancel("session-cancel").unwrap();
        let state = open_state(&table, "session-cancel");
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        assert_eq!(state.snapshot.content, old_snapshot.content);
        assert_eq!(state.snapshot.status, old_snapshot.status);
        assert_eq!(state.snapshot.request_id, old_snapshot.request_id);
        let failure = state.snapshot.preparation_failure.as_ref().unwrap();
        assert_eq!(failure.request_id, reservation.ticket.request_id);
        assert_eq!(failure.code, "PREPARATION_CANCELLED");
        assert!(state.snapshot.last_sequence > old_snapshot.last_sequence);
        assert_eq!(
            table.commit_prepare_success(&reservation.ticket),
            TransitionResult::Rejected
        );
        let cancellation = transition.cancellation.expect("token must be returned");
        assert!(!cancellation.is_cancelled());
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn replacement_prepare_failure_preserves_snapshot_and_returns_vacant() {
        let mut table = terminal_table(
            "session-failure",
            ActionSnapshotStatus::Completed,
            "old answer",
        );
        let before = open_state(&table, "session-failure").snapshot.clone();
        let reservation = table
            .reserve_retry("session-failure", "retry-failure".to_owned())
            .unwrap();

        assert_eq!(
            table.commit_prepare_failure(
                &reservation.ticket,
                SessionFailure {
                    code: "PREPARE_FAILED".to_owned(),
                    message: "could not prepare".to_owned(),
                    retryable: true,
                    question: Some("follow-up".to_owned()),
                },
            ),
            TransitionResult::Applied
        );
        let state = open_state(&table, "session-failure");
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        assert_eq!(state.snapshot.content, before.content);
        assert_eq!(state.snapshot.status, before.status);
        assert_eq!(state.snapshot.request_id, before.request_id);
        assert_eq!(state.snapshot.request_generation, before.request_generation);
        assert_eq!(
            state.snapshot.last_content_sequence,
            before.last_content_sequence
        );
        assert!(state.snapshot.last_sequence > before.last_sequence);
        let failure = state.snapshot.preparation_failure.as_ref().unwrap();
        assert_eq!(failure.request_id, reservation.ticket.request_id);
        assert_eq!(
            failure.request_generation,
            reservation.ticket.request_generation
        );
        assert_eq!(failure.code, "PREPARE_FAILED");
        assert_eq!(failure.question.as_deref(), Some("follow-up"));
        assert!(matches!(
            state.pending_events.back().unwrap().payload,
            ActionStreamPayload::PreparationFailed { .. }
        ));
    }

    #[test]
    fn initial_prepare_failure_commits_error_and_returns_vacant() {
        let mut table = SessionTable::default();
        let reservation = table
            .reserve_initial(initial_input("session-initial-fail"))
            .unwrap();

        assert_eq!(
            table.commit_prepare_failure(
                &reservation.ticket,
                SessionFailure {
                    code: "NO_ROUTE".to_owned(),
                    message: "route unavailable".to_owned(),
                    retryable: false,
                    question: None,
                },
            ),
            TransitionResult::Applied
        );
        let state = open_state(&table, "session-initial-fail");
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        assert_eq!(state.pending_events.len(), 2);
        assert_eq!(state.pending_events[0].sequence, EventSequence(1));
        assert!(matches!(
            state.pending_events[0].payload,
            ActionStreamPayload::Started { .. }
        ));
        assert_eq!(state.pending_events[1].sequence, EventSequence(2));
        assert!(matches!(
            &state.pending_events[1].payload,
            ActionStreamPayload::Error { code, message, retryable }
                if code == "NO_ROUTE" && message == "route unavailable" && !retryable
        ));
        assert_eq!(state.snapshot.status, ActionSnapshotStatus::Error);
        assert_eq!(state.snapshot.content, "");
        assert_eq!(state.snapshot.content_scalar_count, 0);
        assert_eq!(state.snapshot.last_sequence, EventSequence(2));
        assert_eq!(state.snapshot.last_content_sequence, EventSequence::NONE);
        assert_eq!(state.snapshot.error_code.as_deref(), Some("NO_ROUTE"));
        assert_eq!(
            state.snapshot.error_message.as_deref(),
            Some("route unavailable")
        );
        assert!(!state.snapshot.retryable);
    }

    #[test]
    fn close_drops_sensitive_state_and_returns_unsignalled_token() {
        let mut table = SessionTable::default();
        table
            .reserve_initial(initial_input("session-close"))
            .unwrap();
        let state = match table.entries.get_mut("session-close").unwrap() {
            SessionEntry::Open(state) => state,
            SessionEntry::Closed(_) => panic!("fixture session must be open"),
        };
        state.window_label = "sensitive-window".to_owned();
        state.snapshot.content = "distinctive secret content".to_owned();
        state.snapshot.error_code = Some("SECRET_CODE".to_owned());
        state.snapshot.error_message = Some("distinctive secret error".to_owned());
        state.snapshot.generation_notice = Some(ActionNotice {
            code: "SECRET_NOTICE".to_owned(),
            message: "distinctive secret notice".to_owned(),
        });

        let transition = table.close("session-close");
        let tombstone = match table.entries.get("session-close").unwrap() {
            SessionEntry::Closed(tombstone) => tombstone,
            SessionEntry::Open(_) => panic!("close must install tombstone before returning"),
        };
        let SessionTombstone {
            session_generation,
            last_request_generation,
        } = tombstone;
        assert_eq!(*session_generation, SessionGeneration(1));
        assert_eq!(*last_request_generation, RequestGeneration(1));
        let cancellation = transition
            .cancellation
            .expect("active token must be returned");
        assert!(!cancellation.is_cancelled());
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn stale_ticket_cannot_complete_newer_preparation() {
        let mut table = terminal_table("session-stale", ActionSnapshotStatus::Completed, "answer");
        let first = table
            .reserve_retry("session-stale", "retry-old".to_owned())
            .unwrap();
        let cancelled = table.cancel("session-stale").unwrap();
        assert!(cancelled.cancellation.is_some());
        let second = table
            .reserve_retry("session-stale", "retry-new".to_owned())
            .unwrap();

        assert_eq!(
            table.commit_prepare_success(&first.ticket),
            TransitionResult::Rejected
        );
        assert_eq!(
            table.commit_prepare_failure(
                &first.ticket,
                SessionFailure {
                    code: "STALE".to_owned(),
                    message: "stale failure".to_owned(),
                    retryable: false,
                    question: None,
                },
            ),
            TransitionResult::Rejected
        );
        let state = open_state(&table, "session-stale");
        match &state.request_slot {
            RequestSlot::Preparing { ticket, .. } => assert_eq!(ticket, &second.ticket),
            _ => panic!("newer preparation must remain active"),
        }
    }

    #[test]
    fn retry_and_continue_enforce_terminal_eligibility() {
        let mut running = SessionTable::default();
        running.reserve_initial(initial_input("running")).unwrap();
        assert_eq!(
            running
                .reserve_retry("running", "retry-running".to_owned())
                .unwrap_err(),
            SessionError::Busy
        );

        for status in [ActionSnapshotStatus::Cancelled, ActionSnapshotStatus::Error] {
            let mut table = terminal_table("continue-ineligible", status, "answer");
            assert_eq!(
                table
                    .reserve_continue("continue-ineligible", "continue".to_owned())
                    .unwrap_err(),
                SessionError::Ineligible
            );
        }
        let mut empty_completed =
            terminal_table("empty-completed", ActionSnapshotStatus::Completed, "");
        assert_eq!(
            empty_completed
                .reserve_continue("empty-completed", "continue".to_owned())
                .unwrap_err(),
            SessionError::Ineligible
        );

        for status in [
            ActionSnapshotStatus::Completed,
            ActionSnapshotStatus::Cancelled,
            ActionSnapshotStatus::Error,
        ] {
            let session_id = format!("retry-{status:?}");
            let mut table = terminal_table(&session_id, status, "answer");
            assert!(table.reserve_retry(&session_id, "retry".to_owned()).is_ok());
        }
        let mut nonempty_completed = terminal_table(
            "nonempty-completed",
            ActionSnapshotStatus::Completed,
            "answer",
        );
        assert!(nonempty_completed
            .reserve_continue("nonempty-completed", "continue".to_owned())
            .is_ok());
    }

    #[test]
    fn ask_session_can_continue_with_empty_completed_content() {
        let mut table = SessionTable::default();
        let ticket = table
            .open_completed_without_generation(InitialReservationInput {
                session_id: "ask-session".to_owned(),
                window_label: "result/ask-session".to_owned(),
                request_id: "ask-open".to_owned(),
                action_id: "ask-ai".to_owned(),
            })
            .unwrap();
        let state = open_state(&table, "ask-session");
        assert_eq!(state.snapshot.status, ActionSnapshotStatus::Completed);
        assert!(state.snapshot.content.is_empty());
        assert!(state.allow_continue_without_content);
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        assert_eq!(ticket.action_id, "ask-ai");
        assert!(table
            .reserve_continue("ask-session", "first-question".to_owned())
            .is_ok());
    }

    #[test]
    fn closed_session_tombstones_stay_bounded() {
        let mut table = SessionTable::default();
        let total = MAX_CLOSED_SESSIONS + 8;
        for index in 0..total {
            let id = format!("session-{index}");
            table.reserve_initial(initial_input(&id)).unwrap();
            let _ = table.close(&id);
        }
        assert_eq!(table.entries.len(), MAX_CLOSED_SESSIONS);
        assert!(!table.entries.contains_key("session-0"));
        assert!(table
            .entries
            .contains_key(&format!("session-{}", total - 1)));
    }

    #[test]
    fn generation_exhaustion_returns_error_without_wrapping_or_partial_mutation() {
        let mut session_exhausted = SessionTable {
            entries: Default::default(),
            closed_order: Default::default(),
            last_session_generation: SessionGeneration(MAX_WIRE_COUNTER),
        };
        assert_eq!(
            session_exhausted
                .reserve_initial(initial_input("cannot-create"))
                .unwrap_err(),
            SessionError::CounterExhausted
        );
        assert!(session_exhausted.entries.is_empty());
        assert_eq!(
            session_exhausted.last_session_generation,
            SessionGeneration(MAX_WIRE_COUNTER)
        );

        let mut request_exhausted = terminal_table(
            "request-exhausted",
            ActionSnapshotStatus::Completed,
            "preserved answer",
        );
        let state = match request_exhausted
            .entries
            .get_mut("request-exhausted")
            .unwrap()
        {
            SessionEntry::Open(state) => state,
            SessionEntry::Closed(_) => unreachable!(),
        };
        state.last_request_generation = RequestGeneration(MAX_WIRE_COUNTER);
        let before_snapshot = state.snapshot.clone();
        let before_events = state.pending_events.clone();
        let before_next_sequence = state.next_sequence;

        assert_eq!(
            request_exhausted
                .reserve_retry("request-exhausted", "cannot-retry".to_owned())
                .unwrap_err(),
            SessionError::CounterExhausted
        );
        let state = open_state(&request_exhausted, "request-exhausted");
        assert_eq!(
            state.last_request_generation,
            RequestGeneration(MAX_WIRE_COUNTER)
        );
        assert!(matches!(state.request_slot, RequestSlot::Vacant));
        assert_eq!(state.snapshot, before_snapshot);
        assert_eq!(state.pending_events, before_events);
        assert_eq!(state.next_sequence, before_next_sequence);
    }
}
