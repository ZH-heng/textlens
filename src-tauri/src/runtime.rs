use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{
    menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, State, WebviewWindow, WindowEvent,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};
use tokio::sync::oneshot;
use url::{Host, Url};
use uuid::Uuid;

#[cfg(target_os = "windows")]
use crate::models::ApplicationCloseBehavior;
use crate::{
    actions::{ActionBeginReady, ActionService, ActionServiceError},
    clipboard,
    models::{
        ActionKind, ActionNotice, ActionSnapshotStatus, ConnectionTestResult, CreateProviderInput,
        EventSequence, ExecuteActionRequest, HandshakeGeneration, Point as ActionPoint,
        PublicSettings, RequestGeneration, ResultDismissMode, ResultReadyAck, SessionGeneration,
        SettingsUpdate, SyncModelsResult, TriggerMode, UpdateProviderInput,
        WindowSize as SettingsWindowSize,
    },
    selection::{
        SelectionBounds, SelectionDirection, SelectionEvent, SelectionEventReceiver,
        SelectionMethod, SelectionMonitor, SelectionMouse, SelectionPayload, SelectionPoint,
        SelectionTrigger, SourceApplication,
    },
    settings::SettingsError,
    settings::SettingsRepository,
    windows::{
        restore_source_app_activation, result_label, DismissMode, Point as WindowPoint,
        ResultWindowOptions, ToolbarPlacement, WindowCoordinator, WindowSize,
    },
};

pub const SELECTION_EVENT: &str = "textlens:selection";
pub const SETTINGS_CHANGED_EVENT: &str = "textlens:settings-changed";
pub const SHORTCUT_ERROR_EVENT: &str = "textlens:shortcut-error";
pub const SETTINGS_CLOSE_REQUEST_EVENT: &str = "textlens:settings-close-requested";
pub const SETTINGS_GUIDANCE_EVENT: &str = "textlens:settings-guidance";
pub const TOOLBAR_DISMISSED_EVENT: &str = "textlens:toolbar-dismissed";

const SELECTION_MONITOR_START_ERROR: &str =
    "无法启动系统划词监听。请重新启动 TextLens；若仍然失败，请检查安全软件或系统策略。";
const SELECTION_MONITOR_DISCONNECTED_ERROR: &str = "系统划词监听意外停止。请重新启动 TextLens。";
const GLOBAL_SHORTCUT_ERROR: &str =
    "全局快捷键注册失败，可能已被其他应用占用。请更换快捷键后重试。";
/// Safety net for a main-thread hop. The queued closure may still run after a
/// timeout, but a stuck UI loop must not leave a settings save pending forever.
const MAIN_THREAD_HOP_TIMEOUT: Duration = Duration::from_secs(10);

const TOOLBAR_LABEL: &str = "selection-toolbar";
const SETTINGS_LABEL: &str = "settings";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSettingsOptions {
    pub focus: Option<String>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsGuidance {
    pub focus: Option<String>,
    pub notice: Option<String>,
}
const RESULT_LABEL_PREFIX: &str = "selection-result-";
const TRAY_ID: &str = "textlens-tray";
const TRAY_TOGGLE_ID: &str = "textlens-toggle";
const TRAY_PERMISSION_ID: &str = "textlens-permission";
const TRAY_SETTINGS_ID: &str = "textlens-settings";
const TRAY_QUIT_ID: &str = "textlens-quit";
const RESULT_DEFAULT_WIDTH: f64 = 520.0;
const RESULT_DEFAULT_HEIGHT: f64 = 420.0;
const RESIZE_PERSIST_DELAY_MS: u64 = 420;
const MAX_RESULT_SESSIONS: usize = 12;
const RESULT_REVEAL_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsCloseAction {
    HideToTray,
    RequestQuitConfirmation,
    QuitImmediately,
}

#[cfg(target_os = "windows")]
fn settings_close_action(
    behavior: ApplicationCloseBehavior,
    renderer_ready: bool,
) -> SettingsCloseAction {
    match behavior {
        ApplicationCloseBehavior::HideToTray => SettingsCloseAction::HideToTray,
        ApplicationCloseBehavior::Quit if renderer_ready => {
            SettingsCloseAction::RequestQuitConfirmation
        }
        ApplicationCloseBehavior::Quit => SettingsCloseAction::QuitImmediately,
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessibilityStatus {
    platform: &'static str,
    trusted: bool,
    can_request: bool,
    available: bool,
    diagnostics: RuntimeDiagnostics,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeDiagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    selection_monitor_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shortcut_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CursorPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolbarSize {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunActionResult {
    accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl RunActionResult {
    fn accepted(session_id: Option<String>, request_id: Option<String>) -> Self {
        Self {
            accepted: true,
            session_id,
            request_id,
            message: None,
        }
    }

    fn rejected(message: impl Into<String>) -> Self {
        Self {
            accepted: false,
            session_id: None,
            request_id: None,
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererSourceApplication {
    name: String,
    bundle_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RendererSelectionAnchor {
    Selection {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    Cursor {
        x: f64,
        y: f64,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererSelectionPayload {
    selection_id: String,
    text: String,
    source_app: RendererSourceApplication,
    anchor: RendererSelectionAnchor,
    direction: SelectionDirection,
    is_fullscreen: bool,
}

/// Payload emitted when the native runtime dismisses the selection toolbar.
///
/// Always force-hides the toolbar; the renderer uses this to clear local
/// selection / copy-success state that would otherwise outlive the window.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolbarDismissedPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    selection_id: Option<String>,
    reason: String,
}

/// Pure dismiss hide decision so unit tests can assert force-hide without an AppHandle.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DismissHidePlan {
    cleared_selection_id: Option<String>,
    force_hide: bool,
}

fn dismiss_hide_plan(cleared_id: Option<String>, scoped_hide_succeeded: bool) -> DismissHidePlan {
    DismissHidePlan {
        cleared_selection_id: cleared_id,
        // A successful selection-scoped hide already performed the native
        // mutation. A second unconditional hide used to add another synchronous
        // UI-thread hop to every consecutive drag.
        force_hide: !scoped_hide_succeeded,
    }
}

/// How far apart a drag's endpoints must be to read as "the user selected this
/// again" instead of the few pixels of drift a plain click always picks up.
const RESELECT_MIN_REACH_PX: f64 = 12.0;

/// Two reads of the same range differ by sub-pixel rounding, not by intent.
const SELECTION_BOUNDS_MATCH_TOLERANCE_PX: f64 = 2.0;

/// How close a re-selection drag must land to the range it is claiming to
/// re-create. Tight on purpose: the result window sits just below the text, so
/// a wider radius would admit the click that closes it and re-open the toolbar.
const RESELECT_ANCHOR_RADIUS_PX: f64 = 16.0;

/// The selection whose toolbar we last presented. A host app keeps reporting
/// the same selected text after an action finishes, so every later click that
/// drifts a few pixels re-captures it and the toolbar jumps back to the cursor.
/// That is what this identity exists to reject; a timer cannot tell the two
/// gestures apart, the range and the drag can.
#[derive(Debug, Clone, PartialEq)]
struct PresentedSelection {
    source: String,
    text: String,
    bounds: Option<SelectionBounds>,
    /// Used as the anchor when the provider exposes no range geometry.
    mouse: SelectionMouse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionBoundsMatch {
    Same,
    Different,
    /// The provider exposes no range geometry, so position carries no evidence.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionPresentation {
    Present,
    AlreadyPresented,
}

fn selection_bounds_match(
    presented: Option<SelectionBounds>,
    current: Option<SelectionBounds>,
) -> SelectionBoundsMatch {
    let (Some(presented), Some(current)) = (presented, current) else {
        return SelectionBoundsMatch::Unknown;
    };
    let tolerance = SELECTION_BOUNDS_MATCH_TOLERANCE_PX;
    let matches = (presented.x - current.x).abs() <= tolerance
        && (presented.y - current.y).abs() <= tolerance
        && (presented.width - current.width).abs() <= tolerance
        && (presented.height - current.height).abs() <= tolerance;
    if matches {
        SelectionBoundsMatch::Same
    } else {
        SelectionBoundsMatch::Different
    }
}

/// Distance actually travelled by the drag. A capture that reports no endpoints
/// is treated as no travel at all, which is the honest reading of a click.
fn selection_gesture_points(mouse: &SelectionMouse) -> [SelectionPoint; 2] {
    [
        mouse.start.unwrap_or(mouse.current),
        mouse.end.unwrap_or(mouse.current),
    ]
}

fn selection_gesture_reach(mouse: &SelectionMouse) -> f64 {
    let [start, end] = selection_gesture_points(mouse);
    (end.x - start.x).hypot(end.y - start.y)
}

fn bounds_contains_point(point: &SelectionPoint, bounds: SelectionBounds, inflation: f64) -> bool {
    point.x >= bounds.x - inflation
        && point.x <= bounds.x + bounds.width + inflation
        && point.y >= bounds.y - inflation
        && point.y <= bounds.y + bounds.height + inflation
}

/// Whether this capture is a drag *over the range that was already presented*.
/// Both conditions must hold: the mouse actually travelled, and it travelled
/// near that range. Travel alone is not enough — a click away from the text
/// picks up more drift than the threshold and used to throw the toolbar at the
/// cursor after a result window closed.
fn gesture_reselects_presented_range(
    presented: &PresentedSelection,
    selection: &SelectionPayload,
) -> bool {
    if selection.trigger != SelectionTrigger::Drag {
        return false;
    }
    if selection_gesture_reach(&selection.mouse) < RESELECT_MIN_REACH_PX {
        return false;
    }
    let radius = RESELECT_ANCHOR_RADIUS_PX;
    let current_points = selection_gesture_points(&selection.mouse);
    match presented.bounds {
        Some(bounds) => current_points
            .iter()
            .any(|point| bounds_contains_point(point, bounds, radius)),
        None => {
            let presented_points = selection_gesture_points(&presented.mouse);
            current_points.iter().any(|point| {
                presented_points
                    .iter()
                    .any(|previous| (point.x - previous.x).hypot(point.y - previous.y) <= radius)
            })
        }
    }
}

fn judge_selection_presentation(
    presented: Option<&PresentedSelection>,
    selection: &SelectionPayload,
) -> SelectionPresentation {
    let Some(presented) = presented else {
        return SelectionPresentation::Present;
    };
    if presented.source != selection.source_app.bundle_id || presented.text != selection.text {
        return SelectionPresentation::Present;
    }
    if selection.trigger == SelectionTrigger::Manual {
        // The user invoked the shortcut; they asked for this selection again.
        return SelectionPresentation::Present;
    }
    match selection_bounds_match(presented.bounds, selection.bounds) {
        SelectionBoundsMatch::Different => SelectionPresentation::Present,
        SelectionBoundsMatch::Same | SelectionBoundsMatch::Unknown => {
            if gesture_reselects_presented_range(presented, selection) {
                SelectionPresentation::Present
            } else {
                SelectionPresentation::AlreadyPresented
            }
        }
    }
}

/// Delay before collapsing the host highlight after a result closes. A synchronous
/// AX/UIA clear on the same gesture that blurs the result races the next drag and
/// can drop the capture (no toolbar). Cancel if a new selection is accepted first.
const HOST_SELECTION_CLEAR_DELAY_MS: u64 = 200;

/// Token match check for deferred host clear. A cancelled / superseded clear
/// holds a stale scheduled token and must be a no-op.
fn should_fire_pending_host_clear(scheduled_token: u64, current_token: u64) -> bool {
    scheduled_token == current_token
}

/// Pure token bump used when cancelling or replacing a pending host clear.
fn next_host_clear_token(current: u64) -> u64 {
    current.wrapping_add(1)
}

/// Arguments for best-effort host OS deselect when a result session ends.
/// Empty capture text skips clear entirely (nothing to match against).
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostSelectionClear {
    text: String,
    bundle_id: Option<String>,
}

/// Stray one-character selections are almost always drag artifacts; require
/// more than one visible character before presenting a toolbar.
fn selection_text_is_too_short(text: &str) -> bool {
    text.trim().chars().count() < 2
}

fn host_selection_clear_args(text: &str, bundle_id: Option<&str>) -> Option<HostSelectionClear> {
    let text = text.to_owned();
    if text.is_empty() {
        return None;
    }
    Some(HostSelectionClear {
        text,
        bundle_id: bundle_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    })
}

impl RendererSelectionPayload {
    fn new(selection_id: &str, selection: &SelectionPayload) -> Self {
        let anchor = selection.bounds.map_or_else(
            || RendererSelectionAnchor::Cursor {
                x: selection.mouse.current.x,
                y: selection.mouse.current.y,
            },
            |bounds| RendererSelectionAnchor::Selection {
                x: bounds.x,
                y: bounds.y,
                width: bounds.width.max(0.0),
                height: bounds.height.max(0.0),
            },
        );
        Self {
            selection_id: selection_id.to_owned(),
            text: selection.text.clone(),
            source_app: RendererSourceApplication {
                name: selection.source_app.name.clone(),
                bundle_id: (!selection.source_app.bundle_id.trim().is_empty())
                    .then(|| selection.source_app.bundle_id.clone()),
            },
            anchor,
            direction: selection.direction,
            is_fullscreen: selection.is_fullscreen,
        }
    }
}

#[derive(Debug, Clone)]
struct CurrentSelection {
    id: String,
    payload: SelectionPayload,
    source_result_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultSessionSnapshot {
    session_id: String,
    session_generation: SessionGeneration,
    request_id: String,
    request_generation: RequestGeneration,
    action_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_id: Option<String>,
    selection: RendererSelectionPayload,
    status: &'static str,
    content: String,
    #[serde(default)]
    thinking_content: String,
    last_sequence: EventSequence,
    last_content_sequence: EventSequence,
    content_scalar_count: u64,
    handshake_generation: HandshakeGeneration,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_notice: Option<ActionNotice>,
    error_message: String,
    retryable: bool,
    pinned: bool,
}

#[derive(Debug, Clone)]
struct ResultSessionMeta {
    selection: CurrentSelection,
    action_id: String,
    pinned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultSessionStart {
    /// Start network generation immediately (translate/explain/summary/…).
    Execute,
    /// Open a completed empty session and wait for the first question (Ask).
    OpenAsk,
}

fn compose_result_ready_snapshot(
    meta: ResultSessionMeta,
    begin: ActionBeginReady,
) -> ResultSessionSnapshot {
    let ActionBeginReady {
        snapshot,
        ack,
        route,
    } = begin;
    let status = match snapshot.status {
        ActionSnapshotStatus::Running => "streaming",
        ActionSnapshotStatus::Completed => "completed",
        ActionSnapshotStatus::Cancelled => "cancelled",
        ActionSnapshotStatus::Error => "error",
    };
    let (provider_id, model_id) = route.map_or((None, None), |route| {
        let _ = route.thinking_mode;
        (Some(route.provider_id), Some(route.model_id))
    });

    ResultSessionSnapshot {
        session_id: snapshot.session_id,
        session_generation: snapshot.session_generation,
        request_id: snapshot.request_id,
        request_generation: snapshot.request_generation,
        action_id: meta.action_id,
        provider_id,
        model_id,
        selection: RendererSelectionPayload::new(&meta.selection.id, &meta.selection.payload),
        status,
        content: snapshot.content,
        thinking_content: snapshot.thinking_content,
        last_sequence: snapshot.last_sequence,
        last_content_sequence: snapshot.last_content_sequence,
        content_scalar_count: snapshot.content_scalar_count,
        handshake_generation: ack.handshake_generation,
        generation_notice: snapshot.generation_notice,
        error_message: snapshot.error_message.unwrap_or_default(),
        retryable: snapshot.retryable,
        pinned: meta.pinned,
    }
}

fn follow_up_error_result(error: ActionServiceError) -> Result<RunActionResult, String> {
    match error {
        error @ (ActionServiceError::Busy | ActionServiceError::SessionEnded) => {
            Ok(RunActionResult::rejected(error.to_string()))
        }
        error => Err(error.to_string()),
    }
}

type ResultRevealReceiver = oneshot::Receiver<Result<(), String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultRevealHandshakePhase {
    Pending,
    Committing,
    Committed,
    Failed,
}

impl ResultRevealHandshakePhase {
    fn begin_commit(&mut self) -> Result<bool, ()> {
        match self {
            Self::Pending => {
                *self = Self::Committing;
                Ok(true)
            }
            Self::Committing | Self::Committed => Ok(false),
            Self::Failed => Err(()),
        }
    }

    fn finish_commit(&mut self, succeeded: bool) -> bool {
        if *self != Self::Committing {
            return false;
        }
        *self = if succeeded {
            Self::Committed
        } else {
            Self::Failed
        };
        true
    }
}

struct ResultRevealHandshake {
    phase: ResultRevealHandshakePhase,
    sender: Option<oneshot::Sender<Result<(), String>>>,
}

#[derive(Debug, Clone, Copy)]
struct PendingResultSize {
    revision: u64,
    size: SettingsWindowSize,
}

/// Releases the per-selection launch token on every exit path, including the
/// early rejections and cancellations inside the async action command.
struct SelectionConsumptionGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for SelectionConsumptionGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().remove(&self.id);
    }
}

pub struct RuntimeState {
    pub settings: Arc<SettingsRepository>,
    pub actions: ActionService,
    pub windows: WindowCoordinator,
    /// Serializes update_settings end-to-end (shortcut switch, disk write and
    /// rollback). Async commands can run concurrently, unlike the old
    /// main-thread sync commands, so the read-modify-write needs its own lock.
    settings_mutation: tokio::sync::Mutex<()>,
    selection_monitor: Mutex<SelectionMonitor>,
    current_selection: Mutex<Option<CurrentSelection>>,
    consuming_selection_ids: Mutex<HashSet<String>>,
    /// The last selection whose toolbar we presented, for
    /// `judge_selection_presentation`.
    presented_selection: Mutex<Option<PresentedSelection>>,
    /// Generation token for deferred host selection clear after result close.
    /// Bumped on cancel or when a newer clear is scheduled so in-flight tasks no-op.
    pending_host_clear_token: AtomicU64,
    /// At most one Windows UIA clear may be in flight. A provider can block
    /// indefinitely, so later clears are dropped instead of accumulating
    /// blocking runtime tasks.
    host_clear_in_flight: AtomicBool,
    /// Latest unpinned result ids awaiting the 16ms reveal commit, and whether
    /// the single delayed closer is already running.
    close_results_pending: Mutex<Option<Vec<String>>>,
    close_results_scheduled: AtomicBool,
    result_creation: Mutex<()>,
    result_sessions: Mutex<HashMap<String, ResultSessionMeta>>,
    result_reveals: Mutex<HashMap<String, ResultRevealHandshake>>,
    /// Generation for shortcut-triggered captures. A capture may outlive the
    /// plugin callback, so only the newest request may publish or hide UI.
    shortcut_capture_generation: AtomicU64,
    shortcut_switch: Mutex<()>,
    registered_shortcut: Mutex<Option<Shortcut>>,
    runtime_diagnostics: Mutex<RuntimeDiagnostics>,
    pending_result_sizes: Mutex<HashMap<String, PendingResultSize>>,
    resize_revision: AtomicU64,
    latest_result_size: Mutex<Option<SettingsWindowSize>>,
    last_permission: Mutex<bool>,
    settings_renderer_ready: AtomicBool,
    shutting_down: AtomicBool,
    pending_settings_guidance: Mutex<Option<SettingsGuidance>>,
}

impl RuntimeState {
    pub fn new(
        settings: Arc<SettingsRepository>,
        actions: ActionService,
        monitor: SelectionMonitor,
    ) -> Self {
        Self {
            settings,
            actions,
            windows: WindowCoordinator::default(),
            settings_mutation: tokio::sync::Mutex::new(()),
            selection_monitor: Mutex::new(monitor),
            current_selection: Mutex::new(None),
            consuming_selection_ids: Mutex::new(HashSet::new()),
            presented_selection: Mutex::new(None),
            pending_host_clear_token: AtomicU64::new(0),
            host_clear_in_flight: AtomicBool::new(false),
            close_results_pending: Mutex::new(None),
            close_results_scheduled: AtomicBool::new(false),
            result_creation: Mutex::new(()),
            result_sessions: Mutex::new(HashMap::new()),
            result_reveals: Mutex::new(HashMap::new()),
            shortcut_capture_generation: AtomicU64::new(0),
            shortcut_switch: Mutex::new(()),
            registered_shortcut: Mutex::new(None),
            runtime_diagnostics: Mutex::new(RuntimeDiagnostics::default()),
            pending_result_sizes: Mutex::new(HashMap::new()),
            resize_revision: AtomicU64::new(0),
            latest_result_size: Mutex::new(None),
            last_permission: Mutex::new(SelectionMonitor::is_accessibility_trusted()),
            settings_renderer_ready: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            pending_settings_guidance: Mutex::new(None),
        }
    }

    /// Remember the selection we are about to present, so a later capture that
    /// reads back the same range is recognized as the still-live selection
    /// rather than a new one.
    fn record_presented_selection(&self, selection: &SelectionPayload) {
        *self.presented_selection.lock() = Some(PresentedSelection {
            source: selection.source_app.bundle_id.clone(),
            text: selection.text.clone(),
            bounds: selection.bounds,
            mouse: selection.mouse,
        });
    }

    /// Bump the deferred-clear generation so any in-flight host clear is a no-op.
    fn cancel_pending_host_selection_clear(&self) {
        // fetch_add(1) matches `next_host_clear_token` including wrap at u64::MAX.
        self.pending_host_clear_token.fetch_add(1, Ordering::AcqRel);
    }

    /// Collapse the host highlight for a selection a toolbar action has taken
    /// over. Result-window selections live inside TextLens's own webview and have
    /// no host highlight to clear.
    fn schedule_selection_host_clear(&self, app: &AppHandle, selection: &CurrentSelection) {
        if selection.source_result_session_id.is_some() {
            return;
        }
        if let Some(clear) = host_selection_clear_args(
            &selection.payload.text,
            Some(selection.payload.source_app.bundle_id.as_str()),
        ) {
            self.schedule_host_selection_clear(app.clone(), clear);
        }
    }

    /// Schedule a best-effort host highlight collapse after
    /// [`HOST_SELECTION_CLEAR_DELAY_MS`]. Replaces any previous pending clear.
    /// Cancelled when a new selection is accepted for toolbar presentation.
    /// When the clear fires, matching same-text suppress is released so an
    /// intentional re-drag of the same phrase can show the toolbar (~200ms after
    /// result close instead of waiting the full 2s echo window).
    fn schedule_host_selection_clear(&self, app: AppHandle, clear: HostSelectionClear) {
        let previous = self.pending_host_clear_token.fetch_add(1, Ordering::AcqRel);
        let token = next_host_clear_token(previous);
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(HOST_SELECTION_CLEAR_DELAY_MS)).await;
            let state = app.state::<RuntimeState>();
            if !should_fire_pending_host_clear(
                token,
                state.pending_host_clear_token.load(Ordering::Acquire),
            ) {
                return;
            }
            if state
                .host_clear_in_flight
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let clear_text = clear.text;
            let clear_bundle_id = clear.bundle_id;
            let clear_app = app.clone();
            let spawned = thread::Builder::new()
                .name("textlens-host-selection-clear".to_owned())
                .spawn(move || {
                    let _ = SelectionMonitor::clear_matching_text(
                        clear_bundle_id.as_deref(),
                        &clear_text,
                    );
                    let state = clear_app.state::<RuntimeState>();
                    state.host_clear_in_flight.store(false, Ordering::Release);
                });
            if spawned.is_err() {
                state.host_clear_in_flight.store(false, Ordering::Release);
            }
        });
    }

    pub fn process_selection_event(&self, app: &AppHandle, event: SelectionEvent) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        match event {
            SelectionEvent::Selection(selection) => self.handle_selection(app, selection, false),
            SelectionEvent::Dismiss(dismiss) => {
                let result_selection_origin = self
                    .current_selection
                    .lock()
                    .as_ref()
                    .and_then(|selection| selection.source_result_session_id.as_deref())
                    .map(str::to_owned);
                if result_selection_dismiss_is_internal(
                    result_selection_origin.as_deref(),
                    &dismiss.reason,
                    dismiss.target_pid,
                    std::process::id(),
                ) {
                    // A result webview explicitly dismisses its own selection
                    // on pointer-down. The toolbar is another TextLens window;
                    // its mouse-down must keep the current selection alive
                    // long enough for the action IPC to consume it.
                    return;
                }
                let point = WindowPoint {
                    x: dismiss.mouse.x,
                    y: dismiss.mouse.y,
                };
                if dismiss.reason == "mouseDown" && self.windows.point_inside_toolbar(app, point) {
                    return;
                }
                let selection_id = {
                    let mut current = self.current_selection.lock();
                    if current.as_ref().is_some_and(|selection| {
                        dismiss_precedes_selection(
                            selection.payload.sequence,
                            selection.payload.timestamp_ms,
                            dismiss.sequence,
                            dismiss.timestamp_ms,
                        )
                    }) {
                        return;
                    }
                    current.take().map(|selection| selection.id)
                };

                // Hiding the toolbar is enough to stop a repeat presentation: the
                // identity recorded when the selection was presented keeps rejecting
                // re-reads of the same range, so nothing has to be armed here.

                // Hide off the selection event thread. The native callback still
                // checks the expected id, so a newer selection cannot be hidden
                // by this older dismiss request.
                let scoped_hide_requested = selection_id.is_some();
                if let Some(ref id) = selection_id {
                    self.windows.hide_toolbar_if_selection_async(app, id);
                } else {
                    self.windows.hide_toolbar_async(app);
                }
                let plan = dismiss_hide_plan(selection_id.clone(), scoped_hide_requested);

                // 3) Notify the toolbar renderer even when no selection was held, so
                // copy-success / React selection state cannot outlive the native window.
                let _ = app.emit_to(
                    TOOLBAR_LABEL,
                    TOOLBAR_DISMISSED_EVENT,
                    ToolbarDismissedPayload {
                        selection_id: plan.cleared_selection_id,
                        reason: dismiss.reason.clone(),
                    },
                );
            }
        }
    }

    fn clear_current_selection_if(&self, selection_id: &str) -> bool {
        let mut current = self.current_selection.lock();
        if current.as_ref().map(|selection| selection.id.as_str()) == Some(selection_id) {
            *current = None;
            true
        } else {
            false
        }
    }

    fn current_selection_matches(&self, selection_id: &str) -> bool {
        self.current_selection
            .lock()
            .as_ref()
            .is_some_and(|selection| selection.id == selection_id)
    }

    fn invalidate_shortcut_capture(&self) {
        self.shortcut_capture_generation
            .fetch_add(1, Ordering::AcqRel);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    fn recover_toolbar_delivery(&self, app: &AppHandle, selection_id: &str) {
        if !self.current_selection_matches(selection_id) {
            return;
        }
        match self.windows.rebuild_toolbar(app, selection_id) {
            Ok(_) => {}
            Err(_) => eprintln!(
                "[toolbar] stage=delivery-recovery selection_id={} error_class=window",
                selection_id
            ),
        }
    }

    fn hide_toolbar_for_selection(&self, app: &AppHandle, selection_id: &str) {
        let _ = self.windows.hide_toolbar_if_selection(app, selection_id);
    }

    fn clear_and_hide_current_selection_if(&self, app: &AppHandle, selection_id: &str) -> bool {
        let cleared = self.clear_current_selection_if(selection_id);
        // The global dismiss hook may already have consumed the logical
        // selection while the native toolbar is still visible. Always retry
        // the selection-bound native hide; its coordinator check protects a
        // newer toolbar generation from an old action completion.
        self.hide_toolbar_for_selection(app, selection_id);
        cleared
    }

    fn clear_current_selection_from_result(&self, session_id: &str) -> Option<String> {
        let mut current = self.current_selection.lock();
        if current.as_ref().is_some_and(|selection| {
            selection_source_matches_result(
                selection.source_result_session_id.as_deref(),
                session_id,
            )
        }) {
            current.take().map(|selection| selection.id)
        } else {
            None
        }
    }

    fn handle_selection(&self, app: &AppHandle, selection: SelectionPayload, force_capture: bool) {
        let settings = self.settings.get_settings_arc();
        if !settings.enabled || !SelectionMonitor::is_accessibility_trusted() {
            self.windows.hide_toolbar(app);
            *self.current_selection.lock() = None;
            return;
        }
        // Shortcut mode keeps the selection monitor running so outside clicks can
        // dismiss the toolbar, but automatic selection events must not replace or
        // clear a shortcut-invoked toolbar.
        if !should_present_selection_for_trigger(force_capture, settings.trigger.mode) {
            return;
        }
        // A capture that reads back the range we already presented is the host
        // app still reporting the live selection, not the user selecting again.
        // Presenting it makes the toolbar jump to the cursor.
        if !force_capture
            && judge_selection_presentation(self.presented_selection.lock().as_ref(), &selection)
                == SelectionPresentation::AlreadyPresented
        {
            return;
        }
        if !application_is_allowed(&settings.filter, &selection)
            || selection_text_is_too_short(&selection.text)
            || selection.text.chars().count() > 1_000_000
        {
            self.windows.hide_toolbar(app);
            *self.current_selection.lock() = None;
            return;
        }

        // Selection accepted for toolbar presentation: remember its identity for
        // the repeat check above, and do not collapse the host highlight that the
        // user is actively dragging / has just captured.
        self.record_presented_selection(&selection);
        self.cancel_pending_host_selection_clear();
        let runtime_started = Instant::now();

        let (anchor, placement) = selection_toolbar_anchor(&selection);
        let current = CurrentSelection {
            id: Uuid::new_v4().to_string(),
            payload: selection,
            source_result_session_id: None,
        };
        let toolbar_selection_id = current.id.clone();
        let public = RendererSelectionPayload::new(&current.id, &current.payload);
        // Publish before touching the native window. If the toolbar is being
        // replaced, a stage/emit failure must leave this validated selection
        // available for the replacement renderer's `toolbar_ready` replay.
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        self.windows.begin_toolbar_selection(&toolbar_selection_id);
        *self.current_selection.lock() = Some(current);
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let stage_failed = match self.windows.stage_toolbar_with_placement(
            app,
            &toolbar_selection_id,
            anchor,
            placement,
        ) {
            Ok(()) => false,
            Err(error) => {
                // Keep the old frame hidden when native staging fails. Showing
                // it here lets a stale renderer receive clicks before the new
                // selection event has committed; the bounded renderer recovery
                // below is the only retry path.
                eprintln!("[toolbar] native stage failed; keeping toolbar hidden: {error}");
                true
            }
        };
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        if let Err(error) =
            self.windows
                .show_toolbar_with_placement(app, &toolbar_selection_id, anchor, placement)
        {
            self.clear_and_hide_current_selection_if(app, &toolbar_selection_id);
            eprintln!("[window] 无法显示划词工具栏：{error}");
            return;
        }
        if !self.current_selection_matches(&toolbar_selection_id) {
            return;
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if stage_failed {
            eprintln!(
                "[toolbar] stage=selection-stage selection_id={} error_class=window",
                toolbar_selection_id
            );
            // Keep publishing below. `recover_toolbar_delivery` may have
            // replaced the WebView, and the replacement's `toolbar_ready`
            // command replays the in-memory selection. Returning here loses
            // the only event for this mouse-up when the new renderer starts.
            self.recover_toolbar_delivery(app, &toolbar_selection_id);
        }
        let emitted = app.emit_to(TOOLBAR_LABEL, SELECTION_EVENT, public).is_ok();
        trace_selection_timing("runtime", runtime_started);
        if !emitted {
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                eprintln!(
                    "[toolbar] stage=selection-emit selection_id={} error_class=window",
                    toolbar_selection_id
                );
                // The selection stays published on purpose: the rebuilt
                // renderer replays it through `toolbar_ready`, so a dead
                // WebView costs one rebuild instead of this whole mouse-up.
                self.recover_toolbar_delivery(app, &toolbar_selection_id);
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            self.clear_and_hide_current_selection_if(app, &toolbar_selection_id);
            return;
        }

        if self.current_selection_matches(&toolbar_selection_id) {
            // Closing result windows can wait on the Windows UI thread. Give
            // the renderer one frame to issue its measured visible commit,
            // then close old unpinned results off the selection event path.
            // Invalidate any older deferred host-clear task before scheduling.
            self.cancel_pending_host_selection_clear();
            self.schedule_close_unpinned_results(app);
        }
    }

    fn schedule_close_unpinned_results(&self, app: &AppHandle) {
        let session_ids = self.unpinned_result_session_ids();
        if session_ids.is_empty() {
            return;
        }
        *self.close_results_pending.lock() = Some(session_ids);
        if self.close_results_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let app = app.clone();
        let _ = thread::Builder::new()
            .name("textlens-close-selection-results".to_owned())
            .spawn(move || loop {
                // Let ToolbarApp's layout effect enqueue the native visible
                // commit before closing result windows on the same UI thread.
                thread::sleep(Duration::from_millis(16));
                let state = app.state::<RuntimeState>();
                let pending = state.close_results_pending.lock().take();
                if let Some(session_ids) = pending {
                    if !state.shutting_down.load(Ordering::Acquire) {
                        state.close_unpinned_result_sessions(&app, session_ids);
                    }
                }
                state
                    .close_results_scheduled
                    .store(false, Ordering::Release);
                if state.close_results_pending.lock().is_none() {
                    break;
                }
                // A newer schedule arrived while this closer was running; keep
                // the same timer instead of spawning one thread per mouse-up.
                if state.close_results_scheduled.swap(true, Ordering::AcqRel) {
                    break;
                }
            });
    }

    pub fn capture_current(&self, app: &AppHandle) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let generation = self
            .shortcut_capture_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let app = app.clone();
        let _ = thread::Builder::new()
            .name("textlens-shortcut-capture".to_owned())
            .spawn(move || {
                let state = app.state::<RuntimeState>();
                let current = || {
                    !state.shutting_down.load(Ordering::Acquire)
                        && state.shortcut_capture_generation.load(Ordering::Acquire) == generation
                };
                if !current() {
                    return;
                }

                // Windows manual capture is enqueue-only here. Releasing the
                // facade lock before waiting lets another shortcut request
                // reach the native latest-wins coordinator and cancel a slow
                // provider probe instead of queueing behind it.
                #[cfg(target_os = "windows")]
                let captured = {
                    let reply = state.selection_monitor.lock().capture_current_async();
                    match reply {
                        Ok(reply) => reply.recv_timeout(Duration::from_secs(5)).map_or_else(
                            |_| Err(crate::selection::SelectionError::Internal),
                            |result| result,
                        ),
                        Err(error) => Err(error),
                    }
                };
                #[cfg(not(target_os = "windows"))]
                let captured = state.selection_monitor.lock().capture_current();
                if !current() {
                    return;
                }
                match captured {
                    Ok(Some(selection)) => state.handle_selection(&app, selection, true),
                    Ok(None) | Err(_) => {
                        state.windows.hide_toolbar(&app);
                        *state.current_selection.lock() = None;
                    }
                }
            });
    }

    pub fn reconcile_capture(&self, app: &AppHandle) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let settings = self.settings.get_settings();
        let trusted = SelectionMonitor::is_accessibility_trusted();
        // Keep the native monitor running in both trigger modes while enabled:
        // shortcut mode still needs dismiss events (outside click) after a
        // shortcut-invoked toolbar is shown. Auto-selection is filtered in
        // handle_selection when mode is Shortcut.
        let should_listen = settings.enabled && trusted;
        if !should_listen {
            self.invalidate_shortcut_capture();
        }
        let start_failed = {
            let monitor = self.selection_monitor.lock();
            #[cfg(target_os = "windows")]
            let settings_applied = monitor
                .update_capture_settings(settings.selection_capture.clone())
                .is_ok();
            if should_listen {
                #[cfg(target_os = "windows")]
                {
                    !settings_applied || monitor.start().is_err()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    monitor.start().is_err()
                }
            } else {
                let _ = monitor.stop();
                self.windows.hide_toolbar(app);
                *self.current_selection.lock() = None;
                false
            }
        };
        let diagnostic = selection_monitor_diagnostic(should_listen, start_failed);
        if self.set_selection_monitor_error(diagnostic) {
            if let Ok(public) = self.settings.get_public_settings() {
                refresh_tray(app, &public);
            }
        }
    }

    pub fn reconcile_shortcut(&self, app: &AppHandle) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let settings = self.settings.get_settings();
        let requested = shortcut_registration_target(
            settings.enabled,
            settings.trigger.mode,
            settings.capture_shortcut.as_str(),
        );
        if let Err(error) = self.switch_shortcut(app, &requested) {
            // Keep the saved value so the settings page can show exactly what
            // needs editing. The event is only a prompt; RuntimeDiagnostics is
            // the replayable source of truth for windows opened afterwards.
            let _ = app.emit(SHORTCUT_ERROR_EVENT, error);
        }
    }

    fn switch_shortcut(&self, app: &AppHandle, requested: &str) -> Result<String, String> {
        let _transaction = self.shortcut_switch.lock();
        let requested = requested.trim();
        let outcome = (|| {
            let requested = if requested.is_empty() {
                None
            } else {
                Some(
                    requested
                        .parse::<Shortcut>()
                        .map_err(|_| GLOBAL_SHORTCUT_ERROR.to_owned())?,
                )
            };
            let current = *self.registered_shortcut.lock();
            if current == requested {
                if let Some(shortcut) = requested {
                    if !app.global_shortcut().is_registered(shortcut) {
                        app.global_shortcut()
                            .register(shortcut)
                            .map_err(|_| GLOBAL_SHORTCUT_ERROR.to_owned())?;
                    }
                    if !app.global_shortcut().is_registered(shortcut) {
                        return Err(GLOBAL_SHORTCUT_ERROR.to_owned());
                    }
                }
                return Ok(current
                    .map(|shortcut| shortcut.to_string())
                    .unwrap_or_default());
            }

            if let Some(shortcut) = requested {
                // A previous failed rollback may have left this shortcut in the
                // plugin registry. Adopt it instead of treating our own stale
                // registration as a system-wide conflict.
                if !app.global_shortcut().is_registered(shortcut) {
                    app.global_shortcut()
                        .register(shortcut)
                        .map_err(|_| GLOBAL_SHORTCUT_ERROR.to_owned())?;
                }
            }

            if let Some(shortcut) = current {
                if app.global_shortcut().is_registered(shortcut)
                    && app.global_shortcut().unregister(shortcut).is_err()
                {
                    if let Some(requested) = requested {
                        let _ = app.global_shortcut().unregister(requested);
                    }
                    return Err(GLOBAL_SHORTCUT_ERROR.to_owned());
                }
            }

            if current.is_some_and(|shortcut| app.global_shortcut().is_registered(shortcut)) {
                if let Some(requested) = requested {
                    let _ = app.global_shortcut().unregister(requested);
                }
                return Err(GLOBAL_SHORTCUT_ERROR.to_owned());
            }
            if requested.is_some_and(|shortcut| !app.global_shortcut().is_registered(shortcut)) {
                return Err(GLOBAL_SHORTCUT_ERROR.to_owned());
            }

            *self.registered_shortcut.lock() = requested;
            Ok(current
                .map(|shortcut| shortcut.to_string())
                .unwrap_or_default())
        })();
        self.set_shortcut_error(shortcut_registration_diagnostic(outcome.is_err()));
        outcome
    }

    fn set_selection_monitor_error(&self, error: Option<String>) -> bool {
        let mut diagnostics = self.runtime_diagnostics.lock();
        if diagnostics.selection_monitor_error == error {
            return false;
        }
        diagnostics.selection_monitor_error = error;
        true
    }

    fn handle_selection_receiver_disconnected(&self, app: &AppHandle) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }

        // A disconnected receiver means no producer can deliver another event.
        // Clear stale UI first, then rebuild the Windows producer and receiver
        // as one lifecycle. Native error details are deliberately not logged
        // because providers may include selected text in their error messages.
        self.invalidate_shortcut_capture();
        *self.current_selection.lock() = None;
        self.windows.hide_toolbar(app);
        self.selection_monitor.lock().shutdown();

        #[cfg(target_os = "windows")]
        let recovered_receiver = self.selection_monitor.lock().rebuild_event_receiver().ok();
        #[cfg(not(target_os = "windows"))]
        let recovered_receiver: Option<SelectionEventReceiver> = None;

        let recovered = recovered_receiver
            .is_some_and(|receiver| spawn_selection_loop(app.clone(), receiver).is_ok());
        let diagnostic = (!recovered).then(|| SELECTION_MONITOR_DISCONNECTED_ERROR.to_owned());
        if self.set_selection_monitor_error(diagnostic) {
            if let Ok(public) = self.settings.get_public_settings() {
                refresh_tray(app, &public);
            }
        }
        if recovered {
            self.reconcile_capture(app);
            eprintln!("[selection] event channel disconnected; native monitor rebuilt");
        } else {
            eprintln!("[selection] event channel disconnected; monitor recovery failed");
        }
    }

    fn set_shortcut_error(&self, error: Option<String>) -> bool {
        let mut diagnostics = self.runtime_diagnostics.lock();
        if diagnostics.shortcut_error == error {
            return false;
        }
        diagnostics.shortcut_error = error;
        true
    }

    fn accessibility_status(&self) -> AccessibilityStatus {
        let platform = accessibility_platform();
        let trusted = SelectionMonitor::is_accessibility_trusted();
        let diagnostics = self.runtime_diagnostics.lock().clone();
        AccessibilityStatus {
            platform,
            trusted,
            can_request: cfg!(target_os = "macos"),
            available: selection_access_available(platform, trusted, &diagnostics),
            diagnostics,
        }
    }

    pub fn after_settings_changed(&self, app: &AppHandle, settings: &PublicSettings) {
        self.invalidate_shortcut_capture();
        self.reconcile_capture(app);
        self.reconcile_shortcut(app);
        refresh_tray(app, settings);
        self.windows.update_result_behavior(
            app,
            dismiss_mode(settings.result.dismiss_mode),
            u64::from(settings.result.dismiss_delay_ms),
            settings.result.remember_size,
        );
        for (label, _) in app.webview_windows() {
            if label.starts_with(RESULT_LABEL_PREFIX) {
                let _ = self
                    .windows
                    .set_result_opacity(app, &label, settings.result.opacity);
            }
        }
        let _ = app.emit(SETTINGS_CHANGED_EVENT, settings.clone());
    }

    pub fn poll_permission(&self, app: &AppHandle) {
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let trusted = SelectionMonitor::is_accessibility_trusted();
        let changed = {
            let mut previous = self.last_permission.lock();
            let changed = *previous != trusted;
            *previous = trusted;
            changed
        };
        if changed {
            self.reconcile_capture(app);
            if let Ok(settings) = self.settings.get_public_settings() {
                refresh_tray(app, &settings);
                let _ = app.emit(SETTINGS_CHANGED_EVENT, settings);
            }
        }
    }

    pub fn shutdown(&self, app: &AppHandle) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }

        // Reject new capture/action work before cancelling anything already in
        // flight. Every exit entry point funnels through this idempotent path.
        self.invalidate_shortcut_capture();
        self.consuming_selection_ids.lock().clear();
        self.windows.hide_toolbar(app);
        *self.current_selection.lock() = None;

        let sessions = self
            .result_sessions
            .lock()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for session_id in &sessions {
            let _ = self.actions.cancel(app, session_id);
        }

        let shortcut = self.registered_shortcut.lock().take();
        if let Some(shortcut) = shortcut {
            let _ = app.global_shortcut().unregister(shortcut);
        }

        // On Windows this sends Shutdown to the UIA/OLE STA worker and only
        // waits for a short grace period. A background reaper owns any slower
        // join, so a blocked third-party COM provider cannot freeze exit.
        self.selection_monitor.lock().shutdown();

        let latest_revision = self.resize_revision.load(Ordering::Acquire);
        let latest_session =
            self.pending_result_sizes
                .lock()
                .iter()
                .find_map(|(session_id, pending)| {
                    (pending.revision == latest_revision).then(|| session_id.clone())
                });
        if let Some(session_id) = latest_session {
            self.persist_pending_result_size(app, &session_id, Some(latest_revision));
        }
        for session_id in sessions {
            self.actions.clear_session(&session_id);
        }
        self.result_reveals.lock().clear();
        self.result_sessions.lock().clear();
    }

    fn create_result_session(
        &self,
        app: &AppHandle,
        action_id: &str,
        action_name: &str,
        selection: CurrentSelection,
        cursor: WindowPoint,
    ) -> Result<(String, String, ResultRevealReceiver), String> {
        self.create_result_session_with(
            app,
            action_id,
            action_name,
            selection,
            cursor,
            ResultSessionStart::Execute,
        )
    }

    fn create_ask_result_session(
        &self,
        app: &AppHandle,
        action_id: &str,
        action_name: &str,
        selection: CurrentSelection,
        cursor: WindowPoint,
    ) -> Result<(String, String, ResultRevealReceiver), String> {
        self.create_result_session_with(
            app,
            action_id,
            action_name,
            selection,
            cursor,
            ResultSessionStart::OpenAsk,
        )
    }

    fn create_result_session_with(
        &self,
        app: &AppHandle,
        action_id: &str,
        action_name: &str,
        selection: CurrentSelection,
        cursor: WindowPoint,
        start: ResultSessionStart,
    ) -> Result<(String, String, ResultRevealReceiver), String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("TextLens 正在退出".to_owned());
        }
        // Different renderer commands can arrive concurrently. Serialize the
        // short create/replace transaction so a newer selection cannot insert
        // its result between another request's capacity check and window
        // creation, which would otherwise allow it to be closed as the
        // "previous" unpinned result or exceed the pinned-session limit.
        let _creation_guard = self.result_creation.lock();
        let session_id = Uuid::new_v4().to_string();
        let label = result_label(&session_id);
        let request = ExecuteActionRequest {
            session_id: session_id.clone(),
            window_label: label.clone(),
            action_id: action_id.to_owned(),
            text: selection.payload.text.clone(),
            cursor: Some(ActionPoint {
                x: cursor.x,
                y: cursor.y,
            }),
            target_language: None,
        };

        // A new unpinned result replaces all existing unpinned results, so
        // only pinned sessions consume the durable session limit. Check this
        // before starting the request, while leaving the previous result
        // untouched if request preparation fails.
        let pinned_sessions = self
            .result_sessions
            .lock()
            .values()
            .filter(|meta| meta.pinned)
            .count();
        if pinned_sessions >= MAX_RESULT_SESSIONS {
            return Err(format!(
                "置顶结果窗口已达到 {MAX_RESULT_SESSIONS} 个，请先关闭部分窗口"
            ));
        }

        // `execute` starts DNS/TLS/model work immediately and overlaps with
        // window creation. `open_ask` only seeds context without network.
        let request_id = match start {
            ResultSessionStart::Execute => self
                .actions
                .execute(app, request)
                .map_err(|error| error.to_string())?,
            ResultSessionStart::OpenAsk => self
                .actions
                .open_ask(app, request)
                .map_err(|error| error.to_string())?,
        };

        self.close_unpinned_results(app);
        if self.result_sessions.lock().len() >= MAX_RESULT_SESSIONS {
            let _ = self.actions.cancel(app, &session_id);
            self.actions.clear_session(&session_id);
            return Err(format!(
                "置顶结果窗口已达到 {MAX_RESULT_SESSIONS} 个，请先关闭部分窗口"
            ));
        }
        // Closing the previous unpinned result may flush a just-resized size;
        // read result preferences only after that flush so this new window
        // immediately uses the user's latest dimensions.
        let result = self.settings.get_settings().result;
        let size = if result.remember_size {
            (*self.latest_result_size.lock())
                .or(result.last_size)
                .unwrap_or(SettingsWindowSize {
                    width: RESULT_DEFAULT_WIDTH,
                    height: RESULT_DEFAULT_HEIGHT,
                })
        } else {
            SettingsWindowSize {
                width: RESULT_DEFAULT_WIDTH,
                height: RESULT_DEFAULT_HEIGHT,
            }
        };
        let options = ResultWindowOptions {
            size: WindowSize {
                width: size.width,
                height: size.height,
            },
            cursor,
            follow_cursor: result.follow_cursor,
            pinned: result.default_pinned,
            dismiss_mode: dismiss_mode(result.dismiss_mode),
            dismiss_delay_ms: u64::from(result.dismiss_delay_ms),
            opacity: result.opacity,
            remember_size: result.remember_size,
        };
        // Publish immutable session metadata before creating the webview. A
        // fast renderer can invoke its ready handshake during `build()`, and
        // must already be able to obtain the original text and pin state.
        self.result_sessions.lock().insert(
            session_id.clone(),
            ResultSessionMeta {
                selection,
                action_id: action_id.to_owned(),
                pinned: result.default_pinned,
            },
        );
        let (reveal_sender, reveal_receiver) = oneshot::channel();
        self.result_reveals.lock().insert(
            session_id.clone(),
            ResultRevealHandshake {
                phase: ResultRevealHandshakePhase::Pending,
                sender: Some(reveal_sender),
            },
        );
        if let Err(error) =
            self.windows
                .create_result_window(app, &session_id, action_name, options)
        {
            self.result_reveals.lock().remove(&session_id);
            let _ = self.actions.cancel(app, &session_id);
            self.actions.clear_session(&session_id);
            self.result_sessions.lock().remove(&session_id);
            return Err(format!("无法创建结果窗口：{error}"));
        }
        #[cfg(not(target_os = "windows"))]
        self.finish_immediate_result_reveal(&session_id);
        Ok((session_id, request_id, reveal_receiver))
    }

    fn close_unpinned_results(&self, app: &AppHandle) {
        self.close_unpinned_result_sessions(app, self.unpinned_result_session_ids());
    }

    fn unpinned_result_session_ids(&self) -> Vec<String> {
        self.result_sessions
            .lock()
            .iter()
            .filter_map(|(session_id, meta)| {
                should_close_for_external_selection(meta.pinned).then(|| session_id.clone())
            })
            .collect()
    }

    fn close_unpinned_result_sessions(&self, app: &AppHandle, sessions: Vec<String>) {
        for session_id in sessions {
            // A result may have become pinned, closed, or replaced while the
            // delayed selection cleanup was waiting for the UI thread.
            let still_unpinned = self
                .result_sessions
                .lock()
                .get(&session_id)
                .is_some_and(|meta| should_close_for_external_selection(meta.pinned));
            if !still_unpinned {
                continue;
            }
            let label = result_label(&session_id);
            self.persist_result_size_now(app, &label);
            if self.windows.close_result(app, &label).is_ok() {
                self.cleanup_result_session(app, &session_id);
            }
        }
    }

    fn begin_result_reveal_commit(&self, session_id: &str) -> Result<bool, String> {
        let mut reveals = self.result_reveals.lock();
        let handshake = reveals
            .get_mut(session_id)
            .ok_or_else(|| "结果显示会话已结束".to_owned())?;
        handshake
            .phase
            .begin_commit()
            .map_err(|()| "结果窗口初始化已经失败".to_owned())
    }

    /// Resolves the toolbar's `run_action` call only after the native window
    /// commit has completed. A false return means a timeout/cleanup won the
    /// race and the caller must close any window it just made visible.
    fn finish_result_reveal_commit(&self, session_id: &str, result: Result<(), String>) -> bool {
        let succeeded = result.is_ok();
        let sender = {
            let mut reveals = self.result_reveals.lock();
            let Some(handshake) = reveals.get_mut(session_id) else {
                return false;
            };
            if !handshake.phase.finish_commit(succeeded) {
                return handshake.phase == ResultRevealHandshakePhase::Committed;
            }
            handshake.sender.take()
        };
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
        true
    }

    #[cfg(not(target_os = "windows"))]
    fn finish_immediate_result_reveal(&self, session_id: &str) {
        let sender = {
            let mut reveals = self.result_reveals.lock();
            let Some(handshake) = reveals.get_mut(session_id) else {
                return;
            };
            handshake.phase = ResultRevealHandshakePhase::Committed;
            handshake.sender.take()
        };
        if let Some(sender) = sender {
            let _ = sender.send(Ok(()));
        }
    }

    fn fail_result_reveal_handshake(&self, session_id: &str, message: String) -> bool {
        let sender = {
            let mut reveals = self.result_reveals.lock();
            let Some(handshake) = reveals.get_mut(session_id) else {
                return false;
            };
            if handshake.phase == ResultRevealHandshakePhase::Committed {
                return true;
            }
            handshake.phase = ResultRevealHandshakePhase::Failed;
            handshake.sender.take()
        };
        if let Some(sender) = sender {
            let _ = sender.send(Err(message));
        }
        true
    }

    fn abort_result_reveal(&self, app: &AppHandle, session_id: &str) {
        self.result_reveals.lock().remove(session_id);
        let _ = self.actions.cancel(app, session_id);
        let _ = self.windows.close_result(app, &result_label(session_id));
        self.cleanup_result_session(app, session_id);
    }

    pub fn cleanup_result_session(&self, app: &AppHandle, session_id: &str) {
        self.fail_result_reveal_handshake(
            session_id,
            "结果窗口在完成显示前被关闭，请重试".to_owned(),
        );
        self.result_reveals.lock().remove(session_id);
        self.actions.clear_session(session_id);
        // Closing a translate/explain/etc. window often coincides with an
        // outside click. The presented-selection identity keeps that click from
        // re-presenting the original text.
        // Host highlight clear is deferred: a synchronous AX/UIA write on the
        // same gesture races the next drag and can drop the capture.
        if let Some(meta) = self.result_sessions.lock().remove(session_id) {
            let text = meta.selection.payload.text.clone();
            if let Some(args) = host_selection_clear_args(
                &text,
                Some(meta.selection.payload.source_app.bundle_id.as_str()),
            ) {
                self.schedule_host_selection_clear(app.clone(), args);
            }
        }
    }

    pub fn handle_window_event(&self, app: &AppHandle, label: &str, event: &WindowEvent) {
        if label == TOOLBAR_LABEL {
            // A renderer-initiated rebuild destroys the singleton toolbar; the
            // Destroyed event recreates it so the replacement renderer can
            // replay the in-memory selection through `toolbar_ready`.
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            if matches!(event, WindowEvent::Destroyed)
                && !self.shutting_down.load(Ordering::Acquire)
                && self.windows.take_toolbar_recovery_pending()
                && self.windows.ensure_toolbar(app).is_err()
            {
                eprintln!("[toolbar] stage=recreate error_class=window");
            }
            return;
        }
        if label == SETTINGS_LABEL {
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    if self.shutting_down.load(Ordering::Acquire) {
                        return;
                    }
                    api.prevent_close();
                    #[cfg(target_os = "windows")]
                    match settings_close_action(
                        self.settings.get_settings().application.close_behavior,
                        self.settings_renderer_ready.load(Ordering::Acquire),
                    ) {
                        SettingsCloseAction::HideToTray => {
                            if let Some(window) = app.get_webview_window(label) {
                                let _ = window.hide();
                            }
                        }
                        SettingsCloseAction::RequestQuitConfirmation => {
                            if !request_settings_close_confirmation(app) {
                                quit_application(app);
                            }
                        }
                        SettingsCloseAction::QuitImmediately => quit_application(app),
                    }
                    #[cfg(not(target_os = "windows"))]
                    if let Some(window) = app.get_webview_window(label) {
                        let _ = window.hide();
                    }
                }
                WindowEvent::Destroyed => {
                    self.settings_renderer_ready.store(false, Ordering::Release);
                }
                _ => {}
            }
            return;
        }
        let Some(session_id) = session_from_result_label(label) else {
            return;
        };
        match event {
            WindowEvent::Focused(focused) => {
                self.windows.on_focus_changed(app, label, *focused);
            }
            WindowEvent::Resized(size) => {
                if self.windows.note_result_resize(label) {
                    if let Some(window) = app.get_webview_window(label) {
                        if let Ok(scale) = window.scale_factor() {
                            let logical_size = SettingsWindowSize {
                                width: size.width as f64 / scale,
                                height: size.height as f64 / scale,
                            };
                            *self.latest_result_size.lock() = Some(logical_size);
                            let revision = self
                                .resize_revision
                                .fetch_add(1, Ordering::AcqRel)
                                .wrapping_add(1);
                            self.pending_result_sizes.lock().insert(
                                session_id.clone(),
                                PendingResultSize {
                                    revision,
                                    size: logical_size,
                                },
                            );
                            self.schedule_resize_persist(app.clone(), session_id, revision);
                        }
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.windows.ignore_resize_temporarily(label);
            }
            WindowEvent::CloseRequested { .. } => {
                self.persist_result_size_now(app, label);
            }
            WindowEvent::Destroyed => {
                self.persist_result_size_now(app, label);
                // Snapshot original selection text before session cleanup so we
                // can drop a same-text live selection that a concurrent
                // mouse-up may have just re-captured.
                // Same-text suppress is armed once in `cleanup_result_session`
                // (not here) so Destroyed + cleanup do not extend the 2s window.
                let result_text = self
                    .result_sessions
                    .lock()
                    .get(&session_id)
                    .map(|meta| meta.selection.payload.text.clone());
                if let Some(ref text) = result_text {
                    let mut current = self.current_selection.lock();
                    if current
                        .as_ref()
                        .is_some_and(|selection| selection.payload.text == *text)
                    {
                        *current = None;
                    }
                }
                if let Some(selection_id) = self.clear_current_selection_from_result(&session_id) {
                    self.hide_toolbar_for_selection(app, &selection_id);
                }
                // If nothing else owns a live selection, force-hide any leftover toolbar.
                if self.current_selection.lock().is_none() {
                    self.windows.hide_toolbar(app);
                }
                self.windows.remove_result(label);
                self.cleanup_result_session(app, &session_id);
            }
            _ => {}
        }
    }

    fn persist_result_size_now(&self, app: &AppHandle, label: &str) {
        let Some(session_id) = session_from_result_label(label) else {
            return;
        };
        self.persist_pending_result_size(app, &session_id, None);
    }

    fn persist_pending_result_size(
        &self,
        app: &AppHandle,
        session_id: &str,
        expected_revision: Option<u64>,
    ) {
        let Some(pending) = self.pending_result_sizes.lock().get(session_id).copied() else {
            return;
        };
        if expected_revision.is_some_and(|revision| revision != pending.revision) {
            return;
        }
        if pending.revision != self.resize_revision.load(Ordering::Acquire) {
            let mut sizes = self.pending_result_sizes.lock();
            if sizes
                .get(session_id)
                .is_some_and(|current| current.revision == pending.revision)
            {
                sizes.remove(session_id);
            }
            return;
        }
        // The transaction holds the settings write lock across an fsync, so
        // keep it off the main thread even when a close/destroy event hands
        // us here, and off shared async workers via the blocking pool.
        let settings = self.settings.clone();
        let app_handle = app.clone();
        let tracked_session = session_id.to_owned();
        let tracked_revision = pending.revision;
        tauri::async_runtime::spawn(async move {
            let result = tauri::async_runtime::spawn_blocking(move || {
                settings
                    .update_result_last_size(pending.size)
                    .map_err(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|join_error| Err(format!("设置任务调度失败：{join_error}")));
            let settle_session = tracked_session.clone();
            let app_for_main = app_handle.clone();
            let _ = app_handle.run_on_main_thread(move || {
                app_for_main
                    .state::<RuntimeState>()
                    .settle_result_size_persist(
                        &app_for_main,
                        &settle_session,
                        tracked_revision,
                        result,
                    );
            });
        });
    }

    fn settle_result_size_persist(
        &self,
        app: &AppHandle,
        session_id: &str,
        revision: u64,
        result: Result<PublicSettings, String>,
    ) {
        {
            let mut sizes = self.pending_result_sizes.lock();
            // A newer resize supersedes this write regardless of its outcome;
            // without removal a failed transaction would leak the entry until
            // process exit.
            if !sizes
                .get(session_id)
                .is_some_and(|current| current.revision == revision)
            {
                return;
            }
            sizes.remove(session_id);
        }
        match result {
            Ok(public) => {
                let _ = app.emit(SETTINGS_CHANGED_EVENT, public);
            }
            Err(error) => {
                eprintln!("[TextLens] 结果窗尺寸持久化失败：{error}");
            }
        }
    }

    fn schedule_resize_persist(&self, app: AppHandle, session_id: String, revision: u64) {
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(RESIZE_PERSIST_DELAY_MS)).await;
            let state = app.state::<RuntimeState>();
            state.persist_pending_result_size(&app, &session_id, Some(revision));
        });
    }
}

async fn wait_for_result_reveal_with_timeout(
    receiver: ResultRevealReceiver,
    timeout: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("结果窗口在显示前已关闭，请重试".to_owned()),
        Err(_) => Err("结果窗口初始化超时，请重试".to_owned()),
    }
}

pub fn spawn_selection_loop(
    app: AppHandle,
    receiver: SelectionEventReceiver,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("textlens-events".to_owned())
        .spawn(move || selection_loop(app, receiver))
        .map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionLoopStep {
    Continue,
    EventPanicked,
    StopForShutdown,
    StopForDisconnect,
}

fn handle_selection_loop_receive<T>(
    shutting_down: bool,
    received: Result<T, RecvTimeoutError>,
    process: impl FnOnce(T),
) -> SelectionLoopStep {
    if shutting_down {
        return SelectionLoopStep::StopForShutdown;
    }
    match received {
        Ok(event) => match catch_unwind(AssertUnwindSafe(|| process(event))) {
            Ok(()) => SelectionLoopStep::Continue,
            Err(_) => SelectionLoopStep::EventPanicked,
        },
        Err(RecvTimeoutError::Timeout) => SelectionLoopStep::Continue,
        Err(RecvTimeoutError::Disconnected) => SelectionLoopStep::StopForDisconnect,
    }
}

fn selection_loop(app: AppHandle, receiver: Receiver<SelectionEvent>) {
    loop {
        let state = app.state::<RuntimeState>();
        if state.shutting_down.load(Ordering::Acquire) {
            break;
        }
        let received = receiver.recv_timeout(Duration::from_millis(200));
        let shutting_down = state.shutting_down.load(Ordering::Acquire);
        match handle_selection_loop_receive(shutting_down, received, |event| {
            state.process_selection_event(&app, event);
        }) {
            SelectionLoopStep::Continue => {}
            SelectionLoopStep::EventPanicked => {
                eprintln!("[selection] event handler panicked; event discarded");
            }
            SelectionLoopStep::StopForShutdown => break,
            SelectionLoopStep::StopForDisconnect => {
                state.handle_selection_receiver_disconnected(&app);
                break;
            }
        }
    }
}

/// While untrusted (typical first run), poll quickly so granting Accessibility
/// in System Settings enables selection capture almost immediately. Once
/// trusted, the poll only detects revocation, so the original slower cadence
/// is enough. Windows is always trusted and keeps the trusted cadence.
const PERMISSION_POLL_TRUSTED_MS: u64 = 2_000;
const PERMISSION_POLL_UNTRUSTED_MS: u64 = 600;

fn permission_poll_interval(trusted: bool) -> Duration {
    if trusted {
        Duration::from_millis(PERMISSION_POLL_TRUSTED_MS)
    } else {
        Duration::from_millis(PERMISSION_POLL_UNTRUSTED_MS)
    }
}

pub fn start_permission_poll(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let trusted = SelectionMonitor::is_accessibility_trusted();
            tokio::time::sleep(permission_poll_interval(trusted)).await;
            let state = app.state::<RuntimeState>();
            if state.shutting_down.load(Ordering::Acquire) {
                break;
            }
            state.poll_permission(&app);
        }
    });
}

pub fn handle_global_shortcut(app: &AppHandle, shortcut: &Shortcut, event: ShortcutEvent) {
    if event.state != ShortcutState::Pressed || event.id != shortcut.id() {
        return;
    }
    let state = app.state::<RuntimeState>();
    if state.shutting_down.load(Ordering::Acquire) {
        return;
    }
    let registered = *state.registered_shortcut.lock();
    if registered != Some(*shortcut) {
        return;
    }
    let settings = state.settings.get_settings();
    if shortcut_event_is_current(
        *shortcut,
        event,
        registered,
        settings.enabled,
        settings.trigger.mode,
        settings.capture_shortcut.as_str(),
    ) {
        state.capture_current(app);
    }
}

pub fn setup_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let settings = app.state::<RuntimeState>().settings.get_public_settings()?;
    let menu = build_tray_menu(app, &settings)?;
    #[cfg(target_os = "macos")]
    let tray_icon = tauri::image::Image::from_bytes(include_bytes!(
        "../../apps/macos/icons/tray-template.png"
    ))?;
    #[cfg(not(target_os = "macos"))]
    let tray_icon =
        tauri::image::Image::from_bytes(include_bytes!("../../apps/windows/icons/32x32.png"))?;
    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("TextLens")
        .icon(tray_icon)
        .show_menu_on_left_click(true)
        .on_menu_event(handle_tray_menu_event);
    #[cfg(target_os = "macos")]
    let tray = tray.icon_as_template(true);
    tray.build(app)?;
    Ok(())
}

fn build_tray_menu(
    app: &AppHandle,
    settings: &PublicSettings,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let toggle = CheckMenuItemBuilder::with_id(TRAY_TOGGLE_ID, "启用划词")
        .checked(settings.enabled)
        .build(app)?;
    #[cfg(not(target_os = "windows"))]
    let accessibility = app.state::<RuntimeState>().accessibility_status();
    #[cfg(not(target_os = "windows"))]
    let (permission_label, permission_enabled) = match accessibility.platform {
        "darwin" if accessibility.available => ("辅助功能：已授权", false),
        "darwin" if accessibility.trusted => ("辅助功能：划词监听不可用", false),
        "darwin" => ("辅助功能：需要授权", true),
        _ => ("选区访问：不受支持", false),
    };
    #[cfg(not(target_os = "windows"))]
    let permission = MenuItemBuilder::with_id(TRAY_PERMISSION_ID, permission_label)
        .enabled(permission_enabled)
        .build(app)?;
    let open_settings = MenuItemBuilder::with_id(TRAY_SETTINGS_ID, "打开设置…").build(app)?;
    let version = MenuItemBuilder::new(format!("TextLens {}", display_version(app)))
        .enabled(false)
        .build(app)?;
    let quit = MenuItemBuilder::with_id(TRAY_QUIT_ID, "退出 TextLens").build(app)?;
    let menu = MenuBuilder::new(app).item(&toggle);
    #[cfg(not(target_os = "windows"))]
    let menu = menu.item(&permission);
    menu.separator()
        .item(&open_settings)
        .item(&version)
        .separator()
        .item(&quit)
        .build()
}

/// The menu must show the installed bundle version, not the Cargo semver:
/// four-part installer versions (0.4.2.3) are applied to Info.plist after the
/// Tauri build, so `package_info()` would still report the three-part base.
fn display_version(app: &AppHandle) -> String {
    #[cfg(target_os = "macos")]
    {
        use core_foundation::{bundle::CFBundle, string::CFString};
        let dictionary = CFBundle::main_bundle().info_dictionary();
        let identifier = dictionary
            .find(&CFString::from_static_string("CFBundleIdentifier"))
            .and_then(|value| value.downcast::<CFString>())
            .map(|value| value.to_string());
        if identifier.as_deref() == Some(app.config().identifier.as_str()) {
            if let Some(version) = dictionary
                .find(&CFString::from_static_string("CFBundleShortVersionString"))
                .and_then(|value| value.downcast::<CFString>())
                .map(|value| value.to_string())
                .filter(|value| !value.trim().is_empty())
            {
                return version;
            }
        }
    }
    app.package_info().version.to_string()
}

fn refresh_tray(app: &AppHandle, settings: &PublicSettings) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Ok(menu) = build_tray_menu(app, settings) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn handle_tray_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        TRAY_TOGGLE_ID => {
            let state = app.state::<RuntimeState>();
            let enabled = !state.settings.get_settings().enabled;
            if let Ok(public) = state.settings.update(SettingsUpdate {
                enabled: Some(enabled),
                ..Default::default()
            }) {
                state.after_settings_changed(app, &public);
            }
        }
        TRAY_PERMISSION_ID => {
            if cfg!(target_os = "macos") {
                request_accessibility_internal(app);
            }
        }
        TRAY_SETTINGS_ID => open_settings_window(app),
        // Keep the tray command as a reliable native escape hatch even if the
        // settings renderer is unhealthy. The settings-page button and the
        // close-behavior confirmation eventually enter this same controller
        // through `quit_app`.
        TRAY_QUIT_ID => quit_application(app),
        _ => {}
    }
}

pub fn open_settings_window(app: &AppHandle) {
    open_settings_window_with_options(app, None);
}

pub fn open_settings_window_with_options(app: &AppHandle, options: Option<OpenSettingsOptions>) {
    if let Some(window) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    let Some(options) = options else {
        return;
    };
    let has_guidance = options
        .focus
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || options
            .notice
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty());
    if !has_guidance {
        return;
    }
    let guidance = SettingsGuidance {
        focus: options
            .focus
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        notice: options
            .notice
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    };
    if let Some(state) = app.try_state::<RuntimeState>() {
        *state.pending_settings_guidance.lock() = Some(guidance.clone());
    }
    let _ = app.emit_to(SETTINGS_LABEL, SETTINGS_GUIDANCE_EVENT, guidance);
}

#[tauri::command]
pub fn open_settings(
    app: AppHandle,
    window: WebviewWindow,
    options: Option<OpenSettingsOptions>,
) -> Result<(), String> {
    // Settings is a shared, long-lived workspace. Result windows, the
    // toolbar, and the settings window itself may all bring it forward; an
    // unknown WebView must still be rejected.
    ensure_known_caller(&window)?;
    open_settings_window_with_options(&app, options);
    Ok(())
}

#[tauri::command]
pub fn take_settings_guidance(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
) -> Result<Option<SettingsGuidance>, String> {
    ensure_settings_caller(&window)?;
    Ok(state.pending_settings_guidance.lock().take())
}

#[cfg(target_os = "windows")]
fn request_settings_close_confirmation(app: &AppHandle) -> bool {
    app.emit_to(SETTINGS_LABEL, SETTINGS_CLOSE_REQUEST_EVENT, ())
        .is_ok()
}

fn quit_application(app: &AppHandle) {
    app.state::<RuntimeState>().shutdown(app);
    app.exit(0);
}

fn request_accessibility_internal(app: &AppHandle) -> AccessibilityStatus {
    let trusted = SelectionMonitor::request_accessibility();
    #[cfg(not(target_os = "macos"))]
    let _ = trusted;
    #[cfg(target_os = "macos")]
    if !trusted {
        let _ = open_system_url(
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        );
    }
    let state = app.state::<RuntimeState>();
    state.reconcile_capture(app);
    state.accessibility_status()
}

fn selection_monitor_diagnostic(should_listen: bool, start_failed: bool) -> Option<String> {
    (should_listen && start_failed).then(|| SELECTION_MONITOR_START_ERROR.to_owned())
}

fn trace_selection_timing(stage: &str, started: Instant) {
    let enabled = std::env::var_os("TEXTLENS_SELECTION_TRACE").is_some_and(|value| {
        let value = value.to_string_lossy();
        value == "1" || value.eq_ignore_ascii_case("true")
    });
    if enabled {
        eprintln!(
            "[selection-timing] stage={stage} duration_ms={:.1}",
            started.elapsed().as_secs_f64() * 1_000.0
        );
    }
}

#[cfg(test)]
fn selection_monitor_disconnect_diagnostic(shutting_down: bool) -> Option<String> {
    (!shutting_down).then(|| SELECTION_MONITOR_DISCONNECTED_ERROR.to_owned())
}

fn shortcut_registration_diagnostic(registration_failed: bool) -> Option<String> {
    registration_failed.then(|| GLOBAL_SHORTCUT_ERROR.to_owned())
}

fn shortcut_registration_target(enabled: bool, mode: TriggerMode, configured: &str) -> String {
    if enabled && mode == TriggerMode::Shortcut {
        configured.trim().to_owned()
    } else {
        String::new()
    }
}

fn shortcut_event_is_current(
    reported: Shortcut,
    event: ShortcutEvent,
    registered: Option<Shortcut>,
    enabled: bool,
    mode: TriggerMode,
    configured: &str,
) -> bool {
    if event.state != ShortcutState::Pressed
        || event.id != reported.id()
        || registered != Some(reported)
    {
        return false;
    }

    enabled
        && mode == TriggerMode::Shortcut
        && configured.trim().parse::<Shortcut>().ok() == Some(reported)
}

/// Automatic selection events only show the toolbar in "selected" trigger mode.
/// Shortcut-driven capture always forces presentation via `force_capture`.
fn should_present_selection_for_trigger(force_capture: bool, mode: TriggerMode) -> bool {
    force_capture || mode == TriggerMode::Selected
}

fn selection_access_available(
    platform: &str,
    trusted: bool,
    diagnostics: &RuntimeDiagnostics,
) -> bool {
    platform != "unsupported" && trusted && diagnostics.selection_monitor_error.is_none()
}

fn accessibility_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "unsupported"
    }
}

#[tauri::command]
pub async fn get_settings(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
) -> Result<PublicSettings, String> {
    ensure_known_caller(&window)?;
    let settings = state.settings.clone();
    run_repository(move || settings.get_public_settings()).await
}

#[tauri::command]
pub fn settings_ready(window: WebviewWindow, state: State<'_, RuntimeState>) -> Result<(), String> {
    ensure_settings_caller(&window)?;
    state.settings_renderer_ready.store(true, Ordering::Release);
    Ok(())
}

/// Runs an operation on the main thread and awaits its result. Settings
/// commands are async so disk IO runs on the blocking pool, but global
/// shortcut registration and window-facing broadcasts keep their original
/// main-thread execution context through this hop. Awaiting a oneshot instead
/// of blocking a shared async worker keeps unrelated commands responsive.
async fn run_on_main_thread<T, F>(app: &AppHandle, operation: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    app.run_on_main_thread(move || {
        let _ignored = sender.send(operation());
    })
    .map_err(|error| error.to_string())?;
    match tokio::time::timeout(MAIN_THREAD_HOP_TIMEOUT, receiver).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err("主线程调度器已退出".to_owned()),
        Err(_) => Err("主线程调度超时".to_owned()),
    }
}

/// Runs repository work on the blocking pool. Transactions hold the settings
/// write lock across an fsync, so they must never occupy the main thread or a
/// shared async worker.
async fn run_repository<T, F>(operation: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, SettingsError> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|error| format!("设置任务调度失败：{error}"))?
        .map_err(|error| error.to_string())
}

async fn broadcast_settings_changed(
    app: &AppHandle,
    public: &PublicSettings,
) -> Result<(), String> {
    let app_for_hop = app.clone();
    let app_for_main = app.clone();
    let public = public.clone();
    run_on_main_thread(&app_for_hop, move || {
        app_for_main
            .state::<RuntimeState>()
            .after_settings_changed(&app_for_main, &public);
    })
    .await
}

/// Commits a repository transaction and broadcasts the resulting snapshot on
/// the main thread. Every settings mutation goes through here so a new command
/// cannot forget the broadcast.
async fn persist_settings_change<F>(
    app: &AppHandle,
    repository: Arc<SettingsRepository>,
    operation: F,
) -> Result<PublicSettings, String>
where
    F: FnOnce(&SettingsRepository) -> Result<PublicSettings, SettingsError> + Send + 'static,
{
    let public = run_repository(move || operation(&repository)).await?;
    broadcast_settings_changed(app, &public).await?;
    Ok(public)
}

#[tauri::command]
pub async fn update_settings(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    update: SettingsUpdate,
) -> Result<PublicSettings, String> {
    ensure_settings_caller(&window)?;
    // Async commands can overlap: serialize the shortcut switch, durable write
    // and rollback so two saves cannot leave the registered hotkey and the
    // persisted setting disagreeing.
    let _settings_guard = state.settings_mutation.lock().await;
    let shortcut_change_requested =
        update.enabled.is_some() || update.capture_shortcut.is_some() || update.trigger.is_some();
    let previous_shortcut = if shortcut_change_requested {
        let current = state.settings.get_settings();
        let enabled = update.enabled.unwrap_or(current.enabled);
        let configured = update
            .capture_shortcut
            .as_deref()
            .unwrap_or(current.capture_shortcut.as_str());
        let mode = update
            .trigger
            .as_ref()
            .map_or(current.trigger.mode, |trigger| trigger.mode);
        let requested = shortcut_registration_target(enabled, mode, configured);
        let app_for_shortcut = app.clone();
        Some(
            run_on_main_thread(&app, move || {
                app_for_shortcut
                    .state::<RuntimeState>()
                    .switch_shortcut(&app_for_shortcut, &requested)
            })
            .await??,
        )
    } else {
        None
    };
    let settings = state.settings.clone();
    let public = match run_repository(move || settings.update(update)).await {
        Ok(public) => public,
        Err(error) => {
            if let Some(previous) = previous_shortcut {
                let app_for_rollback = app.clone();
                if let Err(rollback_error) = run_on_main_thread(&app, move || {
                    app_for_rollback
                        .state::<RuntimeState>()
                        .switch_shortcut(&app_for_rollback, &previous)
                })
                .await
                {
                    // The registration is now neither the requested nor the
                    // previous shortcut while the settings file still holds
                    // the previous value. Surface the loss instead of letting
                    // the capture hotkey disappear silently.
                    let _ = app.emit(SHORTCUT_ERROR_EVENT, rollback_error.clone());
                    return Err(format!(
                        "{error}；捕获快捷键恢复失败（{rollback_error}），请重新在设置中保存快捷键"
                    ));
                }
            }
            return Err(error);
        }
    };
    broadcast_settings_changed(&app, &public).await?;
    Ok(public)
}

#[tauri::command]
pub async fn reset_result_size(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
) -> Result<PublicSettings, String> {
    ensure_settings_caller(&window)?;
    // Reset is deliberately separate from the ordinary settings form update:
    // this removes the ambiguity between an explicit reset and a stale draft.
    state.pending_result_sizes.lock().clear();
    *state.latest_result_size.lock() = None;
    persist_settings_change(&app, state.settings.clone(), |repository| {
        repository.clear_result_last_size()
    })
    .await
}

#[tauri::command]
pub async fn create_provider(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    input: CreateProviderInput,
) -> Result<PublicSettings, String> {
    ensure_settings_caller(&window)?;
    persist_settings_change(&app, state.settings.clone(), move |repository| {
        repository.create_provider(input)
    })
    .await
}

#[tauri::command]
pub async fn update_provider(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
    update: UpdateProviderInput,
) -> Result<PublicSettings, String> {
    ensure_settings_caller(&window)?;
    persist_settings_change(&app, state.settings.clone(), move |repository| {
        repository.update_provider(&provider_id, update)
    })
    .await
}

#[tauri::command]
pub async fn delete_provider(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
) -> Result<PublicSettings, String> {
    ensure_settings_caller(&window)?;
    persist_settings_change(&app, state.settings.clone(), move |repository| {
        repository.delete_provider(&provider_id)
    })
    .await
}

#[tauri::command]
pub async fn set_provider_api_key(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
    api_key: String,
) -> Result<PublicSettings, String> {
    ensure_settings_caller(&window)?;
    persist_settings_change(&app, state.settings.clone(), move |repository| {
        repository.set_provider_api_key(&provider_id, &api_key)
    })
    .await
}

#[tauri::command]
pub async fn clear_provider_api_key(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
) -> Result<PublicSettings, String> {
    ensure_settings_caller(&window)?;
    persist_settings_change(&app, state.settings.clone(), move |repository| {
        repository.clear_provider_api_key(&provider_id)
    })
    .await
}

/// Settings-window only: reveal a saved provider API key for display/editing.
/// Keys are intentionally never included in `get_settings` / PublicSettings.
#[tauri::command]
pub async fn get_provider_api_key(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
) -> Result<Option<String>, String> {
    ensure_settings_caller(&window)?;
    let settings = state.settings.clone();
    run_repository(move || settings.get_api_key(&provider_id)).await
}

#[tauri::command]
pub async fn test_provider_connection(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
) -> Result<ConnectionTestResult, String> {
    ensure_settings_caller(&window)?;
    Ok(state.actions.test_provider_connection(&provider_id).await)
}

#[tauri::command]
pub async fn list_provider_models(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
) -> Result<ConnectionTestResult, String> {
    ensure_settings_caller(&window)?;
    Ok(state.actions.list_provider_models(&provider_id).await)
}

#[tauri::command]
pub async fn sync_provider_models(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    provider_id: String,
) -> Result<SyncModelsResult, String> {
    ensure_settings_caller(&window)?;
    let result = state.actions.sync_provider_models(&provider_id).await;
    if let Some(settings) = &result.settings {
        // Keep the tray/window-facing side effects on the main thread; this
        // command runs on the async runtime now.
        if let Err(error) = broadcast_settings_changed(&app, settings).await {
            eprintln!("[TextLens] 模型同步后的设置广播失败：{error}");
        }
    }
    Ok(result)
}

#[tauri::command]
pub fn get_accessibility_status(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
) -> Result<AccessibilityStatus, String> {
    ensure_settings_caller(&window)?;
    Ok(state.accessibility_status())
}

#[tauri::command]
pub fn request_accessibility(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<AccessibilityStatus, String> {
    ensure_settings_caller(&window)?;
    Ok(request_accessibility_internal(&app))
}

#[tauri::command]
pub fn quit_app(app: AppHandle, window: WebviewWindow) -> Result<(), String> {
    ensure_settings_caller(&window)?;
    quit_application(&app);
    Ok(())
}

#[tauri::command]
pub fn toolbar_ready(
    // Used on macOS/Windows to restage the toolbar when the renderer
    // reconnects (e.g. after a WebView2 rebuild on Windows, or a rare WKWebView
    // reload on macOS).
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "macos")),
        allow(unused_variables)
    )]
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
) -> Result<Option<RendererSelectionPayload>, String> {
    ensure_toolbar_caller(&window)?;
    let current = state.current_selection.lock().clone();
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Some(selection) = &current {
        let (anchor, placement) = selection_toolbar_anchor(&selection.payload);
        if state
            .windows
            .stage_toolbar_with_placement(&app, &selection.id, anchor, placement)
            .is_err()
        {
            // Do not discard the replay payload: the renderer can still use
            // the selection ID to retry presentation or request a rebuild.
            eprintln!(
                "[toolbar] stage=replay selection_id={} error_class=window",
                selection.id
            );
        }
    }
    Ok(current
        .as_ref()
        .map(|selection| RendererSelectionPayload::new(&selection.id, &selection.payload)))
}

fn toolbar_commit_matches_selection(current_id: Option<&str>, requested_id: &str) -> bool {
    !requested_id.is_empty() && current_id == Some(requested_id)
}

#[tauri::command]
pub fn recover_toolbar(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    selection_id: String,
) -> Result<bool, String> {
    ensure_toolbar_caller(&window)?;
    if selection_id.is_empty() || selection_id.len() > 128 {
        return Err("Invalid toolbar recovery parameters".to_owned());
    }
    let matches = {
        let current = state.current_selection.lock();
        toolbar_commit_matches_selection(
            current.as_ref().map(|selection| selection.id.as_str()),
            &selection_id,
        )
    };
    if !matches {
        return Ok(false);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let rebuilt = state
            .windows
            .rebuild_toolbar(&app, &selection_id)
            .map_err(|_| {
                eprintln!(
                    "[toolbar] stage=rebuild selection_id={} error_class=window",
                    selection_id
                );
                "Unable to rebuild selection toolbar".to_owned()
            })?;
        Ok(rebuilt)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = app;
        Ok(false)
    }
}

#[tauri::command]
pub fn present_toolbar(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    selection_id: String,
    size: ToolbarSize,
) -> Result<bool, String> {
    ensure_toolbar_caller(&window)?;
    if selection_id.len() > 128 || !size.width.is_finite() || !size.height.is_finite() {
        return Err("Invalid toolbar presentation parameters".to_owned());
    }
    let matches = {
        let current = state.current_selection.lock();
        toolbar_commit_matches_selection(
            current.as_ref().map(|selection| selection.id.as_str()),
            &selection_id,
        )
    };
    if !matches {
        return Ok(false);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let presented = state
        .windows
        .present_toolbar(
            &app,
            &selection_id,
            WindowSize {
                width: size.width,
                height: size.height,
            },
        )
        .map_err(|error| error.to_string())?;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let presented = state
        .windows
        .update_toolbar_size(
            &app,
            Some(&selection_id),
            WindowSize {
                width: size.width,
                height: size.height,
            },
        )
        .map_err(|error| error.to_string())?;
    if !presented {
        return Ok(false);
    }

    // A global dismiss or a newer selection may win while the native UI
    // operation is queued. Never leave that stale toolbar visible.
    let still_matches = {
        let current = state.current_selection.lock();
        toolbar_commit_matches_selection(
            current.as_ref().map(|selection| selection.id.as_str()),
            &selection_id,
        )
    };
    if !still_matches {
        let _ = state.windows.hide_toolbar_if_selection(&app, &selection_id);
    }
    Ok(still_matches)
}

#[tauri::command]
pub async fn run_action(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    action_id: String,
    cursor: Option<CursorPoint>,
    selection_id: Option<String>,
    search_engine_id: Option<String>,
) -> Result<RunActionResult, String> {
    if let Err(message) = ensure_toolbar_caller(&window) {
        return Ok(RunActionResult::rejected(message));
    }
    if state.shutting_down.load(Ordering::Acquire) {
        return Ok(RunActionResult::rejected("TextLens 正在退出"));
    }
    let Some(selection) = state.current_selection.lock().clone() else {
        return Ok(RunActionResult::rejected("当前没有可用的选中文本"));
    };
    if selection_id.as_ref().is_some_and(|value| value.len() > 128) {
        return Ok(RunActionResult::rejected("选区标识无效"));
    }
    if selection_id.as_deref() != Some(selection.id.as_str()) {
        return Ok(RunActionResult::rejected("选区已发生变化，请重新选择文字"));
    }
    let settings = state.settings.get_settings();
    let Some(action) = settings
        .actions
        .iter()
        .find(|candidate| candidate.id == action_id && candidate.enabled)
        .cloned()
    else {
        return Ok(RunActionResult::rejected("动作不存在或已停用"));
    };

    let selection_token = selection.id.clone();
    if !state
        .consuming_selection_ids
        .lock()
        .insert(selection_token.clone())
    {
        return Ok(RunActionResult::rejected("该选区动作正在启动，请稍候"));
    }
    let _consuming = SelectionConsumptionGuard {
        set: &state.consuming_selection_ids,
        id: selection_token.clone(),
    };

    let result = match action.kind {
        ActionKind::Copy => match clipboard::write_text(&selection.payload.text) {
            Ok(()) => {
                // Keep the toolbar for the success icon. A later click cannot
                // re-present this selection: its identity is already recorded.
                // Quietly return key focus to the source app (no ActivateAllWindows
                // flash). Next outside click then deselects + dismisses in one step.
                state.schedule_selection_host_clear(&app, &selection);
                let _ =
                    restore_source_app_activation(&app, &selection.payload.source_app.bundle_id);
                RunActionResult::accepted(None, None)
            }
            Err(message) => RunActionResult::rejected(message),
        },
        ActionKind::Search => {
            let template = match crate::models::resolve_search_template(
                action.search_engine_id.as_deref(),
                search_engine_id.as_deref(),
            ) {
                Ok(value) => value,
                Err(message) => return Ok(RunActionResult::rejected(message)),
            };
            match resolve_search_target(&selection.payload.text, template) {
                Ok(target) => match open_system_url(&target) {
                    Ok(()) => {
                        state.schedule_selection_host_clear(&app, &selection);
                        state.clear_and_hide_current_selection_if(&app, &selection_token);
                        RunActionResult::accepted(None, None)
                    }
                    Err(_) => RunActionResult::rejected("无法打开浏览器"),
                },
                Err(message) => RunActionResult::rejected(message),
            }
        }
        _ if action.kind.opens_result_without_generation() => {
            let cursor = action_result_cursor(&app, cursor, &selection.payload);
            state.schedule_selection_host_clear(&app, &selection);
            match state.create_ask_result_session(&app, &action.id, &action.name, selection, cursor)
            {
                Ok((session_id, request_id, reveal_receiver)) => {
                    match wait_for_result_reveal_with_timeout(
                        reveal_receiver,
                        RESULT_REVEAL_TIMEOUT,
                    )
                    .await
                    {
                        Ok(()) => {
                            state.clear_and_hide_current_selection_if(&app, &selection_token);
                            // Do NOT restore_source_app_activation here: default
                            // result dismiss is Blur; activating the source app
                            // immediately steals focus and closes the new window.
                            RunActionResult::accepted(Some(session_id), Some(request_id))
                        }
                        Err(message) => {
                            state.abort_result_reveal(&app, &session_id);
                            RunActionResult::rejected(message)
                        }
                    }
                }
                Err(message) => RunActionResult::rejected(message),
            }
        }
        _ if action.kind.is_ai() => {
            let cursor = action_result_cursor(&app, cursor, &selection.payload);
            state.schedule_selection_host_clear(&app, &selection);
            match state.create_result_session(&app, &action.id, &action.name, selection, cursor) {
                Ok((session_id, request_id, reveal_receiver)) => {
                    // Window creation has its own bounded error path. Start
                    // the renderer reveal budget only after a real session
                    // exists so cold WebView2 construction cannot consume the
                    // entire handshake timeout before the renderer can reply.
                    match wait_for_result_reveal_with_timeout(
                        reveal_receiver,
                        RESULT_REVEAL_TIMEOUT,
                    )
                    .await
                    {
                        Ok(()) => {
                            state.clear_and_hide_current_selection_if(&app, &selection_token);
                            // Do NOT restore_source_app_activation here: default
                            // result dismiss is Blur; activating the source app
                            // immediately steals focus and closes the new window.
                            // Copy may soft-restore because it has no result UI.
                            RunActionResult::accepted(Some(session_id), Some(request_id))
                        }
                        Err(message) => {
                            state.abort_result_reveal(&app, &session_id);
                            RunActionResult::rejected(message)
                        }
                    }
                }
                Err(message) => RunActionResult::rejected(message),
            }
        }
        _ => RunActionResult::rejected("暂不支持该动作"),
    };
    Ok(result)
}

#[tauri::command]
pub fn hide_toolbar(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    selection_id: Option<String>,
) -> Result<(), String> {
    ensure_toolbar_caller(&window)?;
    let (should_hide, current_id) = {
        let mut current = state.current_selection.lock();
        let current_id = current.as_ref().map(|selection| selection.id.clone());
        let should_hide =
            should_hide_toolbar_for_selection(current_id.as_deref(), selection_id.as_deref());
        if should_hide
            && (selection_id.is_none() || current_id.as_deref() == selection_id.as_deref())
        {
            *current = None;
        }
        (should_hide, current_id)
    };
    if should_hide {
        if let Some(id) = selection_id.as_deref().or(current_id.as_deref()) {
            let _ = state.windows.hide_toolbar_if_selection(&app, id);
        } else {
            state.windows.hide_toolbar(&app);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn report_toolbar_size(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    size: ToolbarSize,
    selection_id: Option<String>,
) -> Result<(), String> {
    ensure_toolbar_caller(&window)?;
    if selection_id.as_ref().is_some_and(|value| value.len() > 128)
        || !size.width.is_finite()
        || !size.height.is_finite()
    {
        return Err("工具栏尺寸无效".to_owned());
    }
    if let Some(selection_id) = selection_id.as_deref() {
        let current = state.current_selection.lock();
        if !toolbar_commit_matches_selection(
            current.as_ref().map(|selection| selection.id.as_str()),
            selection_id,
        ) {
            return Ok(());
        }
    }
    state
        .windows
        .update_toolbar_size(
            &app,
            selection_id.as_deref(),
            WindowSize {
                width: size.width,
                height: size.height,
            },
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn begin_result_ready(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
) -> Result<ResultSessionSnapshot, String> {
    ensure_result_caller(&window, &session_id)?;
    let meta = state
        .result_sessions
        .lock()
        .get(&session_id)
        .cloned()
        .ok_or_else(|| "结果会话已结束".to_owned())?;
    let begin = state
        .actions
        .begin_ready(&session_id, window.label())
        .map_err(|error| error.to_string())?;
    Ok(compose_result_ready_snapshot(meta, begin))
}

#[tauri::command]
pub fn ack_result_ready(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    ack: ResultReadyAck,
) -> Result<bool, String> {
    ensure_result_caller(&window, &ack.session_id)?;
    Ok(state.actions.ack_ready(&app, ack))
}

#[tauri::command]
pub fn prepare_result_reveal(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
) -> Result<(), String> {
    ensure_result_caller(&window, &session_id)?;
    if !state.result_sessions.lock().contains_key(&session_id) {
        return Err("结果会话已结束".to_owned());
    }
    if let Err(error) = state.windows.prepare_result_reveal(&app, window.label()) {
        let message = format!("无法准备结果窗口：{error}");
        state.fail_result_reveal_handshake(&session_id, message.clone());
        return Err(message);
    }
    Ok(())
}

#[tauri::command]
pub fn commit_result_reveal(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
) -> Result<(), String> {
    ensure_result_caller(&window, &session_id)?;
    if !state.begin_result_reveal_commit(&session_id)? {
        return Ok(());
    }

    let commit = state
        .windows
        .commit_result_reveal(&app, window.label())
        .map(|_| ())
        .map_err(|error| format!("无法显示结果窗口：{error}"));
    let accepted = state.finish_result_reveal_commit(&session_id, commit.clone());
    if !accepted {
        // A 3-second timeout may win while the UI-thread operation is already
        // running. Never leave the resulting stale window visible.
        let _ = state.windows.close_result(&app, window.label());
        return Err("结果显示会话已结束".to_owned());
    }
    commit
}

#[tauri::command]
pub fn fail_result_reveal(
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
    message: String,
) -> Result<(), String> {
    ensure_result_caller(&window, &session_id)?;
    let message = message.trim();
    let message = if message.is_empty() || message.chars().count() > 512 {
        "结果窗口初始化失败".to_owned()
    } else {
        message.to_owned()
    };
    state
        .fail_result_reveal_handshake(&session_id, message)
        .then_some(())
        .ok_or_else(|| "结果显示会话已结束".to_owned())
}

#[tauri::command]
pub fn set_result_pinned(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
    pinned: bool,
) -> Result<bool, String> {
    ensure_result_caller(&window, &session_id)?;
    state
        .windows
        .set_result_pinned(&app, window.label(), pinned)
        .map_err(|error| error.to_string())?;
    let mut sessions = state.result_sessions.lock();
    let Some(meta) = sessions.get_mut(&session_id) else {
        return Ok(false);
    };
    meta.pinned = pinned;
    Ok(true)
}

#[tauri::command]
pub fn set_result_pointer_inside(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
    inside: bool,
) -> Result<(), String> {
    ensure_result_caller(&window, &session_id)?;
    state
        .windows
        .set_pointer_inside(&app, window.label(), inside);
    Ok(())
}

/// Accepts a second selection made strictly inside a known result webview.
/// The text is kept only in `current_selection`; it is never written to the
/// settings repository, result history, logs, clipboard, or filesystem.
#[tauri::command]
pub fn show_result_selection(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
    text: String,
    cursor: CursorPoint,
) -> Result<(), String> {
    ensure_result_caller(&window, &session_id)?;
    if !state.result_sessions.lock().contains_key(&session_id) {
        return Err("结果会话已结束".to_owned());
    }
    if text.trim().is_empty() {
        return Err("所选文字为空".to_owned());
    }
    if selection_text_is_too_short(&text) {
        return Err("所选文字过短".to_owned());
    }
    if text.chars().count() > 1_000_000 {
        return Err("所选文字过长".to_owned());
    }
    if !cursor.x.is_finite() || !cursor.y.is_finite() {
        return Err("选区位置无效".to_owned());
    }

    let cursor = result_selection_cursor(&app, cursor)?;
    let point = SelectionPoint {
        x: cursor.x,
        y: cursor.y,
    };
    let selection = SelectionPayload {
        text,
        source_app: SourceApplication {
            bundle_id: app.config().identifier.clone(),
            name: "TextLens 结果".to_owned(),
        },
        bounds: None,
        start_top: None,
        start_bottom: None,
        end_top: None,
        end_bottom: None,
        mouse: SelectionMouse {
            start: Some(point),
            end: Some(point),
            current: point,
        },
        direction: SelectionDirection::Forward,
        is_fullscreen: false,
        method: SelectionMethod::Accessibility,
        trigger: SelectionTrigger::Manual,
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        sequence: 0,
    };
    let current = CurrentSelection {
        id: Uuid::new_v4().to_string(),
        payload: selection,
        source_result_session_id: Some(session_id.clone()),
    };
    let public = RendererSelectionPayload::new(&current.id, &current.payload);
    let toolbar_anchor = WindowPoint {
        x: cursor.x,
        y: cursor.y,
    };
    let toolbar_placement = ToolbarPlacement::BottomMiddle;
    #[cfg(target_os = "windows")]
    state.windows.begin_toolbar_selection(&current.id);
    *state.current_selection.lock() = Some(current.clone());
    #[cfg(target_os = "windows")]
    let stage_failed = match state.windows.stage_toolbar_with_placement(
        &app,
        &current.id,
        toolbar_anchor,
        toolbar_placement,
    ) {
        Ok(()) => false,
        Err(error) => {
            eprintln!("[toolbar] native result stage failed; keeping toolbar hidden: {error}");
            true
        }
    };
    #[cfg(target_os = "macos")]
    if let Err(error) = state.windows.stage_toolbar_with_placement(
        &app,
        &current.id,
        toolbar_anchor,
        toolbar_placement,
    ) {
        state.clear_and_hide_current_selection_if(&app, &current.id);
        return Err(format!("无法显示划词工具栏：{error}"));
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    if let Err(error) = state.windows.show_toolbar_with_placement(
        &app,
        &current.id,
        toolbar_anchor,
        toolbar_placement,
    ) {
        state.clear_and_hide_current_selection_if(&app, &current.id);
        return Err(format!("无法显示划词工具栏：{error}"));
    }
    if !state.current_selection_matches(&current.id) {
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    if stage_failed {
        eprintln!(
            "[toolbar] stage=result-selection-stage selection_id={} error_class=window",
            current.id
        );
        state.recover_toolbar_delivery(&app, &current.id);
    }
    if app.emit_to(TOOLBAR_LABEL, SELECTION_EVENT, public).is_err() {
        #[cfg(target_os = "windows")]
        {
            eprintln!(
                "[toolbar] stage=result-selection-emit selection_id={} error_class=window",
                current.id
            );
            state.recover_toolbar_delivery(&app, &current.id);
            return Ok(());
        }
        #[cfg(not(target_os = "windows"))]
        {
            state.clear_and_hide_current_selection_if(&app, &current.id);
            return Err("无法更新划词工具栏".to_owned());
        }
    }
    Ok(())
}

/// Dismiss a toolbar that was opened from a selection inside this result window.
///
/// Clicks inside TextLens webviews (especially macOS, where the native hook
/// ignores own-process targets) do not generate a global outside-click dismiss.
/// The result renderer calls this on pointer-down so in-window clicks match the
/// normal “click outside toolbar → hide” behavior without closing the result.
#[tauri::command]
pub fn hide_result_selection(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
) -> Result<(), String> {
    ensure_result_caller(&window, &session_id)?;
    let Some(selection_id) = state.clear_current_selection_from_result(&session_id) else {
        return Ok(());
    };
    let _ = state.windows.hide_toolbar_if_selection(&app, &selection_id);
    // Force-hide matches the native dismiss path when scoped hide is a no-op.
    state.windows.hide_toolbar(&app);
    let _ = app.emit_to(
        TOOLBAR_LABEL,
        TOOLBAR_DISMISSED_EVENT,
        ToolbarDismissedPayload {
            selection_id: Some(selection_id),
            reason: "resultClick".to_owned(),
        },
    );
    Ok(())
}

#[tauri::command]
pub fn cancel_action(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
) -> Result<(), String> {
    ensure_result_caller(&window, &session_id)?;
    state
        .actions
        .cancel(&app, &session_id)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn retry_action(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
    target_language: Option<crate::models::TranslationLanguage>,
    provider_id: Option<String>,
    model_id: Option<String>,
) -> Result<RunActionResult, String> {
    ensure_result_caller(&window, &session_id)?;
    match state.actions.retry_with_options(
        &app,
        &session_id,
        target_language,
        provider_id,
        model_id,
    ) {
        Ok(request_id) => Ok(RunActionResult::accepted(
            Some(session_id),
            Some(request_id),
        )),
        Err(error) => follow_up_error_result(error),
    }
}

#[tauri::command]
pub fn continue_action(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
    question: String,
) -> Result<RunActionResult, String> {
    ensure_result_caller(&window, &session_id)?;
    if !state.result_sessions.lock().contains_key(&session_id) {
        return Ok(RunActionResult::rejected("结果会话已结束"));
    }
    match state
        .actions
        .continue_with_question(&app, &session_id, &question)
    {
        Ok(request_id) => Ok(RunActionResult::accepted(
            Some(session_id),
            Some(request_id),
        )),
        Err(error) => follow_up_error_result(error),
    }
}

#[tauri::command]
pub fn copy_text(window: WebviewWindow, text: String) -> Result<(), String> {
    ensure_result_window(&window)?;
    if text.chars().count() > 1_000_000 {
        return Err("复制内容过长".to_owned());
    }
    clipboard::write_text(&text)
}

#[tauri::command]
pub fn open_external(window: WebviewWindow, url: String) -> Result<(), String> {
    ensure_result_window(&window)?;
    if url.len() > 2_048 {
        return Err("外部链接过长".to_owned());
    }
    let parsed = safe_http_url(&url).ok_or_else(|| "仅允许打开安全的 HTTP(S) 链接".to_owned())?;
    open_system_url(parsed.as_str()).map_err(|_| "无法打开外部链接".to_owned())
}

#[tauri::command]
pub fn hide_result(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
) -> Result<(), String> {
    close_result_session(&app, &window, &state, &session_id)
}

#[tauri::command]
pub fn close_result(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, RuntimeState>,
    session_id: String,
) -> Result<(), String> {
    close_result_session(&app, &window, &state, &session_id)
}

/// Shared body of `hide_result` (pointer-leave dismissal) and `close_result`
/// (explicit close): both persist size, close the window and tear down the
/// session identically.
fn close_result_session(
    app: &AppHandle,
    window: &WebviewWindow,
    state: &State<'_, RuntimeState>,
    session_id: &str,
) -> Result<(), String> {
    ensure_result_caller(window, session_id)?;
    state.persist_result_size_now(app, window.label());
    state
        .windows
        .close_result(app, window.label())
        .map_err(|error| error.to_string())?;
    state.cleanup_result_session(app, session_id);
    Ok(())
}

fn ensure_result_caller(window: &WebviewWindow, session_id: &str) -> Result<(), String> {
    if Uuid::parse_str(session_id).is_err() || window.label() != result_label(session_id) {
        return Err("结果会话无效".to_owned());
    }
    Ok(())
}

fn ensure_known_caller(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == SETTINGS_LABEL
        || window.label() == TOOLBAR_LABEL
        || session_from_result_label(window.label()).is_some()
    {
        Ok(())
    } else {
        Err("拒绝来自未知窗口的请求".to_owned())
    }
}

fn ensure_settings_caller(window: &WebviewWindow) -> Result<(), String> {
    (window.label() == SETTINGS_LABEL)
        .then_some(())
        .ok_or_else(|| "该操作只能从设置窗口执行".to_owned())
}

fn ensure_toolbar_caller(window: &WebviewWindow) -> Result<(), String> {
    (window.label() == TOOLBAR_LABEL)
        .then_some(())
        .ok_or_else(|| "该操作只能从划词工具栏执行".to_owned())
}

fn ensure_result_window(window: &WebviewWindow) -> Result<(), String> {
    session_from_result_label(window.label())
        .map(|_| ())
        .ok_or_else(|| "该操作只能从结果窗口执行".to_owned())
}

fn dismiss_mode(mode: ResultDismissMode) -> DismissMode {
    match mode {
        ResultDismissMode::Manual => DismissMode::Manual,
        ResultDismissMode::Blur => DismissMode::Blur,
        ResultDismissMode::PointerLeave => DismissMode::PointerLeave,
    }
}

fn selection_toolbar_anchor(selection: &SelectionPayload) -> (WindowPoint, ToolbarPlacement) {
    let cursor = selection.mouse.end.unwrap_or(selection.mouse.current);
    // The release point is the only coordinate shared reliably by UIA, MSAA,
    // COM and clipboard fallback paths. Treat it as the toolbar's top edge;
    // the window coordinator performs screen-edge clamping afterward.
    (
        WindowPoint {
            x: cursor.x,
            y: cursor.y,
        },
        ToolbarPlacement::BottomMiddle,
    )
}

fn should_hide_toolbar_for_selection(
    current_selection_id: Option<&str>,
    requested_selection_id: Option<&str>,
) -> bool {
    match requested_selection_id {
        None => true,
        Some(requested) => current_selection_id.is_none_or(|current| current == requested),
    }
}

fn dismiss_precedes_selection(
    selection_sequence: u64,
    selection_timestamp_ms: u64,
    dismiss_sequence: u64,
    dismiss_timestamp_ms: u64,
) -> bool {
    // Native producers that supply a monotonic sequence get exact ordering.
    // Otherwise fall back to the millisecond timestamp: Windows hook and
    // WinEvent timestamps share a tick, and a delayed dismiss in the same tick
    // must not kill the selection that just completed.
    if selection_sequence != 0 && dismiss_sequence != 0 {
        return dismiss_sequence <= selection_sequence;
    }
    dismiss_timestamp_ms <= selection_timestamp_ms
}

fn action_result_cursor(
    app: &AppHandle,
    renderer_cursor: Option<CursorPoint>,
    selection: &SelectionPayload,
) -> WindowPoint {
    #[cfg(target_os = "windows")]
    {
        let native_cursor = app.cursor_position().ok().map(|cursor| WindowPoint {
            x: cursor.x,
            y: cursor.y,
        });
        windows_physical_cursor(
            renderer_cursor,
            native_cursor,
            Some(WindowPoint {
                x: selection.mouse.current.x,
                y: selection.mouse.current.y,
            }),
        )
        .unwrap_or(WindowPoint {
            x: selection.mouse.current.x,
            y: selection.mouse.current.y,
        })
    }

    #[cfg(not(target_os = "windows"))]
    renderer_cursor
        .filter(|point| point.x.is_finite() && point.y.is_finite())
        .map(|point| WindowPoint {
            x: point.x,
            y: point.y,
        })
        .unwrap_or_else(|| cursor_near_toolbar(app, selection))
}

fn result_selection_cursor(
    _app: &AppHandle,
    renderer_cursor: CursorPoint,
) -> Result<WindowPoint, String> {
    #[cfg(target_os = "windows")]
    {
        let native_cursor = _app.cursor_position().ok().map(|cursor| WindowPoint {
            x: cursor.x,
            y: cursor.y,
        });
        return windows_physical_cursor(Some(renderer_cursor), native_cursor, None)
            .ok_or_else(|| "无法读取鼠标的物理屏幕位置".to_owned());
    }

    #[cfg(not(target_os = "windows"))]
    Ok(WindowPoint {
        x: renderer_cursor.x,
        y: renderer_cursor.y,
    })
}

#[cfg(any(target_os = "windows", test))]
fn windows_physical_cursor(
    _renderer_cursor: Option<CursorPoint>,
    native_cursor: Option<WindowPoint>,
    fallback: Option<WindowPoint>,
) -> Option<WindowPoint> {
    // WebView2 MouseEvent.screenX/screenY are CSS/DIP values. They cannot be
    // mixed with UIA, Win32 cursor, monitor, and window coordinates, which are
    // all physical virtual-desktop pixels in the Windows backend.
    native_cursor.or(fallback)
}

#[cfg(not(target_os = "windows"))]
fn cursor_near_toolbar(app: &AppHandle, selection: &SelectionPayload) -> WindowPoint {
    if let Ok(cursor) = app.cursor_position() {
        let scale = app
            .primary_monitor()
            .ok()
            .flatten()
            .map(|monitor| monitor.scale_factor())
            .unwrap_or(1.0);
        return WindowPoint {
            x: cursor.x / scale,
            y: cursor.y / scale,
        };
    }
    WindowPoint {
        x: selection.mouse.current.x,
        y: selection.mouse.current.y,
    }
}

fn session_from_result_label(label: &str) -> Option<String> {
    let session = label.strip_prefix(RESULT_LABEL_PREFIX)?;
    Uuid::parse_str(session).ok().map(|value| value.to_string())
}

fn application_is_allowed(
    filter: &crate::models::ApplicationFilterSettings,
    selection: &SelectionPayload,
) -> bool {
    use crate::models::FilterMode;

    if filter.mode == FilterMode::Default {
        return true;
    }
    let name = selection.source_app.name.trim().to_lowercase();
    let bundle = selection.source_app.bundle_id.trim().to_lowercase();
    let matches = filter.applications.iter().any(|item| {
        let item = item.trim().to_lowercase();
        !item.is_empty()
            && [name.as_str(), bundle.as_str()].iter().any(|candidate| {
                !candidate.is_empty() && (candidate.contains(&item) || item.contains(candidate))
            })
    });
    match filter.mode {
        FilterMode::Whitelist => matches,
        FilterMode::Blacklist => !matches,
        FilterMode::Default => true,
    }
}

fn should_close_for_external_selection(pinned: bool) -> bool {
    !pinned
}

fn selection_source_matches_result(
    source_result_session_id: Option<&str>,
    result_session_id: &str,
) -> bool {
    source_result_session_id == Some(result_session_id)
}

fn result_selection_dismiss_is_internal(
    source_result_session_id: Option<&str>,
    reason: &str,
    target_pid: i64,
    own_process_id: u32,
) -> bool {
    source_result_session_id.is_some()
        && reason == "mouseDown"
        && target_pid == i64::from(own_process_id)
}

fn safe_http_url(value: &str) -> Option<Url> {
    let url = Url::parse(value.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url)
}

/// Opens a validated URL through the platform workspace without exposing a
/// general-purpose launcher to the renderer. On macOS this deliberately uses
/// AppKit directly instead of spawning `/usr/bin/open`.
#[cfg(target_os = "macos")]
fn open_system_url(value: &str) -> Result<(), ()> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSString, NSURL};

    let value = NSString::from_str(value);
    let url = NSURL::URLWithString(&value).ok_or(())?;
    NSWorkspace::sharedWorkspace()
        .openURL(&url)
        .then_some(())
        .ok_or(())
}

#[cfg(target_os = "windows")]
fn open_system_url(value: &str) -> Result<(), ()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use windows::{
        core::PCWSTR,
        Win32::UI::{
            Shell::{ShellExecuteExW, SHELLEXECUTEINFOW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    };

    // The previous launcher cold-started a hidden PowerShell process before
    // handing the URL to the browser, adding a visible 1-2 second delay. Ask
    // the Windows shell directly and allow it to complete slow DDE dispatch in
    // the background. Immediate association failures still return to the
    // toolbar so the current selection remains usable.
    let target = OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut execute = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: windows_url_shell_execute_mask(),
        lpFile: PCWSTR::from_raw(target.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    unsafe { ShellExecuteExW(&mut execute) }.map_err(|_| ())
}

#[cfg(target_os = "windows")]
fn windows_url_shell_execute_mask() -> u32 {
    use windows::Win32::UI::Shell::{SEE_MASK_ASYNCOK, SEE_MASK_FLAG_NO_UI};

    SEE_MASK_ASYNCOK | SEE_MASK_FLAG_NO_UI
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn open_system_url(_value: &str) -> Result<(), ()> {
    Err(())
}

fn resolve_search_target(text: &str, template: &str) -> Result<String, String> {
    let value = text.trim();
    if value.is_empty() {
        return Err("选中文本为空".to_owned());
    }
    if value.chars().count() > 2_000 {
        return Err("选中文本过长，无法作为浏览器地址或搜索词打开".to_owned());
    }
    if !value.chars().any(char::is_whitespace) {
        if let Some(url) = safe_http_url(value) {
            return Ok(url.to_string());
        }
        if let Ok(address) = value.parse::<IpAddr>() {
            return Ok(match address {
                IpAddr::V4(address) => format!("https://{address}/"),
                IpAddr::V6(address) => format!("https://[{address}]/"),
            });
        }
        if let Some(url) = infer_domain_or_ip_url(value) {
            return Ok(url.to_string());
        }
    }
    if template.matches("{{text}}").count() != 1 {
        return Err("搜索地址必须且只能包含一个 {{text}}".to_owned());
    }
    let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
    let target = template.replace("{{text}}", &encoded);
    safe_http_url(&target)
        .map(|url| url.to_string())
        .ok_or_else(|| "搜索地址无效".to_owned())
}

fn infer_domain_or_ip_url(value: &str) -> Option<Url> {
    let url = safe_http_url(&format!("https://{value}"))?;
    match url.host()? {
        Host::Ipv4(_) | Host::Ipv6(_) => Some(url),
        Host::Domain(host) => {
            let looks_numeric = host
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.');
            let valid_domain = !looks_numeric
                && host.contains('.')
                && host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '-')
                });
            let localhost_with_port =
                host.eq_ignore_ascii_case("localhost") && url.port().is_some();
            (valid_domain || localhost_with_port).then_some(url)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        actions::{ActionBeginReady, SessionRouteIdentity},
        models::{
            ActionNotice, ActionSnapshot, EventSequence, HandshakeGeneration, RequestGeneration,
            ResultReadyAck, SessionGeneration,
        },
        selection::SelectionBounds,
    };

    fn result_meta() -> ResultSessionMeta {
        ResultSessionMeta {
            selection: CurrentSelection {
                id: "selection-1".to_owned(),
                payload: SelectionPayload {
                    text: "selected".to_owned(),
                    source_app: SourceApplication {
                        bundle_id: "com.test.app".to_owned(),
                        name: "Test".to_owned(),
                    },
                    bounds: None,
                    start_top: None,
                    start_bottom: None,
                    end_top: None,
                    end_bottom: None,
                    mouse: SelectionMouse {
                        start: None,
                        end: None,
                        current: SelectionPoint { x: 10.0, y: 20.0 },
                    },
                    direction: SelectionDirection::Unknown,
                    is_fullscreen: false,
                    method: SelectionMethod::Accessibility,
                    trigger: SelectionTrigger::Manual,
                    timestamp_ms: 0,
                    sequence: 0,
                },
                source_result_session_id: Some("s".to_owned()),
            },
            action_id: "translate".to_owned(),
            pinned: false,
        }
    }

    #[test]
    fn selection_toolbar_uses_release_point_as_top_edge() {
        let mut selection = result_meta().selection.payload;
        selection.mouse.end = Some(SelectionPoint { x: 240.0, y: 360.0 });
        selection.mouse.current = SelectionPoint { x: 210.0, y: 330.0 };
        selection.start_top = Some(SelectionPoint { x: 5.0, y: 5.0 });
        selection.end_bottom = Some(SelectionPoint { x: 900.0, y: 900.0 });

        let (anchor, placement) = selection_toolbar_anchor(&selection);
        assert_eq!(anchor, WindowPoint { x: 240.0, y: 360.0 });
        assert_eq!(placement, ToolbarPlacement::BottomMiddle);
    }

    #[test]
    fn selection_toolbar_falls_back_to_current_point_without_release_point() {
        let mut selection = result_meta().selection.payload;
        selection.mouse.end = None;
        selection.mouse.current = SelectionPoint { x: 72.0, y: 88.0 };

        let (anchor, placement) = selection_toolbar_anchor(&selection);
        assert_eq!(anchor, WindowPoint { x: 72.0, y: 88.0 });
        assert_eq!(placement, ToolbarPlacement::BottomMiddle);
    }

    fn initial_preparing_begin() -> ActionBeginReady {
        ActionBeginReady {
            snapshot: ActionSnapshot {
                session_id: "s".to_owned(),
                session_generation: SessionGeneration(7),
                request_id: "r".to_owned(),
                request_generation: RequestGeneration(3),
                action_id: "translate".to_owned(),
                status: ActionSnapshotStatus::Running,
                content: String::new(),
                thinking_content: String::new(),
                last_sequence: EventSequence::NONE,
                last_content_sequence: EventSequence::NONE,
                content_scalar_count: 0,
                generation_notice: None,
                error_code: None,
                error_message: None,
                retryable: false,
            },
            ack: ResultReadyAck {
                session_id: "s".to_owned(),
                session_generation: SessionGeneration(7),
                request_generation: RequestGeneration(3),
                last_sequence: EventSequence::NONE,
                handshake_generation: HandshakeGeneration(1),
            },
            route: None,
        }
    }

    #[test]
    fn result_ready_snapshot_preserves_watermarks_and_ordered_notice() {
        let begin = ActionBeginReady {
            snapshot: ActionSnapshot {
                session_id: "s".into(),
                session_generation: SessionGeneration(7),
                request_id: "r".into(),
                request_generation: RequestGeneration(3),
                action_id: "translate".into(),
                status: ActionSnapshotStatus::Running,
                content: "A".into(),
                thinking_content: String::new(),
                last_sequence: EventSequence(12),
                last_content_sequence: EventSequence(11),
                content_scalar_count: 1,
                generation_notice: Some(ActionNotice {
                    code: "THINKING_CONTROL_DOWNGRADED".into(),
                    message: "Provider default is in use".into(),
                }),
                error_code: None,
                error_message: None,
                retryable: false,
            },
            ack: ResultReadyAck {
                session_id: "s".into(),
                session_generation: SessionGeneration(7),
                request_generation: RequestGeneration(3),
                last_sequence: EventSequence(12),
                handshake_generation: HandshakeGeneration(4),
            },
            route: Some(SessionRouteIdentity {
                provider_id: "p".into(),
                model_id: "m".into(),
                thinking_mode: crate::models::ThinkingMode::Off,
            }),
        };
        let result = compose_result_ready_snapshot(result_meta(), begin);
        assert_eq!(result.last_sequence, EventSequence(12));
        assert_eq!(result.last_content_sequence, EventSequence(11));
        assert_eq!(result.handshake_generation, HandshakeGeneration(4));
        assert_eq!(
            result
                .generation_notice
                .as_ref()
                .map(|notice| notice.code.as_str()),
            Some("THINKING_CONTROL_DOWNGRADED")
        );
        assert_eq!(result.provider_id.as_deref(), Some("p"));
        assert_eq!(result.model_id.as_deref(), Some("m"));
    }

    #[test]
    fn result_ready_snapshot_before_initial_prepare_has_optional_route_fields() {
        let result = compose_result_ready_snapshot(result_meta(), initial_preparing_begin());
        assert_eq!(result.provider_id, None);
        assert_eq!(result.model_id, None);
    }

    #[test]
    fn result_snapshot_matches_the_shared_renderer_fixture() {
        let mut meta = result_meta();
        meta.selection.id = "selection-fixture".to_owned();
        meta.selection.payload.text = "Hello".to_owned();
        meta.selection.payload.source_app = SourceApplication {
            bundle_id: "com.example.fixture".to_owned(),
            name: "Fixture App".to_owned(),
        };
        meta.selection.payload.bounds = Some(SelectionBounds {
            x: 100.0,
            y: 200.0,
            width: 120.0,
            height: 24.0,
        });
        meta.selection.payload.direction = SelectionDirection::Forward;
        meta.selection.payload.is_fullscreen = false;
        let begin = ActionBeginReady {
            snapshot: ActionSnapshot {
                session_id: "session-fixture".to_owned(),
                session_generation: SessionGeneration(1),
                request_id: "request-fixture".to_owned(),
                request_generation: RequestGeneration(1),
                action_id: "translate".to_owned(),
                status: ActionSnapshotStatus::Running,
                content: "你好".to_owned(),
                thinking_content: String::new(),
                last_sequence: EventSequence(7),
                last_content_sequence: EventSequence(6),
                content_scalar_count: 2,
                generation_notice: None,
                error_code: None,
                error_message: None,
                retryable: false,
            },
            ack: ResultReadyAck {
                session_id: "session-fixture".to_owned(),
                session_generation: SessionGeneration(1),
                request_generation: RequestGeneration(1),
                last_sequence: EventSequence(7),
                handshake_generation: HandshakeGeneration(1),
            },
            route: None,
        };
        let actual = serde_json::to_value(compose_result_ready_snapshot(meta, begin)).unwrap();
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../src/shared/__tests__/fixtures/result-session-snapshot.json"
        ))
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn retry_and_continue_return_busy_and_ended_as_rejections() {
        for error in [ActionServiceError::Busy, ActionServiceError::SessionEnded] {
            let result = follow_up_error_result(error).unwrap();
            assert!(!result.accepted);
            assert!(result.message.is_some());
        }
        assert_eq!(
            follow_up_error_result(ActionServiceError::NothingToRetry).unwrap_err(),
            "没有可重试的动作"
        );
    }

    #[test]
    fn selection_loop_isolates_a_panicking_event_and_processes_the_next_one() {
        let processed = std::cell::Cell::new(0_u8);
        assert_eq!(
            handle_selection_loop_receive(false, Ok(()), |_| panic!("test event panic")),
            SelectionLoopStep::EventPanicked
        );
        assert_eq!(processed.get(), 0);

        assert_eq!(
            handle_selection_loop_receive(false, Ok(()), |_| processed.set(1)),
            SelectionLoopStep::Continue
        );
        assert_eq!(processed.get(), 1);
    }

    #[test]
    fn selection_loop_classifies_idle_disconnect_and_shutdown_without_dispatching() {
        let processed = std::cell::Cell::new(false);
        assert_eq!(
            handle_selection_loop_receive(
                false,
                Result::<(), _>::Err(RecvTimeoutError::Timeout),
                |_| processed.set(true),
            ),
            SelectionLoopStep::Continue
        );
        assert_eq!(
            handle_selection_loop_receive(
                false,
                Result::<(), _>::Err(RecvTimeoutError::Disconnected),
                |_| processed.set(true),
            ),
            SelectionLoopStep::StopForDisconnect
        );
        assert_eq!(
            handle_selection_loop_receive(true, Ok(()), |_| processed.set(true)),
            SelectionLoopStep::StopForShutdown
        );
        assert!(!processed.get());
    }

    #[test]
    fn selection_receiver_disconnect_is_not_reported_during_shutdown() {
        assert_eq!(selection_monitor_disconnect_diagnostic(true), None);
        assert_eq!(
            selection_monitor_disconnect_diagnostic(false).as_deref(),
            Some(SELECTION_MONITOR_DISCONNECTED_ERROR)
        );
    }

    #[test]
    fn selection_monitor_diagnostic_only_reports_an_expected_listener_failure() {
        assert_eq!(
            selection_monitor_diagnostic(true, true).as_deref(),
            Some(SELECTION_MONITOR_START_ERROR)
        );
        assert_eq!(selection_monitor_diagnostic(true, false), None);
        assert_eq!(selection_monitor_diagnostic(false, true), None);
    }

    #[test]
    fn runtime_availability_and_shortcut_diagnostics_clear_after_success() {
        let mut diagnostics = RuntimeDiagnostics {
            selection_monitor_error: selection_monitor_diagnostic(true, true),
            shortcut_error: shortcut_registration_diagnostic(true),
        };
        assert!(!selection_access_available("windows", true, &diagnostics));
        assert_eq!(
            diagnostics.shortcut_error.as_deref(),
            Some(GLOBAL_SHORTCUT_ERROR)
        );

        diagnostics.selection_monitor_error = selection_monitor_diagnostic(true, false);
        diagnostics.shortcut_error = shortcut_registration_diagnostic(false);
        assert!(selection_access_available("windows", true, &diagnostics));
        assert_eq!(diagnostics.shortcut_error, None);
        assert!(!selection_access_available("darwin", false, &diagnostics));
        assert!(!selection_access_available(
            "unsupported",
            true,
            &diagnostics
        ));
    }

    #[test]
    fn runtime_diagnostics_never_include_native_errors_or_selected_text() {
        let native_error = "hook failed while reading TOP SECRET selected text";
        let diagnostics = RuntimeDiagnostics {
            selection_monitor_error: selection_monitor_diagnostic(true, !native_error.is_empty()),
            shortcut_error: shortcut_registration_diagnostic(true),
        };
        let serialized = serde_json::to_string(&diagnostics).unwrap();
        assert!(!serialized.contains(native_error));
        assert!(!serialized.contains("TOP SECRET"));
    }

    #[test]
    fn global_shortcut_is_registered_only_while_enabled_in_shortcut_mode() {
        let configured = "CommandOrControl+Shift+S";
        assert_eq!(
            shortcut_registration_target(true, TriggerMode::Shortcut, configured),
            configured
        );
        assert_eq!(
            shortcut_registration_target(true, TriggerMode::Selected, configured),
            ""
        );
        assert_eq!(
            shortcut_registration_target(false, TriggerMode::Shortcut, configured),
            ""
        );
        // Only the effective registration changes; the saved configuration is
        // retained for a later switch back to shortcut mode.
        assert_eq!(configured, "CommandOrControl+Shift+S");
    }

    #[test]
    fn global_shortcut_events_match_structural_identity_and_saved_settings() {
        let registered = "Ctrl+Shift+S".parse::<Shortcut>().unwrap();
        let alias = "Control+Shift+S".parse::<Shortcut>().unwrap();
        assert_eq!(registered, alias);

        let pressed = ShortcutEvent {
            id: alias.id(),
            state: ShortcutState::Pressed,
        };
        assert!(shortcut_event_is_current(
            alias,
            pressed,
            Some(registered),
            true,
            TriggerMode::Shortcut,
            "Control+Shift+S",
        ));
        assert!(!shortcut_event_is_current(
            alias,
            pressed,
            Some(registered),
            false,
            TriggerMode::Shortcut,
            "Control+Shift+S",
        ));
        assert!(!shortcut_event_is_current(
            alias,
            ShortcutEvent {
                id: alias.id(),
                state: ShortcutState::Released,
            },
            Some(registered),
            true,
            TriggerMode::Shortcut,
            "Control+Shift+S",
        ));

        let stale = "Alt+Shift+S".parse::<Shortcut>().unwrap();
        assert!(!shortcut_event_is_current(
            stale,
            ShortcutEvent {
                id: stale.id(),
                state: ShortcutState::Pressed,
            },
            Some(registered),
            true,
            TriggerMode::Shortcut,
            "Control+Shift+S",
        ));
    }

    #[test]
    fn selection_auto_present_is_gated_by_trigger_mode() {
        assert!(should_present_selection_for_trigger(
            false,
            TriggerMode::Selected
        ));
        assert!(!should_present_selection_for_trigger(
            false,
            TriggerMode::Shortcut
        ));
        assert!(should_present_selection_for_trigger(
            true,
            TriggerMode::Shortcut
        ));
        assert!(should_present_selection_for_trigger(
            true,
            TriggerMode::Selected
        ));
    }

    #[test]
    fn toolbar_requires_more_than_one_visible_character() {
        assert!(selection_text_is_too_short(""));
        assert!(selection_text_is_too_short(" \n\t "));
        assert!(selection_text_is_too_short("字"));
        assert!(selection_text_is_too_short(" 字 "));
        assert!(selection_text_is_too_short("a"));
        assert!(selection_text_is_too_short(" a "));
        assert!(!selection_text_is_too_short("中文"));
        assert!(!selection_text_is_too_short(" ab "));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_close_behavior_accounts_for_renderer_readiness() {
        assert_eq!(
            settings_close_action(ApplicationCloseBehavior::HideToTray, false),
            SettingsCloseAction::HideToTray
        );
        assert_eq!(
            settings_close_action(ApplicationCloseBehavior::Quit, true),
            SettingsCloseAction::RequestQuitConfirmation
        );
        assert_eq!(
            settings_close_action(ApplicationCloseBehavior::Quit, false),
            SettingsCloseAction::QuitImmediately
        );
    }

    #[test]
    fn direct_urls_domains_and_ips_take_precedence_over_search() {
        let template = "https://www.google.com/search?q={{text}}";
        assert_eq!(
            resolve_search_target("https://example.com/a", template).unwrap(),
            "https://example.com/a"
        );
        assert_eq!(
            resolve_search_target("example.com/a", template).unwrap(),
            "https://example.com/a"
        );
        assert_eq!(
            resolve_search_target("192.168.1.10", template).unwrap(),
            "https://192.168.1.10/"
        );
        assert_eq!(
            resolve_search_target("2001:db8::1", template).unwrap(),
            "https://[2001:db8::1]/"
        );
    }

    #[test]
    fn malformed_addresses_and_words_are_searched() {
        let template = "https://www.google.com/search?q={{text}}";
        assert_eq!(
            resolve_search_target("999.168.1.10", template).unwrap(),
            "https://www.google.com/search?q=999.168.1.10"
        );
        assert_eq!(
            resolve_search_target("Tauri 划词", template).unwrap(),
            "https://www.google.com/search?q=Tauri+%E5%88%92%E8%AF%8D"
        );
    }

    #[test]
    fn search_engines_use_the_expected_regional_endpoints() {
        assert_eq!(
            crate::models::resolve_search_template(Some("google"), None).unwrap(),
            "https://www.google.com/search?q={{text}}"
        );
        assert_eq!(
            crate::models::resolve_search_template(Some("google"), Some("bing-china")).unwrap(),
            "https://cn.bing.com/search?q={{text}}"
        );
        assert_eq!(
            crate::models::resolve_search_template(Some("google"), Some("baidu")).unwrap(),
            "https://www.baidu.com/s?wd={{text}}"
        );
    }

    #[test]
    fn persisted_search_engine_is_used_unless_an_override_is_supplied() {
        assert_eq!(
            crate::models::resolve_search_template(Some("bing-china"), None).unwrap(),
            "https://cn.bing.com/search?q={{text}}"
        );
        assert_eq!(
            crate::models::resolve_search_template(Some("bing-china"), Some("baidu")).unwrap(),
            "https://www.baidu.com/s?wd={{text}}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_search_uses_non_blocking_native_shell_dispatch() {
        use windows::Win32::UI::Shell::{SEE_MASK_ASYNCOK, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC};

        let mask = windows_url_shell_execute_mask();
        assert_ne!(mask & SEE_MASK_ASYNCOK, 0);
        assert_ne!(mask & SEE_MASK_FLAG_NO_UI, 0);
        assert_eq!(mask & SEE_MASK_NOASYNC, 0);
    }

    #[test]
    fn only_uuid_namespaced_result_labels_are_accepted() {
        let session = "3f82ff66-6d82-45e8-a854-e28a62cfb510";
        assert_eq!(
            session_from_result_label(&result_label(session)).as_deref(),
            Some(session)
        );
        assert!(session_from_result_label("selection-result-").is_none());
        assert!(session_from_result_label("selection-result-settings").is_none());
        assert!(session_from_result_label("selection-result-../../settings").is_none());
        assert!(session_from_result_label("settings").is_none());
    }

    #[test]
    fn external_selection_closes_only_unpinned_results() {
        assert!(should_close_for_external_selection(false));
        assert!(!should_close_for_external_selection(true));
    }

    #[test]
    fn destroyed_result_matches_only_its_own_selection() {
        assert!(selection_source_matches_result(
            Some("result-a"),
            "result-a"
        ));
        assert!(!selection_source_matches_result(
            Some("result-b"),
            "result-a"
        ));
        assert!(!selection_source_matches_result(None, "result-a"));
    }

    #[test]
    fn result_selection_keeps_toolbar_action_clicks_alive() {
        assert!(result_selection_dismiss_is_internal(
            Some("result-a"),
            "mouseDown",
            42,
            42,
        ));
        assert!(!result_selection_dismiss_is_internal(
            Some("result-a"),
            "mouseDown",
            7,
            42,
        ));
        assert!(!result_selection_dismiss_is_internal(
            Some("result-a"),
            "scroll",
            42,
            42,
        ));
        assert!(!result_selection_dismiss_is_internal(
            None,
            "mouseDown",
            42,
            42,
        ));
    }

    #[test]
    fn result_reveal_commit_is_idempotent_and_failed_sessions_cannot_restart() {
        let mut phase = ResultRevealHandshakePhase::Pending;
        assert_eq!(phase.begin_commit(), Ok(true));
        assert_eq!(phase.begin_commit(), Ok(false));
        assert!(phase.finish_commit(true));
        assert_eq!(phase, ResultRevealHandshakePhase::Committed);
        assert_eq!(phase.begin_commit(), Ok(false));
        assert!(!phase.finish_commit(true));

        let mut failed = ResultRevealHandshakePhase::Pending;
        assert_eq!(failed.begin_commit(), Ok(true));
        assert!(failed.finish_commit(false));
        assert_eq!(failed, ResultRevealHandshakePhase::Failed);
        assert_eq!(failed.begin_commit(), Err(()));
    }

    #[tokio::test]
    async fn result_reveal_wait_is_bounded_and_reports_a_closed_renderer() {
        let (_sender, receiver) = oneshot::channel();
        let timeout = wait_for_result_reveal_with_timeout(receiver, Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(timeout.contains("超时"));

        let (sender, receiver) = oneshot::channel::<Result<(), String>>();
        drop(sender);
        let closed = wait_for_result_reveal_with_timeout(receiver, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(closed.contains("关闭"));
    }

    #[test]
    fn stale_toolbar_hide_is_safe_but_can_finish_an_already_consumed_selection() {
        assert!(should_hide_toolbar_for_selection(None, Some("finished")));
        assert!(should_hide_toolbar_for_selection(
            Some("same"),
            Some("same")
        ));
        assert!(!should_hide_toolbar_for_selection(Some("new"), Some("old")));
        assert!(should_hide_toolbar_for_selection(Some("new"), None));
    }

    #[test]
    fn stale_dismiss_cannot_clear_a_newer_selection() {
        assert!(dismiss_precedes_selection(0, 2_000, 0, 1_999));
        assert!(dismiss_precedes_selection(0, 2_000, 0, 2_000));
        assert!(!dismiss_precedes_selection(0, 2_000, 0, 2_001));
        // Sequences win over timestamps when both producers provide them.
        assert!(dismiss_precedes_selection(5, 1, 5, 9_999));
        assert!(!dismiss_precedes_selection(5, 9_999, 6, 1));
    }

    #[test]
    fn dismiss_should_force_hide_when_selection_scoped_hide_returns_false() {
        let plan = dismiss_hide_plan(Some("selection-1".to_owned()), false);
        assert_eq!(plan.cleared_selection_id.as_deref(), Some("selection-1"));
        assert!(plan.force_hide);
    }

    #[test]
    fn dismiss_without_selection_still_force_hides() {
        let plan = dismiss_hide_plan(None, false);
        assert!(plan.cleared_selection_id.is_none());
        assert!(plan.force_hide);
    }

    #[test]
    fn dismiss_does_not_repeat_a_successful_scoped_hide() {
        let plan = dismiss_hide_plan(Some("selection-1".to_owned()), true);
        assert!(!plan.force_hide);
    }

    fn fixture_bounds(x: f64, y: f64, width: f64, height: f64) -> Option<SelectionBounds> {
        Some(SelectionBounds {
            x,
            y,
            width,
            height,
        })
    }

    /// A presented selection over the range (0,0,100,20), dragged (10,20) to
    /// (120,20).
    fn fixture_presented(
        source: &str,
        text: &str,
        bounds: Option<SelectionBounds>,
    ) -> PresentedSelection {
        fixture_presented_gesture(source, text, bounds, (10.0, 20.0), (120.0, 20.0))
    }

    fn fixture_presented_gesture(
        source: &str,
        text: &str,
        bounds: Option<SelectionBounds>,
        start: (f64, f64),
        end: (f64, f64),
    ) -> PresentedSelection {
        PresentedSelection {
            source: source.to_owned(),
            text: text.to_owned(),
            bounds,
            mouse: SelectionMouse {
                start: Some(SelectionPoint {
                    x: start.0,
                    y: start.1,
                }),
                end: Some(SelectionPoint { x: end.0, y: end.1 }),
                current: SelectionPoint { x: end.0, y: end.1 },
            },
        }
    }

    fn fixture_capture(
        source: &str,
        text: &str,
        bounds: Option<SelectionBounds>,
        trigger: SelectionTrigger,
        start: (f64, f64),
        end: (f64, f64),
    ) -> SelectionPayload {
        SelectionPayload {
            text: text.to_owned(),
            source_app: SourceApplication {
                bundle_id: source.to_owned(),
                name: "Fixture".to_owned(),
            },
            bounds,
            start_top: None,
            start_bottom: None,
            end_top: None,
            end_bottom: None,
            mouse: SelectionMouse {
                start: Some(SelectionPoint {
                    x: start.0,
                    y: start.1,
                }),
                end: Some(SelectionPoint { x: end.0, y: end.1 }),
                current: SelectionPoint { x: end.0, y: end.1 },
            },
            direction: SelectionDirection::Forward,
            is_fullscreen: false,
            method: SelectionMethod::Accessibility,
            trigger,
            timestamp_ms: 1,
            sequence: 1,
        }
    }

    #[test]
    fn a_click_that_drifts_over_a_live_selection_is_not_presented_again() {
        // The bug: translate finishes, the host still reports the old text, and
        // any click 5-8px off target re-captures it and throws the toolbar at
        // the cursor.
        let presented = fixture_presented(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 0.0, 100.0, 20.0),
        );
        let micro_drag = fixture_capture(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 0.0, 100.0, 20.0),
            SelectionTrigger::Drag,
            (640.0, 480.0),
            (646.0, 481.0),
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &micro_drag),
            SelectionPresentation::AlreadyPresented
        );
        assert_eq!(
            judge_selection_presentation(None, &micro_drag),
            SelectionPresentation::Present
        );
    }

    #[test]
    fn a_real_drag_over_the_same_range_is_presented_again() {
        let presented = fixture_presented(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 0.0, 100.0, 20.0),
        );
        let reselect = fixture_capture(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 0.0, 100.0, 20.0),
            SelectionTrigger::Drag,
            (4.0, 12.0),
            (98.0, 14.0),
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &reselect),
            SelectionPresentation::Present
        );
    }

    #[test]
    fn a_long_drag_far_from_the_selection_is_not_a_reselect() {
        // The reported flow: translate closes on an outside click, the text is
        // still selected, and the next click picks up 50px of drift away from
        // it. Travel alone must not throw the toolbar back at the cursor.
        let presented = fixture_presented(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 0.0, 100.0, 20.0),
        );
        let far_drag = fixture_capture(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 0.0, 100.0, 20.0),
            SelectionTrigger::Drag,
            (600.0, 500.0),
            (650.0, 505.0),
        );
        assert!(selection_gesture_reach(&far_drag.mouse) >= RESELECT_MIN_REACH_PX);
        assert_eq!(
            judge_selection_presentation(Some(&presented), &far_drag),
            SelectionPresentation::AlreadyPresented
        );
    }

    #[test]
    fn without_range_geometry_the_previous_gesture_is_the_anchor() {
        // Canvas apps captured through the clipboard report no bounds, so the
        // presented drag points carry the location evidence instead.
        let presented = fixture_presented_gesture(
            "com.example.canvas",
            "hello",
            None,
            (10.0, 20.0),
            (120.0, 20.0),
        );
        let over_the_text = fixture_capture(
            "com.example.canvas",
            "hello",
            None,
            SelectionTrigger::Drag,
            (8.0, 18.0),
            (122.0, 22.0),
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &over_the_text),
            SelectionPresentation::Present
        );
        let across_the_page = fixture_capture(
            "com.example.canvas",
            "hello",
            None,
            SelectionTrigger::Drag,
            (400.0, 400.0),
            (450.0, 400.0),
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &across_the_page),
            SelectionPresentation::AlreadyPresented
        );
    }

    #[test]
    fn the_same_words_at_a_different_range_are_a_new_selection() {
        let presented = fixture_presented(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 0.0, 100.0, 20.0),
        );
        let elsewhere = fixture_capture(
            "com.example.editor",
            "hello",
            fixture_bounds(0.0, 320.0, 100.0, 20.0),
            SelectionTrigger::Drag,
            (2.0, 326.0),
            (4.0, 326.0),
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &elsewhere),
            SelectionPresentation::Present
        );
        // Sub-pixel rounding on the same range still reads as the same range.
        assert_eq!(
            selection_bounds_match(
                fixture_bounds(0.0, 0.0, 100.0, 20.0),
                fixture_bounds(1.4, 0.0, 100.0, 20.6)
            ),
            SelectionBoundsMatch::Same
        );
        assert_eq!(
            selection_bounds_match(
                fixture_bounds(0.0, 0.0, 100.0, 20.0),
                fixture_bounds(0.0, 320.0, 100.0, 20.0)
            ),
            SelectionBoundsMatch::Different
        );
        assert_eq!(
            selection_bounds_match(fixture_bounds(0.0, 0.0, 100.0, 20.0), None),
            SelectionBoundsMatch::Unknown
        );
    }

    #[test]
    fn double_click_and_keyboard_triggers_cannot_reuse_a_live_selection() {
        // A double-click on blank space re-reads whatever the app still has
        // selected, and without range geometry there is no evidence of a new
        // selection.
        let presented = fixture_presented("com.example.editor", "hello", None);
        for trigger in [
            SelectionTrigger::DoubleClick,
            SelectionTrigger::Keyboard,
            SelectionTrigger::ShiftClick,
        ] {
            let capture = fixture_capture(
                "com.example.editor",
                "hello",
                None,
                trigger,
                (300.0, 300.0),
                (300.0, 300.0),
            );
            assert_eq!(
                judge_selection_presentation(Some(&presented), &capture),
                SelectionPresentation::AlreadyPresented,
                "trigger {trigger:?} reused the live selection"
            );
        }
        // The shortcut is an explicit request, so it presents.
        let manual = fixture_capture(
            "com.example.editor",
            "hello",
            None,
            SelectionTrigger::Manual,
            (300.0, 300.0),
            (300.0, 300.0),
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &manual),
            SelectionPresentation::Present
        );
    }

    #[test]
    fn a_different_app_or_text_always_presents() {
        let presented = fixture_presented("com.example.editor", "hello", None);
        let other_text = fixture_capture(
            "com.example.editor",
            "world",
            None,
            SelectionTrigger::Drag,
            (10.0, 10.0),
            (11.0, 10.0),
        );
        let other_app = fixture_capture(
            "com.example.browser",
            "hello",
            None,
            SelectionTrigger::Drag,
            (10.0, 10.0),
            (11.0, 10.0),
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &other_text),
            SelectionPresentation::Present
        );
        assert_eq!(
            judge_selection_presentation(Some(&presented), &other_app),
            SelectionPresentation::Present
        );
    }

    #[test]
    fn gesture_reach_divides_a_click_from_a_reselect_without_bounds() {
        // Pinned because it is the number users feel: below it a drag is a click.
        assert_eq!(RESELECT_MIN_REACH_PX, 12.0);
        assert_eq!(RESELECT_ANCHOR_RADIUS_PX, 16.0);
        let reach = |start: (f64, f64), end: (f64, f64)| {
            selection_gesture_reach(&SelectionMouse {
                start: Some(SelectionPoint {
                    x: start.0,
                    y: start.1,
                }),
                end: Some(SelectionPoint { x: end.0, y: end.1 }),
                current: SelectionPoint { x: end.0, y: end.1 },
            })
        };
        assert!((reach((0.0, 0.0), (9.0, 0.0)) - 9.0).abs() < 0.001);
        assert!(reach((0.0, 0.0), (5.0, 5.0)) < RESELECT_MIN_REACH_PX);
        assert!(reach((0.0, 0.0), (12.0, 0.0)) >= RESELECT_MIN_REACH_PX);
        // No endpoints at all reads as no travel, which is the honest answer.
        assert_eq!(
            selection_gesture_reach(&SelectionMouse {
                start: None,
                end: None,
                current: SelectionPoint { x: 40.0, y: 40.0 },
            }),
            0.0
        );
    }

    #[test]
    fn pending_host_clear_fires_only_when_token_current() {
        assert!(should_fire_pending_host_clear(3, 3));
        assert!(!should_fire_pending_host_clear(3, 4));
        assert!(!should_fire_pending_host_clear(3, 2));
        assert!(should_fire_pending_host_clear(0, 0));
    }

    #[test]
    fn cancel_pending_host_clear_bumps_token_so_scheduled_is_noop() {
        let scheduled = 5u64;
        let after_cancel = next_host_clear_token(scheduled);
        assert_eq!(after_cancel, 6);
        assert!(!should_fire_pending_host_clear(scheduled, after_cancel));
        assert!(should_fire_pending_host_clear(after_cancel, after_cancel));
        // Wrapping is supported so generation never panics at u64::MAX.
        assert_eq!(next_host_clear_token(u64::MAX), 0);
        assert!(!should_fire_pending_host_clear(u64::MAX, 0));
    }

    #[test]
    fn permission_poll_polls_faster_while_untrusted() {
        let trusted = permission_poll_interval(true);
        let untrusted = permission_poll_interval(false);
        // Grant pickup after the user approves Accessibility must feel
        // immediate; revocation detection once trusted can stay relaxed.
        assert_eq!(trusted, Duration::from_millis(PERMISSION_POLL_TRUSTED_MS));
        assert_eq!(
            untrusted,
            Duration::from_millis(PERMISSION_POLL_UNTRUSTED_MS)
        );
        assert!(untrusted < trusted);
    }

    #[test]
    fn host_selection_clear_delay_is_within_design_range() {
        assert!(
            (150..=300).contains(&HOST_SELECTION_CLEAR_DELAY_MS),
            "deferred clear delay must stay in 150–300ms (got {})",
            HOST_SELECTION_CLEAR_DELAY_MS
        );
        assert_eq!(HOST_SELECTION_CLEAR_DELAY_MS, 200);
        assert_eq!(SELECTION_BOUNDS_MATCH_TOLERANCE_PX, 2.0);
    }

    #[test]
    fn host_clear_args_skip_empty_text() {
        assert!(host_selection_clear_args("", Some("com.apple.TextEdit")).is_none());
    }

    #[test]
    fn host_clear_args_keep_bundle_and_text() {
        let args = host_selection_clear_args("你好", Some("com.apple.TextEdit")).unwrap();
        assert_eq!(args.text, "你好");
        assert_eq!(args.bundle_id.as_deref(), Some("com.apple.TextEdit"));
    }

    #[test]
    fn host_clear_args_drop_blank_bundle_id() {
        let args = host_selection_clear_args("hello", Some("   ")).unwrap();
        assert_eq!(args.text, "hello");
        assert!(args.bundle_id.is_none());
        let no_bundle = host_selection_clear_args("hello", None).unwrap();
        assert!(no_bundle.bundle_id.is_none());
    }

    #[test]
    fn toolbar_commit_only_matches_the_current_selection() {
        assert!(toolbar_commit_matches_selection(
            Some("selection-current"),
            "selection-current"
        ));
        assert!(!toolbar_commit_matches_selection(
            Some("selection-new"),
            "selection-stale"
        ));
        assert!(!toolbar_commit_matches_selection(None, "selection-stale"));
    }

    #[test]
    fn windows_cursor_resolution_never_mixes_webview_dips_with_physical_pixels() {
        let renderer_dip = CursorPoint {
            x: 1_280.0,
            y: 320.0,
        };
        let native_physical = WindowPoint {
            x: 2_080.0,
            y: 480.0,
        };
        let selection_physical = WindowPoint {
            x: -400.0,
            y: 700.0,
        };

        assert_eq!(
            windows_physical_cursor(
                Some(renderer_dip),
                Some(native_physical),
                Some(selection_physical),
            ),
            Some(native_physical)
        );
        assert_eq!(
            windows_physical_cursor(Some(renderer_dip), None, Some(selection_physical)),
            Some(selection_physical)
        );
        assert_eq!(
            windows_physical_cursor(Some(renderer_dip), None, None),
            None
        );
    }
}
