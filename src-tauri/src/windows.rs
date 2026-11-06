use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tauri::Emitter;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
#[cfg(not(target_os = "windows"))]
use tauri::{LogicalPosition, LogicalSize};
#[cfg(target_os = "windows")]
use tauri::{PhysicalPosition, PhysicalSize};

const TOOLBAR_LABEL: &str = "selection-toolbar";
pub const TOOLBAR_POINTER_EVENT: &str = "textlens:toolbar-pointer";
#[cfg(target_os = "windows")]
const STARTUP_NOTICE_LABEL_PREFIX: &str = "startup-notice-";
#[cfg(target_os = "windows")]
static STARTUP_NOTICE_GENERATION: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "windows")]
static WINDOWS_UI_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const SCREEN_MARGIN: f64 = 8.0;
const TOOLBAR_SCREEN_MARGIN: f64 = 20.0;
// WebView2's client extent can land a few physical pixels short of a
// borderless SetWindowPos request on some DPI/driver combinations. Keep a
// small, scale-aware transparent trailing area outside the measured toolbar
// content so the last icon is never the pixel that gets clipped at a display
// edge.
#[cfg(target_os = "windows")]
const WINDOWS_TOOLBAR_TRAILING_GUARD_RATIO: f64 = 0.04;
#[cfg(target_os = "windows")]
const WINDOWS_TOOLBAR_TRAILING_GUARD_MIN: f64 = 4.0;
#[cfg(target_os = "windows")]
const WINDOWS_TOOLBAR_TRAILING_GUARD_MAX: f64 = 12.0;
#[cfg(any(target_os = "windows", test))]
const STARTUP_NOTICE_MARGIN: f64 = 18.0;
// The action click sits one fifth of a result window in from its left edge,
// leaving four fifths to the right. This keeps the result out of the way of
// the toolbar while retaining a stable, intentional click anchor.
const RESULT_CURSOR_LEFT_ANCHOR_RATIO: f64 = 0.20;
const RESULT_VERTICAL_SHIFT_RATIO: f64 = 0.30;
#[cfg(target_os = "windows")]
const RESULT_BLUR_VERIFY_DELAY: Duration = Duration::from_millis(60);
/// Mirrors the Windows window-operation bound: a stalled AppKit main loop must
/// not freeze the single selection-event consumer indefinitely.
#[cfg(target_os = "macos")]
const MACOS_MAIN_THREAD_HOP_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "windows")]
const RESULT_BLUR_VERIFY_ATTEMPTS: usize = 2;
#[cfg(target_os = "windows")]
const WINDOWS_TOOLBAR_HIT_TOLERANCE: f64 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultForegroundScope {
    Internal,
    External,
    Unknown,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupNoticeKind {
    Started,
    AlreadyRunning,
}

#[cfg(target_os = "windows")]
impl StartupNoticeKind {
    fn query_value(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::AlreadyRunning => "running",
        }
    }
}

/// Records Tauri's Windows event-loop thread before any configured WebView is
/// created. The setup hook runs on that thread; registering it here prevents
/// the first toolbar operation from trying to post a task to, and then wait
/// on, the same thread.
#[cfg(target_os = "windows")]
pub fn initialize_windows_ui_thread() {
    use windows::Win32::System::Threading::GetCurrentThreadId;

    let thread_id = unsafe { GetCurrentThreadId() };
    if thread_id != 0 {
        WINDOWS_UI_THREAD_ID.store(thread_id, Ordering::Release);
    }
}

#[cfg(target_os = "windows")]
fn is_windows_ui_thread() -> bool {
    use windows::Win32::System::Threading::GetCurrentThreadId;

    windows_window_operation_runs_inline(
        unsafe { GetCurrentThreadId() },
        WINDOWS_UI_THREAD_ID.load(Ordering::Acquire),
    )
}

/// Shows a short, click-through Windows notice without activating TextLens or
/// adding another taskbar entry. The renderer owns the 1 s hold + 0.5 s fade;
/// the native window is retained briefly after that animation, then destroyed.
#[cfg(target_os = "windows")]
pub fn show_startup_notice(app: &AppHandle, kind: StartupNoticeKind) -> tauri::Result<()> {
    for (label, window) in app.webview_windows() {
        if label.starts_with(STARTUP_NOTICE_LABEL_PREFIX) {
            let _ = window.close();
        }
    }

    let generation = STARTUP_NOTICE_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    let label = format!("{STARTUP_NOTICE_LABEL_PREFIX}{generation}");
    let logical_size = WindowSize {
        width: 268.0,
        height: 54.0,
    };
    let window = WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::App(format!("startup/index.html?kind={}", kind.query_value()).into()),
    )
    .title("TextLens")
    .inner_size(logical_size.width, logical_size.height)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .resizable(false)
    .minimizable(false)
    .maximizable(false)
    .fullscreen(false)
    .always_on_top(true)
    .visible_on_all_workspaces(true)
    .skip_taskbar(true)
    .focusable(false)
    .focused(false)
    .visible(false)
    .build()?;

    let cursor = app
        .cursor_position()
        .map(|point| Point {
            x: point.x,
            y: point.y,
        })
        .unwrap_or(Point { x: 0.0, y: 0.0 });
    let geometry = monitor_geometry_for_point(&window, cursor)?;
    let coordinate_scale = geometry
        .map(|value| value.coordinate_scale)
        .unwrap_or_else(|| current_window_coordinate_scale(&window));
    let coordinate_size = logical_size_to_coordinates(logical_size, coordinate_scale);
    let position = geometry.map_or(cursor, |value| {
        startup_notice_position_in_area(coordinate_size, value.work_area, value.coordinate_scale)
    });

    apply_window_layout(
        &window,
        WindowLayout {
            position,
            coordinate_size,
        },
    )?;
    window.set_ignore_cursor_events(true)?;
    configure_toolbar_native(&window)?;
    order_front_without_focus(&window)?;

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2_000));
        let _ = window.close();
    });
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolbarPlacement {
    #[default]
    BottomMiddle,
    BottomLeft,
    BottomRight,
    TopRight,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WindowSize {
    pub width: f64,
    pub height: f64,
}

/// Pointer coordinates relative to the toolbar WebView's client area.
///
/// The native toolbar is deliberately non-activating. WKWebView and WebView2
/// do not reliably deliver `mousemove` events for such a window, so the native
/// coordinator samples the global cursor while the toolbar is visible and
/// forwards this small, validated payload to the renderer.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolbarPointerPosition {
    pub x: f64,
    pub y: f64,
    pub inside: bool,
}

impl WindowSize {
    pub fn clamped(self) -> Self {
        Self {
            width: self.width.clamp(360.0, 1_600.0),
            height: self.height.clamp(260.0, 1_200.0),
        }
    }
}

fn toolbar_size(size: WindowSize) -> WindowSize {
    WindowSize {
        width: size.width.clamp(44.0, 900.0).ceil(),
        height: size.height.clamp(36.0, 160.0).ceil(),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DismissMode {
    Manual,
    Blur,
    PointerLeave,
}

#[derive(Debug, Clone)]
pub struct ResultWindowOptions {
    pub size: WindowSize,
    pub cursor: Point,
    pub follow_cursor: bool,
    pub pinned: bool,
    pub dismiss_mode: DismissMode,
    pub dismiss_delay_ms: u64,
    pub opacity: f64,
    pub remember_size: bool,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
struct ResultPlacement {
    size: WindowSize,
    cursor: Point,
    follow_cursor: bool,
}

#[derive(Debug, Clone)]
struct ResultRuntime {
    session_id: String,
    pinned: bool,
    opacity: f64,
    placement: ResultPlacement,
    reveal_phase: ResultRevealPhase,
    reveal_operation: Arc<Mutex<()>>,
    focus_seen: bool,
    blur_generation: u64,
    pointer_inside: bool,
    pointer_seen: bool,
    dismiss_mode: DismissMode,
    dismiss_delay_ms: u64,
    hide_generation: u64,
    pending_auto_close: Option<AutoCloseOrigin>,
    remember_size: bool,
    ignore_resize_until: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultRevealPhase {
    Hidden,
    Prepared,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoCloseOrigin {
    PointerLeave(u64),
    Blur(u64),
}

fn initial_result_reveal_phase() -> ResultRevealPhase {
    ResultRevealPhase::Hidden
}

fn initial_result_resize_ignore_until() -> Instant {
    // Enable persistence at the exact hydrated reveal commit boundary.
    Instant::now() + std::time::Duration::from_millis(3_500)
}

fn result_reveal_error(message: &'static str) -> tauri::Error {
    std::io::Error::other(message).into()
}

fn result_window_step_error(step: &'static str, error: impl std::fmt::Display) -> tauri::Error {
    std::io::Error::other(format!("result window {step} failed: {error}")).into()
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn trace_toolbar_timing(stage: &str, started: Instant) {
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

impl ResultRuntime {
    fn pointer_leave_armed(&self) -> bool {
        self.reveal_phase == ResultRevealPhase::Committed
            && self.pointer_seen
            && !self.pointer_inside
            && !self.pinned
            && self.dismiss_mode == DismissMode::PointerLeave
    }

    fn note_pointer_change(&mut self, inside: bool) -> bool {
        self.pointer_inside = inside;
        if self.reveal_phase != ResultRevealPhase::Committed {
            self.pointer_seen = false;
        } else if inside {
            self.pointer_seen = true;
        }
        self.hide_generation = self.hide_generation.wrapping_add(1);
        self.pointer_leave_armed()
    }

    fn pointer_leave_is_current(&self, generation: u64) -> bool {
        self.hide_generation == generation && self.pointer_leave_armed()
    }

    fn auto_close_is_current(&self, origin: AutoCloseOrigin) -> bool {
        match origin {
            AutoCloseOrigin::PointerLeave(generation) => self.pointer_leave_is_current(generation),
            AutoCloseOrigin::Blur(generation) => self.blur_is_current(generation),
        }
    }

    fn prepare_auto_close_request(&mut self, origin: AutoCloseOrigin) -> bool {
        if !self.auto_close_is_current(origin) {
            return false;
        }
        // A queued close can represent the newest independently eligible
        // dismissal. Never enqueue a second event that would lack a token.
        self.pending_auto_close.replace(origin).is_none()
    }

    fn cancel_auto_close_request(&mut self) {
        self.pending_auto_close = None;
    }

    /// Returns a generation token for a blur that is eligible for dismissal.
    ///
    /// Windows must not close synchronously from the raw Tauri focus event:
    /// WebView2 moves focus from the top-level HWND to document controls,
    /// native select popups and the move/resize loop. Those internal transfers
    /// also surface as `Focused(false)` for the top-level window.
    fn note_focus_change(&mut self, focused: bool) -> Option<u64> {
        self.blur_generation = self.blur_generation.wrapping_add(1);
        // Wry/WebView2 can emit a transient Focused(true) -> Focused(false)
        // while constructing an invisible controller. Blur dismissal must
        // not arm until the renderer has prepared and committed the result.
        if self.reveal_phase != ResultRevealPhase::Committed {
            self.focus_seen = false;
            return None;
        }
        if focused {
            self.focus_seen = true;
            return None;
        }
        (self.focus_seen && !self.pinned && self.dismiss_mode == DismissMode::Blur)
            .then_some(self.blur_generation)
    }

    fn blur_is_current(&self, generation: u64) -> bool {
        self.reveal_phase == ResultRevealPhase::Committed
            && self.focus_seen
            && !self.pinned
            && self.dismiss_mode == DismissMode::Blur
            && self.blur_generation == generation
    }
}

struct WindowState {
    toolbar_revision: u64,
    toolbar_hide_pending: bool,
    toolbar_reveal_requested: bool,
    toolbar_present_pending: bool,
    toolbar_size: WindowSize,
    toolbar_anchor: Option<Point>,
    toolbar_placement: Option<ToolbarPlacement>,
    toolbar_selection_id: Option<String>,
    toolbar_recovery_selection_id: Option<String>,
    toolbar_recovery_attempts: u8,
    toolbar_recovery_pending_selection_id: Option<String>,
    results: HashMap<String, ResultRuntime>,
}

impl WindowState {
    fn begin_toolbar_selection(&mut self, selection_id: &str) {
        if self.toolbar_recovery_selection_id.as_deref() != Some(selection_id) {
            self.invalidate_toolbar_operations();
            self.toolbar_recovery_selection_id = Some(selection_id.to_owned());
            self.toolbar_recovery_attempts = 0;
            self.toolbar_recovery_pending_selection_id = None;
        }
    }

    fn invalidate_toolbar_operations(&mut self) -> u64 {
        self.toolbar_revision = self.toolbar_revision.wrapping_add(1);
        self.toolbar_revision
    }

    fn request_toolbar_reveal(&mut self) -> u64 {
        self.toolbar_reveal_requested = true;
        self.toolbar_present_pending = true;
        self.invalidate_toolbar_operations()
    }

    fn toolbar_operation_is_current(&self, revision: u64) -> bool {
        self.toolbar_revision == revision
    }

    fn toolbar_selection_is_current(&self, selection_id: &str) -> bool {
        !self.toolbar_hide_pending
            && self.toolbar_selection_id.as_deref() == Some(selection_id)
            && self
                .toolbar_recovery_selection_id
                .as_deref()
                .is_none_or(|id| id == selection_id)
    }

    fn reserve_toolbar_stage(
        &mut self,
        selection_id: &str,
        anchor: Point,
        placement: ToolbarPlacement,
    ) -> Option<(u64, WindowSize)> {
        // A late replay/stage may not steal ownership from a newer selection
        // already accepted by the selection-event consumer.
        if self
            .toolbar_recovery_selection_id
            .as_deref()
            .is_some_and(|id| id != selection_id)
        {
            return None;
        }
        let size = self.commit_toolbar_selection_with_placement(selection_id, anchor, placement);
        Some((self.invalidate_toolbar_operations(), size))
    }

    fn reserve_toolbar_operation(&mut self, selection_id: Option<&str>) -> Option<u64> {
        if selection_id.is_some() && self.toolbar_selection_id.as_deref() != selection_id {
            return None;
        }
        // Keep ownership for a failed-hide retry, but prevent a late renderer
        // present/resize from superseding this pending dismiss.
        self.toolbar_hide_pending = true;
        self.toolbar_reveal_requested = false;
        self.toolbar_present_pending = false;
        Some(self.invalidate_toolbar_operations())
    }

    fn clear_toolbar_if_current(&mut self, revision: u64) -> bool {
        if !self.toolbar_operation_is_current(revision) {
            return false;
        }
        self.toolbar_anchor = None;
        self.toolbar_placement = None;
        self.toolbar_selection_id = None;
        self.toolbar_hide_pending = false;
        self.toolbar_reveal_requested = false;
        self.toolbar_present_pending = false;
        true
    }

    #[cfg(test)]
    fn commit_toolbar_selection(&mut self, selection_id: &str, anchor: Point) -> WindowSize {
        self.commit_toolbar_selection_with_placement(
            selection_id,
            anchor,
            ToolbarPlacement::BottomMiddle,
        )
    }

    /// Records the toolbar's current selection ownership and returns the size
    /// that should be used for layout. Callers must drop the `WindowState` lock
    /// before any work that waits on the UI thread.
    fn commit_toolbar_selection_with_placement(
        &mut self,
        selection_id: &str,
        anchor: Point,
        placement: ToolbarPlacement,
    ) -> WindowSize {
        self.toolbar_anchor = Some(anchor);
        self.toolbar_placement = Some(placement);
        self.toolbar_selection_id = Some(selection_id.to_owned());
        self.toolbar_hide_pending = false;
        self.toolbar_reveal_requested = false;
        self.toolbar_present_pending = false;
        self.toolbar_size
    }

    #[cfg(any(test, target_os = "windows", target_os = "macos"))]
    fn reserve_toolbar_recovery(&mut self, selection_id: &str) -> bool {
        if self.toolbar_recovery_selection_id.as_deref() != Some(selection_id)
            || self.toolbar_recovery_attempts >= 1
        {
            return false;
        }
        self.toolbar_recovery_attempts += 1;
        self.invalidate_toolbar_operations();
        self.toolbar_recovery_pending_selection_id = Some(selection_id.to_owned());
        true
    }

    #[cfg(any(test, target_os = "windows", target_os = "macos"))]
    fn take_toolbar_recovery_pending(&mut self) -> Option<String> {
        self.toolbar_recovery_pending_selection_id.take()
    }

    #[cfg(any(test, target_os = "windows", target_os = "macos"))]
    fn cancel_toolbar_recovery(&mut self, selection_id: &str) {
        if self.toolbar_recovery_pending_selection_id.as_deref() == Some(selection_id) {
            self.toolbar_recovery_pending_selection_id = None;
        }
        if self.toolbar_recovery_selection_id.as_deref() == Some(selection_id) {
            self.toolbar_recovery_attempts = self.toolbar_recovery_attempts.saturating_sub(1);
        }
    }
}

type ToolbarNativeJob = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct ToolbarNativeQueue {
    running: bool,
    pending: VecDeque<ToolbarNativeJob>,
}

fn enqueue_toolbar_native_job(queue: &Arc<Mutex<ToolbarNativeQueue>>, job: ToolbarNativeJob) {
    {
        let mut queue = queue.lock();
        queue.pending.push_back(job);
        if queue.running {
            return;
        }
        queue.running = true;
    }
    loop {
        let next = {
            let mut queue = queue.lock();
            match queue.pending.pop_front() {
                Some(job) => job,
                None => {
                    queue.running = false;
                    return;
                }
            }
        };
        // A native call can synchronously invoke another window-event
        // callback. That callback only enqueues its transaction; the outer
        // drain executes it after the current native transaction completes.
        next();
    }
}

fn toolbar_is_ui_thread() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2::MainThreadMarker::new().is_some()
    }
    #[cfg(target_os = "windows")]
    {
        is_windows_ui_thread()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        true
    }
}

#[derive(Clone)]
pub struct WindowCoordinator {
    state: Arc<Mutex<WindowState>>,
    /// Serializes native commits on the UI thread, including callbacks that
    /// reenter from SetWindowPos/AppKit. This mutex protects only the queue;
    /// native work, monitor getters and dispatcher waits never hold it.
    toolbar_operation: Arc<Mutex<ToolbarNativeQueue>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    toolbar_tracking_generation: Arc<AtomicU64>,
}

impl Default for WindowCoordinator {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(WindowState {
                toolbar_revision: 0,
                toolbar_hide_pending: false,
                toolbar_reveal_requested: false,
                toolbar_present_pending: false,
                toolbar_size: WindowSize {
                    width: 480.0,
                    height: 40.0,
                },
                toolbar_anchor: None,
                toolbar_placement: None,
                toolbar_selection_id: None,
                toolbar_recovery_selection_id: None,
                toolbar_recovery_attempts: 0,
                toolbar_recovery_pending_selection_id: None,
                results: HashMap::new(),
            })),
            toolbar_operation: Arc::new(Mutex::new(ToolbarNativeQueue::default())),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            toolbar_tracking_generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl WindowCoordinator {
    fn run_toolbar_operation_async(
        &self,
        window: &WebviewWindow,
        operation: impl FnOnce(&WebviewWindow) -> tauri::Result<bool> + Send + 'static,
    ) -> tauri::Result<()> {
        let queue = Arc::clone(&self.toolbar_operation);
        let native_window = window.clone();
        window.app_handle().run_on_main_thread(move || {
            enqueue_toolbar_native_job(
                &queue,
                Box::new(move || {
                    let _ = operation(&native_window);
                }),
            );
        })
    }

    fn run_toolbar_operation(
        &self,
        window: &WebviewWindow,
        operation: impl FnOnce(&WebviewWindow) -> tauri::Result<bool> + Send + 'static,
    ) -> tauri::Result<Option<bool>> {
        let on_ui_thread = toolbar_is_ui_thread();
        let native_window = window.clone();
        let queue = Arc::clone(&self.toolbar_operation);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        // 0 = pending, 1 = running, 2 = cancelled before start.
        let dispatch = Arc::new(AtomicU8::new(0));
        let callback_dispatch = Arc::clone(&dispatch);
        let callback = move || {
            enqueue_toolbar_native_job(
                &queue,
                Box::new(move || {
                    if callback_dispatch
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        return;
                    }
                    let _ = sender.send(operation(&native_window));
                }),
            );
        };
        if on_ui_thread {
            callback();
            // A reentrant main-thread handler must never wait for the native
            // transaction it interrupted. Its queued operation commits when
            // the outer drain resumes; callers treat None as accepted.
            return match receiver.try_recv() {
                Ok(result) => result.map(Some),
                Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err(tauri::Error::FailedToReceiveMessage)
                }
            };
        }
        window.app_handle().run_on_main_thread(callback)?;
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result.map(Some),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(tauri::Error::FailedToReceiveMessage)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = dispatch.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire);
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "toolbar native operation timed out",
                )
                .into())
            }
        }
    }

    pub fn begin_toolbar_selection(&self, selection_id: &str) {
        self.state.lock().begin_toolbar_selection(selection_id);
    }

    pub fn ensure_toolbar(&self, app: &AppHandle) -> tauri::Result<WebviewWindow> {
        if let Some(window) = app.get_webview_window(TOOLBAR_LABEL) {
            return Ok(window);
        }

        #[cfg(target_os = "windows")]
        {
            if is_windows_ui_thread() {
                return self.create_toolbar_if_missing(app);
            }

            // Window creation and native configuration are one UI-thread
            // transaction. Multiple callers are serialized by the event loop
            // and each callback rechecks the singleton label before building.
            let coordinator = self.clone();
            let callback_app = app.clone();
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            app.run_on_main_thread(move || {
                let _ = sender.send(coordinator.create_toolbar_if_missing(&callback_app));
            })?;
            return receiver
                .recv_timeout(WINDOWS_WINDOW_OPERATION_TIMEOUT)
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "toolbar creation timed out",
                    )
                    .into(),
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        tauri::Error::FailedToReceiveMessage
                    }
                })?;
        }

        #[cfg(not(target_os = "windows"))]
        self.create_toolbar_if_missing(app)
    }

    fn create_toolbar_if_missing(&self, app: &AppHandle) -> tauri::Result<WebviewWindow> {
        if let Some(window) = app.get_webview_window(TOOLBAR_LABEL) {
            return Ok(window);
        }

        let size = self.state.lock().toolbar_size;
        let window = WebviewWindowBuilder::new(
            app,
            TOOLBAR_LABEL,
            WebviewUrl::App("toolbar/index.html".into()),
        )
        .title("TextLens")
        .inner_size(size.width, size.height)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .fullscreen(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .focusable(false)
        .focused(false)
        .accept_first_mouse(true)
        .visible(false)
        .build()?;

        if let Err(error) = configure_toolbar_native(&window) {
            let _ = window.destroy();
            return Err(error);
        }
        Ok(window)
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn take_toolbar_recovery_pending(&self) -> bool {
        self.state.lock().take_toolbar_recovery_pending().is_some()
    }

    /// Replaces a toolbar WebView whose renderer or IPC channel stopped
    /// responding. `WindowEvent::Destroyed` recreates the singleton after the
    /// old label has been removed from Tauri's window registry; the new
    /// renderer replays the in-memory selection through `toolbar_ready`.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn rebuild_toolbar(&self, app: &AppHandle, selection_id: &str) -> tauri::Result<bool> {
        {
            let mut state = self.state.lock();
            if state.toolbar_recovery_pending_selection_id.is_some() {
                return Ok(true);
            }
            // A persistent machine-wide WebView failure must not create an
            // unbounded destroy/recreate loop. A new selection has a new ID
            // and receives its own single recovery attempt.
            if !state.reserve_toolbar_recovery(selection_id) {
                return Ok(false);
            }
        }
        self.stop_toolbar_pointer_tracking();
        let result = if let Some(window) = app.get_webview_window(TOOLBAR_LABEL) {
            window.destroy()
        } else {
            // There will be no Destroyed event to consume the request.
            self.state.lock().take_toolbar_recovery_pending();
            self.ensure_toolbar(app).map(|_| ())
        };
        if result.is_err() {
            self.state.lock().cancel_toolbar_recovery(selection_id);
        }
        result.map(|_| true)
    }

    pub fn show_toolbar(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
    ) -> tauri::Result<()> {
        self.show_toolbar_with_placement(app, selection_id, anchor, ToolbarPlacement::BottomMiddle)
    }

    pub fn show_toolbar_with_placement(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
        placement: ToolbarPlacement,
    ) -> tauri::Result<()> {
        let reserved = {
            let mut state = self.state.lock();
            state
                .reserve_toolbar_stage(selection_id, anchor, placement)
                .map(|(_, size)| (state.request_toolbar_reveal(), size))
        };
        let Some((revision, size)) = reserved else {
            return Ok(());
        };
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.stop_toolbar_pointer_tracking();
        let window = self.ensure_toolbar(app)?;
        let layout = toolbar_layout_with_placement(&window, anchor, size, placement)?;
        let coordinator = self.clone();
        let callback_app = app.clone();
        self.run_toolbar_operation(&window, move |window| {
            if !coordinator
                .state
                .lock()
                .toolbar_operation_is_current(revision)
            {
                return Ok(false);
            }
            #[cfg(not(target_os = "windows"))]
            stage_toolbar_window_raw(window, layout)?;
            if !coordinator
                .state
                .lock()
                .toolbar_operation_is_current(revision)
            {
                return Ok(false);
            }
            present_toolbar_window_raw(window, layout)?;
            {
                let mut state = coordinator.state.lock();
                if !state.toolbar_operation_is_current(revision) {
                    return Ok(false);
                }
                state.toolbar_present_pending = false;
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            coordinator.start_toolbar_pointer_tracking(&callback_app, window);
            Ok(true)
        })?;
        Ok(())
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn stage_toolbar(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
    ) -> tauri::Result<()> {
        self.stage_toolbar_with_placement(app, selection_id, anchor, ToolbarPlacement::BottomMiddle)
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn stage_toolbar_with_placement(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
        placement: ToolbarPlacement,
    ) -> tauri::Result<()> {
        // Reserve ownership before potentially blocking monitor reads. A later
        // input/hide/rebuild invalidates this revision while layout is computed.
        let reserved = self
            .state
            .lock()
            .reserve_toolbar_stage(selection_id, anchor, placement);
        let Some((revision, size)) = reserved else {
            return Ok(());
        };
        self.stop_toolbar_pointer_tracking();
        let window = self.ensure_toolbar(app)?;
        let layout = toolbar_layout_with_placement(&window, anchor, size, placement)?;
        let state = Arc::clone(&self.state);
        self.run_toolbar_operation(&window, move |window| {
            if !state.lock().toolbar_operation_is_current(revision) {
                return Ok(false);
            }
            stage_toolbar_window_raw(window, layout)?;
            Ok(state.lock().toolbar_operation_is_current(revision))
        })?;
        Ok(())
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn present_toolbar(
        &self,
        app: &AppHandle,
        selection_id: &str,
        size: WindowSize,
    ) -> tauri::Result<bool> {
        let size = toolbar_size(size);
        let reserved = {
            let mut state = self.state.lock();
            if !state.toolbar_selection_is_current(selection_id) {
                return Ok(false);
            }
            let Some(anchor) = state.toolbar_anchor else {
                return Ok(false);
            };
            let placement = state.toolbar_placement.unwrap_or_default();
            let revision = state.request_toolbar_reveal();
            (revision, anchor, placement)
        };
        let (revision, anchor, placement) = reserved;
        let window = self.ensure_toolbar(app)?;
        let layout = toolbar_layout_with_placement(&window, anchor, size, placement)?;
        let coordinator = self.clone();
        let callback_app = app.clone();
        let native_started = Instant::now();
        self.run_toolbar_operation(&window, move |window| {
            if !coordinator
                .state
                .lock()
                .toolbar_operation_is_current(revision)
            {
                return Ok(false);
            }
            // macOS lays out behind alpha 0, including timeout retries.
            // Windows commits position, size and visibility in one SetWindowPos.
            #[cfg(not(target_os = "windows"))]
            stage_toolbar_window_raw(window, layout)?;
            if !coordinator
                .state
                .lock()
                .toolbar_operation_is_current(revision)
            {
                return Ok(false);
            }
            present_toolbar_window_raw(window, layout)?;
            {
                let mut state = coordinator.state.lock();
                if !state.toolbar_operation_is_current(revision) {
                    return Ok(false);
                }
                state.toolbar_size = size;
                state.toolbar_present_pending = false;
            }
            trace_toolbar_timing("native-visible", native_started);
            coordinator.start_toolbar_pointer_tracking(&callback_app, window);
            Ok(true)
        })
        .map(|result| result.unwrap_or(true))
    }

    pub fn hide_toolbar(&self, app: &AppHandle) {
        let _ = self.hide_toolbar_for_owner(app, None);
    }

    pub fn hide_toolbar_if_selection_async(&self, app: &AppHandle, selection_id: &str) {
        let Some(revision) = self
            .state
            .lock()
            .reserve_toolbar_operation(Some(selection_id))
        else {
            return;
        };
        let _ = self.hide_toolbar_revision(app, revision, true);
    }

    pub fn hide_toolbar_if_selection(&self, app: &AppHandle, selection_id: &str) -> bool {
        self.hide_toolbar_for_owner(app, Some(selection_id))
    }

    fn hide_toolbar_for_owner(&self, app: &AppHandle, selection_id: Option<&str>) -> bool {
        let Some(revision) = self.state.lock().reserve_toolbar_operation(selection_id) else {
            return false;
        };
        self.hide_toolbar_revision(app, revision, false)
    }

    fn hide_toolbar_revision(&self, app: &AppHandle, revision: u64, asynchronous: bool) -> bool {
        let Some(window) = app.get_webview_window(TOOLBAR_LABEL) else {
            let hidden = self.state.lock().clear_toolbar_if_current(revision);
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if hidden {
                self.stop_toolbar_pointer_tracking();
            }
            return hidden;
        };
        let coordinator = self.clone();
        let operation = move |window: &WebviewWindow| {
            if !coordinator
                .state
                .lock()
                .toolbar_operation_is_current(revision)
            {
                return Ok(false);
            }
            hide_toolbar_window_raw(window)?;
            let hidden = coordinator.state.lock().clear_toolbar_if_current(revision);
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if hidden {
                coordinator.stop_toolbar_pointer_tracking();
            }
            Ok(hidden)
        };
        if asynchronous {
            self.run_toolbar_operation_async(&window, operation).is_ok()
        } else {
            self.run_toolbar_operation(&window, operation)
                .map(|result| result.unwrap_or(true))
                .unwrap_or(false)
        }
    }

    /// Available renderer CSS width on the display containing this anchor.
    /// Called while building/replaying the selection payload, with no extra IPC.
    pub fn toolbar_max_width(&self, app: &AppHandle, anchor: Point) -> f64 {
        app.get_webview_window(TOOLBAR_LABEL)
            .and_then(|window| monitor_geometry_for_point(&window, anchor).ok().flatten())
            .map(toolbar_max_width_for_geometry)
            .unwrap_or(900.0)
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn stop_toolbar_pointer_tracking(&self) {
        self.toolbar_tracking_generation
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Sample the cursor at ~60 Hz and push hover updates over
    /// `TOOLBAR_POINTER_EVENT`. Required on both desktop platforms:
    /// - Windows: WebView2 does not reliably deliver mouse-move to a
    ///   non-activated toolbar window.
    /// - macOS: the toolbar is intentionally non-key; NSTrackingArea alone is
    ///   not enough for continuous hover across action buttons, so the same
    ///   sampler drives `data-hovered` (background / soft shadow feedback).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn start_toolbar_pointer_tracking(&self, app: &AppHandle, window: &WebviewWindow) {
        // Invalidating the previous generation before spawning a new sampler
        // prevents duplicate 60 Hz loops when a new selection reuses the
        // singleton toolbar window.
        let generation = self
            .toolbar_tracking_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let generation_ref = Arc::clone(&self.toolbar_tracking_generation);
        let app = app.clone();
        let window = window.clone();
        #[cfg(target_os = "windows")]
        let pointer_source = WindowsToolbarPointerSource::new(&window);

        std::thread::spawn(move || {
            // The toolbar must always open visually neutral, even when its
            // new frame happens to appear below a stationary cursor. Treat
            // the initial position as a baseline and only notify the
            // renderer after the pointer actually moves.
            #[cfg(target_os = "windows")]
            let mut last = pointer_source.and_then(WindowsToolbarPointerSource::position);
            #[cfg(target_os = "macos")]
            let mut last = toolbar_pointer_position(&app, &window);
            loop {
                if generation_ref.load(Ordering::Acquire) != generation {
                    break;
                }
                #[cfg(target_os = "windows")]
                if !pointer_source.is_some_and(WindowsToolbarPointerSource::is_visible) {
                    break;
                }
                #[cfg(target_os = "macos")]
                if !window.is_visible().unwrap_or(false) {
                    break;
                }

                #[cfg(target_os = "windows")]
                let position = pointer_source.and_then(WindowsToolbarPointerSource::position);
                #[cfg(target_os = "macos")]
                let position = toolbar_pointer_position(&app, &window);
                let payload = match position {
                    Some(payload) => payload,
                    None => {
                        std::thread::sleep(Duration::from_millis(16));
                        continue;
                    }
                };

                if last.is_none() {
                    last = Some(payload);
                    std::thread::sleep(Duration::from_millis(16));
                    continue;
                }

                let Some(previous) = last else {
                    continue;
                };
                let changed = toolbar_pointer_sample_changed(previous, payload);
                if generation_ref.load(Ordering::Acquire) != generation {
                    break;
                }
                if changed {
                    last = Some(payload);
                    // Movement anywhere outside the toolbar does not affect
                    // renderer state. Emit only while inside, plus one leave
                    // sample to clear the active action immediately.
                    if toolbar_pointer_sample_should_emit(previous, payload) {
                        let _ = app.emit_to(TOOLBAR_LABEL, TOOLBAR_POINTER_EVENT, payload);
                    }
                }

                std::thread::sleep(Duration::from_millis(16));
            }
        });
    }

    pub fn update_toolbar_size(
        &self,
        app: &AppHandle,
        selection_id: Option<&str>,
        size: WindowSize,
    ) -> tauri::Result<bool> {
        let size = toolbar_size(size);
        let reserved = {
            let mut state = self.state.lock();
            if state.toolbar_hide_pending {
                return Ok(false);
            }
            if selection_id.is_some_and(|id| !state.toolbar_selection_is_current(id)) {
                return Ok(false);
            }
            if state.toolbar_size == size && !state.toolbar_present_pending {
                return Ok(true);
            }
            let revision = state.invalidate_toolbar_operations();
            let Some(anchor) = state.toolbar_anchor else {
                state.toolbar_size = size;
                return Ok(true);
            };
            (
                revision,
                anchor,
                state.toolbar_placement.unwrap_or_default(),
                state.toolbar_reveal_requested,
            )
        };
        let Some(window) = app.get_webview_window(TOOLBAR_LABEL) else {
            return Ok(false);
        };
        let (revision, anchor, placement, reveal) = reserved;
        let layout = toolbar_layout_with_placement(&window, anchor, size, placement)?;
        let coordinator = self.clone();
        let callback_app = app.clone();
        self.run_toolbar_operation(&window, move |window| {
            if !coordinator
                .state
                .lock()
                .toolbar_operation_is_current(revision)
            {
                return Ok(false);
            }
            // A measured resize may overtake a pending presentation's monitor
            // read. Carry its reveal intent so the newest size still becomes
            // visible rather than cancelling the only show operation.
            if reveal {
                #[cfg(not(target_os = "windows"))]
                stage_toolbar_window_raw(window, layout)?;
                if !coordinator
                    .state
                    .lock()
                    .toolbar_operation_is_current(revision)
                {
                    return Ok(false);
                }
                present_toolbar_window_raw(window, layout)?;
            } else {
                resize_toolbar_window_raw(window, layout)?;
            }
            {
                let mut state = coordinator.state.lock();
                if !state.toolbar_operation_is_current(revision) {
                    return Ok(false);
                }
                state.toolbar_size = size;
                state.toolbar_present_pending = false;
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if reveal {
                coordinator.start_toolbar_pointer_tracking(&callback_app, window);
            }
            Ok(true)
        })
        .map(|result| result.unwrap_or(true))
    }

    pub fn point_inside_toolbar(&self, app: &AppHandle, point: Point) -> bool {
        let Some(window) = app.get_webview_window(TOOLBAR_LABEL) else {
            return false;
        };
        #[cfg(target_os = "windows")]
        {
            return windows_point_inside_window(&window, point);
        }
        #[cfg(not(target_os = "windows"))]
        {
            if !window.is_visible().unwrap_or(false) {
                return false;
            }
            let Ok(position) = window.outer_position() else {
                return false;
            };
            let Ok(size) = window.outer_size() else {
                return false;
            };
            let scale = window_coordinate_divisor(window.scale_factor().unwrap_or(1.0));
            let x = position.x as f64 / scale;
            let y = position.y as f64 / scale;
            let width = size.width as f64 / scale;
            let height = size.height as f64 / scale;
            point.x >= x && point.x <= x + width && point.y >= y && point.y <= y + height
        }
    }

    pub fn create_result_window(
        &self,
        app: &AppHandle,
        session_id: &str,
        title: &str,
        options: ResultWindowOptions,
    ) -> tauri::Result<WebviewWindow> {
        let label = result_label(session_id);
        let size = options.size.clamped();
        // Publish the reveal state before WebView construction. A very fast
        // renderer can issue its hydration handshake while `build()` is still
        // returning to the caller.
        self.state.lock().results.insert(
            label.clone(),
            ResultRuntime {
                session_id: session_id.to_owned(),
                pinned: options.pinned,
                opacity: options.opacity.clamp(0.2, 1.0),
                placement: ResultPlacement {
                    size,
                    cursor: options.cursor,
                    follow_cursor: options.follow_cursor,
                },
                reveal_phase: initial_result_reveal_phase(),
                reveal_operation: Arc::new(Mutex::new(())),
                focus_seen: false,
                blur_generation: 0,
                pointer_inside: false,
                pointer_seen: false,
                dismiss_mode: options.dismiss_mode,
                dismiss_delay_ms: options.dismiss_delay_ms.clamp(100, 5_000),
                hide_generation: 0,
                pending_auto_close: None,
                remember_size: options.remember_size,
                ignore_resize_until: initial_result_resize_ignore_until(),
            },
        );
        let builder = WebviewWindowBuilder::new(
            app,
            &label,
            WebviewUrl::App(format!("result/index.html?sessionId={session_id}").into()),
        )
        .title(format!("{title} - TextLens"))
        .inner_size(size.width, size.height)
        .min_inner_size(360.0, 260.0)
        .decorations(false)
        .transparent(true)
        .resizable(true)
        .minimizable(false)
        .fullscreen(false)
        .always_on_top(options.pinned)
        .skip_taskbar(true)
        .accept_first_mouse(true)
        .focusable(true)
        .focused(false)
        .visible(false);
        // The renderer owns the visible surface and already provides its own
        // rounded background. A native shadow around the transparent WebView
        // creates a second rectangular card, especially noticeable over PDFs.
        let builder = builder.shadow(false);
        let window = match builder
            .build()
            .map_err(|error| result_window_step_error("webview creation", error))
        {
            Ok(window) => window,
            Err(error) => {
                self.state.lock().results.remove(&label);
                return Err(error);
            }
        };
        // On Windows `build()` returns a detached runtime handle as soon as
        // the CreateWindow message is queued. Touching HWND, monitor getters
        // or window setters here races the real WebView2 creation and yields
        // `FailedToReceiveMessage`. The result renderer's prepare IPC is the
        // first deterministic proof that both the native window and WebView
        // exist, so all Windows native work is deliberately deferred there.
        // Both platforms reveal the first hydrated frame. Native preparation
        // remains transparent and never focuses an empty WebView.
        Ok(window)
    }

    /// Makes a fully hydrated result WebView participate in native
    /// composition while it is still completely transparent and inactive.
    /// Repeated calls are harmless and never move a committed window back to
    /// the transparent state.
    pub fn prepare_result_reveal(&self, app: &AppHandle, label: &str) -> tauri::Result<()> {
        let operation = self.result_reveal_operation(label)?;
        let _operation_guard = operation.lock();
        let (phase, pinned, placement) = self.result_reveal_context(label)?;
        if phase != ResultRevealPhase::Hidden {
            return Ok(());
        }
        let window = app
            .get_webview_window(label)
            .ok_or_else(|| result_reveal_error("result window no longer exists"))?;
        prepare_result_window(&window, pinned, placement)?;
        self.set_result_reveal_phase(
            label,
            ResultRevealPhase::Hidden,
            ResultRevealPhase::Prepared,
        )
    }

    /// Restores the configured alpha and focuses the already-composited result
    /// window. Returns true only for the call that performed the commit.
    pub fn commit_result_reveal(&self, app: &AppHandle, label: &str) -> tauri::Result<bool> {
        let operation = self.result_reveal_operation(label)?;
        let _operation_guard = operation.lock();
        let (phase, pinned, _) = self.result_reveal_context(label)?;
        match phase {
            ResultRevealPhase::Committed => return Ok(false),
            ResultRevealPhase::Hidden => {
                return Err(result_reveal_error("result window was not prepared"));
            }
            ResultRevealPhase::Prepared => {}
        }
        let opacity = self
            .state
            .lock()
            .results
            .get(label)
            .map(|runtime| runtime.opacity)
            .ok_or_else(|| result_reveal_error("result session no longer exists"))?;
        let window = app
            .get_webview_window(label)
            .ok_or_else(|| result_reveal_error("result window no longer exists"))?;
        show_result_window(&window, pinned, opacity)?;
        self.set_result_reveal_phase(
            label,
            ResultRevealPhase::Prepared,
            ResultRevealPhase::Committed,
        )?;
        if window.is_focused().unwrap_or(false) {
            if let Some(runtime) = self.state.lock().results.get_mut(label) {
                runtime.focus_seen = true;
            }
        }
        Ok(true)
    }

    fn result_reveal_operation(&self, label: &str) -> tauri::Result<Arc<Mutex<()>>> {
        self.state
            .lock()
            .results
            .get(label)
            .map(|runtime| Arc::clone(&runtime.reveal_operation))
            .ok_or_else(|| result_reveal_error("result session no longer exists"))
    }

    fn result_reveal_context(
        &self,
        label: &str,
    ) -> tauri::Result<(ResultRevealPhase, bool, ResultPlacement)> {
        self.state
            .lock()
            .results
            .get(label)
            .map(|runtime| (runtime.reveal_phase, runtime.pinned, runtime.placement))
            .ok_or_else(|| result_reveal_error("result session no longer exists"))
    }

    fn set_result_reveal_phase(
        &self,
        label: &str,
        expected: ResultRevealPhase,
        next: ResultRevealPhase,
    ) -> tauri::Result<()> {
        let mut state = self.state.lock();
        let runtime = state
            .results
            .get_mut(label)
            .ok_or_else(|| result_reveal_error("result session no longer exists"))?;
        if runtime.reveal_phase == next {
            return Ok(());
        }
        if runtime.reveal_phase != expected {
            return Err(result_reveal_error(
                "result reveal state changed unexpectedly",
            ));
        }
        runtime.reveal_phase = next;
        if next == ResultRevealPhase::Committed {
            // Only focus events observed after commit may arm blur dismissal.
            runtime.focus_seen = false;
            // Preserve a pointer currently inside the revealed surface, while
            // invalidating every timer and historical enter from preparation.
            runtime.pointer_seen = runtime.pointer_inside;
            runtime.hide_generation = runtime.hide_generation.wrapping_add(1);
            runtime.ignore_resize_until = Instant::now();
        }
        Ok(())
    }

    pub fn set_result_pinned(
        &self,
        app: &AppHandle,
        label: &str,
        pinned: bool,
    ) -> tauri::Result<()> {
        let (opacity, committed) = self
            .state
            .lock()
            .results
            .get(label)
            .map(|runtime| {
                (
                    runtime.opacity,
                    runtime.reveal_phase == ResultRevealPhase::Committed,
                )
            })
            .ok_or_else(|| result_reveal_error("result session no longer exists"))?;
        if committed {
            let window = app
                .get_webview_window(label)
                .ok_or_else(|| result_reveal_error("result window no longer exists"))?;
            apply_result_pinned(&window, pinned, opacity)?;
        }
        // Commit coordinator state only after the native mutation succeeds;
        // otherwise blur/pointer dismissal and the renderer session would
        // disagree about whether the result is pinned.
        let should_schedule = {
            let mut state = self.state.lock();
            let runtime = state
                .results
                .get_mut(label)
                .ok_or_else(|| result_reveal_error("result session no longer exists"))?;
            runtime.pinned = pinned;
            runtime.hide_generation = runtime.hide_generation.wrapping_add(1);
            runtime.pointer_leave_armed()
        };
        if should_schedule {
            self.schedule_hide(app.clone(), label.to_owned());
        }
        Ok(())
    }

    pub fn set_result_opacity(
        &self,
        app: &AppHandle,
        label: &str,
        opacity: f64,
    ) -> tauri::Result<()> {
        let should_apply = if let Some(runtime) = self.state.lock().results.get_mut(label) {
            runtime.opacity = opacity.clamp(0.2, 1.0);
            runtime.reveal_phase == ResultRevealPhase::Committed
        } else {
            false
        };
        // A setting change during hidden preparation must not accidentally
        // reveal an unpainted WebView. The committed value is read again by
        // `commit_result_reveal`.
        if should_apply {
            let Some(window) = app.get_webview_window(label) else {
                return Ok(());
            };
            set_native_opacity(&window, opacity)?;
        }
        Ok(())
    }

    pub fn update_result_behavior(
        &self,
        app: &AppHandle,
        dismiss_mode: DismissMode,
        dismiss_delay_ms: u64,
        remember_size: bool,
    ) {
        let mut schedule = Vec::new();
        {
            let mut state = self.state.lock();
            for (label, runtime) in &mut state.results {
                runtime.dismiss_mode = dismiss_mode;
                runtime.dismiss_delay_ms = dismiss_delay_ms.clamp(100, 5_000);
                runtime.remember_size = remember_size;
                runtime.hide_generation = runtime.hide_generation.wrapping_add(1);
                if runtime.pointer_leave_armed() {
                    schedule.push(label.clone());
                }
            }
        }
        for label in schedule {
            self.schedule_hide(app.clone(), label);
        }
    }

    pub fn set_pointer_inside(&self, app: &AppHandle, label: &str, inside: bool) {
        let should_schedule = {
            let mut state = self.state.lock();
            let Some(runtime) = state.results.get_mut(label) else {
                return;
            };
            runtime.note_pointer_change(inside)
        };
        if should_schedule {
            self.schedule_hide(app.clone(), label.to_owned());
        }
    }

    pub fn on_focus_changed(&self, app: &AppHandle, label: &str, focused: bool) {
        let blur_generation = self
            .state
            .lock()
            .results
            .get_mut(label)
            .and_then(|runtime| runtime.note_focus_change(focused));
        #[cfg(not(target_os = "windows"))]
        if let Some(blur_generation) = blur_generation {
            let _ =
                self.request_result_auto_close(app, label, AutoCloseOrigin::Blur(blur_generation));
        }
        #[cfg(target_os = "windows")]
        if let Some(blur_generation) = blur_generation {
            self.schedule_verified_result_blur(app.clone(), label.to_owned(), blur_generation);
        }
    }

    #[cfg(target_os = "windows")]
    fn schedule_verified_result_blur(&self, app: AppHandle, label: String, blur_generation: u64) {
        let state = Arc::clone(&self.state);
        let coordinator = self.clone();
        std::thread::spawn(move || {
            for _ in 0..RESULT_BLUR_VERIFY_ATTEMPTS {
                std::thread::sleep(RESULT_BLUR_VERIFY_DELAY);
                let still_current = state
                    .lock()
                    .results
                    .get(&label)
                    .is_some_and(|runtime| runtime.blur_is_current(blur_generation));
                if !still_current {
                    return;
                }
                let Some(window) = app.get_webview_window(&label) else {
                    return;
                };
                match result_foreground_scope(&app, &window) {
                    ResultForegroundScope::Internal => return,
                    ResultForegroundScope::Unknown => continue,
                    ResultForegroundScope::External => {
                        let _ = coordinator.request_result_auto_close(
                            &app,
                            &label,
                            AutoCloseOrigin::Blur(blur_generation),
                        );
                        return;
                    }
                }
            }
        });
    }

    pub fn close_result(&self, app: &AppHandle, label: &str) -> tauri::Result<()> {
        if let Some(runtime) = self.state.lock().results.get_mut(label) {
            // Explicit closes retain their ordinary close semantics.
            runtime.cancel_auto_close_request();
        }
        if let Some(window) = app.get_webview_window(label) {
            window.close()?;
        } else {
            self.state.lock().results.remove(label);
        }
        Ok(())
    }

    fn request_result_auto_close(
        &self,
        app: &AppHandle,
        label: &str,
        origin: AutoCloseOrigin,
    ) -> tauri::Result<()> {
        let Some(window) = app.get_webview_window(label) else {
            return Ok(());
        };
        let request = self
            .state
            .lock()
            .results
            .get_mut(label)
            .is_some_and(|runtime| runtime.prepare_auto_close_request(origin));
        if !request {
            return Ok(());
        }
        if let Err(error) = window.close() {
            if let Some(runtime) = self.state.lock().results.get_mut(label) {
                if runtime.pending_auto_close == Some(origin) {
                    runtime.cancel_auto_close_request();
                }
            }
            return Err(error);
        }
        Ok(())
    }

    /// Revalidates a queued automatic dismissal at Tauri's final CloseRequested
    /// boundary. Requests with no automatic token are ordinary/manual closes.
    pub fn accept_result_close_request(&self, app: &AppHandle, label: &str) -> bool {
        let decision = verified_result_close_request_decision(
            &self.state,
            label,
            || {
                (
                    cursor_is_outside(app, label),
                    cursor_is_inside_visible_window(app, TOOLBAR_LABEL),
                )
            },
            || {
                let Some(window) = app.get_webview_window(label) else {
                    return ResultForegroundScope::Unknown;
                };
                #[cfg(target_os = "windows")]
                {
                    result_foreground_scope(app, &window)
                }
                #[cfg(not(target_os = "windows"))]
                {
                    match window.is_focused() {
                        Ok(true) => ResultForegroundScope::Internal,
                        Ok(false) => ResultForegroundScope::External,
                        Err(_) => ResultForegroundScope::Unknown,
                    }
                }
            },
        );
        match decision {
            PointerLeaveTimeoutDecision::Close => true,
            PointerLeaveTimeoutDecision::Retry => {
                self.schedule_hide(app.clone(), label.to_owned());
                false
            }
            PointerLeaveTimeoutDecision::Ignore => false,
        }
    }

    pub fn remove_result(&self, label: &str) -> Option<String> {
        self.state
            .lock()
            .results
            .remove(label)
            .map(|runtime| runtime.session_id)
    }

    pub fn session_for_label(&self, label: &str) -> Option<String> {
        self.state
            .lock()
            .results
            .get(label)
            .map(|runtime| runtime.session_id.clone())
    }

    pub fn note_result_resize(&self, label: &str) -> bool {
        let mut state = self.state.lock();
        let Some(runtime) = state.results.get_mut(label) else {
            return false;
        };
        runtime.reveal_phase == ResultRevealPhase::Committed
            && runtime.remember_size
            && Instant::now() >= runtime.ignore_resize_until
    }

    pub fn ignore_resize_temporarily(&self, label: &str) {
        if let Some(runtime) = self.state.lock().results.get_mut(label) {
            runtime.ignore_resize_until = Instant::now() + std::time::Duration::from_millis(750);
        }
    }

    fn schedule_hide(&self, app: AppHandle, label: String) {
        let (generation, delay) = {
            let state = self.state.lock();
            let Some(runtime) = state.results.get(&label) else {
                return;
            };
            (runtime.hide_generation, runtime.dismiss_delay_ms)
        };
        let coordinator = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            let callback_app = app.clone();
            // Geometry getters and the close must share the UI task. A worker
            // must not close using a snapshot taken before queued input/pin IPC.
            let _ = app.run_on_main_thread(move || {
                if callback_app.get_webview_window(&label).is_none() {
                    return;
                }
                let decision = verified_pointer_leave_timeout_decision(
                    &coordinator.state,
                    &label,
                    generation,
                    || {
                        (
                            cursor_is_outside(&callback_app, &label),
                            cursor_is_inside_visible_window(&callback_app, TOOLBAR_LABEL),
                        )
                    },
                );
                match decision {
                    PointerLeaveTimeoutDecision::Close => {
                        // The helper rechecked the token after all native reads;
                        // no state lock remains held across Destroyed callbacks.
                        let _ = coordinator.request_result_auto_close(
                            &callback_app,
                            &label,
                            AutoCloseOrigin::PointerLeave(generation),
                        );
                    }
                    PointerLeaveTimeoutDecision::Retry => {
                        // The toolbar temporarily extends the result's surface.
                        coordinator.schedule_hide(callback_app, label);
                    }
                    PointerLeaveTimeoutDecision::Ignore => {}
                }
            });
        });
    }
}

pub fn result_label(session_id: &str) -> String {
    format!("selection-result-{session_id}")
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct WorkArea {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MonitorGeometry {
    bounds: WorkArea,
    work_area: WorkArea,
    /// Converts a logical window dimension into the coordinate space used by
    /// `bounds` and `work_area`. This is the target display scale on Windows,
    /// where points are physical pixels, and 1.0 on macOS/Linux, where the
    /// existing logical-coordinate path is retained.
    coordinate_scale: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct WindowLayout {
    position: Point,
    coordinate_size: WindowSize,
}

fn toolbar_layout_with_placement(
    window: &WebviewWindow,
    point: Point,
    logical_size: WindowSize,
    placement: ToolbarPlacement,
) -> tauri::Result<WindowLayout> {
    // Window creation can briefly precede monitor enumeration on Windows.
    // Placement still has a safe cursor-relative fallback, so a transient
    // monitor query failure must not suppress an otherwise valid selection.
    let geometry = monitor_geometry_for_point(window, point).ok().flatten();
    let coordinate_scale = geometry
        .map(|geometry| geometry.coordinate_scale)
        .unwrap_or_else(|| current_window_coordinate_scale(window));
    let coordinate_size = toolbar_native_frame_size(
        logical_size_to_coordinates(logical_size, coordinate_scale),
        coordinate_scale,
    );
    let position = geometry.map_or_else(
        || toolbar_position_without_area(point, coordinate_size, placement),
        |geometry| {
            toolbar_position_in_area_with_placement(
                point,
                coordinate_size,
                geometry.work_area,
                coordinate_scale,
                placement,
            )
        },
    );
    Ok(WindowLayout {
        position,
        coordinate_size,
    })
}

fn result_layout(
    window: &WebviewWindow,
    point: Point,
    logical_size: WindowSize,
) -> tauri::Result<WindowLayout> {
    let geometry = monitor_geometry_for_point(window, point)?;
    let coordinate_scale = geometry
        .map(|geometry| geometry.coordinate_scale)
        .unwrap_or_else(|| current_window_coordinate_scale(window));
    let coordinate_size = logical_size_to_coordinates(logical_size, coordinate_scale);
    let position = geometry.map_or_else(
        || Point {
            x: point.x - coordinate_size.width * RESULT_CURSOR_LEFT_ANCHOR_RATIO,
            y: point.y - coordinate_size.height / 2.0
                + coordinate_size.height * RESULT_VERTICAL_SHIFT_RATIO,
        },
        |geometry| {
            result_position_in_area(point, coordinate_size, geometry.work_area, coordinate_scale)
        },
    );
    Ok(WindowLayout {
        position,
        coordinate_size,
    })
}

#[cfg(test)]
fn toolbar_position_in_area(
    point: Point,
    size: WindowSize,
    area: WorkArea,
    coordinate_scale: f64,
) -> Point {
    toolbar_position_in_area_with_placement(
        point,
        size,
        area,
        coordinate_scale,
        ToolbarPlacement::BottomMiddle,
    )
}

fn toolbar_position_without_area(
    point: Point,
    size: WindowSize,
    placement: ToolbarPlacement,
) -> Point {
    let x = match placement {
        ToolbarPlacement::BottomLeft => point.x - size.width,
        ToolbarPlacement::BottomMiddle => point.x - size.width / 2.0,
        ToolbarPlacement::BottomRight | ToolbarPlacement::TopRight => point.x,
    };
    let y = match placement {
        ToolbarPlacement::TopRight => point.y - size.height,
        ToolbarPlacement::BottomLeft
        | ToolbarPlacement::BottomMiddle
        | ToolbarPlacement::BottomRight => point.y,
    };
    Point { x, y }
}

fn toolbar_position_in_area_with_placement(
    point: Point,
    size: WindowSize,
    area: WorkArea,
    coordinate_scale: f64,
    placement: ToolbarPlacement,
) -> Point {
    let margin = TOOLBAR_SCREEN_MARGIN * coordinate_scale;
    let left = area.x + margin;
    let right = area.x + area.width - margin;
    let top = area.y + margin;
    let bottom = area.y + area.height - margin;

    // `point` is Cherry's reference point: the toolbar grows left/right from
    // the selected endpoint and above it for a backward multi-line range.
    // Work-area clamping takes precedence near taskbars and display edges.
    let preferred = toolbar_position_without_area(point, size, placement);
    let x = preferred.x.clamp(left, (right - size.width).max(left));
    let y = preferred.y.clamp(top, (bottom - size.height).max(top));
    Point { x, y }
}

#[cfg(any(target_os = "windows", test))]
fn startup_notice_position_in_area(
    size: WindowSize,
    area: WorkArea,
    coordinate_scale: f64,
) -> Point {
    let margin = STARTUP_NOTICE_MARGIN * coordinate_scale;
    let left = area.x + margin;
    let right = area.x + area.width - margin;
    let top = area.y + margin;
    Point {
        // Keep the short-lived notice visually centered on the display the
        // pointer is currently using. Work-area clamping still wins on very
        // small displays and prevents the notice from crossing an edge.
        x: (area.x + (area.width - size.width) / 2.0).clamp(left, (right - size.width).max(left)),
        y: (area.y + area.height - margin - size.height).max(top),
    }
}

fn result_position_in_area(
    point: Point,
    size: WindowSize,
    area: WorkArea,
    coordinate_scale: f64,
) -> Point {
    let margin = SCREEN_MARGIN * coordinate_scale;
    let left = area.x + margin;
    let right = area.x + area.width - margin;
    let top = area.y + margin;
    let bottom = area.y + area.height - margin;

    // Anchor the action click at 20% of the result width, then shift down by
    // 30% of its height. Near display edges the complete window takes
    // priority over the preferred click-relative position.
    let x = (point.x - size.width * RESULT_CURSOR_LEFT_ANCHOR_RATIO)
        .clamp(left, (right - size.width).max(left));
    let y = (point.y - size.height / 2.0 + size.height * RESULT_VERTICAL_SHIFT_RATIO)
        .clamp(top, (bottom - size.height).max(top));
    Point { x, y }
}

fn centered_layout(
    window: &WebviewWindow,
    point: Point,
    logical_size: WindowSize,
) -> tauri::Result<WindowLayout> {
    let geometry = monitor_geometry_for_point(window, point)?;
    let coordinate_scale = geometry
        .map(|geometry| geometry.coordinate_scale)
        .unwrap_or_else(|| current_window_coordinate_scale(window));
    let coordinate_size = logical_size_to_coordinates(logical_size, coordinate_scale);
    let position = geometry.map_or(point, |geometry| Point {
        x: geometry.work_area.x + (geometry.work_area.width - coordinate_size.width) / 2.0,
        y: geometry.work_area.y + (geometry.work_area.height - coordinate_size.height) / 2.0,
    });
    Ok(WindowLayout {
        position,
        coordinate_size,
    })
}

fn fit_size_to_monitor(
    window: &WebviewWindow,
    point: Point,
    size: WindowSize,
) -> tauri::Result<WindowSize> {
    let Some(geometry) = monitor_geometry_for_point(window, point)? else {
        return Ok(size.clamped());
    };
    Ok(fit_logical_size_to_geometry(size, geometry))
}

fn fit_logical_size_to_geometry(size: WindowSize, geometry: MonitorGeometry) -> WindowSize {
    let scale = normalized_scale(geometry.coordinate_scale);
    let max_width = (geometry.work_area.width / scale - SCREEN_MARGIN * 2.0).max(360.0);
    let max_height = (geometry.work_area.height / scale - SCREEN_MARGIN * 2.0).max(260.0);
    WindowSize {
        width: size.width.min(max_width),
        height: size.height.min(max_height),
    }
    .clamped()
}

fn monitor_geometry_for_point(
    window: &WebviewWindow,
    point: Point,
) -> tauri::Result<Option<MonitorGeometry>> {
    let geometries = window
        .available_monitors()?
        .iter()
        .map(monitor_geometry)
        .collect::<Vec<_>>();
    Ok(monitor_index_for_point(&geometries, point).map(|index| geometries[index]))
}

fn monitor_index_for_point(monitors: &[MonitorGeometry], point: Point) -> Option<usize> {
    let mut nearest: Option<(f64, usize)> = None;
    for (index, monitor) in monitors.iter().enumerate() {
        let area = monitor.bounds;
        // Half-open bounds make a point on a shared edge belong to the display
        // that starts at that edge instead of whichever monitor Tauri lists
        // first.
        if point.x >= area.x
            && point.x < area.x + area.width
            && point.y >= area.y
            && point.y < area.y + area.height
        {
            return Some(index);
        }
        let dx = (area.x - point.x)
            .max(0.0)
            .max(point.x - (area.x + area.width));
        let dy = (area.y - point.y)
            .max(0.0)
            .max(point.y - (area.y + area.height));
        let distance = dx * dx + dy * dy;
        if nearest.as_ref().is_none_or(|(best, _)| distance < *best) {
            nearest = Some((distance, index));
        }
    }
    nearest.map(|(_, index)| index)
}

fn monitor_geometry(monitor: &tauri::Monitor) -> MonitorGeometry {
    let scale = normalized_scale(monitor.scale_factor());
    let bounds = coordinate_area(
        monitor.position().x as f64,
        monitor.position().y as f64,
        monitor.size().width as f64,
        monitor.size().height as f64,
        scale,
    );
    let work_area = monitor.work_area();
    MonitorGeometry {
        bounds,
        work_area: coordinate_area(
            work_area.position.x as f64,
            work_area.position.y as f64,
            work_area.size.width as f64,
            work_area.size.height as f64,
            scale,
        ),
        coordinate_scale: monitor_coordinate_scale(scale),
    }
}

fn logical_size_to_coordinates(size: WindowSize, coordinate_scale: f64) -> WindowSize {
    let scale = normalized_scale(coordinate_scale);
    WindowSize {
        width: size.width * scale,
        height: size.height * scale,
    }
}

fn toolbar_native_frame_size(size: WindowSize, coordinate_scale: f64) -> WindowSize {
    #[cfg(target_os = "windows")]
    {
        let scale = normalized_scale(coordinate_scale);
        // The renderer reports CSS pixels while SetWindowPos consumes physical
        // pixels. Base the guard on the measured frame width, then bias it by
        // the active scale so narrow icon-only bars get only the clearance
        // they need while fractional DPI still has room for rounding drift.
        let width_guard = (size.width * WINDOWS_TOOLBAR_TRAILING_GUARD_RATIO).ceil();
        let dpi_guard = (scale * 4.0).ceil();
        let guard = width_guard.max(dpi_guard).clamp(
            WINDOWS_TOOLBAR_TRAILING_GUARD_MIN,
            WINDOWS_TOOLBAR_TRAILING_GUARD_MAX,
        );
        WindowSize {
            width: size.width + guard,
            height: size.height,
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = coordinate_scale;
        size
    }
}

fn toolbar_max_width_for_geometry(geometry: MonitorGeometry) -> f64 {
    let available = geometry.work_area.width / normalized_scale(geometry.coordinate_scale)
        - TOOLBAR_SCREEN_MARGIN * 2.0;
    if available.is_finite() {
        available.clamp(44.0, 900.0)
    } else {
        900.0
    }
}

fn normalized_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

#[cfg(target_os = "windows")]
fn coordinate_area(x: f64, y: f64, width: f64, height: f64, _scale: f64) -> WorkArea {
    WorkArea {
        x,
        y,
        width,
        height,
    }
}

#[cfg(not(target_os = "windows"))]
fn coordinate_area(x: f64, y: f64, width: f64, height: f64, scale: f64) -> WorkArea {
    WorkArea {
        x: x / scale,
        y: y / scale,
        width: width / scale,
        height: height / scale,
    }
}

#[cfg(target_os = "windows")]
fn monitor_coordinate_scale(scale: f64) -> f64 {
    scale
}

#[cfg(not(target_os = "windows"))]
fn monitor_coordinate_scale(_scale: f64) -> f64 {
    1.0
}

#[cfg(not(target_os = "windows"))]
fn window_coordinate_divisor(scale: f64) -> f64 {
    normalized_scale(scale)
}

#[cfg(target_os = "windows")]
fn current_window_coordinate_scale(window: &WebviewWindow) -> f64 {
    normalized_scale(window.scale_factor().unwrap_or(1.0))
}

#[cfg(not(target_os = "windows"))]
fn current_window_coordinate_scale(_window: &WebviewWindow) -> f64 {
    1.0
}

#[cfg(target_os = "windows")]
fn apply_window_layout(window: &WebviewWindow, layout: WindowLayout) -> tauri::Result<()> {
    window.set_position(PhysicalPosition::new(
        physical_coordinate(layout.position.x),
        physical_coordinate(layout.position.y),
    ))?;
    window.set_size(PhysicalSize::new(
        physical_dimension(layout.coordinate_size.width),
        physical_dimension(layout.coordinate_size.height),
    ))
}

#[cfg(target_os = "windows")]
fn apply_toolbar_layout_raw(
    window: &WebviewWindow,
    layout: WindowLayout,
    show: bool,
) -> tauri::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    };

    let hwnd = window.hwnd()?;
    let flags = SWP_NOACTIVATE
        | if show {
            SWP_SHOWWINDOW
        } else {
            SET_WINDOW_POS_FLAGS(0)
        };
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            physical_coordinate(layout.position.x),
            physical_coordinate(layout.position.y),
            physical_dimension(layout.coordinate_size.width) as i32,
            physical_dimension(layout.coordinate_size.height) as i32,
            flags,
        )
        .map_err(windows_error)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn hide_toolbar_window_raw(window: &WebviewWindow) -> tauri::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };

    let hwnd = window.hwnd()?;
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        )
        .map_err(windows_error)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn hide_toolbar_window_raw(window: &WebviewWindow) -> tauri::Result<()> {
    window.hide()
}

fn stage_toolbar_window_raw(window: &WebviewWindow, layout: WindowLayout) -> tauri::Result<()> {
    #[cfg(target_os = "windows")]
    {
        hide_toolbar_window_raw(window)?;
        apply_toolbar_layout_raw(window, layout, false)
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSWindow;
        let pointer = window.ns_window()?;
        unsafe {
            (&*(pointer as *const NSWindow)).setAlphaValue(0.0);
        }
        apply_window_layout(window, layout)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        window.hide()?;
        apply_window_layout(window, layout)
    }
}

fn present_toolbar_window_raw(window: &WebviewWindow, layout: WindowLayout) -> tauri::Result<()> {
    #[cfg(target_os = "windows")]
    {
        apply_toolbar_layout_raw(window, layout, true)
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSWindow;
        // The same UI transaction already applied the hidden layout.
        let _ = layout;
        let pointer = window.ns_window()?;
        unsafe {
            let window = &*(pointer as *const NSWindow);
            window.orderFrontRegardless();
            window.setAlphaValue(1.0);
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = layout;
        window.show()
    }
}

fn resize_toolbar_window_raw(window: &WebviewWindow, layout: WindowLayout) -> tauri::Result<()> {
    #[cfg(target_os = "windows")]
    {
        apply_toolbar_layout_raw(window, layout, false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        apply_window_layout(window, layout)
    }
}

#[cfg(not(target_os = "windows"))]
fn apply_window_layout(window: &WebviewWindow, layout: WindowLayout) -> tauri::Result<()> {
    window.set_position(LogicalPosition::new(layout.position.x, layout.position.y))?;
    window.set_size(LogicalSize::new(
        layout.coordinate_size.width,
        layout.coordinate_size.height,
    ))
}

#[cfg(target_os = "windows")]
fn physical_coordinate(value: f64) -> i32 {
    value.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(target_os = "windows")]
fn physical_dimension(value: f64) -> u32 {
    value.ceil().clamp(1.0, u32::MAX as f64) as u32
}

fn cursor_is_outside(app: &AppHandle, label: &str) -> bool {
    let Some(window) = app.get_webview_window(label) else {
        return true;
    };
    let Ok(cursor) = app.cursor_position() else {
        return true;
    };
    let Ok(position) = window.outer_position() else {
        return true;
    };
    let Ok(size) = window.outer_size() else {
        return true;
    };
    point_is_outside_physical_rect(
        cursor.x,
        cursor.y,
        position.x as f64,
        position.y as f64,
        size.width as f64,
        size.height as f64,
    )
}

#[cfg(any(target_os = "windows", test))]
fn classify_result_foreground(
    foreground_available: bool,
    direct_match: bool,
    child_match: bool,
    root_match: bool,
    root_owner_match: bool,
    owner_chain_match: bool,
    toolbar_match: bool,
) -> ResultForegroundScope {
    if !foreground_available {
        ResultForegroundScope::Unknown
    } else if direct_match
        || child_match
        || root_match
        || root_owner_match
        || owner_chain_match
        || toolbar_match
    {
        ResultForegroundScope::Internal
    } else {
        ResultForegroundScope::External
    }
}

#[cfg(target_os = "windows")]
fn result_foreground_scope(app: &AppHandle, window: &WebviewWindow) -> ResultForegroundScope {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetForegroundWindow, GetWindow, IsChild, GA_ROOT, GA_ROOTOWNER, GW_OWNER,
    };

    let Ok(result_hwnd) = window.hwnd() else {
        return ResultForegroundScope::Unknown;
    };
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return ResultForegroundScope::Unknown;
    }

    let direct_match = foreground == result_hwnd;
    let child_match = unsafe { IsChild(result_hwnd, foreground).as_bool() };
    let root_match = unsafe { GetAncestor(foreground, GA_ROOT) } == result_hwnd;
    let root_owner_match = unsafe { GetAncestor(foreground, GA_ROOTOWNER) } == result_hwnd;
    let mut owner_chain_match = false;
    let mut candidate = foreground;
    for _ in 0..16 {
        let Ok(owner) = (unsafe { GetWindow(candidate, GW_OWNER) }) else {
            break;
        };
        if owner == result_hwnd || unsafe { IsChild(result_hwnd, owner).as_bool() } {
            owner_chain_match = true;
            break;
        }
        if owner == candidate {
            break;
        }
        candidate = owner;
    }

    // The selection toolbar is a separate no-activate top-level webview. On
    // some WebView2 builds clicking it briefly makes that window foreground;
    // it is still an internal continuation of a result-window selection and
    // must not trigger the source result's blur-close before run_action reads
    // the selection.
    let toolbar_match = app
        .get_webview_window(TOOLBAR_LABEL)
        .and_then(|toolbar| toolbar.hwnd().ok())
        .is_some_and(|toolbar_hwnd| {
            foreground == toolbar_hwnd
                || unsafe { IsChild(toolbar_hwnd, foreground).as_bool() }
                || unsafe { GetAncestor(foreground, GA_ROOT) } == toolbar_hwnd
                || unsafe { GetAncestor(foreground, GA_ROOTOWNER) } == toolbar_hwnd
        });

    classify_result_foreground(
        true,
        direct_match,
        child_match,
        root_match,
        root_owner_match,
        owner_chain_match,
        toolbar_match,
    )
}

fn cursor_is_inside_visible_window(app: &AppHandle, label: &str) -> bool {
    let Some(window) = app.get_webview_window(label) else {
        return false;
    };
    if !window.is_visible().unwrap_or(false) {
        return false;
    }
    let (Ok(cursor), Ok(position), Ok(size)) = (
        app.cursor_position(),
        window.outer_position(),
        window.outer_size(),
    ) else {
        return false;
    };
    !point_is_outside_physical_rect(
        cursor.x,
        cursor.y,
        position.x as f64,
        position.y as f64,
        size.width as f64,
        size.height as f64,
    )
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct WindowsToolbarPointerSource {
    hwnd: isize,
    scale_factor: f64,
}

#[cfg(target_os = "windows")]
fn windows_point_inside_window(window: &WebviewWindow, point: Point) -> bool {
    use windows::Win32::{
        Foundation::RECT,
        UI::WindowsAndMessaging::{GetWindowRect, IsWindowVisible},
    };

    let Ok(hwnd) = window.hwnd() else {
        return false;
    };
    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return false;
    }
    let mut rectangle = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rectangle) }.is_err() {
        return false;
    }
    let tolerance = WINDOWS_TOOLBAR_HIT_TOLERANCE;
    point.x >= f64::from(rectangle.left) - tolerance
        && point.x <= f64::from(rectangle.right) + tolerance
        && point.y >= f64::from(rectangle.top) - tolerance
        && point.y <= f64::from(rectangle.bottom) + tolerance
}

#[cfg(target_os = "windows")]
impl WindowsToolbarPointerSource {
    fn new(window: &WebviewWindow) -> Option<Self> {
        Some(Self {
            hwnd: window.hwnd().ok()?.0 as isize,
            scale_factor: window.scale_factor().ok()?,
        })
    }

    fn hwnd(self) -> windows::Win32::Foundation::HWND {
        windows::Win32::Foundation::HWND(self.hwnd as *mut std::ffi::c_void)
    }

    fn is_visible(self) -> bool {
        unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(self.hwnd()).as_bool() }
    }

    fn position(self) -> Option<ToolbarPointerPosition> {
        use windows::Win32::{
            Foundation::{POINT, RECT},
            UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect},
        };

        let mut cursor = POINT::default();
        let mut rectangle = RECT::default();
        unsafe {
            GetCursorPos(&mut cursor).ok()?;
            GetWindowRect(self.hwnd(), &mut rectangle).ok()?;
        }
        Some(toolbar_pointer_position_from_physical(
            f64::from(cursor.x),
            f64::from(cursor.y),
            f64::from(rectangle.left),
            f64::from(rectangle.top),
            f64::from(rectangle.right - rectangle.left),
            f64::from(rectangle.bottom - rectangle.top),
            self.scale_factor,
        ))
    }
}

#[cfg(target_os = "macos")]
fn toolbar_pointer_position(
    app: &AppHandle,
    window: &WebviewWindow,
) -> Option<ToolbarPointerPosition> {
    let cursor = app.cursor_position().ok()?;
    let position = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    Some(toolbar_pointer_position_from_physical(
        cursor.x,
        cursor.y,
        position.x as f64,
        position.y as f64,
        size.width as f64,
        size.height as f64,
        window.scale_factor().ok()?,
    ))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn toolbar_pointer_position_from_physical(
    cursor_x: f64,
    cursor_y: f64,
    window_x: f64,
    window_y: f64,
    window_width: f64,
    window_height: f64,
    scale_factor: f64,
) -> ToolbarPointerPosition {
    // `cursor_position`, `outer_position`, and `outer_size` share Tauri's
    // physical-pixel coordinate space on both desktop platforms. The renderer
    // consumes client coordinates in CSS pixels, including on mixed-DPI
    // Windows desktops, so only the relative offset is divided by the scale.
    let scale = normalized_scale(scale_factor);
    ToolbarPointerPosition {
        x: (cursor_x - window_x) / scale,
        y: (cursor_y - window_y) / scale,
        inside: !point_is_outside_physical_rect(
            cursor_x,
            cursor_y,
            window_x,
            window_y,
            window_width,
            window_height,
        ),
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn toolbar_pointer_sample_changed(
    previous: ToolbarPointerPosition,
    current: ToolbarPointerPosition,
) -> bool {
    previous.inside != current.inside
        || (previous.x - current.x).abs() >= 0.25
        || (previous.y - current.y).abs() >= 0.25
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn toolbar_pointer_sample_should_emit(
    previous: ToolbarPointerPosition,
    current: ToolbarPointerPosition,
) -> bool {
    toolbar_pointer_sample_changed(previous, current) && (previous.inside || current.inside)
}

fn point_is_outside_physical_rect(
    point_x: f64,
    point_y: f64,
    rect_x: f64,
    rect_y: f64,
    rect_width: f64,
    rect_height: f64,
) -> bool {
    !(point_x >= rect_x
        && point_x <= rect_x + rect_width
        && point_y >= rect_y
        && point_y <= rect_y + rect_height)
}

fn pointer_leave_should_close(
    timer_is_current: bool,
    cursor_outside_result: bool,
    cursor_inside_toolbar: bool,
) -> bool {
    timer_is_current && cursor_outside_result && !cursor_inside_toolbar
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerLeaveTimeoutDecision {
    Ignore,
    Close,
    Retry,
}

fn verified_pointer_leave_timeout_decision(
    state: &Mutex<WindowState>,
    label: &str,
    generation: u64,
    read_cursor_scope: impl FnOnce() -> (bool, bool),
) -> PointerLeaveTimeoutDecision {
    let timer_is_current = || {
        state
            .lock()
            .results
            .get(label)
            .is_some_and(|runtime| runtime.pointer_leave_is_current(generation))
    };
    if !timer_is_current() {
        return PointerLeaveTimeoutDecision::Ignore;
    }
    let (cursor_outside_result, cursor_inside_toolbar) = read_cursor_scope();
    // Native reads may reenter handlers or overlap a settings/input update.
    // Validate again immediately before the caller performs native close.
    let current = timer_is_current();
    if pointer_leave_should_close(current, cursor_outside_result, cursor_inside_toolbar) {
        PointerLeaveTimeoutDecision::Close
    } else if current && cursor_outside_result && cursor_inside_toolbar {
        PointerLeaveTimeoutDecision::Retry
    } else {
        PointerLeaveTimeoutDecision::Ignore
    }
}

fn verified_result_close_request_decision(
    state: &Mutex<WindowState>,
    label: &str,
    read_cursor_scope: impl FnOnce() -> (bool, bool),
    read_blur_scope: impl FnOnce() -> ResultForegroundScope,
) -> PointerLeaveTimeoutDecision {
    let origin = state
        .lock()
        .results
        .get_mut(label)
        .and_then(|runtime| runtime.pending_auto_close.take());
    let Some(origin) = origin else {
        // No automatic provenance: honor ordinary manual close requests.
        return PointerLeaveTimeoutDecision::Close;
    };
    match origin {
        AutoCloseOrigin::PointerLeave(generation) => {
            verified_pointer_leave_timeout_decision(state, label, generation, read_cursor_scope)
        }
        AutoCloseOrigin::Blur(generation) => {
            let blur_is_current = || {
                state
                    .lock()
                    .results
                    .get(label)
                    .is_some_and(|runtime| runtime.blur_is_current(generation))
            };
            if !blur_is_current() {
                return PointerLeaveTimeoutDecision::Ignore;
            }
            // macOS drains proxy Close events before pending Focused(true)
            // events. Native key/foreground state closes that delivery gap.
            let scope = read_blur_scope();
            let current = blur_is_current();
            if scope == ResultForegroundScope::External && current {
                PointerLeaveTimeoutDecision::Close
            } else {
                PointerLeaveTimeoutDecision::Ignore
            }
        }
    }
}

/// Quietly return key-focus to the selection source after a toolbar action.
///
/// Prefer cooperative yield + a soft activate (no `ActivateAllWindows`) so the
/// host app becomes key without the z-order flash of forcing every window
/// forward. Falls back to `NSApp.deactivate()` when the bundle id is unknown.
#[cfg(target_os = "macos")]
pub fn restore_source_app_activation(app: &AppHandle, bundle_id: &str) -> bool {
    let bundle_id = bundle_id.trim().to_owned();
    let app = app.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    if app
        .run_on_main_thread(move || {
            use objc2::MainThreadMarker;
            use objc2_app_kit::{
                NSApplication, NSApplicationActivationOptions, NSRunningApplication,
            };
            use objc2_foundation::NSString;

            let Some(mtm) = MainThreadMarker::new() else {
                let _ = sender.send(false);
                return;
            };

            // If TextLens became active after the toolbar click, drop activation
            // first — this restores the previous app without a window parade.
            let ns_app = NSApplication::sharedApplication(mtm);
            if ns_app.isActive() {
                ns_app.deactivate();
            }

            if bundle_id.is_empty() {
                let _ = sender.send(true);
                return;
            }

            let identifier = NSString::from_str(&bundle_id);
            // Cooperative hand-off (macOS 14+) — avoids IgnoringOtherApps thrash.
            ns_app.yieldActivationToApplicationWithBundleIdentifier(&identifier);

            let running =
                NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier);
            // Soft activate: only main/key windows, NOT ActivateAllWindows (that
            // was the visible "window flash" after copy).
            let activated = running.firstObject().is_some_and(|target| {
                target.activateWithOptions(NSApplicationActivationOptions::empty())
            });
            let _ = sender.send(activated || true);
        })
        .is_err()
    {
        return false;
    }
    receiver
        .recv_timeout(MACOS_MAIN_THREAD_HOP_TIMEOUT)
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn restore_source_app_activation(_app: &AppHandle, _bundle_id: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn configure_toolbar_native(window: &WebviewWindow) -> tauri::Result<()> {
    use objc2_app_kit::{NSStatusWindowLevel, NSWindow, NSWindowCollectionBehavior};

    let app = window.app_handle().clone();
    let native_window = window.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = native_window.ns_window().map(|pointer| unsafe {
            let window = &*(pointer as *const NSWindow);
            window.setHidesOnDeactivate(false);
            // The selection toolbar is intentionally non-focusable, so it
            // never becomes the key window. Keep mouse-move delivery enabled
            // explicitly so WKWebView can continuously update the toolbar's
            // pointer-driven hover state while the cursor crosses actions.
            window.setAcceptsMouseMovedEvents(true);
            window.setLevel(NSStatusWindowLevel);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::Transient
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
        });
        let _ = sender.send(result);
    })?;
    receiver
        .recv_timeout(MACOS_MAIN_THREAD_HOP_TIMEOUT)
        .map_err(|_| tauri::Error::FailedToReceiveMessage)??;
    install_toolbar_mouse_tracking(window)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_toolbar_mouse_tracking(window: &WebviewWindow) -> tauri::Result<()> {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSTrackingArea, NSTrackingAreaOptions, NSView};

    window.with_webview(|webview| unsafe {
        // The toolbar never becomes key, so WKWebView's normal tracking area
        // only refreshes reliably while dragging. Attach an always-active
        // tracking area to the WKWebView itself so ordinary mouse movement is
        // delivered to WebKit while TextLens remains in the background.
        let view = &*(webview.inner().cast::<NSView>());
        let options = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::InVisibleRect;
        let tracking_area = NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            view.bounds(),
            options,
            Some(view),
            None,
        );
        view.addTrackingArea(&tracking_area);
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn configure_result_native(window: &WebviewWindow, pinned: bool) -> tauri::Result<()> {
    run_macos_window_operation(window, move |window| {
        configure_result_native_raw(window, pinned)
    })
}

#[cfg(target_os = "macos")]
fn run_macos_window_operation<T: Send + 'static>(
    window: &WebviewWindow,
    operation: impl FnOnce(&WebviewWindow) -> tauri::Result<T> + Send + 'static,
) -> tauri::Result<T> {
    if objc2::MainThreadMarker::new().is_some() {
        return operation(window);
    }
    let app = window.app_handle().clone();
    let window = window.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = sender.send(operation(&window));
    })?;
    receiver
        .recv_timeout(MACOS_MAIN_THREAD_HOP_TIMEOUT)
        .map_err(|_| tauri::Error::FailedToReceiveMessage)?
}

#[cfg(target_os = "macos")]
fn configure_result_native_raw(window: &WebviewWindow, pinned: bool) -> tauri::Result<()> {
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};

    window.ns_window().map(|pointer| unsafe {
        let window = &*(pointer as *const NSWindow);
        let behavior = if pinned {
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
        } else {
            NSWindowCollectionBehavior::MoveToActiveSpace
                | NSWindowCollectionBehavior::FullScreenAuxiliary
        };
        window.setCollectionBehavior(behavior);
    })
}

#[cfg(target_os = "windows")]
fn configure_result_native_raw(window: &WebviewWindow, pinned: bool) -> tauri::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };

    let hwnd = window
        .hwnd()
        .map_err(|error| result_window_step_error("native HWND access", error))?;
    set_windows_extended_style(hwnd, WS_EX_TOOLWINDOW.0, WS_EX_APPWINDOW.0)?;
    configure_windows_result_corners(hwnd);
    let insert_after = if pinned { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        SetWindowPos(
            hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .map_err(windows_error)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn configure_windows_result_corners(hwnd: windows::Win32::Foundation::HWND) {
    use std::ffi::c_void;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMSBT_NONE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
        DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };

    unsafe fn set_attribute<T: Copy>(
        hwnd: windows::Win32::Foundation::HWND,
        attribute: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE,
        value: &T,
    ) {
        let _ = DwmSetWindowAttribute(
            hwnd,
            attribute,
            value as *const T as *const c_void,
            std::mem::size_of::<T>() as u32,
        );
    }

    // These attributes are supported on Windows 11. Windows 10 returns an
    // unsupported-attribute error, which is intentionally ignored so the CSS
    // transparent clipping fallback remains available there.
    unsafe {
        set_attribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &DWMWCP_ROUND);
        set_attribute(hwnd, DWMWA_BORDER_COLOR, &DWMWA_COLOR_NONE);
        set_attribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &DWMSBT_NONE);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn configure_result_native(_window: &WebviewWindow, _pinned: bool) -> tauri::Result<()> {
    Ok(())
}

#[cfg(target_os = "windows")]
fn configure_toolbar_native(window: &WebviewWindow) -> tauri::Result<()> {
    run_windows_window_operation(window, configure_toolbar_native_raw)
}

#[cfg(target_os = "windows")]
fn configure_toolbar_native_raw(window: &WebviewWindow) -> tauri::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    let hwnd = window.hwnd()?;
    set_windows_extended_style(
        hwnd,
        WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0,
        WS_EX_APPWINDOW.0,
    )?;
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .map_err(windows_error)?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn configure_toolbar_native(_window: &WebviewWindow) -> tauri::Result<()> {
    Ok(())
}

/// Commits the staged macOS toolbar in a single AppKit main-thread hop:
/// position + size, `orderFrontRegardless` and the alpha flip all run inside
/// one dispatch. Tauri's `set_position`/`set_size` execute inline when the
/// caller is already on the main thread, so one hop replaces the three
/// cross-thread round trips this commit path previously paid.
#[cfg(target_os = "windows")]
fn order_front_without_focus(window: &WebviewWindow) -> tauri::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };

    run_windows_window_operation(window, |window| {
        window.show()?;
        configure_toolbar_native_raw(window)?;
        let hwnd = window.hwnd()?;
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .map_err(windows_error)?;
        }
        Ok(())
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn order_front_without_focus(window: &WebviewWindow) -> tauri::Result<()> {
    window.show()
}

#[cfg(target_os = "macos")]
fn set_native_alpha_raw(window: &WebviewWindow, alpha: f64) -> tauri::Result<()> {
    use objc2_app_kit::NSWindow;

    run_macos_window_operation(window, move |window| {
        window.ns_window().map(|pointer| unsafe {
            let window = &*(pointer as *const NSWindow);
            window.setAlphaValue(alpha);
        })
    })
}

#[cfg(target_os = "macos")]
fn set_native_opacity(window: &WebviewWindow, opacity: f64) -> tauri::Result<()> {
    set_native_alpha_raw(window, opacity.clamp(0.2, 1.0))
}

#[cfg(target_os = "windows")]
fn set_native_opacity(window: &WebviewWindow, opacity: f64) -> tauri::Result<()> {
    run_windows_window_operation(window, move |window| {
        set_native_opacity_raw(window, opacity)
    })
}

#[cfg(target_os = "windows")]
fn set_native_opacity_raw(window: &WebviewWindow, opacity: f64) -> tauri::Result<()> {
    set_native_alpha_raw(window, opacity_alpha(opacity))
}

#[cfg(target_os = "windows")]
fn set_native_alpha_raw(window: &WebviewWindow, alpha: u8) -> tauri::Result<()> {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetLayeredWindowAttributes, SetWindowPos, LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_EX_LAYERED,
    };

    let hwnd = window.hwnd()?;
    let style_changed = set_windows_extended_style(hwnd, WS_EX_LAYERED.0, 0)?;
    unsafe {
        if style_changed {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
            .map_err(windows_error)?;
        }
        SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA).map_err(windows_error)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn prepare_result_window(
    window: &WebviewWindow,
    pinned: bool,
    placement: ResultPlacement,
) -> tauri::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    };

    let size = fit_size_to_monitor(window, placement.cursor, placement.size)
        .map_err(|error| result_window_step_error("monitor sizing", error))?;
    let layout = if placement.follow_cursor {
        result_layout(window, placement.cursor, size)
            .map_err(|error| result_window_step_error("cursor layout", error))?
    } else {
        centered_layout(window, placement.cursor, size)
            .map_err(|error| result_window_step_error("centered layout", error))?
    };

    run_windows_window_operation(window, move |window| {
        configure_result_native_raw(window, pinned)?;
        set_native_alpha_raw(window, 0)?;
        let hwnd = window.hwnd()?;
        let insert_after = if pinned { HWND_TOPMOST } else { HWND_NOTOPMOST };
        unsafe {
            SetWindowPos(
                hwnd,
                Some(insert_after),
                physical_coordinate(layout.position.x),
                physical_coordinate(layout.position.y),
                physical_dimension(layout.coordinate_size.width) as i32,
                physical_dimension(layout.coordinate_size.height) as i32,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .map_err(windows_error)?;
        }
        Ok(())
    })
}

#[cfg(target_os = "macos")]
fn prepare_result_window(
    window: &WebviewWindow,
    pinned: bool,
    placement: ResultPlacement,
) -> tauri::Result<()> {
    run_macos_window_operation(window, move |window| {
        use objc2_app_kit::NSWindow;
        configure_result_native_raw(window, pinned)?;
        let size = fit_size_to_monitor(window, placement.cursor, placement.size)?;
        let layout = if placement.follow_cursor {
            result_layout(window, placement.cursor, size)?
        } else {
            centered_layout(window, placement.cursor, size)?
        };
        let pointer = window.ns_window()?;
        let native = unsafe { &*(pointer as *const NSWindow) };
        native.setAlphaValue(0.0);
        apply_window_layout(window, layout)?;
        // Let WebKit composite the hydrated frame without activating TextLens.
        native.orderFrontRegardless();
        Ok(())
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn prepare_result_window(
    window: &WebviewWindow,
    _pinned: bool,
    placement: ResultPlacement,
) -> tauri::Result<()> {
    let layout = result_layout(window, placement.cursor, placement.size)?;
    apply_window_layout(window, layout)
}

#[cfg(target_os = "windows")]
fn show_result_window(window: &WebviewWindow, pinned: bool, opacity: f64) -> tauri::Result<()> {
    run_windows_window_operation(window, move |window| {
        window.show()?;
        configure_result_native_raw(window, pinned)?;
        set_native_opacity_raw(window, opacity)?;
        window.set_focus()
    })
}

#[cfg(target_os = "macos")]
fn show_result_window(window: &WebviewWindow, _pinned: bool, opacity: f64) -> tauri::Result<()> {
    run_macos_window_operation(window, move |window| {
        set_native_opacity(window, opacity)?;
        window.show()?;
        window.set_focus()
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn show_result_window(window: &WebviewWindow, _pinned: bool, _opacity: f64) -> tauri::Result<()> {
    window.show()?;
    window.set_focus()
}

#[cfg(target_os = "windows")]
fn apply_result_pinned(window: &WebviewWindow, pinned: bool, opacity: f64) -> tauri::Result<()> {
    run_windows_window_operation(window, move |window| {
        window.set_always_on_top(pinned)?;
        configure_result_native_raw(window, pinned)?;
        set_native_opacity_raw(window, opacity)
    })
}

#[cfg(not(target_os = "windows"))]
fn apply_result_pinned(window: &WebviewWindow, pinned: bool, _opacity: f64) -> tauri::Result<()> {
    window.set_always_on_top(pinned)?;
    configure_result_native(window, pinned)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn set_native_opacity(_window: &WebviewWindow, _opacity: f64) -> tauri::Result<()> {
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn opacity_alpha(opacity: f64) -> u8 {
    (opacity.clamp(0.2, 1.0) * 255.0).round() as u8
}

#[cfg(target_os = "windows")]
fn set_windows_extended_style(
    hwnd: windows::Win32::Foundation::HWND,
    add: u32,
    remove: u32,
) -> tauri::Result<bool> {
    use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE,
    };

    unsafe {
        SetLastError(WIN32_ERROR(0));
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if current == 0 {
            let error = GetLastError();
            if error.0 != 0 {
                return Err(std::io::Error::from_raw_os_error(error.0 as i32).into());
            }
        }
        let next = (current | add) & !remove;
        if current == next {
            return Ok(false);
        }
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next as isize);
        if previous == 0 {
            let error = GetLastError();
            if error.0 != 0 {
                return Err(std::io::Error::from_raw_os_error(error.0 as i32).into());
            }
        }
    }
    Ok(true)
}

#[cfg(target_os = "windows")]
const WINDOWS_WINDOW_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(target_os = "windows")]
const WINDOWS_WINDOW_OPERATION_PENDING: u8 = 0;
#[cfg(target_os = "windows")]
const WINDOWS_WINDOW_OPERATION_RUNNING: u8 = 1;
#[cfg(target_os = "windows")]
const WINDOWS_WINDOW_OPERATION_CANCELLED: u8 = 2;

#[cfg(target_os = "windows")]
fn run_windows_window_operation(
    window: &WebviewWindow,
    operation: impl FnOnce(&WebviewWindow) -> tauri::Result<()> + Send + 'static,
) -> tauri::Result<()> {
    use std::sync::atomic::AtomicU8;
    use windows::Win32::System::Threading::GetCurrentThreadId;

    let current_thread_id = unsafe { GetCurrentThreadId() };
    let ui_thread_id = windows_ui_thread_id(window)?;
    if windows_window_operation_runs_inline(current_thread_id, ui_thread_id) {
        return operation(window);
    }

    let app = window.app_handle().clone();
    let window = window.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let dispatch_state = Arc::new(AtomicU8::new(WINDOWS_WINDOW_OPERATION_PENDING));
    let callback_state = Arc::clone(&dispatch_state);
    if let Err(error) = app.run_on_main_thread(move || {
        if !try_start_windows_window_operation(&callback_state) {
            return;
        }
        let _ = sender.send(operation(&window));
    }) {
        dispatch_state.store(WINDOWS_WINDOW_OPERATION_CANCELLED, Ordering::Release);
        return Err(error);
    }

    wait_for_windows_window_operation(receiver, &dispatch_state, WINDOWS_WINDOW_OPERATION_TIMEOUT)
}

/// Returns Tauri's event-loop/UI thread without touching the target window's
/// native handle. WRY returns a detached window handle before Windows has
/// necessarily attached its HWND. Asking `window.hwnd()` at that boundary is
/// a race: it can report `RawHandleError::Unavailable` or
/// `FailedToReceiveMessage` even though the queued CreateWindow operation will
/// complete normally. A main-thread task is ordered after that CreateWindow
/// message, so native configuration can safely access the HWND inside the
/// operation callback instead.
#[cfg(target_os = "windows")]
fn windows_ui_thread_id(window: &WebviewWindow) -> tauri::Result<u32> {
    use std::sync::mpsc::RecvTimeoutError;
    use windows::Win32::System::Threading::GetCurrentThreadId;

    let cached = WINDOWS_UI_THREAD_ID.load(Ordering::Acquire);
    if cached != 0 {
        return Ok(cached);
    }

    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    window.app_handle().run_on_main_thread(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        let _ = sender.send(thread_id);
    })?;
    let thread_id = match receiver.recv_timeout(WINDOWS_WINDOW_OPERATION_TIMEOUT) {
        Ok(thread_id) if thread_id != 0 => thread_id,
        Ok(_) => {
            return Err(std::io::Error::other("could not determine the Windows UI thread").into())
        }
        Err(RecvTimeoutError::Timeout) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Windows UI thread lookup timed out",
            )
            .into())
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(std::io::Error::other("Windows UI thread lookup was disconnected").into())
        }
    };
    let _ =
        WINDOWS_UI_THREAD_ID.compare_exchange(0, thread_id, Ordering::AcqRel, Ordering::Acquire);
    Ok(WINDOWS_UI_THREAD_ID.load(Ordering::Acquire))
}

#[cfg(target_os = "windows")]
fn windows_window_operation_runs_inline(current_thread_id: u32, window_thread_id: u32) -> bool {
    window_thread_id != 0 && current_thread_id == window_thread_id
}

#[cfg(target_os = "windows")]
fn try_start_windows_window_operation(state: &std::sync::atomic::AtomicU8) -> bool {
    state
        .compare_exchange(
            WINDOWS_WINDOW_OPERATION_PENDING,
            WINDOWS_WINDOW_OPERATION_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

#[cfg(target_os = "windows")]
fn wait_for_windows_window_operation(
    receiver: std::sync::mpsc::Receiver<tauri::Result<()>>,
    dispatch_state: &std::sync::atomic::AtomicU8,
    timeout: Duration,
) -> tauri::Result<()> {
    use std::sync::mpsc::RecvTimeoutError;

    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Disconnected) => Err(tauri::Error::FailedToReceiveMessage),
        Err(RecvTimeoutError::Timeout) => {
            // If the callback has not started, prevent a stale window mutation
            // from running after its caller has already observed a timeout.
            let cancelled_before_start = dispatch_state
                .compare_exchange(
                    WINDOWS_WINDOW_OPERATION_PENDING,
                    WINDOWS_WINDOW_OPERATION_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
            let detail = if cancelled_before_start {
                "before it started"
            } else {
                "while it was running"
            };
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Windows window operation timed out {detail}"),
            )
            .into())
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_error(error: windows::core::Error) -> tauri::Error {
    std::io::Error::other(error.to_string()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blur_result_runtime(reveal_phase: ResultRevealPhase) -> ResultRuntime {
        ResultRuntime {
            session_id: "focus-test".to_owned(),
            pinned: false,
            opacity: 1.0,
            placement: ResultPlacement {
                size: WindowSize {
                    width: 520.0,
                    height: 420.0,
                },
                cursor: Point { x: 640.0, y: 360.0 },
                follow_cursor: true,
            },
            reveal_phase,
            reveal_operation: Arc::new(Mutex::new(())),
            focus_seen: false,
            blur_generation: 0,
            pointer_inside: false,
            pointer_seen: false,
            dismiss_mode: DismissMode::Blur,
            dismiss_delay_ms: 450,
            hide_generation: 0,
            pending_auto_close: None,
            remember_size: true,
            ignore_resize_until: Instant::now(),
        }
    }

    #[test]
    fn result_size_is_safely_clamped() {
        assert_eq!(
            WindowSize {
                width: 100.0,
                height: 2_000.0,
            }
            .clamped(),
            WindowSize {
                width: 360.0,
                height: 1_200.0,
            }
        );
    }

    #[test]
    fn result_opacity_converts_to_a_bounded_native_alpha() {
        assert_eq!(opacity_alpha(0.0), 51);
        assert_eq!(opacity_alpha(0.5), 128);
        assert_eq!(opacity_alpha(1.5), 255);
    }

    #[test]
    fn result_reveal_starts_hidden_until_the_hydrated_frame_on_every_platform() {
        assert_eq!(initial_result_reveal_phase(), ResultRevealPhase::Hidden);
    }

    #[test]
    fn blur_dismissal_ignores_focus_churn_until_result_reveal_is_committed() {
        for phase in [ResultRevealPhase::Hidden, ResultRevealPhase::Prepared] {
            let mut runtime = blur_result_runtime(phase);

            assert_eq!(runtime.note_focus_change(true), None);
            assert_eq!(runtime.note_focus_change(false), None);
            assert!(!runtime.focus_seen);
        }

        let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
        assert_eq!(runtime.note_focus_change(true), None);
        let blur = runtime.note_focus_change(false).expect("blur token");
        assert!(runtime.blur_is_current(blur));
    }

    #[test]
    fn reveal_commit_does_not_inherit_focus_seen_during_hidden_preparation() {
        let mut runtime = blur_result_runtime(ResultRevealPhase::Hidden);
        runtime.focus_seen = true;

        assert_eq!(runtime.note_focus_change(false), None);
        assert!(!runtime.focus_seen);

        runtime.reveal_phase = ResultRevealPhase::Prepared;
        assert_eq!(runtime.note_focus_change(true), None);
        assert!(!runtime.focus_seen);

        runtime.reveal_phase = ResultRevealPhase::Committed;
        assert_eq!(runtime.note_focus_change(false), None);
        assert_eq!(runtime.note_focus_change(true), None);
        let blur = runtime.note_focus_change(false).expect("blur token");
        assert!(runtime.blur_is_current(blur));

        assert_eq!(runtime.note_focus_change(true), None);
        assert!(!runtime.blur_is_current(blur));
    }

    #[test]
    fn windows_blur_only_closes_for_an_explicit_external_foreground() {
        assert_eq!(
            classify_result_foreground(false, false, false, false, false, false, false),
            ResultForegroundScope::Unknown
        );
        for internal_relation in 0..5 {
            let mut relations = [false; 5];
            relations[internal_relation] = true;
            assert_eq!(
                classify_result_foreground(
                    true,
                    relations[0],
                    relations[1],
                    relations[2],
                    relations[3],
                    relations[4],
                    false,
                ),
                ResultForegroundScope::Internal
            );
        }
        assert_eq!(
            classify_result_foreground(true, false, false, false, false, false, false),
            ResultForegroundScope::External
        );
        assert_eq!(
            classify_result_foreground(true, false, false, false, false, false, true),
            ResultForegroundScope::Internal
        );
    }

    #[test]
    fn queued_pointer_close_revalidates_state_and_deduplicates_requests() {
        let transitions: [fn(&mut ResultRuntime); 4] = [
            |runtime| {
                runtime.note_pointer_change(true);
            },
            |runtime| {
                runtime.pinned = true;
            },
            |runtime| {
                runtime.reveal_phase = ResultRevealPhase::Prepared;
            },
            |runtime| {
                runtime.dismiss_mode = DismissMode::Manual;
            },
        ];
        for transition in transitions {
            let coordinator = WindowCoordinator::default();
            let label = "selection-result-queued-pointer";
            let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
            runtime.dismiss_mode = DismissMode::PointerLeave;
            runtime.note_pointer_change(true);
            assert!(runtime.note_pointer_change(false));
            let origin = AutoCloseOrigin::PointerLeave(runtime.hide_generation);
            assert!(runtime.prepare_auto_close_request(origin));
            assert!(!runtime.prepare_auto_close_request(origin));
            transition(&mut runtime);
            coordinator
                .state
                .lock()
                .results
                .insert(label.to_owned(), runtime);
            assert_eq!(
                verified_result_close_request_decision(
                    &coordinator.state,
                    label,
                    || { panic!("stale automatic close must skip native reads") },
                    || panic!("pointer close does not read native focus")
                ),
                PointerLeaveTimeoutDecision::Ignore
            );
            assert!(coordinator
                .state
                .lock()
                .results
                .get(label)
                .unwrap()
                .pending_auto_close
                .is_none());
        }
    }

    #[test]
    fn queued_pointer_close_checks_physical_reentry_and_toolbar_before_frontend_ipc() {
        for (outside, toolbar, expected) in [
            (false, false, PointerLeaveTimeoutDecision::Ignore),
            (true, true, PointerLeaveTimeoutDecision::Retry),
            (true, false, PointerLeaveTimeoutDecision::Close),
        ] {
            let coordinator = WindowCoordinator::default();
            let label = "selection-result-queued-physical-pointer";
            let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
            runtime.dismiss_mode = DismissMode::PointerLeave;
            runtime.note_pointer_change(true);
            assert!(runtime.note_pointer_change(false));
            assert!(
                runtime.prepare_auto_close_request(AutoCloseOrigin::PointerLeave(
                    runtime.hide_generation
                ))
            );
            coordinator
                .state
                .lock()
                .results
                .insert(label.to_owned(), runtime);
            // Cached pointerInside still says outside. Fresh physical geometry
            // must veto a queued close before the enter IPC reaches the backend.
            assert_eq!(
                verified_result_close_request_decision(
                    &coordinator.state,
                    label,
                    || (outside, toolbar),
                    || panic!("pointer close does not read native focus")
                ),
                expected
            );
            assert!(
                !coordinator
                    .state
                    .lock()
                    .results
                    .get(label)
                    .unwrap()
                    .pointer_inside
            );
            assert!(coordinator
                .state
                .lock()
                .results
                .get(label)
                .unwrap()
                .pending_auto_close
                .is_none());
        }
    }

    #[test]
    fn queued_blur_close_checks_native_focus_before_pending_focus_event() {
        for (scope, expected) in [
            (
                ResultForegroundScope::Internal,
                PointerLeaveTimeoutDecision::Ignore,
            ),
            (
                ResultForegroundScope::Unknown,
                PointerLeaveTimeoutDecision::Ignore,
            ),
            (
                ResultForegroundScope::External,
                PointerLeaveTimeoutDecision::Close,
            ),
        ] {
            let coordinator = WindowCoordinator::default();
            let label = "selection-result-pending-native-focus";
            let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
            runtime.note_focus_change(true);
            let generation = runtime.note_focus_change(false).unwrap();
            assert!(runtime.prepare_auto_close_request(AutoCloseOrigin::Blur(generation)));
            coordinator
                .state
                .lock()
                .results
                .insert(label.to_owned(), runtime);
            let mut native_read = false;
            assert_eq!(
                verified_result_close_request_decision(
                    &coordinator.state,
                    label,
                    || panic!("blur close must not read pointer geometry"),
                    || {
                        native_read = true;
                        // Native focus may have changed while Focused(true)
                        // remains pending behind the proxy Close event.
                        assert_eq!(
                            coordinator
                                .state
                                .lock()
                                .results
                                .get(label)
                                .unwrap()
                                .blur_generation,
                            generation
                        );
                        scope
                    },
                ),
                expected
            );
            assert!(native_read);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .results
                    .get(label)
                    .unwrap()
                    .blur_generation,
                generation
            );
        }
    }

    #[test]
    fn queued_blur_close_revalidates_generation_and_pin_after_native_focus_read() {
        let transitions: [fn(&mut ResultRuntime); 2] = [
            |runtime| {
                runtime.note_focus_change(true);
            },
            |runtime| {
                runtime.pinned = true;
            },
        ];
        for transition in transitions {
            let coordinator = WindowCoordinator::default();
            let label = "selection-result-native-focus-read-race";
            let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
            runtime.note_focus_change(true);
            let generation = runtime.note_focus_change(false).unwrap();
            assert!(runtime.prepare_auto_close_request(AutoCloseOrigin::Blur(generation)));
            coordinator
                .state
                .lock()
                .results
                .insert(label.to_owned(), runtime);
            assert_eq!(
                verified_result_close_request_decision(
                    &coordinator.state,
                    label,
                    || panic!("blur close must not read pointer geometry"),
                    || {
                        let mut state = coordinator
                            .state
                            .try_lock()
                            .expect("no state lock across native focus read");
                        transition(state.results.get_mut(label).unwrap());
                        // An old native sample alone must not authorize close
                        // after the runtime generation or pin state changes.
                        ResultForegroundScope::External
                    },
                ),
                PointerLeaveTimeoutDecision::Ignore
            );
        }
    }

    #[test]
    fn queued_blur_close_is_cancelled_by_focus_pin_or_reveal_changes() {
        let transitions: [fn(&mut ResultRuntime); 5] = [
            |runtime| {
                runtime.note_focus_change(true);
            },
            |runtime| {
                runtime.pinned = true;
            },
            |runtime| {
                runtime.reveal_phase = ResultRevealPhase::Prepared;
            },
            |runtime| {
                runtime.focus_seen = false;
            },
            |runtime| {
                runtime.dismiss_mode = DismissMode::Manual;
            },
        ];
        for transition in transitions {
            let coordinator = WindowCoordinator::default();
            let label = "selection-result-queued-blur";
            let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
            runtime.note_focus_change(true);
            let generation = runtime.note_focus_change(false).unwrap();
            assert!(runtime.prepare_auto_close_request(AutoCloseOrigin::Blur(generation)));
            transition(&mut runtime);
            coordinator
                .state
                .lock()
                .results
                .insert(label.to_owned(), runtime);
            assert_eq!(
                verified_result_close_request_decision(
                    &coordinator.state,
                    label,
                    || { panic!("blur close does not depend on pointer geometry") },
                    || panic!("stale blur skips native focus reads")
                ),
                PointerLeaveTimeoutDecision::Ignore
            );
        }
    }

    #[test]
    fn queued_auto_close_reuses_latest_valid_origin_and_honors_explicit_close() {
        let coordinator = WindowCoordinator::default();
        let label = "selection-result-queued-close-origin";
        let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
        runtime.note_focus_change(true);
        let old_blur = runtime.note_focus_change(false).unwrap();
        assert!(runtime.prepare_auto_close_request(AutoCloseOrigin::Blur(old_blur)));
        runtime.note_focus_change(true);
        let new_blur = runtime.note_focus_change(false).unwrap();
        assert!(!runtime.prepare_auto_close_request(AutoCloseOrigin::Blur(new_blur)));
        assert_eq!(
            runtime.pending_auto_close,
            Some(AutoCloseOrigin::Blur(new_blur))
        );
        coordinator
            .state
            .lock()
            .results
            .insert(label.to_owned(), runtime);
        assert_eq!(
            verified_result_close_request_decision(
                &coordinator.state,
                label,
                || { panic!("eligible blur close does not read cursor") },
                || ResultForegroundScope::External
            ),
            PointerLeaveTimeoutDecision::Close
        );
        {
            let mut state = coordinator.state.lock();
            let runtime = state.results.get_mut(label).unwrap();
            runtime.dismiss_mode = DismissMode::PointerLeave;
            runtime.note_pointer_change(true);
            runtime.note_pointer_change(false);
            assert!(
                runtime.prepare_auto_close_request(AutoCloseOrigin::PointerLeave(
                    runtime.hide_generation
                ))
            );
            // Explicit close_result cancels automatic provenance before its
            // queued event, so a pinned result can still be manually closed.
            runtime.cancel_auto_close_request();
            runtime.pinned = true;
        }
        assert_eq!(
            verified_result_close_request_decision(
                &coordinator.state,
                label,
                || { panic!("explicit close does not require cursor reads") },
                || panic!("explicit close does not require focus reads")
            ),
            PointerLeaveTimeoutDecision::Close
        );
    }

    #[test]
    fn result_pointer_timeout_ignores_reentry_during_cursor_reads() {
        let coordinator = WindowCoordinator::default();
        let label = "selection-result-pointer-reentry";
        let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
        runtime.dismiss_mode = DismissMode::PointerLeave;
        assert!(!runtime.note_pointer_change(true));
        assert!(runtime.note_pointer_change(false));
        let generation = runtime.hide_generation;
        coordinator
            .state
            .lock()
            .results
            .insert(label.to_owned(), runtime);

        let decision =
            verified_pointer_leave_timeout_decision(&coordinator.state, label, generation, || {
                // Geometry reads run without the state lock. An enter can
                // invalidate the leave while an old physical sample is read.
                let mut state = coordinator
                    .state
                    .try_lock()
                    .expect("unlocked during native read");
                let runtime = state.results.get_mut(label).unwrap();
                assert!(!runtime.note_pointer_change(true));
                (true, false)
            });
        assert_eq!(decision, PointerLeaveTimeoutDecision::Ignore);
    }

    #[test]
    fn result_pointer_timeout_ignores_pinning_during_cursor_reads() {
        let coordinator = WindowCoordinator::default();
        let label = "selection-result-pointer-pin";
        let mut runtime = blur_result_runtime(ResultRevealPhase::Committed);
        runtime.dismiss_mode = DismissMode::PointerLeave;
        assert!(!runtime.note_pointer_change(true));
        assert!(runtime.note_pointer_change(false));
        let generation = runtime.hide_generation;
        coordinator
            .state
            .lock()
            .results
            .insert(label.to_owned(), runtime);

        // With no intervening input, the same timeout remains eligible.
        assert_eq!(
            verified_pointer_leave_timeout_decision(&coordinator.state, label, generation, || (
                true, false
            ),),
            PointerLeaveTimeoutDecision::Close
        );
        let decision =
            verified_pointer_leave_timeout_decision(&coordinator.state, label, generation, || {
                coordinator
                    .state
                    .lock()
                    .results
                    .get_mut(label)
                    .unwrap()
                    .pinned = true;
                // Check pin state independently of generation invalidation.
                (true, false)
            });
        assert_eq!(decision, PointerLeaveTimeoutDecision::Ignore);
    }

    #[test]
    fn result_pointer_leave_ignores_pre_reveal_enter_and_leave() {
        for phase in [ResultRevealPhase::Hidden, ResultRevealPhase::Prepared] {
            let mut runtime = blur_result_runtime(phase);
            runtime.dismiss_mode = DismissMode::PointerLeave;
            assert!(!runtime.note_pointer_change(true));
            assert!(!runtime.pointer_seen);
            assert!(!runtime.note_pointer_change(false));
            assert!(!runtime.pointer_leave_is_current(runtime.hide_generation));
            // A historical enter or settings change must not arm preparation.
            runtime.pointer_seen = true;
            assert!(!runtime.pointer_leave_is_current(runtime.hide_generation));
        }
    }

    #[test]
    fn result_pointer_leave_commit_preserves_current_entry_and_cancels_old_timer() {
        let coordinator = WindowCoordinator::default();
        let label = "selection-result-pointer-reveal";
        for inside_at_commit in [false, true] {
            let mut runtime = blur_result_runtime(ResultRevealPhase::Prepared);
            runtime.dismiss_mode = DismissMode::PointerLeave;
            assert!(!runtime.note_pointer_change(true));
            if !inside_at_commit {
                assert!(!runtime.note_pointer_change(false));
            }
            let old_generation = runtime.hide_generation;
            coordinator
                .state
                .lock()
                .results
                .insert(label.to_owned(), runtime);
            coordinator
                .set_result_reveal_phase(
                    label,
                    ResultRevealPhase::Prepared,
                    ResultRevealPhase::Committed,
                )
                .unwrap();

            let mut state = coordinator.state.lock();
            let runtime = state.results.get_mut(label).unwrap();
            assert_eq!(runtime.pointer_seen, inside_at_commit);
            assert_ne!(runtime.hide_generation, old_generation);
            assert!(!runtime.pointer_leave_is_current(old_generation));
            if !inside_at_commit {
                assert!(!runtime.note_pointer_change(false));
                assert!(!runtime.note_pointer_change(true));
            }
            assert!(runtime.note_pointer_change(false));
            assert!(runtime.pointer_leave_is_current(runtime.hide_generation));
        }
    }

    #[test]
    fn result_resize_persistence_ignores_uncommitted_windows_after_deadline() {
        let coordinator = WindowCoordinator::default();
        let label = "selection-result-delayed-initialization";
        for phase in [ResultRevealPhase::Hidden, ResultRevealPhase::Prepared] {
            let mut runtime = blur_result_runtime(phase);
            runtime.remember_size = true;
            runtime.ignore_resize_until = Instant::now() - std::time::Duration::from_secs(1);
            coordinator
                .state
                .lock()
                .results
                .insert(label.to_owned(), runtime);
            assert!(!coordinator.note_result_resize(label), "phase: {phase:?}");
        }
    }

    #[test]
    fn result_resize_persistence_begins_at_reveal_commit() {
        let coordinator = WindowCoordinator::default();
        let label = "selection-result-test";
        coordinator.state.lock().results.insert(
            label.to_owned(),
            ResultRuntime {
                session_id: "test".to_owned(),
                pinned: false,
                opacity: 1.0,
                placement: ResultPlacement {
                    size: WindowSize {
                        width: 520.0,
                        height: 420.0,
                    },
                    cursor: Point { x: 640.0, y: 360.0 },
                    follow_cursor: true,
                },
                reveal_phase: ResultRevealPhase::Prepared,
                reveal_operation: Arc::new(Mutex::new(())),
                focus_seen: false,
                blur_generation: 0,
                pointer_inside: false,
                pointer_seen: false,
                dismiss_mode: DismissMode::Manual,
                dismiss_delay_ms: 450,
                hide_generation: 0,
                pending_auto_close: None,
                remember_size: true,
                ignore_resize_until: Instant::now() + std::time::Duration::from_secs(5),
            },
        );

        assert!(!coordinator.note_result_resize(label));
        coordinator
            .set_result_reveal_phase(
                label,
                ResultRevealPhase::Prepared,
                ResultRevealPhase::Committed,
            )
            .unwrap();
        assert!(coordinator.note_result_resize(label));
    }

    #[test]
    fn result_reveal_retains_placement_through_prepare_and_commit() {
        let coordinator = WindowCoordinator::default();
        let label = "selection-result-placement";
        let expected_size = WindowSize {
            width: 612.0,
            height: 488.0,
        };
        let expected_cursor = Point {
            x: -720.0,
            y: 540.0,
        };
        coordinator.state.lock().results.insert(
            label.to_owned(),
            ResultRuntime {
                session_id: "placement".to_owned(),
                pinned: true,
                opacity: 0.8,
                placement: ResultPlacement {
                    size: expected_size,
                    cursor: expected_cursor,
                    follow_cursor: false,
                },
                reveal_phase: ResultRevealPhase::Hidden,
                reveal_operation: Arc::new(Mutex::new(())),
                focus_seen: false,
                blur_generation: 0,
                pointer_inside: false,
                pointer_seen: false,
                dismiss_mode: DismissMode::Manual,
                dismiss_delay_ms: 450,
                hide_generation: 0,
                pending_auto_close: None,
                remember_size: true,
                ignore_resize_until: Instant::now() + std::time::Duration::from_secs(5),
            },
        );

        for (expected_phase, next_phase) in [
            (ResultRevealPhase::Hidden, ResultRevealPhase::Prepared),
            (ResultRevealPhase::Prepared, ResultRevealPhase::Committed),
        ] {
            let (phase, pinned, placement) = coordinator
                .result_reveal_context(label)
                .expect("the reveal session should remain available");
            assert_eq!(phase, expected_phase);
            assert!(pinned);
            assert_eq!(placement.size, expected_size);
            assert_eq!(placement.cursor, expected_cursor);
            assert!(!placement.follow_cursor);
            coordinator
                .set_result_reveal_phase(label, expected_phase, next_phase)
                .expect("the reveal phase should advance exactly once");
        }

        let (phase, _, placement) = coordinator.result_reveal_context(label).unwrap();
        assert_eq!(phase, ResultRevealPhase::Committed);
        assert_eq!(placement.size, expected_size);
        assert_eq!(placement.cursor, expected_cursor);
        assert!(!placement.follow_cursor);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn window_operations_run_inline_only_on_the_hwnd_owner_thread() {
        assert!(windows_window_operation_runs_inline(41, 41));
        assert!(!windows_window_operation_runs_inline(41, 42));
        assert!(!windows_window_operation_runs_inline(41, 0));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn timed_out_window_operation_is_cancelled_before_dispatch() {
        use std::sync::atomic::AtomicU8;

        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        let dispatch_state = AtomicU8::new(WINDOWS_WINDOW_OPERATION_PENDING);
        let error =
            wait_for_windows_window_operation(receiver, &dispatch_state, Duration::from_millis(5))
                .expect_err("an undispatched operation should time out");

        assert_eq!(
            dispatch_state.load(Ordering::Acquire),
            WINDOWS_WINDOW_OPERATION_CANCELLED
        );
        assert!(!try_start_windows_window_operation(&dispatch_state));
        assert!(error.to_string().contains("timed out before it started"));
    }

    #[test]
    fn result_labels_are_namespaced() {
        assert_eq!(result_label("abc"), "selection-result-abc");
    }

    #[test]
    fn pointer_leave_uses_one_physical_coordinate_space() {
        assert!(!point_is_outside_physical_rect(
            -1_800.0, 240.0, -2_000.0, 100.0, 600.0, 500.0,
        ));
        assert!(point_is_outside_physical_rect(
            -2_100.0, 240.0, -2_000.0, 100.0, 600.0, 500.0,
        ));
        assert!(point_is_outside_physical_rect(
            -1_800.0, 700.0, -2_000.0, 100.0, 600.0, 500.0,
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn toolbar_pointer_is_converted_to_webview_css_coordinates() {
        let inside = toolbar_pointer_position_from_physical(
            1_500.0, 760.0, 1_200.0, 700.0, 1_040.0, 88.0, 2.0,
        );
        assert_eq!(inside.x, 150.0);
        assert_eq!(inside.y, 30.0);
        assert!(inside.inside);

        let outside = toolbar_pointer_position_from_physical(
            1_100.0, 760.0, 1_200.0, 700.0, 1_040.0, 88.0, 2.0,
        );
        assert_eq!(outside.x, -50.0);
        assert_eq!(outside.y, 30.0);
        assert!(!outside.inside);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn toolbar_pointer_conversion_handles_negative_mixed_dpi_coordinates() {
        let pointer = toolbar_pointer_position_from_physical(
            -2_250.0, -45.0, -2_400.0, -120.0, 650.0, 55.0, 1.25,
        );
        assert_eq!(pointer.x, 120.0);
        assert_eq!(pointer.y, 60.0);
        assert!(!pointer.inside);

        let pointer = toolbar_pointer_position_from_physical(
            -2_250.0, -95.0, -2_400.0, -120.0, 650.0, 55.0, 1.25,
        );
        assert_eq!(pointer.x, 120.0);
        assert_eq!(pointer.y, 20.0);
        assert!(pointer.inside);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn toolbar_pointer_ipc_is_limited_to_inside_entry_move_and_leave() {
        let outside = ToolbarPointerPosition {
            x: -40.0,
            y: 12.0,
            inside: false,
        };
        let moved_outside = ToolbarPointerPosition {
            x: -20.0,
            y: 12.0,
            inside: false,
        };
        assert!(toolbar_pointer_sample_changed(outside, moved_outside));
        assert!(!toolbar_pointer_sample_should_emit(outside, moved_outside));

        let entered = ToolbarPointerPosition {
            x: 1.0,
            y: 12.0,
            inside: true,
        };
        assert!(toolbar_pointer_sample_should_emit(moved_outside, entered));

        let subpixel_jitter = ToolbarPointerPosition {
            x: 1.1,
            y: 12.1,
            inside: true,
        };
        assert!(!toolbar_pointer_sample_should_emit(
            entered,
            subpixel_jitter
        ));

        let moved_inside = ToolbarPointerPosition {
            x: 20.0,
            y: 12.0,
            inside: true,
        };
        assert!(toolbar_pointer_sample_should_emit(entered, moved_inside));
        assert!(toolbar_pointer_sample_should_emit(
            moved_inside,
            moved_outside
        ));
    }

    #[test]
    fn toolbar_centers_on_pointer_and_aligns_its_top_on_a_negative_monitor() {
        let area = WorkArea {
            x: -1_920.0,
            y: 0.0,
            width: 1_920.0,
            height: 1_080.0,
        };
        let size = WindowSize {
            width: 520.0,
            height: 44.0,
        };
        let position = toolbar_position_in_area(
            Point {
                x: -960.0,
                y: 360.0,
            },
            size,
            area,
            1.0,
        );
        assert_eq!(position.x, -1_220.0);
        assert_eq!(position.x + size.width / 2.0, -960.0);
        assert_eq!(position.y, 360.0);

        let clamped = toolbar_position_in_area(
            Point {
                x: -1_850.0,
                y: 1_060.0,
            },
            size,
            area,
            1.0,
        );
        assert_eq!(clamped.x, -1_900.0);
        assert_eq!(clamped.y, 1_016.0);
    }

    #[test]
    fn startup_notice_uses_the_target_work_area_bottom_center() {
        let position = startup_notice_position_in_area(
            WindowSize {
                width: 335.0,
                height: 67.5,
            },
            WorkArea {
                x: -2_560.0,
                y: -120.0,
                width: 2_560.0,
                height: 1_390.0,
            },
            1.25,
        );

        assert_eq!(position.x, -1_447.5);
        assert_eq!(position.y, 1_180.0);
    }

    #[test]
    fn result_anchors_click_at_one_fifth_width_and_clamps_at_work_area_edges() {
        let area = WorkArea {
            x: 0.0,
            y: 0.0,
            width: 1_440.0,
            height: 900.0,
        };
        let size = WindowSize {
            width: 520.0,
            height: 420.0,
        };
        let anchored = result_position_in_area(Point { x: 800.0, y: 450.0 }, size, area, 1.0);
        assert_eq!(anchored, Point { x: 696.0, y: 366.0 });

        let top = result_position_in_area(Point { x: 800.0, y: 20.0 }, size, area, 1.0);
        assert_eq!(top, Point { x: 696.0, y: 8.0 });

        let edge = result_position_in_area(Point { x: 40.0, y: 880.0 }, size, area, 1.0);
        assert_eq!(edge.x, 8.0);
        assert_eq!(edge.y, 472.0);
    }

    #[test]
    fn mixed_dpi_monitor_selection_uses_physical_virtual_desktop_edges() {
        let monitors = [
            MonitorGeometry {
                bounds: WorkArea {
                    x: 0.0,
                    y: 0.0,
                    width: 1_920.0,
                    height: 1_080.0,
                },
                work_area: WorkArea {
                    x: 0.0,
                    y: 0.0,
                    width: 1_920.0,
                    height: 1_040.0,
                },
                coordinate_scale: 1.0,
            },
            MonitorGeometry {
                bounds: WorkArea {
                    x: 1_920.0,
                    y: 0.0,
                    width: 2_560.0,
                    height: 1_440.0,
                },
                work_area: WorkArea {
                    x: 1_920.0,
                    y: 0.0,
                    width: 2_560.0,
                    height: 1_380.0,
                },
                coordinate_scale: 1.5,
            },
        ];

        assert_eq!(
            monitor_index_for_point(
                &monitors,
                Point {
                    x: 1_919.0,
                    y: 400.0
                }
            ),
            Some(0)
        );
        assert_eq!(
            monitor_index_for_point(
                &monitors,
                Point {
                    x: 1_920.0,
                    y: 400.0
                }
            ),
            Some(1)
        );
        assert_eq!(
            monitor_index_for_point(
                &monitors,
                Point {
                    x: 2_100.0,
                    y: 400.0
                }
            ),
            Some(1)
        );
    }

    #[test]
    fn negative_mixed_dpi_monitor_keeps_its_physical_origin() {
        let monitors = [
            MonitorGeometry {
                bounds: WorkArea {
                    x: -2_560.0,
                    y: -120.0,
                    width: 2_560.0,
                    height: 1_440.0,
                },
                work_area: WorkArea {
                    x: -2_560.0,
                    y: -120.0,
                    width: 2_560.0,
                    height: 1_390.0,
                },
                coordinate_scale: 1.25,
            },
            MonitorGeometry {
                bounds: WorkArea {
                    x: 0.0,
                    y: 0.0,
                    width: 1_920.0,
                    height: 1_080.0,
                },
                work_area: WorkArea {
                    x: 0.0,
                    y: 0.0,
                    width: 1_920.0,
                    height: 1_040.0,
                },
                coordinate_scale: 1.0,
            },
        ];

        assert_eq!(
            monitor_index_for_point(&monitors, Point { x: -1.0, y: 200.0 }),
            Some(0)
        );
        assert_eq!(
            monitor_index_for_point(&monitors, Point { x: 0.0, y: 200.0 }),
            Some(1)
        );
    }

    #[test]
    fn mixed_dpi_toolbar_and_result_are_clamped_in_target_work_area() {
        let geometry = MonitorGeometry {
            bounds: WorkArea {
                x: 1_920.0,
                y: 0.0,
                width: 2_560.0,
                height: 1_440.0,
            },
            work_area: WorkArea {
                x: 1_920.0,
                y: 0.0,
                width: 2_560.0,
                height: 1_380.0,
            },
            coordinate_scale: 1.5,
        };
        let toolbar_size = toolbar_native_frame_size(
            logical_size_to_coordinates(
                WindowSize {
                    width: 520.0,
                    height: 44.0,
                },
                geometry.coordinate_scale,
            ),
            geometry.coordinate_scale,
        );
        let toolbar = toolbar_position_in_area(
            Point {
                x: 1_925.0,
                y: 1_370.0,
            },
            toolbar_size,
            geometry.work_area,
            geometry.coordinate_scale,
        );
        assert!(toolbar.x >= geometry.work_area.x + 12.0);
        assert!(
            toolbar.x + toolbar_size.width
                <= geometry.work_area.x + geometry.work_area.width - 12.0
        );
        assert!(toolbar.y >= geometry.work_area.y + 12.0);
        assert!(
            toolbar.y + toolbar_size.height
                <= geometry.work_area.y + geometry.work_area.height - 12.0
        );

        let icon_toolbar = toolbar_native_frame_size(
            logical_size_to_coordinates(
                WindowSize {
                    width: 132.0,
                    height: 36.0,
                },
                geometry.coordinate_scale,
            ),
            geometry.coordinate_scale,
        );
        let icon_toolbar_at_right_edge = toolbar_position_in_area(
            Point {
                x: geometry.work_area.x + geometry.work_area.width - 1.0,
                y: 80.0,
            },
            icon_toolbar,
            geometry.work_area,
            geometry.coordinate_scale,
        );
        assert!(
            icon_toolbar_at_right_edge.x + icon_toolbar.width
                <= geometry.work_area.x + geometry.work_area.width - 12.0
        );

        let logical_result = fit_logical_size_to_geometry(
            WindowSize {
                width: 1_600.0,
                height: 1_200.0,
            },
            geometry,
        );
        let result_size = logical_size_to_coordinates(logical_result, geometry.coordinate_scale);
        let result = result_position_in_area(
            Point {
                x: 4_470.0,
                y: 1_370.0,
            },
            result_size,
            geometry.work_area,
            geometry.coordinate_scale,
        );
        assert!(result.x >= geometry.work_area.x + 12.0);
        assert!(
            result.x + result_size.width <= geometry.work_area.x + geometry.work_area.width - 12.0
        );
        assert!(result.y >= geometry.work_area.y + 12.0);
        assert!(
            result.y + result_size.height
                <= geometry.work_area.y + geometry.work_area.height - 12.0
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn toolbar_frame_guard_scales_with_content_and_dpi() {
        let icon_at_100 = toolbar_native_frame_size(
            WindowSize {
                width: 132.0,
                height: 36.0,
            },
            1.0,
        );
        let icon_at_150 = toolbar_native_frame_size(
            WindowSize {
                width: 198.0,
                height: 54.0,
            },
            1.5,
        );
        let wide = toolbar_native_frame_size(
            WindowSize {
                width: 1_000.0,
                height: 54.0,
            },
            1.5,
        );

        assert_eq!(icon_at_100.width, 138.0);
        assert_eq!(icon_at_150.width, 206.0);
        assert_eq!(wide.width, 1_012.0);
    }

    #[test]
    fn initial_false_focus_event_cannot_close_a_new_result() {
        let mut runtime = ResultRuntime {
            session_id: "session".to_owned(),
            pinned: false,
            opacity: 1.0,
            placement: ResultPlacement {
                size: WindowSize {
                    width: 520.0,
                    height: 420.0,
                },
                cursor: Point { x: 640.0, y: 360.0 },
                follow_cursor: true,
            },
            reveal_phase: ResultRevealPhase::Committed,
            reveal_operation: Arc::new(Mutex::new(())),
            focus_seen: false,
            blur_generation: 0,
            pointer_inside: false,
            pointer_seen: false,
            dismiss_mode: DismissMode::Blur,
            dismiss_delay_ms: 450,
            hide_generation: 0,
            pending_auto_close: None,
            remember_size: true,
            ignore_resize_until: Instant::now(),
        };
        assert_eq!(runtime.note_focus_change(false), None);
        assert_eq!(runtime.note_focus_change(true), None);
        let blur = runtime.note_focus_change(false).expect("blur token");
        assert!(runtime.blur_is_current(blur));

        runtime.pinned = true;
        assert_eq!(runtime.note_focus_change(false), None);
    }

    #[test]
    fn pointer_leave_wait_does_not_close_while_user_moves_to_toolbar() {
        assert!(!pointer_leave_should_close(true, true, true));
        assert!(pointer_leave_should_close(true, true, false));
        assert!(!pointer_leave_should_close(false, true, false));
        assert!(!pointer_leave_should_close(true, false, false));
    }

    #[test]
    fn toolbar_native_queue_defers_reentrant_hide_without_holding_a_mutex() {
        let queue = Arc::new(Mutex::new(ToolbarNativeQueue::default()));
        let steps = Arc::new(Mutex::new(Vec::new()));
        let nested_queue = Arc::clone(&queue);
        let nested_steps = Arc::clone(&steps);
        enqueue_toolbar_native_job(
            &queue,
            Box::new(move || {
                assert!(
                    nested_queue.try_lock().is_some(),
                    "native work must release the queue mutex"
                );
                nested_steps.lock().push("present-start");
                let hide_steps = Arc::clone(&nested_steps);
                enqueue_toolbar_native_job(
                    &nested_queue,
                    Box::new(move || {
                        hide_steps.lock().push("hide");
                    }),
                );
                // The native event handler returned instead of blocking on the
                // presentation transaction it synchronously interrupted.
                nested_steps.lock().push("present-end");
            }),
        );
        assert_eq!(*steps.lock(), vec!["present-start", "present-end", "hide"]);
        assert!(!queue.lock().running);
        assert!(queue.lock().pending.is_empty());
    }

    #[test]
    fn monitor_wait_cannot_block_main_thread_hide_or_restore_old_toolbar_owner() {
        use std::sync::{mpsc, Barrier};
        let coordinator = WindowCoordinator::default();
        coordinator.begin_toolbar_selection("old");
        let state = Arc::clone(&coordinator.state);
        let (reserved_sender, reserved_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let worker = std::thread::spawn(move || {
            let (revision, _) = state
                .lock()
                .reserve_toolbar_stage(
                    "old",
                    Point { x: 1.0, y: 2.0 },
                    ToolbarPlacement::BottomMiddle,
                )
                .unwrap();
            reserved_sender.send(revision).unwrap();
            worker_barrier.wait();
            // Represents a synchronous available_monitors dispatcher wait.
            let _ = release_receiver.recv();
            revision
        });
        let old_revision = reserved_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        barrier.wait();
        assert!(coordinator.state.try_lock().is_some());
        assert!(coordinator.toolbar_operation.try_lock().is_some());
        let hide_revision = coordinator
            .state
            .lock()
            .reserve_toolbar_operation(Some("old"))
            .unwrap();
        let hide_state = Arc::clone(&coordinator.state);
        enqueue_toolbar_native_job(
            &coordinator.toolbar_operation,
            Box::new(move || {
                assert!(hide_state.lock().clear_toolbar_if_current(hide_revision));
            }),
        );
        assert_eq!(coordinator.state.lock().toolbar_selection_id, None);
        release_sender.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), old_revision);
        assert!(!coordinator
            .state
            .lock()
            .toolbar_operation_is_current(old_revision));
    }

    #[test]
    fn toolbar_revision_rejects_old_stage_and_hide_even_when_selection_id_is_reused() {
        let coordinator = WindowCoordinator::default();
        let mut state = coordinator.state.lock();
        state.begin_toolbar_selection("a");
        let (old_stage, _) = state
            .reserve_toolbar_stage(
                "a",
                Point { x: 10.0, y: 20.0 },
                ToolbarPlacement::BottomMiddle,
            )
            .unwrap();
        let old_hide = state.reserve_toolbar_operation(Some("a")).unwrap();
        assert!(
            !state.toolbar_selection_is_current("a"),
            "late present must not supersede a pending hide"
        );
        let (new_stage, _) = state
            .reserve_toolbar_stage(
                "a",
                Point { x: 20.0, y: 30.0 },
                ToolbarPlacement::BottomRight,
            )
            .unwrap();
        assert!(!state.toolbar_operation_is_current(old_stage));
        assert!(!state.clear_toolbar_if_current(old_hide));
        assert!(state.toolbar_operation_is_current(new_stage));
        assert!(
            state.toolbar_selection_is_current("a"),
            "explicit restaging can reopen the selection"
        );
        assert_eq!(state.toolbar_anchor, Some(Point { x: 20.0, y: 30.0 }));
        state.begin_toolbar_selection("b");
        assert!(!state.toolbar_selection_is_current("a"));
        assert!(state
            .reserve_toolbar_stage(
                "a",
                Point { x: 10.0, y: 20.0 },
                ToolbarPlacement::BottomMiddle,
            )
            .is_none());
        let (newer_stage, _) = state
            .reserve_toolbar_stage(
                "b",
                Point { x: 40.0, y: 50.0 },
                ToolbarPlacement::BottomMiddle,
            )
            .unwrap();
        assert!(!state.clear_toolbar_if_current(new_stage));
        assert!(state.toolbar_operation_is_current(newer_stage));
        assert_eq!(state.toolbar_selection_id.as_deref(), Some("b"));
    }

    #[test]
    fn measured_resize_inherits_pending_reveal_and_dismiss_revokes_it() {
        let coordinator = WindowCoordinator::default();
        let mut state = coordinator.state.lock();
        state.begin_toolbar_selection("a");
        state
            .reserve_toolbar_stage(
                "a",
                Point { x: 10.0, y: 20.0 },
                ToolbarPlacement::BottomMiddle,
            )
            .unwrap();
        assert!(!state.toolbar_reveal_requested);
        let present_revision = state.request_toolbar_reveal();
        let resize_revision = state.invalidate_toolbar_operations();
        assert!(!state.toolbar_operation_is_current(present_revision));
        assert!(state.toolbar_operation_is_current(resize_revision));
        assert!(state.toolbar_reveal_requested);
        assert!(state.toolbar_present_pending);
        // The resize callback must perform the latest reveal. A later dismiss
        // changes both the revision and visibility intent before it can run.
        state.reserve_toolbar_operation(Some("a")).unwrap();
        assert!(!state.toolbar_operation_is_current(resize_revision));
        assert!(!state.toolbar_reveal_requested);
        assert!(!state.toolbar_present_pending);
        assert!(!state.toolbar_selection_is_current("a"));
    }

    #[test]
    fn failed_toolbar_hide_keeps_owner_and_rebuild_invalidates_pending_native_work() {
        let coordinator = WindowCoordinator::default();
        let mut state = coordinator.state.lock();
        state.begin_toolbar_selection("a");
        state
            .reserve_toolbar_stage(
                "a",
                Point { x: 10.0, y: 20.0 },
                ToolbarPlacement::BottomMiddle,
            )
            .unwrap();
        let hide_revision = state.reserve_toolbar_operation(Some("a")).unwrap();
        // A failed native hide never calls clear_toolbar_if_current.
        assert_eq!(state.toolbar_selection_id.as_deref(), Some("a"));
        assert!(state.toolbar_operation_is_current(hide_revision));
        assert!(!state.toolbar_selection_is_current("a"));
        assert!(state.reserve_toolbar_recovery("a"));
        assert!(!state.toolbar_operation_is_current(hide_revision));
        assert_eq!(state.toolbar_selection_id.as_deref(), Some("a"));
    }

    #[test]
    fn toolbar_width_budget_uses_target_work_area_in_renderer_css_units() {
        let mut geometry = MonitorGeometry {
            bounds: WorkArea {
                x: -1_280.0,
                y: 0.0,
                width: 1_280.0,
                height: 720.0,
            },
            work_area: WorkArea {
                x: -1_280.0,
                y: 0.0,
                width: 1_280.0,
                height: 680.0,
            },
            coordinate_scale: 1.0,
        };
        assert_eq!(toolbar_max_width_for_geometry(geometry), 900.0);
        geometry.work_area.width = 800.0;
        assert_eq!(toolbar_max_width_for_geometry(geometry), 760.0);
        geometry.coordinate_scale = 2.0;
        assert_eq!(toolbar_max_width_for_geometry(geometry), 360.0);
        geometry.coordinate_scale = 1.25;
        assert_eq!(toolbar_max_width_for_geometry(geometry), 600.0);
        geometry.work_area.width = 50.0;
        assert_eq!(toolbar_max_width_for_geometry(geometry), 44.0);
    }

    #[test]
    fn commit_toolbar_selection_updates_ownership_without_requiring_ui_work() {
        let coordinator = WindowCoordinator::default();
        let mut state = coordinator.state.lock();
        let anchor = Point { x: 120.0, y: 80.0 };
        let size = state.commit_toolbar_selection("selection-show", anchor);
        assert_eq!(size, state.toolbar_size);
        assert_eq!(
            state.toolbar_selection_id.as_deref(),
            Some("selection-show")
        );
        assert_eq!(state.toolbar_anchor, Some(anchor));

        // Drop the state lock before any main-thread window work would run.
        // This documents the show_toolbar ordering that prevents the macOS
        // tray deadlock (events thread holds state → waits for main orderFront;
        // main holds nested popup menu → remove_result waits for state).
        drop(state);
        let state = coordinator.state.lock();
        assert_eq!(
            state.toolbar_selection_id.as_deref(),
            Some("selection-show")
        );
    }

    #[test]
    fn toolbar_recovery_state_is_single_use_per_selection() {
        let coordinator = WindowCoordinator::default();
        let mut state = coordinator.state.lock();
        state.begin_toolbar_selection("selection-a");
        assert!(state.reserve_toolbar_recovery("selection-a"));
        assert!(!state.reserve_toolbar_recovery("selection-a"));
        assert_eq!(
            state.take_toolbar_recovery_pending().as_deref(),
            Some("selection-a")
        );
        assert_eq!(state.take_toolbar_recovery_pending(), None);
        assert!(!state.reserve_toolbar_recovery("selection-a"));

        state.begin_toolbar_selection("selection-b");
        assert!(!state.reserve_toolbar_recovery("selection-a"));
        assert!(state.reserve_toolbar_recovery("selection-b"));
        assert_eq!(
            state.toolbar_recovery_selection_id.as_deref(),
            Some("selection-b")
        );
        assert_eq!(state.toolbar_recovery_attempts, 1);
    }

    #[test]
    fn failed_toolbar_recovery_can_be_retried_without_leaving_pending_state() {
        let coordinator = WindowCoordinator::default();
        let mut state = coordinator.state.lock();
        state.begin_toolbar_selection("selection-a");
        assert!(state.reserve_toolbar_recovery("selection-a"));
        state.cancel_toolbar_recovery("selection-a");
        assert_eq!(state.toolbar_recovery_pending_selection_id, None);
        assert_eq!(state.toolbar_recovery_attempts, 0);
        assert!(state.reserve_toolbar_recovery("selection-a"));
    }

    #[test]
    fn newer_selection_invalidates_a_queued_stale_toolbar_recovery() {
        let coordinator = WindowCoordinator::default();
        let mut state = coordinator.state.lock();
        state.begin_toolbar_selection("selection-a");
        assert!(state.reserve_toolbar_recovery("selection-a"));
        assert_eq!(
            state.toolbar_recovery_pending_selection_id.as_deref(),
            Some("selection-a")
        );

        state.begin_toolbar_selection("selection-b");
        assert_eq!(state.toolbar_recovery_pending_selection_id, None);
        assert!(!state.reserve_toolbar_recovery("selection-a"));
        assert!(state.reserve_toolbar_recovery("selection-b"));
    }
}
