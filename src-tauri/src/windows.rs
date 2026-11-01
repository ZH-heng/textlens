use std::{collections::HashMap, sync::Arc, time::Instant};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
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

#[cfg(any(target_os = "windows", test))]
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
    remember_size: bool,
    ignore_resize_until: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultRevealPhase {
    Hidden,
    Prepared,
    Committed,
}

#[cfg(target_os = "windows")]
fn initial_result_reveal_phase() -> ResultRevealPhase {
    ResultRevealPhase::Hidden
}

#[cfg(not(target_os = "windows"))]
fn initial_result_reveal_phase() -> ResultRevealPhase {
    ResultRevealPhase::Committed
}

fn initial_result_resize_ignore_until() -> Instant {
    #[cfg(target_os = "windows")]
    {
        // The result is not user-resizable until reveal commit. Ignore every
        // WebView/native initialization resize, then enable persistence at
        // the exact commit boundary.
        Instant::now() + std::time::Duration::from_millis(3_500)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Instant::now() + std::time::Duration::from_millis(180)
    }
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

    #[cfg(any(test, target_os = "windows"))]
    fn blur_is_current(&self, generation: u64) -> bool {
        self.reveal_phase == ResultRevealPhase::Committed
            && self.focus_seen
            && !self.pinned
            && self.dismiss_mode == DismissMode::Blur
            && self.blur_generation == generation
    }
}

struct WindowState {
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
            self.toolbar_recovery_selection_id = Some(selection_id.to_owned());
            self.toolbar_recovery_attempts = 0;
            self.toolbar_recovery_pending_selection_id = None;
        }
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

#[derive(Clone)]
pub struct WindowCoordinator {
    state: Arc<Mutex<WindowState>>,
    /// Serializes the native toolbar transaction across stage, layout, reveal
    /// and hide. State rechecks alone cannot protect the gap between AppKit
    /// calls where a stale reveal could otherwise resurrect the toolbar.
    toolbar_operation: Arc<Mutex<()>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    toolbar_tracking_generation: Arc<AtomicU64>,
}

impl Default for WindowCoordinator {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(WindowState {
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
            toolbar_operation: Arc::new(Mutex::new(())),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            toolbar_tracking_generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl WindowCoordinator {
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
        let window = self.ensure_toolbar(app)?;
        let _toolbar_operation = self.toolbar_operation.lock();
        // Never hold WindowState across UI-thread hops. On macOS,
        // order_front_without_focus (and often set_position/set_size) blocks
        // until the AppKit main thread runs the work. Holding the coordinator
        // lock while waiting deadlocks with main-thread handlers such as
        // WindowEvent::Destroyed → remove_result, which freezes the tray icon.
        let size = {
            let mut state = self.state.lock();
            state.commit_toolbar_selection_with_placement(selection_id, anchor, placement)
        };
        let layout = toolbar_layout_with_placement(&window, anchor, size, placement)?;
        // A concurrent hide/rebuild may have replaced this selection while we
        // computed layout off-lock. Do not resurrect a stale toolbar.
        if self.state.lock().toolbar_selection_id.as_deref() != Some(selection_id) {
            return Ok(());
        }
        // On Windows keep all monitor/scale queries outside the event-loop
        // callback. Calling Tauri window getters from a `run_on_main_thread`
        // closure can wait on the same dispatcher and leave the toolbar
        // permanently hidden after the first stalled presentation.
        #[cfg(target_os = "windows")]
        apply_toolbar_layout(&window, layout, true)?;
        #[cfg(not(target_os = "windows"))]
        {
            apply_window_layout(&window, layout)?;
            order_front_without_focus(&window)?;
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        self.start_toolbar_pointer_tracking(app, &window);
        Ok(())
    }

    /// Stages a Windows toolbar update while keeping the singleton WebView
    /// ready for the renderer update. The native frame remains hidden until
    /// the renderer commits the new selection and measured size, so a reused
    /// WebView can never expose an old clickable frame for a new selection.
    #[cfg(target_os = "windows")]
    pub fn stage_toolbar(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
    ) -> tauri::Result<()> {
        self.stage_toolbar_with_placement(app, selection_id, anchor, ToolbarPlacement::BottomMiddle)
    }

    #[cfg(target_os = "windows")]
    pub fn stage_toolbar_with_placement(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
        placement: ToolbarPlacement,
    ) -> tauri::Result<()> {
        let window = self.ensure_toolbar(app)?;
        let _toolbar_operation = self.toolbar_operation.lock();
        // Move the hidden native frame before publishing the next selection.
        // Renderer updates remain generation-checked, and no cold WebView2
        // frame can receive a click before the matching selection is rendered.
        stage_windows_toolbar_with_placement(
            &window,
            Arc::clone(&self.state),
            selection_id.to_owned(),
            anchor,
            placement,
        )?;
        Ok(())
    }

    /// Atomically positions, sizes and reveals the prepared Windows toolbar.
    /// Keeping this as one `SetWindowPos` prevents a stale frame or intermediate
    /// size from becoming visible on cold start and after an action error.
    #[cfg(target_os = "windows")]
    pub fn present_toolbar(
        &self,
        app: &AppHandle,
        selection_id: &str,
        size: WindowSize,
    ) -> tauri::Result<bool> {
        let window = self.ensure_toolbar(app)?;
        let _toolbar_operation = self.toolbar_operation.lock();
        let native_started = Instant::now();
        let size = toolbar_size(size);
        let presented = present_windows_toolbar(
            &window,
            Arc::clone(&self.state),
            selection_id.to_owned(),
            size,
        )?;
        if !presented {
            return Ok(false);
        }
        trace_toolbar_timing("native-visible", native_started);
        self.start_toolbar_pointer_tracking(app, &window);
        Ok(true)
    }

    /// Stages a macOS toolbar update while keeping the singleton WebView fully
    /// transparent. AppKit's `order_front_without_focus` is idempotent, so
    /// unlike Windows this does not need a paired native hide — dropping
    /// alpha to 0 is enough to guarantee the next `present_toolbar` is the
    /// first visible frame at the new selection's position/size.
    #[cfg(target_os = "macos")]
    pub fn stage_toolbar(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
    ) -> tauri::Result<()> {
        self.stage_toolbar_with_placement(app, selection_id, anchor, ToolbarPlacement::BottomMiddle)
    }

    #[cfg(target_os = "macos")]
    pub fn stage_toolbar_with_placement(
        &self,
        app: &AppHandle,
        selection_id: &str,
        anchor: Point,
        placement: ToolbarPlacement,
    ) -> tauri::Result<()> {
        let window = self.ensure_toolbar(app)?;
        let _toolbar_operation = self.toolbar_operation.lock();
        // The alpha hop only matters when a previous toolbar frame could
        // still be visible. Every hide path clears `toolbar_selection_id`,
        // so None means the singleton window is already invisible and the
        // cross-thread hop can be skipped for the most common case: a fresh
        // selection arriving while the toolbar is hidden.
        let was_hidden = self.state.lock().toolbar_selection_id.is_none();
        if !was_hidden {
            // Never hold WindowState across the AppKit main-thread hop (see
            // show_toolbar's invariant above): perform the alpha change
            // first, then take the lock only for plain in-memory bookkeeping.
            set_native_alpha_raw(&window, 0.0)?;
        }
        {
            let mut state = self.state.lock();
            state.toolbar_anchor = Some(anchor);
            state.toolbar_placement = Some(placement);
            state.toolbar_selection_id = Some(selection_id.to_owned());
        }
        self.stop_toolbar_pointer_tracking();
        Ok(())
    }

    /// Positions, sizes and reveals the prepared macOS toolbar. AppKit has no
    /// single-call equivalent of Windows' atomic `SetWindowPos`, so position
    /// and size are applied while alpha is still 0 and only the final alpha
    /// flip to 1 is ever visible — matching Windows' one-visible-frame commit.
    #[cfg(target_os = "macos")]
    pub fn present_toolbar(
        &self,
        app: &AppHandle,
        selection_id: &str,
        size: WindowSize,
    ) -> tauri::Result<bool> {
        let window = self.ensure_toolbar(app)?;
        let _toolbar_operation = self.toolbar_operation.lock();
        let native_started = Instant::now();
        let size = toolbar_size(size);
        let (anchor, placement) = {
            let mut state = self.state.lock();
            if state.toolbar_selection_id.as_deref() != Some(selection_id) {
                return Ok(false);
            }
            state.toolbar_size = size;
            (
                state.toolbar_anchor,
                state.toolbar_placement.unwrap_or_default(),
            )
        };
        let Some(anchor) = anchor else {
            return Ok(false);
        };
        let layout = toolbar_layout_with_placement(&window, anchor, size, placement)?;
        // A concurrent hide/newer selection may have replaced this one while
        // layout was computed off-lock (mirrors show_toolbar's existing
        // recheck).
        if self.state.lock().toolbar_selection_id.as_deref() != Some(selection_id) {
            return Ok(false);
        }
        commit_toolbar_presentation(&window, layout)?;
        trace_toolbar_timing("native-visible", native_started);
        // NSTrackingArea helps WKWebView receive mouse-move while non-key, but
        // it is not reliable enough alone for continuous hover across action
        // buttons. Keep the same 60 Hz pointer sampler Windows uses so
        // `data-hovered` (background / soft shadow) updates on every move.
        self.start_toolbar_pointer_tracking(app, &window);
        Ok(true)
    }

    pub fn hide_toolbar(&self, app: &AppHandle) {
        let selection_id = self.state.lock().toolbar_selection_id.clone();
        if let Some(selection_id) = selection_id {
            let _ = self.hide_toolbar_if_selection(app, &selection_id);
            return;
        }
        let _toolbar_operation = self.toolbar_operation.lock();
        let hidden = if let Some(window) = app.get_webview_window(TOOLBAR_LABEL) {
            hide_toolbar_window(&window).is_ok()
        } else {
            true
        };
        if hidden {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            self.stop_toolbar_pointer_tracking();
            let mut state = self.state.lock();
            state.toolbar_anchor = None;
            state.toolbar_placement = None;
            state.toolbar_selection_id = None;
        }
    }

    /// Hides a toolbar without making the selection event consumer wait for
    /// the Windows UI dispatcher. The native operation rechecks the expected
    /// selection id before mutating the singleton window, so a newer staged
    /// selection cannot be hidden by an older dismiss request.
    pub fn hide_toolbar_if_selection_async(&self, app: &AppHandle, selection_id: &str) {
        let coordinator = self.clone();
        let app = app.clone();
        let selection_id = selection_id.to_owned();
        let _ = std::thread::Builder::new()
            .name("textlens-toolbar-hide".to_owned())
            .spawn(move || {
                let _ = coordinator.hide_toolbar_if_selection(&app, &selection_id);
            });
    }

    /// Fallback hide for a dismiss event that has no logical selection owner.
    /// This is also kept off the selection event thread for the same reason as
    /// the selection-scoped variant above.
    pub fn hide_toolbar_async(&self, app: &AppHandle) {
        let coordinator = self.clone();
        let app = app.clone();
        let _ = std::thread::Builder::new()
            .name("textlens-toolbar-hide".to_owned())
            .spawn(move || coordinator.hide_toolbar(&app));
    }

    pub fn hide_toolbar_if_selection(&self, app: &AppHandle, selection_id: &str) -> bool {
        let Some(window) = app.get_webview_window(TOOLBAR_LABEL) else {
            return false;
        };
        let _toolbar_operation = self.toolbar_operation.lock();
        #[cfg(target_os = "windows")]
        let hidden = hide_windows_toolbar_if_selection(
            &window,
            Arc::clone(&self.state),
            selection_id.to_owned(),
        )
        .unwrap_or(false);
        #[cfg(not(target_os = "windows"))]
        let hidden = {
            if self.state.lock().toolbar_selection_id.as_deref() != Some(selection_id) {
                return false;
            }
            // On Windows `WebviewWindow::hide()` from a command thread only
            // enqueues a runtime message. Waiting for a native UI-thread hide here
            // keeps the visual window and its selection owner in sync and leaves
            // the owner intact when hiding fails so the renderer can retry.
            if hide_toolbar_window(&window).is_err() {
                return false;
            }
            let mut state = self.state.lock();
            if state.toolbar_selection_id.as_deref() != Some(selection_id) {
                false
            } else {
                state.toolbar_anchor = None;
                state.toolbar_placement = None;
                state.toolbar_selection_id = None;
                true
            }
        };
        if hidden {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            self.stop_toolbar_pointer_tracking();
        }
        hidden
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
        let window = app.get_webview_window(TOOLBAR_LABEL);
        let _toolbar_operation = self.toolbar_operation.lock();
        #[cfg(target_os = "windows")]
        if let Some(window) = window {
            return update_windows_toolbar_size(
                &window,
                Arc::clone(&self.state),
                selection_id.map(str::to_owned),
                size,
            );
        }
        #[cfg(target_os = "windows")]
        {
            let mut state = self.state.lock();
            if selection_id.is_some() && state.toolbar_selection_id.as_deref() != selection_id {
                return Ok(false);
            }
            state.toolbar_size = size;
            return Ok(true);
        }
        #[cfg(not(target_os = "windows"))]
        let (anchor, placement) = {
            let mut state = self.state.lock();
            if selection_id.is_some() && state.toolbar_selection_id.as_deref() != selection_id {
                return Ok(false);
            }
            if state.toolbar_size == size {
                return Ok(true);
            }
            state.toolbar_size = size;
            (
                state.toolbar_anchor,
                state.toolbar_placement.unwrap_or_default(),
            )
        };
        #[cfg(not(target_os = "windows"))]
        if let Some(window) = window {
            if let (true, Some(anchor)) = (window.is_visible().unwrap_or(false), anchor) {
                let layout = toolbar_layout_with_placement(&window, anchor, size, placement)?;
                #[cfg(target_os = "windows")]
                apply_toolbar_layout(&window, layout, false)?;
                #[cfg(not(target_os = "windows"))]
                apply_window_layout(&window, layout)?;
            } else {
                set_window_size_for_current_monitor(&window, size)?;
            }
        }
        #[cfg(not(target_os = "windows"))]
        Ok(true)
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
        #[cfg(not(target_os = "windows"))]
        {
            let layout = match (|| -> tauri::Result<WindowLayout> {
                configure_result_native(&window, options.pinned)
                    .map_err(|error| result_window_step_error("native configuration", error))?;
                prime_result_window_opacity(&window, options.opacity)
                    .map_err(|error| result_window_step_error("opacity preparation", error))?;
                let size = fit_size_to_monitor(&window, options.cursor, size)
                    .map_err(|error| result_window_step_error("monitor sizing", error))?;
                if options.follow_cursor {
                    result_layout(&window, options.cursor, size)
                        .map_err(|error| result_window_step_error("cursor layout", error))
                } else {
                    centered_layout(&window, options.cursor, size)
                        .map_err(|error| result_window_step_error("centered layout", error))
                }
            })() {
                Ok(layout) => layout,
                Err(error) => {
                    self.state.lock().results.remove(&label);
                    let _ = window.close();
                    return Err(error);
                }
            };
            let show_result = apply_window_layout(&window, layout)
                .map_err(|error| result_window_step_error("layout application", error))
                .and_then(|_| show_result_window(&window, options.pinned, options.opacity));
            if let Err(error) = show_result {
                self.state.lock().results.remove(&label);
                let _ = window.close();
                return Err(error);
            }
        }
        Ok(window)
    }

    /// Makes a fully hydrated Windows result WebView participate in native
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
            !pinned
                && runtime.pointer_seen
                && !runtime.pointer_inside
                && runtime.dismiss_mode == DismissMode::PointerLeave
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
                if dismiss_mode == DismissMode::PointerLeave
                    && !runtime.pinned
                    && runtime.pointer_seen
                    && !runtime.pointer_inside
                {
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
            runtime.pointer_inside = inside;
            if inside {
                runtime.pointer_seen = true;
            }
            runtime.hide_generation = runtime.hide_generation.wrapping_add(1);
            !inside
                && runtime.pointer_seen
                && !runtime.pinned
                && runtime.dismiss_mode == DismissMode::PointerLeave
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
        if blur_generation.is_some() {
            if let Some(window) = app.get_webview_window(label) {
                let _ = window.close();
            }
        }
        #[cfg(target_os = "windows")]
        if let Some(blur_generation) = blur_generation {
            self.schedule_verified_result_blur(app.clone(), label.to_owned(), blur_generation);
        }
    }

    #[cfg(target_os = "windows")]
    fn schedule_verified_result_blur(&self, app: AppHandle, label: String, blur_generation: u64) {
        let state = Arc::clone(&self.state);
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
                        let still_current = state
                            .lock()
                            .results
                            .get(&label)
                            .is_some_and(|runtime| runtime.blur_is_current(blur_generation));
                        if still_current {
                            let _ = window.close();
                        }
                        return;
                    }
                }
            }
        });
    }

    pub fn close_result(&self, app: &AppHandle, label: &str) -> tauri::Result<()> {
        if let Some(window) = app.get_webview_window(label) {
            window.close()?;
        } else {
            self.state.lock().results.remove(label);
        }
        Ok(())
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
        runtime.remember_size && Instant::now() >= runtime.ignore_resize_until
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
            let should_hide = coordinator
                .state
                .lock()
                .results
                .get(&label)
                .is_some_and(|runtime| {
                    runtime.hide_generation == generation
                        && !runtime.pointer_inside
                        && !runtime.pinned
                        && runtime.dismiss_mode == DismissMode::PointerLeave
                });
            let cursor_outside_result = cursor_is_outside(&app, &label);
            let cursor_inside_toolbar = cursor_is_inside_visible_window(&app, TOOLBAR_LABEL);
            if pointer_leave_should_close(should_hide, cursor_outside_result, cursor_inside_toolbar)
            {
                if let Some(window) = app.get_webview_window(&label) {
                    let _ = window.close();
                }
            } else if should_hide && cursor_outside_result && cursor_inside_toolbar {
                // Treat the selection toolbar as a temporary extension of the
                // result window. Keep checking so the source result still
                // closes after the toolbar disappears or the pointer leaves it.
                coordinator.schedule_hide(app, label);
            }
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
fn apply_toolbar_layout(
    window: &WebviewWindow,
    layout: WindowLayout,
    show: bool,
) -> tauri::Result<()> {
    run_windows_window_operation(window, move |window| {
        apply_toolbar_layout_raw(window, layout, show)
    })
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
fn hide_toolbar_window(window: &WebviewWindow) -> tauri::Result<()> {
    run_windows_window_operation(window, hide_toolbar_window_raw)
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

#[cfg(target_os = "windows")]
fn stage_windows_toolbar(
    window: &WebviewWindow,
    state: Arc<Mutex<WindowState>>,
    selection_id: String,
    anchor: Point,
) -> tauri::Result<()> {
    stage_windows_toolbar_with_placement(
        window,
        state,
        selection_id,
        anchor,
        ToolbarPlacement::BottomMiddle,
    )
}

#[cfg(target_os = "windows")]
fn stage_windows_toolbar_with_placement(
    window: &WebviewWindow,
    state: Arc<Mutex<WindowState>>,
    selection_id: String,
    anchor: Point,
    placement: ToolbarPlacement,
) -> tauri::Result<()> {
    // Resolve monitor geometry before entering the event-loop callback. WRY's
    // monitor and scale getters may synchronously dispatch work; using them
    // from the UI callback itself can deadlock that dispatcher on Windows.
    let size = {
        let mut state = state.lock();
        state.toolbar_anchor = Some(anchor);
        state.toolbar_placement = Some(placement);
        state.toolbar_selection_id = Some(selection_id.clone());
        state.toolbar_size
    };
    let layout = toolbar_layout_with_placement(window, anchor, size, placement)?;
    run_windows_window_operation(window, move |window| {
        // Do not let a delayed native callback resurrect an older selection.
        if state.lock().toolbar_selection_id.as_deref() != Some(selection_id.as_str()) {
            return Ok(());
        }
        // Hide the old frame before moving it. The renderer will reveal this
        // generation through present_toolbar after React has received the new
        // selection and measured the actual DOM size.
        hide_toolbar_window_raw(window)?;
        apply_toolbar_layout_raw(window, layout, false)?;
        Ok(())
    })
}

#[cfg(target_os = "windows")]
fn present_windows_toolbar(
    window: &WebviewWindow,
    state: Arc<Mutex<WindowState>>,
    selection_id: String,
    size: WindowSize,
) -> tauri::Result<bool> {
    let presented = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let committed = Arc::clone(&presented);
    let (anchor, placement) = {
        let state = state.lock();
        if state.toolbar_selection_id.as_deref() != Some(selection_id.as_str()) {
            return Ok(false);
        }
        let anchor = state
            .toolbar_anchor
            .ok_or_else(|| std::io::Error::other("toolbar anchor is unavailable"))?;
        let placement = state.toolbar_placement.unwrap_or_default();
        (anchor, placement)
    };
    // See `stage_windows_toolbar_with_placement`: this must stay outside the
    // UI operation to avoid re-entering the Windows event dispatcher.
    let layout = toolbar_layout_with_placement(window, anchor, size, placement)?;
    run_windows_window_operation(window, move |window| {
        if state.lock().toolbar_selection_id.as_deref() != Some(selection_id.as_str()) {
            return Ok(());
        }
        apply_toolbar_layout_raw(window, layout, true)?;
        // Publish the measured size only after the native visible commit has
        // succeeded. A stale or timed-out operation must not poison the next
        // selection's initial layout with an unpresented size.
        state.lock().toolbar_size = size;
        committed.store(true, Ordering::Release);
        Ok(())
    })?;
    Ok(presented.load(Ordering::Acquire))
}

#[cfg(target_os = "windows")]
fn hide_windows_toolbar_if_selection(
    window: &WebviewWindow,
    state: Arc<Mutex<WindowState>>,
    selection_id: String,
) -> tauri::Result<bool> {
    let hidden = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let committed = Arc::clone(&hidden);
    run_windows_window_operation(window, move |window| {
        let mut state = state.lock();
        if state.toolbar_selection_id.as_deref() != Some(selection_id.as_str()) {
            return Ok(());
        }
        hide_toolbar_window_raw(window)?;
        state.toolbar_anchor = None;
        state.toolbar_placement = None;
        state.toolbar_selection_id = None;
        committed.store(true, Ordering::Release);
        Ok(())
    })?;
    Ok(hidden.load(Ordering::Acquire))
}

#[cfg(target_os = "windows")]
fn update_windows_toolbar_size(
    window: &WebviewWindow,
    state: Arc<Mutex<WindowState>>,
    selection_id: Option<String>,
    size: WindowSize,
) -> tauri::Result<bool> {
    let anchor_and_placement = {
        let state = state.lock();
        if selection_id.is_some()
            && state.toolbar_selection_id.as_deref() != selection_id.as_deref()
        {
            return Ok(false);
        }
        if state.toolbar_size == size {
            return Ok(true);
        }
        state
            .toolbar_anchor
            .map(|anchor| (anchor, state.toolbar_placement.unwrap_or_default()))
    };

    let Some((anchor, placement)) = anchor_and_placement else {
        // There is no staged selection yet. Scale lookup is safe here because
        // this call is outside the native UI operation.
        set_window_size_for_current_monitor(window, size)?;
        let mut state = state.lock();
        if selection_id.is_some()
            && state.toolbar_selection_id.as_deref() != selection_id.as_deref()
        {
            return Ok(false);
        }
        state.toolbar_size = size;
        return Ok(true);
    };
    let layout = toolbar_layout_with_placement(window, anchor, size, placement)?;
    let updated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let committed = Arc::clone(&updated);
    run_windows_window_operation(window, move |window| {
        {
            let state = state.lock();
            if selection_id.is_some()
                && state.toolbar_selection_id.as_deref() != selection_id.as_deref()
            {
                return Ok(());
            }
        }
        apply_toolbar_layout_raw(window, layout, false)?;
        let mut state = state.lock();
        if selection_id.is_some()
            && state.toolbar_selection_id.as_deref() != selection_id.as_deref()
        {
            return Ok(());
        }
        state.toolbar_size = size;
        committed.store(true, Ordering::Release);
        Ok(())
    })?;
    Ok(updated.load(Ordering::Acquire))
}

#[cfg(not(target_os = "windows"))]
fn hide_toolbar_window(window: &WebviewWindow) -> tauri::Result<()> {
    window.hide()
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
fn set_window_size_for_current_monitor(
    window: &WebviewWindow,
    logical_size: WindowSize,
) -> tauri::Result<()> {
    let size = logical_size_to_coordinates(logical_size, current_window_coordinate_scale(window));
    window.set_size(PhysicalSize::new(
        physical_dimension(size.width),
        physical_dimension(size.height),
    ))
}

#[cfg(not(target_os = "windows"))]
fn set_window_size_for_current_monitor(
    window: &WebviewWindow,
    logical_size: WindowSize,
) -> tauri::Result<()> {
    window.set_size(LogicalSize::new(logical_size.width, logical_size.height))
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
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};

    let app = window.app_handle().clone();
    let window = window.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = window.ns_window().map(|pointer| unsafe {
            let window = &*(pointer as *const NSWindow);
            let behavior = if pinned {
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
            } else {
                NSWindowCollectionBehavior::MoveToActiveSpace
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
            };
            window.setCollectionBehavior(behavior);
        });
        let _ = sender.send(result);
    })?;
    receiver
        .recv_timeout(MACOS_MAIN_THREAD_HOP_TIMEOUT)
        .map_err(|_| tauri::Error::FailedToReceiveMessage)??;
    Ok(())
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
#[cfg(target_os = "macos")]
fn commit_toolbar_presentation(window: &WebviewWindow, layout: WindowLayout) -> tauri::Result<()> {
    use objc2_app_kit::NSWindow;

    let app = window.app_handle().clone();
    let window = window.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = (|| -> tauri::Result<()> {
            apply_window_layout(&window, layout)?;
            let pointer = window.ns_window()?;
            unsafe {
                let ns_window = &*(pointer as *const NSWindow);
                ns_window.orderFrontRegardless();
                ns_window.setAlphaValue(1.0);
            }
            Ok(())
        })();
        let _ = sender.send(result);
    })?;
    receiver
        .recv_timeout(MACOS_MAIN_THREAD_HOP_TIMEOUT)
        .map_err(|_| tauri::Error::FailedToReceiveMessage)??;
    Ok(())
}

#[cfg(target_os = "macos")]
fn order_front_without_focus(window: &WebviewWindow) -> tauri::Result<()> {
    use objc2_app_kit::NSWindow;

    let app = window.app_handle().clone();
    let window = window.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = window.ns_window().map(|pointer| unsafe {
            let window = &*(pointer as *const NSWindow);
            window.orderFrontRegardless();
        });
        let _ = sender.send(result);
    })?;
    receiver
        .recv_timeout(MACOS_MAIN_THREAD_HOP_TIMEOUT)
        .map_err(|_| tauri::Error::FailedToReceiveMessage)??;
    Ok(())
}

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

    let app = window.app_handle().clone();
    let window = window.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = window.ns_window().map(|pointer| unsafe {
            let window = &*(pointer as *const NSWindow);
            window.setAlphaValue(alpha);
        });
        let _ = sender.send(result);
    })?;
    receiver
        .recv_timeout(MACOS_MAIN_THREAD_HOP_TIMEOUT)
        .map_err(|_| tauri::Error::FailedToReceiveMessage)??;
    Ok(())
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

#[cfg(not(target_os = "windows"))]
fn prime_result_window_opacity(window: &WebviewWindow, opacity: f64) -> tauri::Result<()> {
    set_native_opacity(window, opacity)
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

#[cfg(not(target_os = "windows"))]
fn prepare_result_window(
    _window: &WebviewWindow,
    _pinned: bool,
    _placement: ResultPlacement,
) -> tauri::Result<()> {
    Ok(())
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

#[cfg(not(target_os = "windows"))]
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
    fn result_reveal_starts_hidden_only_on_windows() {
        #[cfg(target_os = "windows")]
        assert_eq!(initial_result_reveal_phase(), ResultRevealPhase::Hidden);
        #[cfg(not(target_os = "windows"))]
        assert_eq!(initial_result_reveal_phase(), ResultRevealPhase::Committed);
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
