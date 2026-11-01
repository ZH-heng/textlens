//! Thread-safe selection capture facade for the native platform backends.
//!
//! On macOS `SelectionMonitor` owns the native event tap and callback context.
//! On Windows it owns a supervised selection worker, a separately pumped
//! low-level hook thread, and a restartable helper process for untrusted
//! UI Automation/OLE calls. Native hook callbacks only enqueue raw input;
//! selection and window work is always performed outside those callbacks.

#[cfg(target_os = "windows")]
#[path = "../../apps/windows/src/selection.rs"]
mod selection_windows;

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::mpsc::{self, Receiver};

#[cfg(target_os = "macos")]
use std::sync::mpsc::Sender;
#[cfg(target_os = "windows")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use std::ffi::{c_char, c_void, CStr, CString};
#[cfg(target_os = "macos")]
use std::ptr::NonNull;

#[cfg(target_os = "windows")]
use selection_windows::WindowsSelectionMonitor;

#[cfg(target_os = "windows")]
pub fn run_windows_selection_helper_if_requested() -> bool {
    selection_windows::run_selection_helper_if_requested()
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_clear_matching_text(bundle_id: Option<&str>, text: &str) -> bool {
    selection_windows::clear_matching_text(bundle_id, text)
}

pub type SelectionEventReceiver = Receiver<SelectionEvent>;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceApplication {
    pub bundle_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionDirection {
    Forward,
    Backward,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionMethod {
    Accessibility,
    Clipboard,
    /// Selection read from the source app's live DOM/AppleScript interface
    /// (Safari `do JavaScript`, Chrome-kernel `execute javascript`). macOS
    /// only; sits between the Accessibility and Clipboard capture paths.
    // The native bridge reports this variant as lowercase "applescript";
    // derive(camelCase) would produce "appleScript".
    #[serde(rename = "applescript")]
    AppleScript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionTrigger {
    Drag,
    DoubleClick,
    ShiftClick,
    Keyboard,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionMouse {
    pub start: Option<SelectionPoint>,
    pub end: Option<SelectionPoint>,
    pub current: SelectionPoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionPayload {
    pub text: String,
    pub source_app: SourceApplication,
    pub bounds: Option<SelectionBounds>,
    pub start_top: Option<SelectionPoint>,
    pub start_bottom: Option<SelectionPoint>,
    pub end_top: Option<SelectionPoint>,
    pub end_bottom: Option<SelectionPoint>,
    pub mouse: SelectionMouse,
    pub direction: SelectionDirection,
    pub is_fullscreen: bool,
    pub method: SelectionMethod,
    pub trigger: SelectionTrigger,
    pub timestamp_ms: u64,
    /// Monotonic emission counter from the native monitor. Zero means the
    /// producer does not supply one (Windows), so ordering falls back to the
    /// millisecond timestamp.
    #[serde(default)]
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismissEvent {
    /// Currently `mouseDown`, `scroll`, or `keyDown`.
    pub reason: String,
    pub mouse: SelectionPoint,
    pub target_pid: i64,
    pub timestamp_ms: u64,
    #[serde(default)]
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SelectionEvent {
    Selection(SelectionPayload),
    Dismiss(DismissEvent),
}

#[derive(Debug)]
pub enum SelectionError {
    UnsupportedPlatform,
    InvalidBundleIdentifier,
    NativeInitializationFailed,
    AccessibilityPermissionRequired,
    EventTapFailed,
    InvalidArgument,
    Internal,
    MalformedNativePayload(String),
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("selection capture is only available on macOS and Windows")
            }
            Self::InvalidBundleIdentifier => {
                formatter.write_str("bundle identifier contains a null byte")
            }
            Self::NativeInitializationFailed => {
                formatter.write_str("failed to initialize native selection monitor")
            }
            Self::AccessibilityPermissionRequired => {
                formatter.write_str("macOS Accessibility permission is required")
            }
            Self::EventTapFailed => formatter.write_str("failed to create the global input hook"),
            Self::InvalidArgument => formatter.write_str("invalid native selection argument"),
            Self::Internal => formatter.write_str("native selection monitor failed"),
            Self::MalformedNativePayload(message) => {
                write!(formatter, "invalid native selection payload: {message}")
            }
        }
    }
}

impl std::error::Error for SelectionError {}

#[cfg(any(target_os = "macos", test))]
#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum WireEvent {
    Selection {
        text: String,
        source_app: SourceApplication,
        bounds: Option<SelectionBounds>,
        start_top: Option<SelectionPoint>,
        start_bottom: Option<SelectionPoint>,
        end_top: Option<SelectionPoint>,
        end_bottom: Option<SelectionPoint>,
        mouse: SelectionMouse,
        direction: SelectionDirection,
        is_fullscreen: bool,
        method: SelectionMethod,
        trigger: SelectionTrigger,
        timestamp_ms: u64,
        #[serde(default)]
        sequence: u64,
    },
    Dismiss {
        reason: String,
        mouse: SelectionPoint,
        target_pid: i64,
        timestamp_ms: u64,
        #[serde(default)]
        sequence: u64,
    },
}

#[cfg(any(target_os = "macos", test))]
impl From<WireEvent> for SelectionEvent {
    fn from(event: WireEvent) -> Self {
        match event {
            WireEvent::Selection {
                text,
                source_app,
                bounds,
                start_top,
                start_bottom,
                end_top,
                end_bottom,
                mouse,
                direction,
                is_fullscreen,
                method,
                trigger,
                timestamp_ms,
                sequence,
            } => Self::Selection(SelectionPayload {
                text,
                source_app,
                bounds,
                start_top,
                start_bottom,
                end_top,
                end_bottom,
                mouse,
                direction,
                is_fullscreen,
                method,
                trigger,
                timestamp_ms,
                sequence,
            }),
            WireEvent::Dismiss {
                reason,
                mouse,
                target_pid,
                timestamp_ms,
                sequence,
            } => Self::Dismiss(DismissEvent {
                reason,
                mouse,
                target_pid,
                timestamp_ms,
                sequence,
            }),
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_native_event(json: &str) -> Result<SelectionEvent, SelectionError> {
    serde_json::from_str::<WireEvent>(json)
        .map(SelectionEvent::from)
        .map_err(|error| SelectionError::MalformedNativePayload(error.to_string()))
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct NativeSelectionMonitor {
    _private: [u8; 0],
}

#[cfg(target_os = "macos")]
type NativeCallback = extern "C" fn(*const c_char, *mut c_void);

#[cfg(target_os = "macos")]
extern "C" {
    fn textlens_accessibility_is_trusted() -> u8;
    fn textlens_accessibility_request() -> u8;
    fn textlens_selection_monitor_create(
        excluded_bundle_id_utf8: *const c_char,
        callback: NativeCallback,
        context: *mut c_void,
    ) -> *mut NativeSelectionMonitor;
    fn textlens_selection_monitor_start(monitor: *mut NativeSelectionMonitor) -> i32;
    fn textlens_selection_monitor_stop(monitor: *mut NativeSelectionMonitor) -> i32;
    fn textlens_selection_monitor_capture_current(
        monitor: *mut NativeSelectionMonitor,
        status_out: *mut i32,
    ) -> *mut c_char;
    fn textlens_selection_string_free(value: *mut c_char);
    fn textlens_selection_clear_matching_text(
        bundle_id_utf8: *const c_char,
        text_utf8: *const c_char,
    ) -> u8;
    fn textlens_selection_monitor_destroy(monitor: *mut NativeSelectionMonitor);
}

#[cfg(target_os = "macos")]
struct CallbackContext {
    sender: Sender<SelectionEvent>,
}

#[cfg(target_os = "macos")]
extern "C" fn native_event_callback(json: *const c_char, context: *mut c_void) {
    if json.is_null() || context.is_null() {
        return;
    }
    // SAFETY: Native code keeps `json` alive during this call. `context` is a
    // boxed CallbackContext owned by SelectionMonitor and is dropped only after
    // native destroy has stopped and joined both callback-producing threads.
    let json = unsafe { CStr::from_ptr(json) };
    let Ok(json) = json.to_str() else {
        return;
    };
    let Ok(event) = parse_native_event(json) else {
        return;
    };
    let callback_context = unsafe { &*context.cast::<CallbackContext>() };
    let _ = callback_context.sender.send(event);
}

/// Owns the native event tap. Call `take_event_receiver` once before starting
/// the monitor, then consume that receiver off the Tauri main thread.
pub struct SelectionMonitor {
    receiver: Option<SelectionEventReceiver>,
    #[cfg(target_os = "windows")]
    excluded_identifier: String,
    #[cfg(target_os = "macos")]
    native: NonNull<NativeSelectionMonitor>,
    #[cfg(target_os = "macos")]
    callback_context: NonNull<CallbackContext>,
    #[cfg(target_os = "windows")]
    windows: WindowsSelectionMonitor,
}

impl fmt::Debug for SelectionMonitor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SelectionMonitor")
            .field("receiver_taken", &self.receiver.is_none())
            .finish_non_exhaustive()
    }
}

// SAFETY: The native monitor serializes lifecycle/capture operations and owns
// its callback threads. SelectionMonitor is deliberately not Sync, so it must
// be moved as a unit or placed behind a Mutex by its owner.
#[cfg(target_os = "macos")]
unsafe impl Send for SelectionMonitor {}

impl SelectionMonitor {
    #[cfg(target_os = "macos")]
    pub fn new(excluded_bundle_id: &str) -> Result<Self, SelectionError> {
        let excluded_bundle_id = CString::new(excluded_bundle_id)
            .map_err(|_| SelectionError::InvalidBundleIdentifier)?;
        let (sender, receiver) = mpsc::channel();
        let callback_context = Box::new(CallbackContext { sender });
        let callback_context = NonNull::new(Box::into_raw(callback_context))
            .expect("Box::into_raw never returns null");

        // SAFETY: The C string and callback function are valid for the call;
        // the boxed context remains alive until Drop, after native teardown.
        let native = unsafe {
            textlens_selection_monitor_create(
                excluded_bundle_id.as_ptr(),
                native_event_callback,
                callback_context.as_ptr().cast(),
            )
        };
        let Some(native) = NonNull::new(native) else {
            // SAFETY: Native creation failed and therefore retained no pointer
            // to the context.
            unsafe { drop(Box::from_raw(callback_context.as_ptr())) };
            return Err(SelectionError::NativeInitializationFailed);
        };

        Ok(Self {
            receiver: Some(receiver),
            native,
            callback_context,
        })
    }

    #[cfg(target_os = "windows")]
    pub fn new(excluded_bundle_id: &str) -> Result<Self, SelectionError> {
        let (sender, receiver) = mpsc::channel();
        let windows =
            WindowsSelectionMonitor::new(excluded_bundle_id, Arc::new(Mutex::new(sender)))?;
        Ok(Self {
            receiver: Some(receiver),
            excluded_identifier: excluded_bundle_id.to_owned(),
            windows,
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn new(_excluded_bundle_id: &str) -> Result<Self, SelectionError> {
        Err(SelectionError::UnsupportedPlatform)
    }

    /// Returns the sole event receiver. Subsequent calls return `None`.
    pub fn take_event_receiver(&mut self) -> Option<SelectionEventReceiver> {
        self.receiver.take()
    }

    /// Rebuilds the Windows native producer and returns a fresh event receiver
    /// after the previous event channel has disconnected. The replacement has
    /// no hooks started yet; the old producer is stopped before the caller's
    /// next reconcile starts the new lifecycle.
    #[cfg(target_os = "windows")]
    pub fn rebuild_event_receiver(&mut self) -> Result<SelectionEventReceiver, SelectionError> {
        let (sender, receiver) = mpsc::channel();
        let replacement =
            WindowsSelectionMonitor::new(&self.excluded_identifier, Arc::new(Mutex::new(sender)))?;
        let old = std::mem::replace(&mut self.windows, replacement);
        old.shutdown();
        self.receiver = Some(receiver);
        self.take_event_receiver().ok_or(SelectionError::Internal)
    }

    #[cfg(target_os = "macos")]
    pub fn start(&self) -> Result<(), SelectionError> {
        // SAFETY: `native` is owned by self and remains alive for this call.
        let status = unsafe { textlens_selection_monitor_start(self.native.as_ptr()) };
        match status {
            0 | 1 => Ok(()),
            status => Err(error_from_native_status(status)),
        }
    }

    #[cfg(target_os = "windows")]
    pub fn start(&self) -> Result<(), SelectionError> {
        self.windows.start()
    }

    /// Updates Windows-only capture routing without restarting the global hook.
    /// The worker snapshots this setting onto each gesture before forwarding it
    /// to the isolated UIA/OLE helper process.
    #[cfg(target_os = "windows")]
    pub fn update_capture_settings(
        &self,
        settings: crate::models::SelectionCaptureSettings,
    ) -> Result<(), SelectionError> {
        self.windows.update_capture_settings(settings)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn start(&self) -> Result<(), SelectionError> {
        Err(SelectionError::UnsupportedPlatform)
    }

    #[cfg(target_os = "macos")]
    pub fn stop(&self) -> Result<(), SelectionError> {
        // SAFETY: `native` is owned by self and remains alive for this call.
        let status = unsafe { textlens_selection_monitor_stop(self.native.as_ptr()) };
        match status {
            0 | 2 => Ok(()),
            status => Err(error_from_native_status(status)),
        }
    }

    #[cfg(target_os = "windows")]
    pub fn stop(&self) -> Result<(), SelectionError> {
        self.windows.stop()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn stop(&self) -> Result<(), SelectionError> {
        Err(SelectionError::UnsupportedPlatform)
    }

    /// Stops producing selection events during application shutdown without
    /// requiring the caller to synchronously join a potentially blocked
    /// Windows UI Automation worker.
    pub fn shutdown(&self) {
        #[cfg(target_os = "macos")]
        {
            let _ = self.stop();
        }
        #[cfg(target_os = "windows")]
        {
            self.windows.shutdown();
        }
    }

    /// Captures the frontmost application's current selection without requiring
    /// the event tap to be running. `None` means no usable non-empty selection.
    #[cfg(target_os = "macos")]
    pub fn capture_current(&self) -> Result<Option<SelectionPayload>, SelectionError> {
        let mut status = 0;
        // SAFETY: `native` and status pointer remain valid for this call.
        let json = unsafe {
            textlens_selection_monitor_capture_current(self.native.as_ptr(), &mut status)
        };
        if status != 0 {
            if !json.is_null() {
                unsafe { textlens_selection_string_free(json) };
            }
            return Err(error_from_native_status(status));
        }
        let Some(json) = NonNull::new(json) else {
            return Ok(None);
        };

        struct NativeString(NonNull<c_char>);
        impl Drop for NativeString {
            fn drop(&mut self) {
                // SAFETY: This allocation came from the matching native ABI.
                unsafe { textlens_selection_string_free(self.0.as_ptr()) };
            }
        }
        let json = NativeString(json);
        let json = unsafe { CStr::from_ptr(json.0.as_ptr()) }
            .to_str()
            .map_err(|error| SelectionError::MalformedNativePayload(error.to_string()))?;
        match parse_native_event(json)? {
            SelectionEvent::Selection(selection) => Ok(Some(selection)),
            SelectionEvent::Dismiss(_) => Err(SelectionError::MalformedNativePayload(
                "capture returned a dismiss event".to_owned(),
            )),
        }
    }

    #[cfg(target_os = "windows")]
    pub fn capture_current(&self) -> Result<Option<SelectionPayload>, SelectionError> {
        self.windows.capture_current()
    }

    /// Enqueues a Windows manual capture without holding the Rust facade lock
    /// until the accessibility provider replies. The runtime uses this for
    /// global-shortcut captures so a second key press can reach the native
    /// latest-wins coordinator and supersede a slow first probe.
    #[cfg(target_os = "windows")]
    pub fn capture_current_async(
        &self,
    ) -> Result<Receiver<Result<Option<SelectionPayload>, SelectionError>>, SelectionError> {
        self.windows.capture_current_async()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn capture_current(&self) -> Result<Option<SelectionPayload>, SelectionError> {
        Err(SelectionError::UnsupportedPlatform)
    }

    pub fn is_accessibility_trusted() -> bool {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: This native function takes no pointers and has no ownership
            // transfer.
            return unsafe { textlens_accessibility_is_trusted() != 0 };
        }
        // Windows UI Automation does not use a user-granted permission like
        // macOS Accessibility. UIPI can still prevent a normal process from
        // reading an elevated application's controls; that is handled as an
        // unavailable selection rather than a global permission failure.
        #[cfg(target_os = "windows")]
        {
            true
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        false
    }

    /// Requests native selection access where the platform requires it.
    /// Windows UI Automation has no equivalent user prompt and returns true.
    pub fn request_accessibility() -> bool {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: This native function takes no pointers and has no ownership
            // transfer.
            return unsafe { textlens_accessibility_request() != 0 };
        }
        #[cfg(target_os = "windows")]
        {
            true
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        false
    }

    /// Best-effort: collapse host selection when it still equals `text`.
    ///
    /// Returns `true` only when the platform wrote a collapsed caret range.
    /// Empty text, no focused match, unwritable AX/UIA, or non-mac/win hosts
    /// all return `false` without panicking.
    pub fn clear_matching_text(bundle_id: Option<&str>, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        #[cfg(target_os = "macos")]
        {
            let Ok(text_c) = CString::new(text) else {
                return false;
            };
            let bundle_c = bundle_id
                .filter(|value| !value.is_empty())
                .and_then(|value| CString::new(value).ok());
            let bundle_ptr = bundle_c
                .as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null());
            // SAFETY: pointers are valid C strings or null for the duration of the call.
            return unsafe {
                textlens_selection_clear_matching_text(bundle_ptr, text_c.as_ptr()) != 0
            };
        }
        #[cfg(target_os = "windows")]
        {
            windows_clear_matching_text(bundle_id, text)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = bundle_id;
            false
        }
    }
}

#[cfg(any(target_os = "windows", test))]
fn direction_from_points(
    start: Option<SelectionPoint>,
    end: Option<SelectionPoint>,
) -> SelectionDirection {
    let (Some(start), Some(end)) = (start, end) else {
        return SelectionDirection::Unknown;
    };
    let delta_y = end.y - start.y;
    let delta_x = end.x - start.x;
    if delta_y.abs() > 2.0 {
        return if delta_y > 0.0 {
            SelectionDirection::Forward
        } else {
            SelectionDirection::Backward
        };
    }
    if delta_x.abs() > 0.5 {
        return if delta_x > 0.0 {
            SelectionDirection::Forward
        } else {
            SelectionDirection::Backward
        };
    }
    SelectionDirection::Unknown
}

#[cfg(any(target_os = "windows", test))]
fn classify_windows_mouse_selection(
    start: SelectionPoint,
    end: SelectionPoint,
    shift_held: bool,
    is_double_click: bool,
    drag_duration_valid: bool,
) -> Option<SelectionTrigger> {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    if drag_duration_valid && dx * dx + dy * dy >= 16.0 {
        return Some(SelectionTrigger::Drag);
    }
    if is_double_click {
        return Some(SelectionTrigger::DoubleClick);
    }
    shift_held.then_some(SelectionTrigger::ShiftClick)
}

#[cfg(any(target_os = "windows", test))]
fn selection_bounds_are_reasonable(rectangle: SelectionBounds) -> bool {
    rectangle.x.is_finite()
        && rectangle.y.is_finite()
        && rectangle.width.is_finite()
        && rectangle.height.is_finite()
        && rectangle.width > 0.0
        && rectangle.height > 0.0
        && rectangle.width < 200_000.0
        && rectangle.height < 200_000.0
        && rectangle.x.abs() < 500_000.0
        && rectangle.y.abs() < 500_000.0
}

#[cfg(any(target_os = "windows", test))]
fn union_selection_bounds(rectangles: &[SelectionBounds]) -> Option<SelectionBounds> {
    let mut valid = rectangles
        .iter()
        .copied()
        .filter(|rectangle| selection_bounds_are_reasonable(*rectangle));
    let first = valid.next()?;
    let mut left = first.x;
    let mut top = first.y;
    let mut right = first.x + first.width;
    let mut bottom = first.y + first.height;
    for rectangle in valid {
        left = left.min(rectangle.x);
        top = top.min(rectangle.y);
        right = right.max(rectangle.x + rectangle.width);
        bottom = bottom.max(rectangle.y + rectangle.height);
    }
    Some(SelectionBounds {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

#[cfg(any(target_os = "windows", test))]
fn automatic_selection_pointer_matches_bounds(
    selection: &SelectionPayload,
    tolerance: f64,
) -> bool {
    let Some(bounds) = selection.bounds else {
        return true;
    };
    let point = selection.mouse.current;
    point.x >= bounds.x - tolerance
        && point.x <= bounds.x + bounds.width + tolerance
        && point.y >= bounds.y - tolerance
        && point.y <= bounds.y + bounds.height + tolerance
}

#[cfg(any(target_os = "windows", test))]
fn automatic_selection_fingerprint(selection: &SelectionPayload) -> u64 {
    use std::{collections::hash_map::DefaultHasher, hash::Hash, hash::Hasher};

    let mut hasher = DefaultHasher::new();
    selection.source_app.bundle_id.hash(&mut hasher);
    selection.text.hash(&mut hasher);
    if let Some(bounds) = selection.bounds {
        for value in [bounds.x, bounds.y, bounds.width, bounds.height] {
            // Half-point quantization prevents harmless provider jitter from
            // defeating duplicate suppression.
            ((value * 2.0).round() as i64).hash(&mut hasher);
        }
    } else {
        // Clipboard fallback has no text bounds. Include a coarse release
        // position so selecting the same word at two different locations in
        // quick succession is not mistaken for one duplicate hook event.
        for value in [selection.mouse.current.x, selection.mouse.current.y] {
            ((value / 8.0).round() as i64).hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[cfg(any(target_os = "windows", test))]
fn windows_text_budget(total_chars: usize, needs_separator: bool) -> Option<(usize, i32)> {
    const MAX_CAPTURE_TEXT_CHARS: usize = 1_000_000;
    const UIA_GET_TEXT_LIMIT: usize = MAX_CAPTURE_TEXT_CHARS + 1;

    let used = total_chars.checked_add(usize::from(needs_separator))?;
    let remaining = MAX_CAPTURE_TEXT_CHARS.checked_sub(used)?;
    let request_limit = remaining.saturating_add(1).min(UIA_GET_TEXT_LIMIT) as i32;
    Some((remaining, request_limit))
}

/// Returns true when a live UIA selection should be collapsed.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn should_clear_host_selection(
    expected_text: &str,
    live_text: &str,
    expected_bundle: Option<&str>,
    live_bundle: Option<&str>,
) -> bool {
    if expected_text.is_empty() || expected_text != live_text {
        return false;
    }
    match expected_bundle.map(str::trim).filter(|v| !v.is_empty()) {
        None => true,
        Some(expected) => live_bundle.is_some_and(|live| live == expected),
    }
}

#[cfg(target_os = "macos")]
impl Drop for SelectionMonitor {
    fn drop(&mut self) {
        // SAFETY: Destroy first stops and joins all native threads. Only then is
        // it safe to release the callback context they may reference.
        unsafe {
            textlens_selection_monitor_destroy(self.native.as_ptr());
            drop(Box::from_raw(self.callback_context.as_ptr()));
        }
    }
}

#[cfg(target_os = "macos")]
fn error_from_native_status(status: i32) -> SelectionError {
    match status {
        -1 => SelectionError::AccessibilityPermissionRequired,
        -2 => SelectionError::EventTapFailed,
        -3 => SelectionError::InvalidArgument,
        _ => SelectionError::Internal,
    }
}

#[cfg(test)]
mod clear_selection_tests {
    use super::should_clear_host_selection;

    #[test]
    fn requires_exact_text_match() {
        assert!(!should_clear_host_selection("hello", "hell", None, None));
        assert!(should_clear_host_selection("hello", "hello", None, None));
    }

    #[test]
    fn respects_bundle_when_provided() {
        assert!(!should_clear_host_selection(
            "hello",
            "hello",
            Some("notepad.exe"),
            Some("code.exe")
        ));
        assert!(should_clear_host_selection(
            "hello",
            "hello",
            Some("notepad.exe"),
            Some("notepad.exe")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_matching_text_rejects_empty_payload() {
        assert!(!SelectionMonitor::clear_matching_text(
            Some("com.apple.TextEdit"),
            ""
        ));
    }

    #[test]
    fn parses_selection_wire_payload() {
        let event = parse_native_event(
            r#"{
                "type":"selection",
                "text":"hello",
                "sourceApp":{"bundleId":"com.apple.TextEdit","name":"TextEdit"},
                "bounds":{"x":-120.0,"y":44.0,"width":80.0,"height":20.0},
                "startTop":{"x":-120.0,"y":44.0},
                "startBottom":{"x":-120.0,"y":64.0},
                "endTop":{"x":-40.0,"y":44.0},
                "endBottom":{"x":-40.0,"y":64.0},
                "mouse":{"start":{"x":-120.0,"y":55.0},"end":{"x":-40.0,"y":55.0},"current":{"x":-40.0,"y":55.0}},
                "direction":"forward",
                "isFullscreen":false,
                "method":"accessibility",
                "trigger":"drag",
                "timestampMs":42
            }"#,
        )
        .expect("valid selection event");

        let SelectionEvent::Selection(selection) = event else {
            panic!("expected selection");
        };
        assert_eq!(selection.text, "hello");
        assert_eq!(selection.source_app.bundle_id, "com.apple.TextEdit");
        assert_eq!(selection.bounds.expect("bounds").x, -120.0);
        assert_eq!(selection.trigger, SelectionTrigger::Drag);

        let serialized = serde_json::to_value(SelectionEvent::Selection(selection))
            .expect("serialize public event");
        assert_eq!(serialized["type"], "selection");
        assert_eq!(serialized["text"], "hello");
        assert!(serialized.get("payload").is_none());
    }

    #[test]
    fn parses_dismiss_wire_payload() {
        let event = parse_native_event(
            r#"{"type":"dismiss","reason":"scroll","mouse":{"x":10.0,"y":20.0},"targetPid":7,"timestampMs":99}"#,
        )
        .expect("valid dismiss event");
        let SelectionEvent::Dismiss(dismiss) = event else {
            panic!("expected dismiss");
        };
        assert_eq!(dismiss.reason, "scroll");
        assert_eq!(dismiss.target_pid, 7);
        assert_eq!(dismiss.sequence, 0);
    }

    #[test]
    fn parses_native_event_sequence() {
        let event = parse_native_event(
            r#"{"type":"dismiss","reason":"scroll","mouse":{"x":10.0,"y":20.0},"targetPid":7,"timestampMs":99,"sequence":4}"#,
        )
        .expect("valid dismiss event");
        let SelectionEvent::Dismiss(dismiss) = event else {
            panic!("expected dismiss");
        };
        assert_eq!(dismiss.sequence, 4);
    }

    #[test]
    fn parses_applescript_method_selection() {
        let event = parse_native_event(
            r#"{
                "type":"selection",
                "text":"selected in safari",
                "sourceApp":{"bundleId":"com.apple.Safari","name":"Safari"},
                "bounds":null,
                "startTop":null,
                "startBottom":null,
                "endTop":null,
                "endBottom":null,
                "mouse":{"start":null,"end":{"x":12.0,"y":34.0},"current":{"x":12.0,"y":34.0}},
                "direction":"forward",
                "isFullscreen":false,
                "method":"applescript",
                "trigger":"drag",
                "timestampMs":11
            }"#,
        )
        .expect("valid applescript selection event");

        let SelectionEvent::Selection(selection) = event else {
            panic!("expected selection");
        };
        assert_eq!(selection.method, SelectionMethod::AppleScript);
        let serialized = serde_json::to_value(SelectionEvent::Selection(selection))
            .expect("serialize public event");
        assert_eq!(serialized["method"], "applescript");
    }

    #[test]
    fn parses_clipboard_method_selection_without_bounds() {
        // Clipboard-fallback captures carry no geometry: the runtime falls
        // back to a cursor anchor for toolbar placement.
        let event = parse_native_event(
            r#"{
                "type":"selection",
                "text":"copied",
                "sourceApp":{"bundleId":"com.kingsoft.wpsoffice.mac","name":"WPS Office"},
                "bounds":null,
                "startTop":null,
                "startBottom":null,
                "endTop":null,
                "endBottom":null,
                "mouse":{"start":null,"end":{"x":30.0,"y":50.0},"current":{"x":30.0,"y":50.0}},
                "direction":"unknown",
                "isFullscreen":false,
                "method":"clipboard",
                "trigger":"drag",
                "timestampMs":7
            }"#,
        )
        .expect("valid clipboard selection event");

        let SelectionEvent::Selection(selection) = event else {
            panic!("expected selection");
        };
        assert_eq!(selection.method, SelectionMethod::Clipboard);
        assert_eq!(selection.bounds, None);
        assert_eq!(selection.source_app.bundle_id, "com.kingsoft.wpsoffice.mac");

        let serialized = serde_json::to_value(SelectionEvent::Selection(selection))
            .expect("serialize public event");
        assert_eq!(serialized["method"], "clipboard");
    }

    #[test]
    fn windows_direction_uses_line_order_then_horizontal_order() {
        let point = |x, y| Some(SelectionPoint { x, y });
        assert_eq!(
            direction_from_points(point(10.0, 20.0), point(40.0, 20.5)),
            SelectionDirection::Forward
        );
        assert_eq!(
            direction_from_points(point(40.0, 20.0), point(10.0, 20.0)),
            SelectionDirection::Backward
        );
        assert_eq!(
            direction_from_points(point(100.0, 20.0), point(5.0, 40.0)),
            SelectionDirection::Forward
        );
        assert_eq!(
            direction_from_points(None, point(5.0, 40.0)),
            SelectionDirection::Unknown
        );
        assert_eq!(
            direction_from_points(point(5.0, 40.0), point(5.2, 40.1)),
            SelectionDirection::Unknown
        );
    }

    #[test]
    fn windows_mouse_trigger_distinguishes_click_selection_modes() {
        let start = SelectionPoint { x: 10.0, y: 10.0 };
        let near = SelectionPoint { x: 11.0, y: 11.0 };
        let far = SelectionPoint { x: 20.0, y: 10.0 };
        assert_eq!(
            classify_windows_mouse_selection(start, near, true, false, true),
            Some(SelectionTrigger::ShiftClick)
        );
        assert_eq!(
            classify_windows_mouse_selection(start, near, false, true, true),
            Some(SelectionTrigger::DoubleClick)
        );
        assert_eq!(
            classify_windows_mouse_selection(start, far, true, true, true),
            Some(SelectionTrigger::Drag)
        );
        assert_eq!(
            classify_windows_mouse_selection(start, near, false, false, true),
            None
        );
        assert_eq!(
            classify_windows_mouse_selection(start, far, true, false, false),
            Some(SelectionTrigger::ShiftClick)
        );
    }

    #[test]
    fn windows_bounds_union_preserves_physical_negative_monitors_and_ignores_invalid_rects() {
        let bounds = union_selection_bounds(&[
            SelectionBounds {
                x: -1_900.0,
                y: 100.0,
                width: 80.0,
                height: 20.0,
            },
            SelectionBounds {
                x: -1_850.0,
                y: 120.0,
                width: 120.0,
                height: 25.0,
            },
            SelectionBounds {
                x: f64::NAN,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
        ])
        .expect("two valid rectangles");
        assert_eq!(
            bounds,
            SelectionBounds {
                x: -1_900.0,
                y: 100.0,
                width: 170.0,
                height: 45.0,
            }
        );
        assert!(union_selection_bounds(&[]).is_none());
        assert!(union_selection_bounds(&[SelectionBounds {
            x: 0.0,
            y: 0.0,
            width: 250_000.0,
            height: 20.0,
        }])
        .is_none());
    }

    #[test]
    fn windows_automatic_capture_rejects_pointer_far_from_unchanged_bounds() {
        let selection = windows_test_selection(
            "old selection",
            Some(SelectionBounds {
                x: 100.0,
                y: 100.0,
                width: 200.0,
                height: 40.0,
            }),
            SelectionPoint { x: 500.0, y: 120.0 },
        );
        assert!(!automatic_selection_pointer_matches_bounds(
            &selection, 24.0
        ));
        let nearby = SelectionPayload {
            mouse: SelectionMouse {
                current: SelectionPoint { x: 310.0, y: 120.0 },
                ..selection.mouse
            },
            ..selection
        };
        assert!(automatic_selection_pointer_matches_bounds(&nearby, 24.0));
    }

    #[test]
    fn windows_automatic_fingerprint_tracks_content_and_stable_range_not_pointer() {
        let original = windows_test_selection(
            "same text",
            Some(SelectionBounds {
                x: 10.0,
                y: 20.0,
                width: 80.0,
                height: 18.0,
            }),
            SelectionPoint { x: 90.0, y: 30.0 },
        );
        let moved_pointer = SelectionPayload {
            mouse: SelectionMouse {
                current: SelectionPoint { x: 500.0, y: 500.0 },
                ..original.mouse
            },
            ..original.clone()
        };
        assert_eq!(
            automatic_selection_fingerprint(&original),
            automatic_selection_fingerprint(&moved_pointer)
        );

        let changed_range = SelectionPayload {
            bounds: Some(SelectionBounds {
                x: 11.0,
                ..original.bounds.expect("bounds")
            }),
            ..original.clone()
        };
        assert_ne!(
            automatic_selection_fingerprint(&original),
            automatic_selection_fingerprint(&changed_range)
        );
        let changed_text = SelectionPayload {
            text: "different".to_owned(),
            ..original.clone()
        };
        assert_ne!(
            automatic_selection_fingerprint(&original),
            automatic_selection_fingerprint(&changed_text)
        );

        let clipboard_first =
            windows_test_selection("same text", None, SelectionPoint { x: 80.0, y: 80.0 });
        let clipboard_elsewhere = SelectionPayload {
            mouse: SelectionMouse {
                current: SelectionPoint { x: 240.0, y: 160.0 },
                ..clipboard_first.mouse
            },
            ..clipboard_first.clone()
        };
        assert_ne!(
            automatic_selection_fingerprint(&clipboard_first),
            automatic_selection_fingerprint(&clipboard_elsewhere)
        );
    }

    #[test]
    fn windows_uia_text_reads_are_bounded_before_allocation() {
        assert_eq!(windows_text_budget(0, false), Some((1_000_000, 1_000_001)));
        assert_eq!(windows_text_budget(999_999, false), Some((1, 2)));
        assert_eq!(windows_text_budget(999_999, true), Some((0, 1)));
        assert_eq!(windows_text_budget(1_000_000, true), None);
        assert_eq!(windows_text_budget(1_000_001, false), None);
    }

    fn windows_test_selection(
        text: &str,
        bounds: Option<SelectionBounds>,
        current: SelectionPoint,
    ) -> SelectionPayload {
        SelectionPayload {
            text: text.to_owned(),
            source_app: SourceApplication {
                bundle_id: "C:\\Windows\\notepad.exe".to_owned(),
                name: "notepad".to_owned(),
            },
            bounds,
            start_top: None,
            start_bottom: None,
            end_top: None,
            end_bottom: None,
            mouse: SelectionMouse {
                start: None,
                end: None,
                current,
            },
            direction: SelectionDirection::Unknown,
            is_fullscreen: false,
            method: SelectionMethod::Accessibility,
            trigger: SelectionTrigger::Drag,
            timestamp_ms: 1,
            sequence: 0,
        }
    }
}
