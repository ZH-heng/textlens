//! Windows selection capture using low-level input hooks and UI Automation.
//!
//! The hooks live on their own message-pumped thread. Their callbacks only
//! copy the small input record into an `mpsc` queue. Selection reads run in a
//! private, versioned helper process so a stuck third-party UI Automation/OLE
//! provider can be isolated and replaced without poisoning the hook/event
//! worker or later captures. All public Windows coordinates stay in physical
//! virtual-desktop pixels so mixed-DPI monitor regions remain unambiguous.

use super::{
    automatic_selection_fingerprint, automatic_selection_pointer_matches_bounds,
    classify_windows_mouse_selection, direction_from_points, selection_bounds_are_reasonable,
    union_selection_bounds, windows_click_pair_matches, windows_double_click_matches_text_range,
    windows_text_budget, DismissEvent, SelectionBounds, SelectionDirection, SelectionError,
    SelectionEvent, SelectionMethod, SelectionMouse, SelectionPayload, SelectionPoint,
    SelectionTrigger, SourceApplication, WindowsSelectionEvidence,
    WindowsSelectionObservationContext, WINDOWS_CLICK_DRIFT_DISTANCE_SQUARED,
};
use crate::{
    clipboard,
    models::{SelectionCaptureSettings, SelectionCaptureStrategy},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::c_void,
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{w, IUnknown, Interface, BSTR, GUID, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        },
        System::{
            Com::{
                CoCreateInstance, IDispatch, CLSCTX_INPROC_SERVER, DISPATCH_PROPERTYGET,
                DISPPARAMS, SAFEARRAY,
            },
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Ole::{
                OleInitialize, OleUninitialize, SafeArrayAccessData, SafeArrayDestroy,
                SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
                SafeArrayUnaccessData,
            },
            Threading::{
                GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
            Variant::{
                VariantClear, VARIANT, VT_ARRAY, VT_BOOL, VT_BSTR, VT_DISPATCH, VT_I4, VT_UNKNOWN,
                VT_VARIANT,
            },
        },
        UI::{
            Accessibility::{
                AccessibleObjectFromPoint, AccessibleObjectFromWindow, CUIAutomation, IAccessible,
                IUIAutomation, IUIAutomationElement, IUIAutomationLegacyIAccessiblePattern,
                IUIAutomationTextPattern, IUIAutomationTextRange, SetWinEventHook,
                TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
                UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_DocumentControlTypeId,
                UIA_EditControlTypeId, UIA_LegacyIAccessiblePatternId, UIA_ListItemControlTypeId,
                UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId,
                UIA_SelectionActiveEndAttributeId, UIA_TabItemControlTypeId, UIA_TextPatternId,
                UIA_TreeItemControlTypeId, UnhookWinEvent,
            },
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, GetDoubleClickTime, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD,
                KEYBDINPUT, KEYEVENTF_KEYUP, VK_A, VK_C, VK_CONTROL, VK_DOWN, VK_END, VK_HOME,
                VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR,
                VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_UP, VK_V, VK_X,
            },
            WindowsAndMessaging::{
                CallNextHookEx, DispatchMessageW, GetAncestor, GetClassNameW, GetCursorPos,
                GetForegroundWindow, GetGUIThreadInfo, GetParent, GetSystemMetrics,
                GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
                IsZoomed, MsgWaitForMultipleObjectsEx, PeekMessageW, PostThreadMessageW,
                SendMessageTimeoutW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
                WindowFromPoint, ES_PASSWORD, EVENT_SYSTEM_FOREGROUND, GA_ROOT, GUITHREADINFO,
                GWL_STYLE, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT,
                MWMO_INPUTAVAILABLE, OBJID_CLIENT, OBJID_NATIVEOM, OBJID_WINDOW, PM_REMOVE,
                QS_ALLINPUT, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SM_CXDOUBLECLK, SM_CYDOUBLECLK,
                WH_KEYBOARD_LL, WH_MOUSE_LL, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
                WM_GETTEXT, WM_GETTEXTLENGTH, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
                WM_MBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN,
                WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN,
            },
        },
    },
};

// The helper only pumps STA messages between captures; a blocking recv wakes
// immediately for new work, so a slower idle cadence avoids 80+ wakeups/s
// without adding capture latency.
const WORKER_PUMP_INTERVAL: Duration = Duration::from_millis(250);
// Hook callbacks are delivered by the OS; this timeout only re-checks the
// reinstall deadline and the stop flag, so 20 wakeups/s bought no extra
// responsiveness.
const HOOK_HEALTH_INTERVAL: Duration = Duration::from_millis(250);
// Windows can silently remove a low-level hook after a callback timeout while
// leaving its owner thread and message loop alive. Periodically replacing the
// two input hooks gives the worker a bounded self-recovery window; an install
// failure exits this thread so the existing supervisor can rebuild it.
const HOOK_REINSTALL_INTERVAL: Duration = Duration::from_secs(10);
const HOOK_RESTART_DELAY: Duration = Duration::from_secs(1);
const HOOK_EXIT_RECHECK_DELAY: Duration = Duration::from_millis(10);
const HOOK_SUPERVISION_INTERVAL: Duration = Duration::from_secs(5);
const MAX_MESSAGES_PER_TICK: usize = 64;
// Low-level hooks run before the target application consumes mouse-up/key-up.
// One short frame is enough for the common UIA path; the retries below cover
// providers which publish their selection asynchronously.
const CAPTURE_SETTLE_DELAY: Duration = Duration::from_millis(12);
// After an action opens a result window, Windows may deliver the source
// application's mouse-up before its foreground activation has completed. Do
// not start a capture against the still-foreground TextLens window; wait for
// the original source process to become foreground for a short, bounded time.
// Normal selections are already foreground and therefore do not pay this
// delay.
// Only wait while TextLens itself (or a transient desktop transition) owns
// the foreground. Once another external window is foreground, let the helper
// validate the preserved source context instead of waiting on a brittle HWND
// relationship in the hook worker.
const FOREGROUND_SETTLE_TIMEOUT: Duration = Duration::from_millis(260);
const FOREGROUND_SETTLE_RETRY: Duration = Duration::from_millis(6);
const ACCESSIBILITY_RETRY_DELAYS: [Duration; 2] = [Duration::ZERO, Duration::from_millis(28)];
// Hard ceiling on the whole UIA retry phase. Slow third-party UIA/CEF
// providers can burn per-attempt delays without timing out individual COM
// calls; attempt 0 always runs so the happy path never pays for the budget.
const UIA_PHASE_BUDGET: Duration = Duration::from_millis(180);
// After enough consecutive UIA misses, omit only the delayed UIA retry on the
// next attempt. The first UIA probe remains mandatory so a temporary provider
// failure cannot suppress a later valid selection. This route is
// non-destructive and never uses the clipboard.
const ADAPTIVE_ROUTE_MISS_THRESHOLD: u8 = 2;
const ADAPTIVE_ROUTE_REPROBE_INTERVAL: Duration = Duration::from_secs(120);
const MAX_APP_ROUTE_CACHE_ENTRIES: usize = 64;
// The point window, foreground frame, and point root cover the PDF viewers
// that expose a real MSAA selection. Do not walk every Office-oriented child
// candidate for a PDF canvas before returning to the bounded UIA path.
const PDF_ACCESSIBLE_WINDOW_PROBE_LIMIT: usize = 3;
// Point-based MSAA reaches custom-rendered text surfaces that do not publish
// a useful child HWND. Probe both ends of a drag, then keep the window walk
// bounded so a broken provider cannot turn one mouse-up into an unbounded COM
// sequence.
const ACCESSIBLE_POINT_PROBE_LIMIT: usize = 2;
const ACCESSIBLE_WINDOW_PROBE_LIMIT: usize = 8;
const MAX_ACCESSIBLE_POINT_ANCESTORS: usize = 4;
// Native Edit/RichEdit controls are a useful non-destructive fallback when an
// application has not published a UIA/MSAA selection. These messages are in
// the system-message range, so Windows marshals their buffers across process
// boundaries. Keep every request bounded so a frozen legacy control cannot
// hold the capture helper or delay a newer selection.
const NATIVE_TEXT_CONTROL_PROBE_LIMIT: usize = 6;
const NATIVE_TEXT_CONTROL_MESSAGE_TIMEOUT_MS: u32 = 24;
const NATIVE_TEXT_CONTROL_POINTER_TOLERANCE: i32 = 8;
const EM_GETSEL_MESSAGE: u32 = 0x00B0;
const EM_GETPASSWORDCHAR_MESSAGE: u32 = 0x00D2;
// `IAccessible` is useful for the subset of PDF readers that expose their
// selection directly, but it must remain a probe so several empty MSAA calls
// cannot delay the normal bounded UIA path on every gesture.
// Native document accessibility is the fastest successful path for many PDF
// readers and Office canvases, but a miss must not postpone the normal UIA
// route by a noticeable amount. PDF implementations are more likely to
// expose a direct accSelection, so give them the shortest bounded probe.
const PDF_ACCESSIBILITY_PROBE_BUDGET: Duration = Duration::from_millis(36);
const OFFICE_ACCESSIBILITY_PROBE_BUDGET: Duration = Duration::from_millis(56);
const MODIFIER_RELEASE_ATTEMPTS: usize = 12;
const MAX_DRAG_DURATION: Duration = Duration::from_secs(15);
// Longer / slower drags need the host to finish committing the selection
// *before* any capture attempt.
const CAPTURE_SETTLE_MEDIUM_DISTANCE_SQUARED: i64 = 2_500; // ~50 px
const CAPTURE_SETTLE_LONG_DISTANCE_SQUARED: i64 = 20_000; // ~141 px
                                                          // Multi-line / cross-paragraph threshold used only for retry budgeting. It is
                                                          // deliberately not a global UIA skip threshold: browsers, editors and Office
                                                          // applications can all produce perfectly good UIA results for tall ranges.
const LATE_RETRY_VERTICAL_DISTANCE: i64 = 64;
const LATE_RETRY_SLOW_PRESS_VERTICAL_DISTANCE: i64 = 32;
const SLOW_PRESS_MS: u32 = 350;
const CAPTURE_SETTLE_MEDIUM_DELAY: Duration = Duration::from_millis(24);
const CAPTURE_SETTLE_LONG_DELAY: Duration = Duration::from_millis(120);
const CAPTURE_SETTLE_SLOW_PRESS_DELAY: Duration = Duration::from_millis(48);
// Known PDF canvases copy from the committed range synchronously on most
// readers. Keep their first attempt close to one display frame; an empty
// result still uses the bounded retry schedule below.
const PDF_CAPTURE_SETTLE_DELAY: Duration = Duration::from_millis(16);
// PDF multi-paragraph selections often commit after the first attempt. Retry
// with increasing delay instead of giving up after one empty result.
const EMPTY_CAPTURE_RETRY_DELAYS: [Duration; 1] = [Duration::from_millis(72)];
// A direct-copy attempt has already waited for the renderer to publish text.
// Retry it promptly on a fresh helper instead of paying the accessibility
// provider delay intended for non-destructive selection-hook captures.
const CLIPBOARD_EMPTY_CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(24);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
// A blocked third-party accessibility provider is isolated in the helper
// process. Keep that isolation responsive: the coordinator cancels and
// replaces a slow helper before it can hold the latest selection hostage.
// UIA/MSAA remains bounded tightly; the larger task budget is only needed
// when a host has no provider and the clipboard transaction is attempted.
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CLIPBOARD_POLL_ATTEMPTS: usize = 120;
const CLIPBOARD_STABLE_POLLS: usize = 2;
const CLIPBOARD_PHASE_BUDGET: Duration = Duration::from_millis(1_200);
// Acrobat and DocBox do not reliably implement WM_COPY or UIA TextPattern.
// Their native selections are exposed through Ctrl+C with a shorter poll
// cadence. Acrobat may publish several clipboard formats before final text.
const PDF_CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(5);
// A swallowed synthetic chord does not advance the clipboard at all. Detect
// that condition in well under one frame of perceived latency and resend
// while the original selection and input-safety checks are still valid.
const PDF_CLIPBOARD_POLL_ATTEMPTS: usize = 14;
const ACROBAT_CLIPBOARD_POLL_ATTEMPTS: usize = 8;
const PDF_COPY_DISPATCH_ATTEMPTS: usize = 2;
const ACROBAT_COPY_DISPATCH_ATTEMPTS: usize = 3;
const PDF_CLIPBOARD_PHASE_BUDGET: Duration = Duration::from_millis(900);
const PDF_COPY_RETRY_DELAY: Duration = Duration::from_millis(4);
const ACROBAT_COPY_RETRY_DELAY: Duration = Duration::from_millis(6);
// A clipboard sequence change does not guarantee that a PDF renderer has
// published CF_UNICODETEXT yet. Probe the actual text for a short per-profile
// window instead of a fixed sleep so successful copies stay fast while late
// publishers remain compatible.
const GENERAL_CLIPBOARD_TEXT_READY_BUDGET: Duration = Duration::from_millis(60);
const PDF_CLIPBOARD_TEXT_READY_BUDGET: Duration = Duration::from_millis(140);
const ACROBAT_CLIPBOARD_TEXT_READY_BUDGET: Duration = Duration::from_millis(48);
// Do not restore the user's clipboard at the first readable format. Canvas
// readers often continue publishing delayed formats for one short frame after
// CF_UNICODETEXT appears; restoring in that gap causes the next drag to race
// a source-owned sequence update and can make every other selection vanish.
const GENERAL_CLIPBOARD_TEXT_SETTLE_DELAY: Duration = Duration::from_millis(12);
// PDF/canvas profiles return their first verified text immediately. The
// recovery ledger owns late source writes, so an additional fixed settle delay
// would only make every successful gesture feel slower.
// Canvas readers commonly publish the Unicode text and one or more delayed
// formats in adjacent clipboard updates. A short quiet window keeps that tail
// inside the current transaction instead of making the next drag repair it.
const PDF_CLIPBOARD_TEXT_SETTLE_DELAY: Duration = Duration::from_millis(12);
// Once Acrobat yields a bounded, valid UTF-16 value, keep one short quiet
// window for any adjacent sequence update. The captured value is cached while
// the sequence remains unchanged, so transient OLE GetData failures cannot
// restart this timer or add hundreds of milliseconds to a valid selection.
const ACROBAT_CLIPBOARD_TEXT_SETTLE_DELAY: Duration = Duration::from_millis(12);
// This is an ownership window, not an extra capture delay. A few document
// renderers publish one final clipboard update after TextLens has restored the
// user's original IDataObject. Retain that original object only long enough to
// repair a late source-owned update before the next gesture starts its copy.
const CLIPBOARD_RECOVERY_WINDOW: Duration = Duration::from_secs(2);
const CAPTURE_TASK_TIMEOUT: Duration = Duration::from_millis(2_200);
const CAPTURE_ENGINE_BUDGET: Duration = Duration::from_millis(2_000);
const MAX_UIA_SELECTION_RANGES: i32 = 256;
const UIA_SELECTION_TEXT_PROBE_LIMIT: i32 = 1_024;
const HELPER_START_TIMEOUT: Duration = Duration::from_secs(2);
// A helper that died while TextLens was idle must not block the first new
// selection for the full process-start timeout. The warm-up continues in the
// background and the bounded empty-capture retry picks it up if necessary.
const HELPER_CAPTURE_WARMUP_WAIT: Duration = Duration::from_millis(64);
const HELPER_CANCEL_GRACE: Duration = Duration::from_millis(24);
const HELPER_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CAPTURE_COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(12);
const CAPTURE_EXECUTOR_RETRY_INTERVAL: Duration = Duration::from_secs(1);
// Keep the isolated helper warm over long idle periods. This is a timed wait
// in the capture lane, not a polling loop, so it repairs a reclaimed child
// process without consuming CPU or delaying hook callbacks.
const HELPER_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
const HELPER_FRAME_LIMIT: usize = 16 * 1024 * 1024;
const SELECTION_HELPER_FLAG: &str = "--textlens-selection-helper";
const SELECTION_HELPER_PROTOCOL_VERSION: u32 = 3;
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);
const MAX_UIA_ANCESTORS: usize = 32;
const MAX_UIA_RUNTIME_ID_VALUES: usize = 128;
const MAX_ACCESSIBLE_SELECTION_ITEMS: usize = 64;
const MAX_PROCESS_ANCESTORS: usize = 32;
const MAX_BOUNDING_VALUES: usize = 262_144;
const MAX_TOTAL_BOUNDING_VALUES: usize = 262_144;
// DocumentRange is only a compatibility fallback for providers that hide a
// real selection. A whole PDF/document can otherwise look like a valid range;
// keep unusually large mouse-driven ranges on the explicit UIA/MSAA paths.
const DOCUMENT_RANGE_FALLBACK_MAX_CHARS: usize = 131_072;
const POINTER_BOUNDS_TOLERANCE: f64 = 24.0;
// Native accessibility providers occasionally surface a window/document
// title as their current "selection". A valid text range should terminate at
// the physical mouse-up location, allowing a generous tolerance for DPI and
// provider rounding without accepting a title bar from elsewhere in the UI.
const ACCESSIBILITY_SELECTION_POINTER_TOLERANCE: f64 = 56.0;
const AUTOMATIC_DUPLICATE_WINDOW: Duration = Duration::from_millis(1_500);

type UnitReply = SyncSender<Result<(), SelectionError>>;
type CaptureReply = SyncSender<Result<Option<SelectionPayload>, SelectionError>>;

pub(super) struct WindowsSelectionMonitor {
    inbox: Sender<WorkerMessage>,
    worker: Mutex<Option<JoinHandle<()>>>,
    shutdown_requested: AtomicBool,
}

impl WindowsSelectionMonitor {
    pub(super) fn new(
        _excluded_identifier: &str,
        event_sender: Arc<Mutex<Sender<SelectionEvent>>>,
    ) -> Result<Self, SelectionError> {
        let (inbox, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker_inbox = inbox.clone();
        let worker_event_sender = event_sender.clone();
        let worker = thread::Builder::new()
            .name("textlens-selection-uia".to_owned())
            .spawn(move || {
                selection_worker_main(receiver, worker_inbox, worker_event_sender, ready_sender);
            })
            .map_err(|_| SelectionError::NativeInitializationFailed)?;

        match ready_receiver.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                inbox,
                worker: Mutex::new(Some(worker)),
                shutdown_requested: AtomicBool::new(false),
            }),
            Ok(Err(error)) => {
                reap_worker(worker, WORKER_SHUTDOWN_GRACE);
                Err(error)
            }
            Err(_) => {
                let _ = inbox.send(WorkerMessage::Shutdown);
                reap_worker(worker, WORKER_SHUTDOWN_GRACE);
                Err(SelectionError::NativeInitializationFailed)
            }
        }
    }

    pub(super) fn start(&self) -> Result<(), SelectionError> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(SelectionError::Internal);
        }
        self.request_unit(WorkerMessage::Start)
    }

    pub(super) fn stop(&self) -> Result<(), SelectionError> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        self.request_unit(WorkerMessage::Stop)
    }

    pub(super) fn capture_current(&self) -> Result<Option<SelectionPayload>, SelectionError> {
        let reply_receiver = self.capture_current_async()?;
        reply_receiver
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| SelectionError::Internal)?
    }

    pub(super) fn capture_current_async(
        &self,
    ) -> Result<Receiver<Result<Option<SelectionPayload>, SelectionError>>, SelectionError> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(SelectionError::Internal);
        }
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self.inbox
            .send(WorkerMessage::Capture(reply_sender))
            .map_err(|_| SelectionError::Internal)?;
        Ok(reply_receiver)
    }

    pub(super) fn update_capture_settings(
        &self,
        settings: SelectionCaptureSettings,
    ) -> Result<(), SelectionError> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(SelectionError::Internal);
        }
        self.request_unit(|reply| WorkerMessage::UpdateCaptureSettings { settings, reply })
    }

    /// Requests shutdown and gives the worker a small grace period to finish.
    ///
    /// UI Automation and third-party accessibility providers can block inside
    /// COM calls. Never leave the Tauri UI thread waiting indefinitely for
    /// such a provider: a background reaper owns the join after the grace
    /// period expires.
    pub(super) fn shutdown(&self) {
        if !self.shutdown_requested.swap(true, Ordering::AcqRel) {
            let _ = self.inbox.send(WorkerMessage::Shutdown);
        }
        let worker = self.worker.lock().ok().and_then(|mut worker| worker.take());
        if let Some(worker) = worker {
            reap_worker(worker, WORKER_SHUTDOWN_GRACE);
        }
    }

    fn request_unit(
        &self,
        make_message: impl FnOnce(UnitReply) -> WorkerMessage,
    ) -> Result<(), SelectionError> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self.inbox
            .send(make_message(reply_sender))
            .map_err(|_| SelectionError::Internal)?;
        reply_receiver
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|_| SelectionError::Internal)?
    }
}

impl Drop for WindowsSelectionMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Joins a worker without allowing its caller to wait longer than `grace`.
/// The waiter thread is intentionally detached when the target does not stop
/// in time; process shutdown must not be held hostage by a stuck COM provider.
fn reap_worker(worker: JoinHandle<()>, grace: Duration) -> bool {
    if worker.is_finished() {
        let _ = worker.join();
        return true;
    }

    let (done_sender, done_receiver) = mpsc::sync_channel(1);
    let _reaper = thread::Builder::new()
        .name("textlens-selection-reaper".to_owned())
        .spawn(move || {
            let _ = worker.join();
            let _ = done_sender.send(());
        });
    done_receiver.recv_timeout(grace).is_ok()
}

enum WorkerMessage {
    Start(UnitReply),
    Stop(UnitReply),
    UpdateCaptureSettings {
        settings: SelectionCaptureSettings,
        reply: UnitReply,
    },
    Capture(CaptureReply),
    /// Wakes the worker as soon as the isolated capture lane publishes a
    /// completion. The completion payload remains on its own channel so raw
    /// input and capture results cannot overtake one another.
    CaptureCompletionWake,
    Raw(RawInput),
    HookExited {
        instance_id: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
enum RawInput {
    Mouse {
        sequence: u64,
        message: u32,
        point: RawPoint,
        event_time: u32,
        generation: u64,
        modifiers: ModifierSnapshot,
        timestamp_ms: u64,
    },
    Keyboard {
        sequence: u64,
        message: u32,
        virtual_key: u32,
        generation: u64,
        modifiers: ModifierSnapshot,
        timestamp_ms: u64,
    },
    Foreground {
        sequence: u64,
        window: isize,
        generation: u64,
        timestamp_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Default)]
struct ModifierSnapshot {
    shift: bool,
    control: bool,
    alt: bool,
    windows: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct RawPoint {
    x: i32,
    y: i32,
}

impl RawPoint {
    fn distance_squared(self, other: Self) -> f64 {
        let dx = f64::from(self.x) - f64::from(other.x);
        let dy = f64::from(self.y) - f64::from(other.y);
        dx * dx + dy * dy
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CaptureRequest {
    /// First-click snapshots never emit a selection or inject a copy chord.
    #[serde(default)]
    observation_only: bool,
    /// Exact first-click identity, independent of later focus/scroll epochs.
    #[serde(default)]
    observation_generation: Option<u64>,
    trigger: SelectionTrigger,
    start: Option<RawPoint>,
    end: Option<RawPoint>,
    current: RawPoint,
    generation: Option<u64>,
    /// Clipboard sequence sampled at mouse-down. It is not a hard capture
    /// gate: document renderers can publish delayed formats after a previous
    /// TextLens restore. Instead it lets the recovery ledger distinguish a
    /// source-owned late write during this gesture from a clipboard update
    /// that was already present before the user started dragging.
    #[serde(default)]
    clipboard_sequence_at_start: Option<u32>,
    /// Mouse-down hold time in ms. Used for settle delay on slow careful
    /// multi-paragraph selections. Default 0 keeps older helper frames
    /// deserializable.
    #[serde(default)]
    press_duration_ms: u32,
    /// Strategy resolved by the hook worker from persisted user settings.
    /// A request carries it into the isolated helper so a settings update can
    /// never race a capture already tied to a user gesture.
    #[serde(default)]
    capture_strategy: SelectionCaptureStrategy,
}

impl CaptureRequest {
    fn may_inject_copy(&self) -> bool {
        !self.observation_only && self.trigger != SelectionTrigger::DoubleClick
    }
}

struct CaptureJob {
    id: u64,
    request: CaptureRequest,
    source_root_window: isize,
    source_process_id: u32,
    process_parents: Option<Option<HashMap<u32, u32>>>,
    empty_attempt: u8,
    cancelled: Arc<AtomicBool>,
    user_keyboard: Arc<AtomicBool>,
    /// A newer capture explicitly superseded this job. This is separate from
    /// soft input invalidation so the latest selection can preempt a slow
    /// accessibility provider immediately.
    superseded: Arc<AtomicBool>,
    scheduled_at: Instant,
}

struct CaptureCompletion {
    id: u64,
    result: Result<Option<SelectionPayload>, SelectionError>,
    suppress_empty_retry: bool,
}

enum CaptureLaneCommand {
    Capture(CaptureJob),
    Shutdown,
}

struct ActiveCapture {
    id: u64,
    request: CaptureRequest,
    source_root_window: isize,
    source_process_id: u32,
    process_parents: Option<Option<HashMap<u32, u32>>>,
    empty_attempt: u8,
    reply: Option<CaptureReply>,
    cancelled: Arc<AtomicBool>,
    user_keyboard: Arc<AtomicBool>,
    superseded: Arc<AtomicBool>,
}

struct CompletedCapture {
    request: CaptureRequest,
    source_root_window: isize,
    source_process_id: u32,
    process_parents: Option<Option<HashMap<u32, u32>>>,
    empty_attempt: u8,
    reply: Option<CaptureReply>,
    result: Result<Option<SelectionPayload>, SelectionError>,
    suppress_empty_retry: bool,
    superseded: bool,
    cancelled: Arc<AtomicBool>,
}

struct CaptureCoordinator {
    own_process_id: u32,
    command_sender: Sender<CaptureLaneCommand>,
    completion_receiver: Receiver<CaptureCompletion>,
    completion_waker: Sender<WorkerMessage>,
    lane: Option<JoinHandle<()>>,
    next_id: u64,
    active: Option<ActiveCapture>,
    queued: Option<(CaptureJob, Option<CaptureReply>)>,
}

impl CaptureCoordinator {
    fn spawn(
        own_process_id: u32,
        completion_waker: Sender<WorkerMessage>,
    ) -> Result<Self, SelectionError> {
        let (command_sender, command_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        let lane_completion_waker = completion_waker.clone();
        let lane = thread::Builder::new()
            .name("textlens-selection-capture".to_owned())
            .spawn(move || {
                capture_lane_main(
                    command_receiver,
                    completion_sender,
                    lane_completion_waker,
                    own_process_id,
                );
            })
            .map_err(|_| SelectionError::NativeInitializationFailed)?;
        Ok(Self {
            own_process_id,
            command_sender,
            completion_receiver,
            completion_waker,
            lane: Some(lane),
            next_id: 1,
            active: None,
            queued: None,
        })
    }

    fn is_active(&self) -> bool {
        self.active.is_some()
    }

    fn active_is_manual(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.request.trigger == SelectionTrigger::Manual)
    }

    fn submit(
        &mut self,
        request: CaptureRequest,
        source_root_window: isize,
        source_process_id: u32,
        process_parents: Option<Option<HashMap<u32, u32>>>,
        empty_attempt: u8,
        scheduled_at: Instant,
        reply: Option<CaptureReply>,
    ) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let job = CaptureJob {
            id,
            request,
            source_root_window,
            source_process_id,
            process_parents,
            empty_attempt,
            cancelled: Arc::new(AtomicBool::new(false)),
            user_keyboard: Arc::new(AtomicBool::new(false)),
            superseded: Arc::new(AtomicBool::new(false)),
            scheduled_at,
        };

        if let Some((_, old_reply)) = self.queued.take() {
            if let Some(old_reply) = old_reply {
                let _ = old_reply.send(Ok(None));
            }
        }
        if let Some(active) = self.active.as_ref() {
            active.cancelled.store(true, Ordering::Release);
            active.superseded.store(true, Ordering::Release);
        }
        self.queued = Some((job, reply));
        self.start_queued();
    }

    fn cancel_active(&self) {
        if let Some(active) = self.active.as_ref() {
            active.cancelled.store(true, Ordering::Release);
        }
    }

    fn cancel_active_for_user_keyboard(&self) {
        if let Some(active) = self.active.as_ref() {
            active.user_keyboard.store(true, Ordering::Release);
            active.cancelled.store(true, Ordering::Release);
        }
    }

    fn start_queued(&mut self) {
        self.start_queued_with_recovery(true);
    }

    /// Dispatch the latest capture once. A dead helper lane can drop its
    /// receiver while the hook thread stays alive; retrying against a freshly
    /// created lane fixes that state without recursively spinning when process
    /// creation itself is temporarily unavailable.
    fn start_queued_with_recovery(&mut self, allow_recovery: bool) {
        if self.active.is_some() {
            return;
        }
        let Some((job, reply)) = self.queued.take() else {
            return;
        };
        let active = ActiveCapture {
            id: job.id,
            request: job.request,
            source_root_window: job.source_root_window,
            source_process_id: job.source_process_id,
            process_parents: job.process_parents.clone(),
            empty_attempt: job.empty_attempt,
            reply,
            cancelled: job.cancelled.clone(),
            user_keyboard: job.user_keyboard.clone(),
            superseded: job.superseded.clone(),
        };
        match self.command_sender.send(CaptureLaneCommand::Capture(job)) {
            Ok(()) => {
                self.active = Some(active);
            }
            Err(error) => {
                let CaptureLaneCommand::Capture(job) = error.0 else {
                    return;
                };
                if allow_recovery && self.restart_lane() {
                    self.queued = Some((job, active.reply));
                    self.start_queued_with_recovery(false);
                } else if let Some(reply) = active.reply {
                    let _ = reply.send(Err(SelectionError::Internal));
                }
            }
        }
    }

    fn try_complete(&mut self) -> Option<CompletedCapture> {
        let completion = match self.completion_receiver.try_recv() {
            Ok(completion) => completion,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                let active = self.active.take();
                let _ = self.restart_lane();
                let Some(active) = active else {
                    self.start_queued();
                    return None;
                };
                let completed = CompletedCapture {
                    request: active.request,
                    source_root_window: active.source_root_window,
                    source_process_id: active.source_process_id,
                    process_parents: active.process_parents,
                    empty_attempt: active.empty_attempt,
                    reply: active.reply,
                    result: Err(SelectionError::Internal),
                    suppress_empty_retry: false,
                    superseded: self.queued.is_some(),
                    cancelled: active.cancelled,
                };
                self.start_queued();
                return Some(completed);
            }
        };
        let Some(active) = self.active.take() else {
            return None;
        };
        if active.id != completion.id {
            self.active = Some(active);
            return None;
        }
        let superseded = self.queued.is_some();
        let completed = CompletedCapture {
            request: active.request,
            source_root_window: active.source_root_window,
            source_process_id: active.source_process_id,
            process_parents: active.process_parents,
            empty_attempt: active.empty_attempt,
            reply: active.reply,
            result: completion.result,
            suppress_empty_retry: completion.suppress_empty_retry,
            superseded,
            cancelled: active.cancelled,
        };
        self.start_queued();
        Some(completed)
    }

    fn restart_lane(&mut self) -> bool {
        if let Some(lane) = self.lane.take() {
            // A disconnected command/completion channel means the worker has
            // exited or is already unwinding. Never wait on that path from the
            // hook worker: drop a still-unwinding handle and join only a
            // completed one so recovery cannot freeze input processing.
            if lane.is_finished() {
                let _ = lane.join();
            }
        }
        let (command_sender, command_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        let own_process_id = self.own_process_id;
        let completion_waker = self.completion_waker.clone();
        let Ok(lane) = thread::Builder::new()
            .name("textlens-selection-capture".to_owned())
            .spawn(move || {
                capture_lane_main(
                    command_receiver,
                    completion_sender,
                    completion_waker,
                    own_process_id,
                );
            })
        else {
            return false;
        };
        self.command_sender = command_sender;
        self.completion_receiver = completion_receiver;
        self.lane = Some(lane);
        true
    }

    fn shutdown(&mut self) {
        self.cancel_active();
        if let Some((_, reply)) = self.queued.take() {
            if let Some(reply) = reply {
                let _ = reply.send(Ok(None));
            }
        }
        if let Some(lane) = self.lane.take() {
            let _ = self.command_sender.send(CaptureLaneCommand::Shutdown);
            let _ = reap_worker(lane, WORKER_SHUTDOWN_GRACE);
        }
        self.active = None;
    }
}

impl Drop for CaptureCoordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn capture_lane_main(
    receiver: Receiver<CaptureLaneCommand>,
    completion_sender: Sender<CaptureCompletion>,
    completion_waker: Sender<WorkerMessage>,
    own_process_id: u32,
) {
    let mut executor = CaptureExecutor::spawn(own_process_id).ok();
    let mut executor_retry_due = Instant::now() + CAPTURE_EXECUTOR_RETRY_INTERVAL;
    loop {
        let now = Instant::now();
        let next_deadline = executor
            .as_ref()
            .and_then(CaptureExecutor::maintenance_due)
            .or(Some(executor_retry_due));
        let message = receiver.recv_timeout(
            next_deadline
                .map(|deadline| deadline.saturating_duration_since(now))
                .unwrap_or(CAPTURE_COMPLETION_POLL_INTERVAL),
        );
        match message {
            Ok(CaptureLaneCommand::Capture(job)) => {
                let capture_started = Instant::now();
                let (result, suppress_empty_retry) = capture_with_timeout_on_lane(
                    &mut executor,
                    own_process_id,
                    job.request,
                    job.source_root_window,
                    job.source_process_id,
                    &job.cancelled,
                    &job.user_keyboard,
                    &job.superseded,
                );
                trace_selection_timing("capture", capture_started.elapsed());
                trace_selection_timing("mouse-up-to-capture-complete", job.scheduled_at.elapsed());
                let _ = completion_sender.send(CaptureCompletion {
                    id: job.id,
                    result,
                    suppress_empty_retry,
                });
                let _ = completion_waker.send(WorkerMessage::CaptureCompletionWake);
            }
            Ok(CaptureLaneCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if executor.is_none() && Instant::now() >= executor_retry_due {
            // Helper startup can be denied transiently by WebView/antivirus
            // process policy. Retry on a timer from this lane instead of
            // silently disabling every later selection or spinning.
            executor = CaptureExecutor::spawn(own_process_id).ok();
            executor_retry_due = Instant::now() + CAPTURE_EXECUTOR_RETRY_INTERVAL;
        }
        if let Some(executor) = executor.as_mut() {
            executor.maintain_pending_helper();
        }
    }
    if let Some(executor) = executor {
        executor.shutdown(WORKER_SHUTDOWN_GRACE);
    }
}

fn capture_with_timeout_on_lane(
    executor: &mut Option<CaptureExecutor>,
    own_process_id: u32,
    request: CaptureRequest,
    source_root_window: isize,
    source_process_id: u32,
    cancelled: &AtomicBool,
    user_keyboard: &AtomicBool,
    superseded: &AtomicBool,
) -> (Result<Option<SelectionPayload>, SelectionError>, bool) {
    if executor.is_none() {
        *executor = CaptureExecutor::spawn(own_process_id).ok();
    }
    let Some(capture_executor) = executor.as_mut() else {
        // Startup can race a transient process/antivirus lock after a long
        // idle period. Treat it like a recoverable empty capture so the
        // bounded retry gets another chance rather than disabling selection.
        return (Ok(None), false);
    };
    match capture_executor.capture(
        request,
        source_root_window,
        source_process_id,
        cancelled,
        user_keyboard,
        superseded,
    ) {
        Ok(result) => {
            if result.is_err() {
                if let Some(capture_executor) = executor.take() {
                    capture_executor.shutdown(Duration::ZERO);
                }
            } else if request.may_inject_copy()
                && request.capture_strategy == SelectionCaptureStrategy::Clipboard
                && matches!(&result, Ok(None))
                && !cancelled.load(Ordering::Acquire)
                && !user_keyboard.load(Ordering::Acquire)
                && !superseded.load(Ordering::Acquire)
                && request
                    .generation
                    .is_none_or(|expected| hook_generation().load(Ordering::Acquire) == expected)
            {
                // Acrobat can leave its OLE clipboard proxy unusable after a
                // few transactions while the helper process itself remains
                // healthy. Replace the STA/OLE context before retrying.
                capture_executor.recycle_helper();
            }
            (result, false)
        }
        // A helper can be reclaimed by Windows while the app is idle. Its
        // executor has already started an asynchronous replacement, so expose
        // this as an empty capture and let the existing bounded retry recover
        // the user's current gesture instead of requiring a second drag.
        Err(CaptureExecutorError::Disconnected) => (Ok(None), false),
        Err(
            CaptureExecutorError::TimedOut
            | CaptureExecutorError::Cancelled
            | CaptureExecutorError::CancelledCooperative,
        ) => (Ok(None), false),
    }
}

struct PendingCapture {
    due: Instant,
    expires_at: Instant,
    scheduled_at: Instant,
    request: CaptureRequest,
    source_root_window: isize,
    source_process_id: u32,
    process_parents: Option<Option<HashMap<u32, u32>>>,
    /// 0 = first attempt; 1.. = late empty retries after the host had more time.
    empty_attempt: u8,
}

#[derive(Debug, Clone, Copy)]
struct RecentCaptureContext {
    generation: u64,
    source_root_window: isize,
    source_process_id: u32,
    valid_until: Instant,
}

const RECENT_CAPTURE_FOREGROUND_GRACE: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundInteractionDecision {
    Ignore,
    PreserveMouseGesture,
    PreservePendingCapture,
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingForegroundDecision {
    Capture,
    Wait,
    Expire,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum HelperCancelReason {
    InputChanged,
    Superseded,
    UserKeyboard,
    Timeout,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
enum HelperCommand {
    Capture {
        request_id: u64,
        request: CaptureRequest,
        source_window: isize,
        source_process_id: u32,
        /// The long-lived TextLens host PID. The helper has a different PID,
        /// so it must not use its own process identity to reject TextLens UIA
        /// providers.
        textlens_process_id: u32,
    },
    Cancel {
        request_id: u64,
        reason: HelperCancelReason,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
enum HelperCaptureResult {
    Selection {
        selection: SelectionPayload,
        source_window: isize,
        process_id: u32,
        provider_window: isize,
        provider_process_id: u32,
    },
    Empty,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum HelperPhase {
    ClipboardInjected { process_id: u32 },
    ClipboardSequenceChanged { process_id: u32 },
    ClipboardTextReady { process_id: u32 },
    ClipboardRestored { process_id: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
enum HelperEvent {
    Ready {
        protocol_version: u32,
    },
    Phase {
        request_id: u64,
        phase: HelperPhase,
    },
    Result {
        request_id: u64,
        result: HelperCaptureResult,
    },
}

fn selection_worker_main(
    receiver: Receiver<WorkerMessage>,
    inbox: Sender<WorkerMessage>,
    event_sender: Arc<Mutex<Sender<SelectionEvent>>>,
    ready_sender: SyncSender<Result<(), SelectionError>>,
) {
    let _ = ready_sender.send(Ok(()));

    let completion_waker = inbox.clone();
    let mut worker = SelectionWorker {
        inbox,
        event_sender,
        own_process_id: unsafe { GetCurrentProcessId() },
        capture_settings: SelectionCaptureSettings::default(),
        // Keep all UIA/OLE work off the hook/event state machine. A timed-out
        // helper lane can be discarded while the hook worker continues to
        // process the next gesture.
        capture_coordinator: CaptureCoordinator::spawn(
            unsafe { GetCurrentProcessId() },
            completion_waker,
        )
        .ok(),
        hook_thread: None,
        desired_listening: false,
        hook_restart_due: None,
        hook_exit_check_due: None,
        hook_health_due: None,
        mouse_down: None,
        mouse_down_at: None,
        mouse_down_on_self: false,
        mouse_down_target_root: 0,
        mouse_down_target_window: 0,
        mouse_down_shift: false,
        mouse_down_unmodified: false,
        mouse_down_clipboard_sequence: None,
        last_mouse_up: None,
        last_click: None,
        keyboard_selection_key: None,
        pending_capture: None,
        last_automatic_fingerprint: None,
        recent_capture: None,
        last_raw_sequence: 0,
    };
    worker.run(receiver);
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, SelectionError> {
        // UI Automation requires a dedicated STA. OleInitialize establishes
        // that apartment for the isolated accessibility worker.
        unsafe { OleInitialize(None) }.map_err(|_| SelectionError::NativeInitializationFailed)?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

struct SelectionWorker {
    inbox: Sender<WorkerMessage>,
    event_sender: Arc<Mutex<Sender<SelectionEvent>>>,
    own_process_id: u32,
    capture_settings: SelectionCaptureSettings,
    capture_coordinator: Option<CaptureCoordinator>,
    hook_thread: Option<HookThread>,
    desired_listening: bool,
    hook_restart_due: Option<Instant>,
    hook_exit_check_due: Option<(u64, Instant)>,
    hook_health_due: Option<Instant>,
    mouse_down: Option<RawPoint>,
    mouse_down_at: Option<u32>,
    mouse_down_on_self: bool,
    mouse_down_target_root: isize,
    mouse_down_target_window: isize,
    mouse_down_shift: bool,
    mouse_down_unmodified: bool,
    mouse_down_clipboard_sequence: Option<u32>,
    last_mouse_up: Option<RawPoint>,
    last_click: Option<(u32, RawPoint, isize, u64)>,
    keyboard_selection_key: Option<u32>,
    pending_capture: Option<PendingCapture>,
    last_automatic_fingerprint: Option<(u64, u64, Instant)>,
    recent_capture: Option<RecentCaptureContext>,
    last_raw_sequence: u64,
}

impl SelectionWorker {
    fn run(&mut self, receiver: Receiver<WorkerMessage>) {
        let mut shutdown = false;
        while !shutdown {
            let now = Instant::now();
            let next_deadline = self
                .pending_capture
                .as_ref()
                .map(|pending| pending.due)
                .into_iter()
                .chain(self.hook_restart_due)
                .chain(self.hook_exit_check_due.map(|(_, due)| due))
                .chain(self.hook_health_due)
                .min();
            if let Some(deadline) = next_deadline {
                match receiver.recv_timeout(deadline.saturating_duration_since(now)) {
                    Ok(message) => shutdown = self.handle_message(message),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => shutdown = true,
                }
            } else {
                match receiver.recv() {
                    Ok(message) => shutdown = self.handle_message(message),
                    Err(_) => shutdown = true,
                }
            }
            let mut drained = 0usize;
            while !shutdown && drained < MAX_MESSAGES_PER_TICK {
                match receiver.try_recv() {
                    Ok(message) => {
                        drained += 1;
                        shutdown = self.handle_message(message);
                    }
                    Err(_) => break,
                }
            }
            self.restart_hooks_if_due();
            self.reap_exited_hook_if_due();
            self.check_hook_health_if_due();
            self.poll_capture_completions();
            self.run_pending_capture();
        }
        self.pending_capture = None;
        if let Some(mut hooks) = self.hook_thread.take() {
            let _ = hooks.stop();
        }
        if let Some(mut coordinator) = self.capture_coordinator.take() {
            coordinator.shutdown();
        }
    }

    /// Returns true when the worker should terminate.
    fn handle_message(&mut self, message: WorkerMessage) -> bool {
        match message {
            WorkerMessage::Start(reply) => {
                self.desired_listening = true;
                let result = self.ensure_hooks_running();
                let _ = reply.send(result);
            }
            WorkerMessage::Stop(reply) => {
                self.desired_listening = false;
                self.hook_restart_due = None;
                self.hook_exit_check_due = None;
                self.hook_health_due = None;
                self.reset_interaction_state();
                let result = self
                    .hook_thread
                    .take()
                    .map_or(Ok(()), |mut thread| thread.stop());
                let _ = reply.send(result);
            }
            WorkerMessage::UpdateCaptureSettings { settings, reply } => {
                self.capture_settings = settings;
                let _ = reply.send(Ok(()));
            }
            WorkerMessage::Capture(reply) => {
                let current = current_cursor_position();
                let foreground = unsafe { GetForegroundWindow() };
                let source_process_id = window_process_id(foreground);
                self.submit_capture(
                    CaptureRequest {
                        observation_generation: None,
                        observation_only: false,
                        trigger: SelectionTrigger::Manual,
                        start: None,
                        end: None,
                        current,
                        generation: None,
                        clipboard_sequence_at_start: None,
                        press_duration_ms: 0,
                        capture_strategy: self.capture_strategy_for(source_process_id),
                    },
                    root_window(foreground).0 as isize,
                    source_process_id,
                    None,
                    0,
                    Instant::now(),
                    Some(reply),
                );
            }
            WorkerMessage::CaptureCompletionWake => {}
            WorkerMessage::Raw(input) => {
                if self.hook_thread.is_some() {
                    self.handle_raw_input(input);
                }
            }
            WorkerMessage::HookExited { instance_id } => {
                self.note_hook_exit(instance_id);
            }
            WorkerMessage::Shutdown => return true,
        }
        false
    }

    fn ensure_hooks_running(&mut self) -> Result<(), SelectionError> {
        if self
            .hook_thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            self.hook_restart_due = None;
            self.hook_health_due = Some(Instant::now() + HOOK_SUPERVISION_INTERVAL);
            return Ok(());
        }
        if let Some(mut stale) = self.hook_thread.take() {
            if stale.is_finished() {
                if let Some(join) = stale.join.take() {
                    let _ = join.join();
                }
                let _ = clear_hook_inbox(stale.instance_id);
            } else {
                let _ = stale.stop();
            }
        }
        match HookThread::start(self.inbox.clone()) {
            Ok(thread) => {
                self.hook_thread = Some(thread);
                self.hook_restart_due = None;
                self.hook_health_due = Some(Instant::now() + HOOK_SUPERVISION_INTERVAL);
                Ok(())
            }
            Err(error) => {
                if self.desired_listening {
                    self.hook_restart_due = Some(Instant::now() + HOOK_RESTART_DELAY);
                }
                Err(error)
            }
        }
    }

    fn restart_hooks_if_due(&mut self) {
        if !self.desired_listening
            || !self
                .hook_restart_due
                .is_some_and(|due| Instant::now() >= due)
        {
            return;
        }
        if let Err(error) = self.ensure_hooks_running() {
            eprintln!("[selection] failed to restart Windows input hooks: {error}");
        }
    }

    fn note_hook_exit(&mut self, instance_id: u64) {
        if !self
            .hook_thread
            .as_ref()
            .is_some_and(|thread| thread.instance_id == instance_id)
        {
            return;
        }
        if !self
            .hook_thread
            .as_ref()
            .is_some_and(HookThread::is_finished)
        {
            self.hook_exit_check_due =
                Some((instance_id, Instant::now() + HOOK_EXIT_RECHECK_DELAY));
            return;
        }
        if let Some(mut thread) = self.hook_thread.take() {
            if let Some(join) = thread.join.take() {
                let _ = reap_worker(join, WORKER_SHUTDOWN_GRACE);
            }
        }
        self.hook_exit_check_due = None;
        self.reset_interaction_state();
        if self.desired_listening {
            self.hook_restart_due = Some(Instant::now() + HOOK_RESTART_DELAY);
        }
        self.hook_health_due = None;
    }

    fn reap_exited_hook_if_due(&mut self) {
        let Some((instance_id, due)) = self.hook_exit_check_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.hook_exit_check_due = None;
        self.note_hook_exit(instance_id);
    }

    fn check_hook_health_if_due(&mut self) {
        if !self.desired_listening {
            self.hook_health_due = None;
            return;
        }
        if !self
            .hook_health_due
            .is_some_and(|due| Instant::now() >= due)
        {
            return;
        }
        self.hook_health_due = Some(Instant::now() + HOOK_SUPERVISION_INTERVAL);
        match self.hook_thread.as_ref() {
            Some(thread) if thread.is_finished() => {
                let instance_id = thread.instance_id;
                self.note_hook_exit(instance_id);
            }
            Some(_) => {}
            None => {
                self.hook_restart_due = Some(Instant::now());
            }
        }
    }

    fn handle_raw_input(&mut self, input: RawInput) {
        let sequence = match input {
            RawInput::Mouse { sequence, .. }
            | RawInput::Keyboard { sequence, .. }
            | RawInput::Foreground { sequence, .. } => sequence,
        };
        if sequence <= self.last_raw_sequence {
            return;
        }
        self.last_raw_sequence = sequence;
        match input {
            RawInput::Mouse {
                sequence: _,
                message,
                point,
                event_time,
                generation,
                modifiers,
                timestamp_ms,
            } => self.handle_mouse(
                message,
                point,
                event_time,
                generation,
                modifiers,
                timestamp_ms,
            ),
            RawInput::Keyboard {
                sequence: _,
                message,
                virtual_key,
                generation,
                modifiers,
                timestamp_ms,
            } => self.handle_keyboard(message, virtual_key, generation, modifiers, timestamp_ms),
            RawInput::Foreground {
                sequence: _,
                window,
                generation,
                timestamp_ms,
            } => self.handle_foreground(window, generation, timestamp_ms),
        }
    }

    fn handle_foreground(&mut self, window: isize, generation: u64, timestamp_ms: u64) {
        let window = HWND(window as *mut c_void);
        if unsafe { GetForegroundWindow() } != window {
            // WINEVENT_OUTOFCONTEXT may deliver an older activation after a
            // newer foreground transition has already completed.
            return;
        }
        let foreground_process_id = window_process_id(window);
        let foreground_root = root_window(window).0 as isize;
        if self.recent_capture_matches_foreground(
            foreground_root,
            foreground_process_id,
            generation,
        ) {
            // Out-of-context WinEvent callbacks can arrive after the helper
            // has emitted the selection. A same-source callback belongs to
            // that completed gesture and must not dismiss its toolbar.
            return;
        }
        match foreground_interaction_decision(
            !window.is_invalid(),
            foreground_process_id,
            self.own_process_id,
            self.mouse_down.is_some(),
            self.mouse_down_on_self,
            self.pending_capture.is_some(),
        ) {
            ForegroundInteractionDecision::Ignore => return,
            ForegroundInteractionDecision::PreserveMouseGesture => {
                self.keyboard_selection_key = None;
                self.last_click = None;
                self.last_automatic_fingerprint = None;
                return;
            }
            ForegroundInteractionDecision::PreservePendingCapture => {
                correlate_pending_capture_with_foreground(
                    &mut self.pending_capture,
                    foreground_root,
                    foreground_process_id,
                    generation,
                );
                self.keyboard_selection_key = None;
                self.last_click = None;
                self.last_automatic_fingerprint = None;
                return;
            }
            ForegroundInteractionDecision::Dismiss => {}
        }
        self.reset_interaction_state_preserving_manual();
        self.emit_dismiss(
            "foregroundChanged",
            current_cursor_position(),
            window,
            timestamp_ms,
        );
    }

    fn reset_interaction_state(&mut self) {
        self.reset_interaction_state_with_manual(false);
    }

    fn reset_interaction_state_preserving_manual(&mut self) {
        self.reset_interaction_state_with_manual(true);
    }

    fn reset_interaction_state_with_manual(&mut self, preserve_manual: bool) {
        // A shortcut callback and its low-level key events are delivered on
        // separate queues. Do not cancel the Manual request because its own
        // key-up/foreground transition is not a newer selection. Lifecycle
        // shutdown/stop still uses the default path and cancels every request.
        if !preserve_manual || !self.manual_capture_active() {
            self.cancel_active_capture();
        }
        self.pending_capture = None;
        self.mouse_down = None;
        self.mouse_down_at = None;
        self.mouse_down_on_self = false;
        self.mouse_down_target_root = 0;
        self.mouse_down_target_window = 0;
        self.mouse_down_shift = false;
        self.mouse_down_unmodified = false;
        self.mouse_down_clipboard_sequence = None;
        self.last_mouse_up = None;
        self.last_click = None;
        self.keyboard_selection_key = None;
        self.last_automatic_fingerprint = None;
        self.recent_capture = None;
    }

    fn cancel_active_capture(&self) {
        if let Some(coordinator) = self.capture_coordinator.as_ref() {
            coordinator.cancel_active();
        }
    }

    fn cancel_active_capture_for_user_keyboard(&self) {
        if let Some(coordinator) = self.capture_coordinator.as_ref() {
            coordinator.cancel_active_for_user_keyboard();
        }
    }

    fn manual_capture_active(&self) -> bool {
        self.capture_coordinator
            .as_ref()
            .is_some_and(CaptureCoordinator::active_is_manual)
    }

    fn recent_capture_matches_foreground(
        &mut self,
        foreground_root: isize,
        foreground_process_id: u32,
        generation: u64,
    ) -> bool {
        let Some(recent) = self.recent_capture else {
            return false;
        };
        if Instant::now() >= recent.valid_until {
            self.recent_capture = None;
            return false;
        }
        let same_source = capture_windows_are_related(
            recent.source_root_window,
            recent.source_process_id,
            foreground_root,
            foreground_process_id,
        );
        if same_source && generation <= recent.generation {
            return true;
        }
        if generation > recent.generation {
            self.recent_capture = None;
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_mouse(
        &mut self,
        message: u32,
        point: RawPoint,
        event_time: u32,
        generation: u64,
        modifiers: ModifierSnapshot,
        timestamp_ms: u64,
    ) {
        // Wheel events after mouse-up are common on multi-paragraph PDF
        // selections (auto-scroll bounce / trackpad inertia). They must dismiss
        // a visible toolbar but must NOT cancel a just-scheduled capture —
        // that was a primary cause of "long cross-paragraph never pops".
        if mouse_message_clears_pending_capture(message) {
            self.cancel_active_capture();
            self.pending_capture = None;
            self.recent_capture = None;
        }
        let target_window = window_at_point(point);
        let target_root = root_window(target_window);
        let target_is_self = window_process_id(target_root) == self.own_process_id;
        match message {
            WM_LBUTTONDOWN => {
                self.keyboard_selection_key = None;
                if target_is_self {
                    self.last_click = None;
                    self.last_mouse_up = None;
                }
                self.mouse_down = Some(point);
                self.mouse_down_at = Some(event_time);
                self.mouse_down_on_self = target_is_self;
                self.mouse_down_target_root = target_root.0 as isize;
                self.mouse_down_target_window = target_window.0 as isize;
                self.mouse_down_shift =
                    modifiers.shift && !modifiers.control && !modifiers.alt && !modifiers.windows;
                self.mouse_down_unmodified =
                    !modifiers.shift && !modifiers.control && !modifiers.alt && !modifiers.windows;
                self.mouse_down_clipboard_sequence = Some(clipboard::windows_clipboard_sequence());
                // Self clicks still produce Dismiss so runtime can preserve a
                // click inside the no-activate toolbar while closing it for a
                // click elsewhere in a TextLens window. Mouse-up below never
                // schedules UIA capture for the self process.
                self.emit_dismiss("mouseDown", point, target_window, timestamp_ms);
            }
            WM_LBUTTONUP => {
                let start = self.mouse_down.take().unwrap_or(point);
                let pressed_at = self.mouse_down_at.take();
                let press_duration = pressed_at.map(|at| event_time.wrapping_sub(at));
                // A missing mouse-down (hook started mid-gesture) must not
                // demote a real drag to a click; movement still gates the
                // trigger in classify_windows_mouse_selection.
                let drag_duration_valid = press_duration
                    .is_none_or(|duration| u128::from(duration) <= MAX_DRAG_DURATION.as_millis());
                let double_click_interval = unsafe { GetDoubleClickTime() };
                let click_duration_valid =
                    press_duration.is_some_and(|duration| duration <= double_click_interval);
                let began_on_self = std::mem::take(&mut self.mouse_down_on_self);
                let gesture_root = std::mem::take(&mut self.mouse_down_target_root);
                let gesture_window = std::mem::take(&mut self.mouse_down_target_window);
                let unmodified_start = std::mem::take(&mut self.mouse_down_unmodified);
                let clipboard_sequence_at_start = self.mouse_down_clipboard_sequence.take();
                let previous_mouse_up = self.last_mouse_up;
                if began_on_self || target_is_self {
                    self.last_click = None;
                    return;
                }
                let unmodified = unmodified_start
                    && !modifiers.shift
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.windows;
                let previous_observation_generation = self.last_click.map(|click| click.3);
                let is_double_click = unmodified
                    && click_duration_valid
                    && gesture_window != 0
                    && gesture_window == target_window.0 as isize
                    && self.last_click.zip(pressed_at).is_some_and(
                        |((at, previous, window, _), current_time)| {
                            windows_click_pair_matches(
                                (at, raw_selection_point(previous), window),
                                (current_time, raw_selection_point(start), gesture_window),
                                double_click_interval,
                                unsafe {
                                    (
                                        GetSystemMetrics(SM_CXDOUBLECLK),
                                        GetSystemMetrics(SM_CYDOUBLECLK),
                                    )
                                },
                            )
                        },
                    );
                let start_point = raw_selection_point(start);
                let end_point = raw_selection_point(point);
                let trigger = classify_windows_mouse_selection(
                    start_point,
                    end_point,
                    self.mouse_down_shift,
                    is_double_click,
                    drag_duration_valid,
                );
                self.last_mouse_up = Some(point);
                if unmodified
                    && click_duration_valid
                    && start.distance_squared(point) <= WINDOWS_CLICK_DRIFT_DISTANCE_SQUARED
                    && gesture_window != 0
                    && gesture_window == target_window.0 as isize
                    && !is_double_click
                {
                    self.last_click = pressed_at.map(|at| (at, start, gesture_window, generation));
                } else {
                    self.last_click = None;
                }
                if let Some(trigger) = trigger {
                    let capture_start = match trigger {
                        SelectionTrigger::DoubleClick => point,
                        SelectionTrigger::ShiftClick => previous_mouse_up.unwrap_or(start),
                        _ => start,
                    };
                    let press_duration_ms = press_duration.unwrap_or(0);
                    self.schedule_capture(
                        CaptureRequest {
                            trigger,
                            start: Some(capture_start),
                            end: Some(point),
                            current: point,
                            generation: Some(generation),
                            clipboard_sequence_at_start,
                            press_duration_ms,
                            capture_strategy: SelectionCaptureStrategy::SelectionHook,
                            observation_only: false,
                            observation_generation: if is_double_click {
                                previous_observation_generation
                            } else {
                                None
                            },
                        },
                        if gesture_root == 0 {
                            target_root.0 as isize
                        } else {
                            gesture_root
                        },
                    );
                } else if unmodified
                    && click_duration_valid
                    && gesture_window != 0
                    && gesture_window == target_window.0 as isize
                {
                    self.schedule_selection_observation(
                        point,
                        generation,
                        if gesture_root == 0 {
                            target_root.0 as isize
                        } else {
                            gesture_root
                        },
                    );
                }
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                self.last_click = None;
                self.emit_dismiss("scroll", point, target_window, timestamp_ms);
            }
            WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                self.last_click = None;
                self.emit_dismiss("mouseDown", point, target_window, timestamp_ms);
            }
            _ => {}
        }
    }

    fn handle_keyboard(
        &mut self,
        message: u32,
        virtual_key: u32,
        generation: u64,
        modifiers: ModifierSnapshot,
        timestamp_ms: u64,
    ) {
        // The configured global shortcut also reaches the low-level hook. Once
        // its Manual capture has been queued, consuming the remaining key
        // events prevents the shortcut from dismissing or cancelling itself.
        // A new mouse gesture remains an explicit superseding input.
        if self.manual_capture_active() && !is_modifier_virtual_key(virtual_key as u16) {
            return;
        }
        if !is_modifier_virtual_key(virtual_key as u16) {
            // Synthetic TextLens keys are filtered in the hook callback. Any
            // remaining non-modifier key is genuine newer input and must
            // invalidate a capture which has not started yet.
            self.cancel_active_capture_for_user_keyboard();
            self.pending_capture = None;
            self.recent_capture = None;
            self.last_click = None;
        }
        let foreground = unsafe { GetForegroundWindow() };
        if window_process_id(foreground) == self.own_process_id {
            if (message == WM_KEYUP || message == WM_SYSKEYUP)
                && self.keyboard_selection_key == Some(virtual_key)
            {
                self.keyboard_selection_key = None;
            }
            return;
        }
        if message == WM_KEYUP || message == WM_SYSKEYUP {
            if self.keyboard_selection_key == Some(virtual_key) {
                self.keyboard_selection_key = None;
                let point = current_cursor_position();
                self.schedule_capture(
                    CaptureRequest {
                        observation_generation: None,
                        observation_only: false,
                        trigger: SelectionTrigger::Keyboard,
                        start: None,
                        end: None,
                        current: point,
                        generation: Some(generation),
                        clipboard_sequence_at_start: None,
                        press_duration_ms: 0,
                        capture_strategy: SelectionCaptureStrategy::SelectionHook,
                    },
                    root_window(foreground).0 as isize,
                );
            }
            return;
        }
        if message != WM_KEYDOWN && message != WM_SYSKEYDOWN {
            return;
        }
        if is_modifier_virtual_key(virtual_key as u16) {
            return;
        }

        let point = current_cursor_position();
        self.emit_dismiss("keyDown", point, foreground, timestamp_ms);
        let navigation = matches!(
            virtual_key as u16,
            key if key == VK_LEFT.0
                || key == VK_RIGHT.0
                || key == VK_UP.0
                || key == VK_DOWN.0
                || key == VK_HOME.0
                || key == VK_END.0
                || key == VK_PRIOR.0
                || key == VK_NEXT.0
        );
        let select_all = modifiers.control
            && !modifiers.alt
            && !modifiers.windows
            && virtual_key as u16 == VK_A.0;
        if (modifiers.shift && navigation) || select_all {
            self.keyboard_selection_key = Some(virtual_key);
        } else {
            self.keyboard_selection_key = None;
        }
    }

    fn schedule_capture(&mut self, request: CaptureRequest, source_root_window: isize) {
        self.schedule_capture_with_options(request, source_root_window, 0, None);
    }

    fn schedule_selection_observation(
        &mut self,
        point: RawPoint,
        generation: u64,
        source_root_window: isize,
    ) {
        self.schedule_capture(
            CaptureRequest {
                observation_only: true,
                observation_generation: Some(generation),
                trigger: SelectionTrigger::DoubleClick,
                start: Some(point),
                end: Some(point),
                current: point,
                generation: Some(generation),
                clipboard_sequence_at_start: None,
                press_duration_ms: 0,
                capture_strategy: SelectionCaptureStrategy::SelectionHook,
            },
            source_root_window,
        );
    }

    fn schedule_capture_with_options(
        &mut self,
        mut request: CaptureRequest,
        source_root_window: isize,
        empty_attempt: u8,
        process_parents: Option<Option<HashMap<u32, u32>>>,
    ) {
        self.cancel_active_capture();
        let now = Instant::now();
        let source_process_id = window_process_id(HWND(source_root_window as *mut c_void));
        let source_image_path = (empty_attempt == 0)
            .then(|| process_image_path(source_process_id))
            .flatten();
        if empty_attempt == 0 {
            request.capture_strategy = source_image_path
                .as_deref()
                .map_or(self.capture_settings.default_strategy, |image_path| {
                    capture_strategy_for_application(&self.capture_settings, image_path)
                });
        }
        trace_selection_capture(
            if empty_attempt == 0 {
                "scheduled"
            } else {
                "scheduled-empty-retry"
            },
            &request,
            source_root_window,
            source_process_id,
        );
        let settle = if empty_attempt == 0 {
            capture_settle_delay_for_application(&request, source_image_path.as_deref())
        } else {
            empty_capture_retry_delay(&request, empty_attempt)
        };
        self.pending_capture = Some(PendingCapture {
            due: now + settle,
            expires_at: now + FOREGROUND_SETTLE_TIMEOUT,
            scheduled_at: now,
            request,
            source_root_window,
            source_process_id,
            process_parents,
            empty_attempt,
        });
    }

    fn capture_strategy_for(&self, process_id: u32) -> SelectionCaptureStrategy {
        process_image_path(process_id)
            .map_or(self.capture_settings.default_strategy, |image_path| {
                capture_strategy_for_application(&self.capture_settings, &image_path)
            })
    }

    fn run_pending_capture(&mut self) {
        if !self
            .pending_capture
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.due)
        {
            return;
        }
        let Some(pending) = self.pending_capture.as_mut() else {
            return;
        };

        // A result window can still own the foreground for a few frames after
        // the user starts the next drag. Wait only while there is no usable
        // external foreground. The HWND/PID observed at mouse-down is merely
        // correlation metadata: WindowFromPoint roots are not stable across
        // shell hosts, renderer processes, owned popups, or window recreation.
        // Once any external foreground exists, the helper's existing exact
        // HWND/PID/generation checks remain the capture authority.
        let foreground = unsafe { GetForegroundWindow() };
        let foreground_process_id = window_process_id(foreground);
        let foreground_root = root_window(foreground).0 as isize;
        let source_root_window = pending.source_root_window;
        let source_process_id = pending.source_process_id;
        // WindowFromPoint may return no usable root for protected surfaces,
        // virtualized PDF panes or a transition between child windows. The
        // mouse-up event itself was observed while an external window was
        // foreground, so let the helper validate that live target instead of
        // spending the whole foreground-settle budget waiting for metadata we
        // do not have.
        let source_related = (source_root_window == 0 || source_process_id == 0)
            || capture_windows_are_related_with_parents(
                source_root_window,
                source_process_id,
                foreground_root,
                foreground_process_id,
                &mut pending.process_parents,
            );
        match pending_foreground_decision(
            !foreground.is_invalid(),
            foreground_process_id,
            self.own_process_id,
            source_related,
            Instant::now() < pending.expires_at,
        ) {
            PendingForegroundDecision::Wait => {
                trace_selection_capture(
                    "wait-foreground",
                    &pending.request,
                    pending.source_root_window,
                    foreground_process_id,
                );
                pending.due = Instant::now() + FOREGROUND_SETTLE_RETRY;
                return;
            }
            PendingForegroundDecision::Expire => {
                trace_selection_capture(
                    "expired",
                    &pending.request,
                    pending.source_root_window,
                    foreground_process_id,
                );
                self.pending_capture = None;
                return;
            }
            PendingForegroundDecision::Capture => {}
        }

        let pending = self
            .pending_capture
            .take()
            .expect("pending capture was checked above");
        let mut request = pending.request;
        let empty_attempt = pending.empty_attempt;
        let retry_root = pending.source_root_window;
        let retry_parents = pending.process_parents;
        let scheduled_at = pending.scheduled_at;
        // Wheel / trackpad inertia after mouse-up bumps hook generation. If no
        // new press is in progress, adopt the latest generation so the helper
        // does not fail-closed on a stale mouse-up id.
        if self.mouse_down.is_none() {
            request.generation = Some(hook_generation().load(Ordering::Acquire));
        }
        trace_selection_capture(
            if empty_attempt == 0 {
                "capture"
            } else {
                "capture-empty-retry"
            },
            &request,
            root_window(foreground).0 as isize,
            foreground_process_id,
        );
        self.submit_capture(
            request,
            retry_root,
            pending.source_process_id,
            retry_parents,
            empty_attempt,
            scheduled_at,
            None,
        );
    }

    fn submit_capture(
        &mut self,
        request: CaptureRequest,
        source_root_window: isize,
        source_process_id: u32,
        process_parents: Option<Option<HashMap<u32, u32>>>,
        empty_attempt: u8,
        scheduled_at: Instant,
        reply: Option<CaptureReply>,
    ) {
        if self.capture_coordinator.is_none() {
            self.capture_coordinator =
                CaptureCoordinator::spawn(self.own_process_id, self.inbox.clone()).ok();
            if self.capture_coordinator.is_none() {
                eprintln!("[selection] capture lane unavailable; will recover on the next gesture");
            }
        }
        let Some(coordinator) = self.capture_coordinator.as_mut() else {
            if let Some(reply) = reply {
                let _ = reply.send(Err(SelectionError::Internal));
            }
            return;
        };
        coordinator.submit(
            request,
            source_root_window,
            source_process_id,
            process_parents,
            empty_attempt,
            scheduled_at,
            reply,
        );
    }

    fn poll_capture_completions(&mut self) {
        loop {
            let Some(completion) = self
                .capture_coordinator
                .as_mut()
                .and_then(CaptureCoordinator::try_complete)
            else {
                return;
            };
            if let Some(reply) = completion.reply {
                if completion.cancelled.load(Ordering::Acquire) {
                    let _ = reply.send(Ok(None));
                    continue;
                }
                let _ = reply.send(completion.result);
                continue;
            }
            if completion.cancelled.load(Ordering::Acquire)
                || completion.superseded
                || self.pending_capture.is_some()
            {
                continue;
            }

            let foreground = unsafe { GetForegroundWindow() };
            let foreground_process_id = window_process_id(foreground);
            let foreground_root = root_window(foreground).0 as isize;
            let (trace_root, trace_process_id) =
                if foreground_process_id != 0 && foreground_process_id != self.own_process_id {
                    (foreground_root, foreground_process_id)
                } else {
                    (completion.source_root_window, completion.source_process_id)
                };
            let request = completion.request;
            match completion.result {
                Ok(Some(selection)) => {
                    self.deliver_captured_selection(
                        request,
                        selection,
                        trace_root,
                        trace_process_id,
                    );
                }
                Ok(None)
                    if !completion.suppress_empty_retry
                        && should_retry_empty_capture(&request)
                        && (completion.empty_attempt as usize)
                            < max_empty_capture_retries(&request) =>
                {
                    trace_selection_capture(
                        "empty-will-retry",
                        &request,
                        trace_root,
                        trace_process_id,
                    );
                    self.schedule_capture_with_options(
                        request,
                        completion.source_root_window,
                        completion.empty_attempt.saturating_add(1),
                        completion.process_parents,
                    );
                }
                Ok(None) => {
                    trace_selection_capture("empty", &request, trace_root, trace_process_id);
                }
                Err(_) => {
                    trace_selection_capture("error", &request, trace_root, trace_process_id);
                }
            }
        }
    }

    fn deliver_captured_selection(
        &mut self,
        request: CaptureRequest,
        mut selection: SelectionPayload,
        source_root_window: isize,
        source_process_id: u32,
    ) -> bool {
        if request.observation_only {
            return false;
        }
        // Every capture is non-destructive and generation-bound. A stale AX
        // result must never repaint the toolbar after a newer selection.
        let generation_mismatch = request
            .generation
            .is_some_and(|generation| generation != hook_generation().load(Ordering::Acquire));
        if generation_mismatch {
            trace_selection_capture(
                "discarded-generation",
                &request,
                source_root_window,
                source_process_id,
            );
            return false;
        }
        if request.trigger == SelectionTrigger::DoubleClick
            && (!selection.has_verified_windows_double_click()
                || selection.mouse.current != raw_selection_point(request.current))
        {
            trace_selection_capture(
                "rejected-missing-selection-evidence",
                &request,
                source_root_window,
                source_process_id,
            );
            return false;
        }
        if request.trigger != SelectionTrigger::Manual {
            if request.trigger != SelectionTrigger::Keyboard
                && !automatic_selection_pointer_matches_bounds(&selection, POINTER_BOUNDS_TOLERANCE)
            {
                // Chromium/WebView2 providers occasionally report stale or
                // mixed-DPI rectangles even when the selected text is valid.
                // Keep the text and fall back to the physical mouse-up point
                // instead of dropping the whole selection.
                selection.bounds = None;
                selection.start_top = None;
                selection.start_bottom = None;
                selection.end_top = None;
                selection.end_bottom = None;
            }
            let fingerprint = automatic_selection_fingerprint(&selection);
            let generation = request.generation.unwrap_or_default();
            let now = Instant::now();
            if self.last_automatic_fingerprint.is_some_and(
                |(previous, previous_generation, emitted_at)| {
                    previous == fingerprint
                        && previous_generation == generation
                        && now.saturating_duration_since(emitted_at) <= AUTOMATIC_DUPLICATE_WINDOW
                },
            ) {
                return false;
            }
            self.last_automatic_fingerprint = Some((fingerprint, generation, now));
        }
        trace_selection_capture(
            "selection-event",
            &request,
            source_root_window,
            source_process_id,
        );
        let sent = self
            .event_sender
            .lock()
            .ok()
            .is_some_and(|sender| sender.send(SelectionEvent::Selection(selection)).is_ok());
        if sent {
            self.recent_capture = Some(RecentCaptureContext {
                generation: request
                    .generation
                    .unwrap_or_else(|| hook_generation().load(Ordering::Acquire)),
                source_root_window,
                source_process_id,
                valid_until: Instant::now() + RECENT_CAPTURE_FOREGROUND_GRACE,
            });
        }
        sent
    }

    fn emit_dismiss(&self, reason: &str, point: RawPoint, target_window: HWND, timestamp_ms: u64) {
        let _ = self.event_sender.lock().ok().map(|sender| {
            sender.send(SelectionEvent::Dismiss(DismissEvent {
                reason: reason.to_owned(),
                mouse: raw_selection_point(point),
                target_pid: i64::from(window_process_id(target_window)),
                timestamp_ms,
                sequence: 0,
            }))
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureExecutorError {
    Disconnected,
    TimedOut,
    Cancelled,
    /// The helper answered a cancel request promptly; it is still healthy and
    /// must not be recycled just because the gesture was superseded.
    CancelledCooperative,
}

struct CaptureExecutor {
    own_process_id: u32,
    helper: Option<SelectionHelperProcess>,
    warming_helper: Option<Receiver<Result<SelectionHelperProcess, ()>>>,
    next_helper_maintenance: Instant,
    next_request_id: u64,
}

impl CaptureExecutor {
    fn spawn(own_process_id: u32) -> Result<Self, SelectionError> {
        Ok(Self {
            own_process_id,
            helper: Some(SelectionHelperProcess::spawn()?),
            warming_helper: None,
            next_helper_maintenance: Instant::now() + HELPER_MAINTENANCE_INTERVAL,
            next_request_id: 1,
        })
    }

    fn capture(
        &mut self,
        request: CaptureRequest,
        source_root_window: isize,
        source_process_id: u32,
        cancelled: &AtomicBool,
        user_keyboard: &AtomicBool,
        superseded: &AtomicBool,
    ) -> Result<Result<Option<SelectionPayload>, SelectionError>, CaptureExecutorError> {
        self.discard_exited_helper();
        self.adopt_warming_helper(false);
        if self.helper.is_none() {
            self.begin_warming_helper();
            self.adopt_warming_helper(true);
        }
        self.next_helper_maintenance = Instant::now() + HELPER_MAINTENANCE_INTERVAL;
        if self.helper.is_none() {
            return Err(CaptureExecutorError::Disconnected);
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let result = self
            .helper
            .as_mut()
            .expect("helper presence checked above")
            .capture(
                request_id,
                request,
                source_root_window,
                source_process_id,
                self.own_process_id,
                cancelled,
                user_keyboard,
                superseded,
            );
        match result {
            Ok(result) => Ok(result),
            Err(CaptureExecutorError::Disconnected) => {
                if let Some(mut helper) = self.helper.take() {
                    helper.terminate();
                }
                self.begin_warming_helper();
                Err(CaptureExecutorError::Disconnected)
            }
            // A prompt cancel reply proves the helper still owns a healthy
            // STA/OLE apartment; keep it to avoid process churn on every
            // keystroke mid-capture.
            Err(CaptureExecutorError::CancelledCooperative) => {
                Err(CaptureExecutorError::CancelledCooperative)
            }
            Err(error) => {
                if let Some(mut helper) = self.helper.take() {
                    helper.terminate();
                }
                self.begin_warming_helper();
                Err(error)
            }
        }
    }

    fn maintenance_due(&self) -> Option<Instant> {
        Some(self.next_helper_maintenance)
    }

    fn maintain_pending_helper(&mut self) {
        if Instant::now() < self.next_helper_maintenance {
            return;
        }
        // A helper can be reclaimed while TextLens is idle. Detect that before
        // the next mouse-up and begin its replacement asynchronously, so the
        // first capture after a long pause does not have to cold-start it.
        self.discard_exited_helper();
        self.adopt_warming_helper(false);
        if self.helper.is_none() {
            self.begin_warming_helper();
        }
        self.next_helper_maintenance = Instant::now() + HELPER_MAINTENANCE_INTERVAL;
    }

    fn recycle_helper(&mut self) {
        if let Some(mut helper) = self.helper.take() {
            helper.terminate();
        }
        self.begin_warming_helper();
        self.next_helper_maintenance = Instant::now() + HELPER_MAINTENANCE_INTERVAL;
    }

    fn discard_exited_helper(&mut self) {
        let exited = self
            .helper
            .as_mut()
            .is_some_and(SelectionHelperProcess::has_exited);
        if exited {
            if let Some(mut helper) = self.helper.take() {
                helper.terminate();
            }
            self.begin_warming_helper();
        }
    }

    fn begin_warming_helper(&mut self) {
        if self.helper.is_some() || self.warming_helper.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        match thread::Builder::new()
            .name("textlens-selection-helper-warmup".to_owned())
            .spawn(move || {
                for attempt in 0..3 {
                    match SelectionHelperProcess::spawn() {
                        Ok(helper) => {
                            let _ = sender.send(Ok(helper));
                            return;
                        }
                        Err(_) if attempt < 2 => thread::sleep(Duration::from_millis(200)),
                        Err(_) => break,
                    }
                }
                let _ = sender.send(Err(()));
            }) {
            Ok(_) => self.warming_helper = Some(receiver),
            Err(_) => self.warming_helper = None,
        }
    }

    fn adopt_warming_helper(&mut self, wait: bool) {
        let result = match self.warming_helper.as_ref() {
            Some(receiver) if wait => match receiver.recv_timeout(HELPER_CAPTURE_WARMUP_WAIT) {
                Ok(result) => Some(result),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(Err(())),
            },
            Some(receiver) => match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(())),
            },
            None => None,
        };
        let Some(result) = result else {
            return;
        };
        self.warming_helper.take();
        if let Ok(helper) = result {
            self.helper = Some(helper);
        }
    }

    fn shutdown(mut self, _grace: Duration) {
        if let Some(mut helper) = self.helper.take() {
            helper.shutdown();
        }
    }
}

struct SelectionHelperProcess {
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    events: Receiver<Result<HelperEvent, ()>>,
    reader: Option<JoinHandle<()>>,
}

impl SelectionHelperProcess {
    fn spawn() -> Result<Self, SelectionError> {
        let executable = std::env::current_exe().map_err(|_| SelectionError::Internal)?;
        // Route diagnostics run inside the isolated helper, where clipboard
        // and accessibility failures actually occur. Preserve stderr only
        // for an explicitly enabled trace session; production launches remain
        // silent and do not retain any selected text or clipboard content.
        let helper_stderr = if selection_trace_enabled() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = Command::new(executable)
            .arg(SELECTION_HELPER_FLAG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(helper_stderr)
            .spawn()
            .map_err(|_| SelectionError::Internal)?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SelectionError::Internal);
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SelectionError::Internal);
        };
        let (event_sender, events) = mpsc::channel();
        let reader = match thread::Builder::new()
            .name("textlens-selection-helper-reader".to_owned())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_helper_frame::<HelperEvent>(&mut reader) {
                        Ok(Some(event)) => {
                            if event_sender.send(Ok(event)).is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            let _ = event_sender.send(Err(()));
                            break;
                        }
                        Err(_) => {
                            let _ = event_sender.send(Err(()));
                            break;
                        }
                    }
                }
            }) {
            Ok(reader) => reader,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SelectionError::Internal);
            }
        };
        let mut helper = Self {
            child: Some(child),
            stdin: Some(BufWriter::new(stdin)),
            events,
            reader: Some(reader),
        };
        match helper.events.recv_timeout(HELPER_START_TIMEOUT) {
            Ok(Ok(HelperEvent::Ready { protocol_version }))
                if protocol_version == SELECTION_HELPER_PROTOCOL_VERSION =>
            {
                Ok(helper)
            }
            _ => {
                helper.terminate();
                Err(SelectionError::NativeInitializationFailed)
            }
        }
    }

    fn send(&mut self, command: &HelperCommand) -> Result<(), CaptureExecutorError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(CaptureExecutorError::Disconnected)?;
        write_helper_frame(stdin, command)
            .and_then(|()| stdin.flush())
            .map_err(|_| CaptureExecutorError::Disconnected)
    }

    fn has_exited(&mut self) -> bool {
        self.child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .is_some()
    }

    fn capture(
        &mut self,
        request_id: u64,
        request: CaptureRequest,
        source_root_window: isize,
        source_process_id: u32,
        own_process_id: u32,
        cancelled: &AtomicBool,
        user_keyboard: &AtomicBool,
        superseded: &AtomicBool,
    ) -> Result<Result<Option<SelectionPayload>, SelectionError>, CaptureExecutorError> {
        let generation_bound = request.trigger != SelectionTrigger::Manual;
        let expected_generation = request
            .generation
            .unwrap_or_else(|| hook_generation().load(Ordering::Acquire));
        if generation_bound && hook_generation().load(Ordering::Acquire) != expected_generation {
            return Ok(Ok(None));
        }
        // Keep the HWND/PID sampled during the original mouse gesture. The
        // foreground window can move to a renderer child, an owned popup, or
        // TextLens itself between mouse-up and this helper request. Re-reading
        // it here loses the source identity that the coordinator already
        // correlated with the gesture.
        let mut source_window = HWND(source_root_window as *mut c_void);
        let mut source_process_id = source_process_id;
        if source_window.0 == ptr::null_mut()
            || source_process_id == 0
            || source_process_id == own_process_id
        {
            source_window = unsafe { GetForegroundWindow() };
            source_process_id = window_process_id(source_window);
        }
        if source_window.0 == ptr::null_mut()
            || source_process_id == 0
            || source_process_id == own_process_id
        {
            return Ok(Ok(None));
        }
        // Preserve the parent's gesture id for first/second-click correlation.
        // Only the parent compares it to hook_generation(); the helper uses
        // CaptureControl's request cancellation channel, never its own hooks.
        self.send(&HelperCommand::Capture {
            request_id,
            request,
            source_window: source_window.0 as isize,
            source_process_id,
            textlens_process_id: own_process_id,
        })?;

        let task_deadline = Instant::now() + CAPTURE_TASK_TIMEOUT;
        let mut cancel_reason = None;
        let mut cancel_deadline = None;
        loop {
            // A clipboard transaction has its own conditional restore path in
            // the helper. Let every newer gesture cancel it promptly so a
            // stalled PDF copy cannot hold the capture lane or later
            // selections hostage.
            let superseded_requested = superseded.load(Ordering::Acquire);
            let generation_changed = generation_bound
                && hook_generation().load(Ordering::Acquire) != expected_generation;
            if cancel_reason.is_none()
                && (superseded_requested || cancelled.load(Ordering::Acquire) || generation_changed)
            {
                let reason = if user_keyboard.load(Ordering::Acquire) {
                    HelperCancelReason::UserKeyboard
                } else if superseded_requested {
                    HelperCancelReason::Superseded
                } else {
                    HelperCancelReason::InputChanged
                };
                self.send(&HelperCommand::Cancel { request_id, reason })?;
                cancel_reason = Some(reason);
                cancel_deadline = Some(Instant::now() + HELPER_CANCEL_GRACE);
            } else if cancel_reason.is_none() && Instant::now() >= task_deadline {
                let reason = HelperCancelReason::Timeout;
                self.send(&HelperCommand::Cancel { request_id, reason })?;
                cancel_reason = Some(reason);
                cancel_deadline = Some(Instant::now() + HELPER_CANCEL_GRACE);
            }

            if cancel_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(match cancel_reason {
                    Some(
                        HelperCancelReason::InputChanged
                        | HelperCancelReason::Superseded
                        | HelperCancelReason::UserKeyboard,
                    ) => CaptureExecutorError::Cancelled,
                    _ => CaptureExecutorError::TimedOut,
                });
            }

            match self.events.recv_timeout(HELPER_POLL_INTERVAL) {
                Ok(Ok(HelperEvent::Result {
                    request_id: event_request_id,
                    result,
                })) if event_request_id == request_id => {
                    if cancel_reason.is_some() {
                        return Err(CaptureExecutorError::CancelledCooperative);
                    }
                    let result = match result {
                        HelperCaptureResult::Selection {
                            mut selection,
                            source_window,
                            process_id,
                            provider_window,
                            provider_process_id,
                        } => {
                            if parent_capture_context_is_valid(
                                source_window,
                                process_id,
                                provider_window,
                                provider_process_id,
                                own_process_id,
                                expected_generation,
                                generation_bound,
                            ) {
                                trace_selection_provider(
                                    "parent-provider-accepted",
                                    HWND(source_window as *mut c_void),
                                    process_id,
                                    HWND(provider_window as *mut c_void),
                                    provider_process_id,
                                );
                                selection.timestamp_ms = strict_parent_timestamp_ms();
                                Ok(Some(selection))
                            } else {
                                trace_selection_provider(
                                    "parent-provider-rejected",
                                    HWND(source_window as *mut c_void),
                                    process_id,
                                    HWND(provider_window as *mut c_void),
                                    provider_process_id,
                                );
                                Ok(None)
                            }
                        }
                        HelperCaptureResult::Empty => Ok(None),
                        HelperCaptureResult::Error => {
                            Err(SelectionError::NativeInitializationFailed)
                        }
                    };
                    return Ok(result);
                }
                Ok(Ok(HelperEvent::Phase {
                    request_id: event_request_id,
                    phase,
                })) if event_request_id == request_id => match phase {
                    HelperPhase::ClipboardInjected { process_id } => {
                        trace_selection_route("clipboard-ctrl-c-posted", source_window, process_id)
                    }
                    HelperPhase::ClipboardSequenceChanged { process_id } => trace_selection_route(
                        "clipboard-sequence-changed",
                        source_window,
                        process_id,
                    ),
                    HelperPhase::ClipboardTextReady { process_id } => {
                        trace_selection_route("clipboard-text-ready", source_window, process_id)
                    }
                    HelperPhase::ClipboardRestored { process_id } => trace_selection_route(
                        "clipboard-restore-success",
                        source_window,
                        process_id,
                    ),
                },
                Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Err(())) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(CaptureExecutorError::Disconnected)
                }
            }
        }
    }

    fn shutdown(&mut self) {
        let _ = self.send(&HelperCommand::Shutdown);
        let deadline = Instant::now() + HELPER_SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .is_some()
            {
                self.child.take();
                self.stdin.take();
                self.finish_reader();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.terminate();
    }

    fn terminate(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.finish_reader();
    }

    fn finish_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            reap_worker(reader, WORKER_SHUTDOWN_GRACE);
        }
    }
}

impl Drop for SelectionHelperProcess {
    fn drop(&mut self) {
        if self.child.is_some() || self.reader.is_some() {
            self.shutdown();
        }
    }
}

/// Whether a helper selection result may still be delivered to the renderer.
///
/// - Exact HWND match is preferred but not required: multi-process PDF hosts
///   (WPS CEF, some 稻壳 builds) may keep a sibling pane focused during capture.
/// - Selection reads are non-destructive, so every delivered result must still
///   match the originating generation.
fn parent_capture_context_is_valid(
    source_window: isize,
    process_id: u32,
    provider_window: isize,
    provider_process_id: u32,
    own_process_id: u32,
    expected_generation: u64,
    require_generation_match: bool,
) -> bool {
    if process_id == 0
        || process_id == own_process_id
        || provider_process_id == 0
        || provider_process_id == own_process_id
    {
        return false;
    }
    let mut process_parents = None;
    if !process_belongs_to_capture_source(provider_process_id, process_id, &mut process_parents) {
        return false;
    }
    let provider_window = HWND(provider_window as *mut c_void);
    if provider_window.0 != ptr::null_mut() {
        let provider_window_process_id = window_process_id(provider_window);
        if provider_window_process_id != 0
            && !process_ids_are_related_with_parents(
                provider_window_process_id,
                provider_process_id,
                &mut process_parents,
            )
        {
            return false;
        }
    }
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0 == ptr::null_mut() {
        return false;
    }
    let foreground_pid = window_process_id(foreground);
    let same_application_family = foreground_pid != 0
        && foreground_pid != own_process_id
        && process_belongs_to_capture_source(foreground_pid, process_id, &mut process_parents);
    let generation_matches = !require_generation_match
        || hook_generation().load(Ordering::Acquire) == expected_generation;
    // The tail decision is shared with unit tests so the production path and
    // parent_capture_accepts cannot drift.
    parent_capture_accepts(
        foreground.0 as isize == source_window,
        foreground_pid,
        process_id,
        provider_process_id,
        own_process_id,
        same_application_family,
        true,
        generation_matches,
        require_generation_match,
    )
}

/// Pure predicate for unit tests of parent acceptance (no live HWND/API).
fn parent_capture_accepts(
    foreground_matches_source_hwnd: bool,
    foreground_pid: u32,
    process_id: u32,
    provider_process_id: u32,
    own_process_id: u32,
    same_application_family: bool,
    provider_belongs_to_source: bool,
    generation_matches: bool,
    require_generation_match: bool,
) -> bool {
    if process_id == 0
        || process_id == own_process_id
        || provider_process_id == 0
        || provider_process_id == own_process_id
        || foreground_pid == 0
    {
        return false;
    }
    if !provider_belongs_to_source {
        return false;
    }
    if foreground_pid == own_process_id {
        return false;
    }
    let related =
        foreground_matches_source_hwnd || foreground_pid == process_id || same_application_family;
    if !related {
        return false;
    }
    if require_generation_match && !generation_matches {
        return false;
    }
    true
}

fn write_helper_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() > HELPER_FRAME_LIMIT || payload.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "selection helper frame exceeds its limit",
        ));
    }
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(&payload)
}

fn read_helper_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<T>> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > HELPER_FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid selection helper frame length",
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn helper_cancel_reason_code(reason: HelperCancelReason) -> u32 {
    match reason {
        HelperCancelReason::InputChanged => 1,
        HelperCancelReason::Superseded => 2,
        HelperCancelReason::UserKeyboard => 3,
        HelperCancelReason::Timeout => 4,
        HelperCancelReason::Shutdown => 5,
    }
}

fn helper_cancel_reason_from_code(code: u32) -> Option<HelperCancelReason> {
    match code {
        1 => Some(HelperCancelReason::InputChanged),
        2 => Some(HelperCancelReason::Superseded),
        3 => Some(HelperCancelReason::UserKeyboard),
        4 => Some(HelperCancelReason::Timeout),
        5 => Some(HelperCancelReason::Shutdown),
        _ => None,
    }
}

struct CaptureControl {
    request_id: u64,
    cancelled_request_id: Arc<AtomicU64>,
    cancel_reason: Arc<AtomicU32>,
    event_writer: Arc<Mutex<BufWriter<std::io::Stdout>>>,
    source_window: HWND,
    source_process_id: u32,
    textlens_process_id: u32,
}

impl CaptureControl {
    fn cancel_reason(&self) -> Option<HelperCancelReason> {
        let cancelled_request_id = self.cancelled_request_id.load(Ordering::Acquire);
        (cancelled_request_id == self.request_id || cancelled_request_id == u64::MAX)
            .then(|| helper_cancel_reason_from_code(self.cancel_reason.load(Ordering::Acquire)))
            .flatten()
    }

    fn is_cancelled(&self) -> bool {
        self.cancel_reason().is_some()
    }

    fn report_phase(&self, phase: HelperPhase) {
        if let Ok(mut writer) = self.event_writer.lock() {
            let _ = write_helper_frame(
                &mut *writer,
                &HelperEvent::Phase {
                    request_id: self.request_id,
                    phase,
                },
            );
            let _ = writer.flush();
        }
    }
}

enum HelperWork {
    Capture {
        request_id: u64,
        request: CaptureRequest,
        source_window: isize,
        source_process_id: u32,
        textlens_process_id: u32,
    },
    Shutdown,
}

pub(super) fn run_selection_helper_if_requested() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(SELECTION_HELPER_FLAG)) {
        return false;
    }
    if let Err(error) = selection_helper_main() {
        eprintln!("selection helper failed: {error}");
    }
    true
}

fn selection_helper_main() -> io::Result<()> {
    let engine = CaptureEngine::initialize()
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
    let writer = Arc::new(Mutex::new(BufWriter::new(std::io::stdout())));
    {
        let mut output = writer
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "helper output lock poisoned"))?;
        write_helper_frame(
            &mut *output,
            &HelperEvent::Ready {
                protocol_version: SELECTION_HELPER_PROTOCOL_VERSION,
            },
        )?;
        output.flush()?;
    }

    let cancelled_request_id = Arc::new(AtomicU64::new(0));
    let cancel_reason = Arc::new(AtomicU32::new(0));
    let reader_cancelled_id = cancelled_request_id.clone();
    let reader_cancel_reason = cancel_reason.clone();
    let (command_sender, command_receiver) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("textlens-selection-helper-input".to_owned())
        .spawn(move || {
            let mut input = BufReader::new(std::io::stdin());
            loop {
                match read_helper_frame::<HelperCommand>(&mut input) {
                    Ok(Some(HelperCommand::Capture {
                        request_id,
                        request,
                        source_window,
                        source_process_id,
                        textlens_process_id,
                    })) => {
                        if command_sender
                            .send(HelperWork::Capture {
                                request_id,
                                request,
                                source_window,
                                source_process_id,
                                textlens_process_id,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Some(HelperCommand::Cancel { request_id, reason })) => {
                        reader_cancel_reason
                            .store(helper_cancel_reason_code(reason), Ordering::Relaxed);
                        reader_cancelled_id.store(request_id, Ordering::Release);
                    }
                    Ok(Some(HelperCommand::Shutdown)) => {
                        reader_cancel_reason.store(
                            helper_cancel_reason_code(HelperCancelReason::Shutdown),
                            Ordering::Relaxed,
                        );
                        reader_cancelled_id.store(u64::MAX, Ordering::Release);
                        let _ = command_sender.send(HelperWork::Shutdown);
                        break;
                    }
                    Ok(None) | Err(_) => {
                        reader_cancel_reason.store(
                            helper_cancel_reason_code(HelperCancelReason::Shutdown),
                            Ordering::Relaxed,
                        );
                        reader_cancelled_id.store(u64::MAX, Ordering::Release);
                        let _ = command_sender.send(HelperWork::Shutdown);
                        break;
                    }
                }
            }
        })?;

    let mut shutdown = false;
    while !shutdown {
        match command_receiver.recv_timeout(WORKER_PUMP_INTERVAL) {
            Ok(HelperWork::Shutdown) | Err(RecvTimeoutError::Disconnected) => shutdown = true,
            Ok(HelperWork::Capture {
                request_id,
                request,
                source_window,
                source_process_id,
                textlens_process_id,
            }) => {
                let control = CaptureControl {
                    request_id,
                    cancelled_request_id: cancelled_request_id.clone(),
                    cancel_reason: cancel_reason.clone(),
                    event_writer: writer.clone(),
                    source_window: HWND(source_window as *mut c_void),
                    source_process_id,
                    textlens_process_id,
                };
                let result = if control.is_cancelled() {
                    HelperCaptureResult::Empty
                } else {
                    match engine.capture(request, &control) {
                        Ok(Some(selection)) => {
                            let provider = engine.capture_provider(&control);
                            if selection_trace_enabled() {
                                trace_selection_line(format!(
                                    "[selection-evidence] trigger={:?} method={:?} evidence={:?} has_bounds={}",
                                    selection.trigger,
                                    selection.method,
                                    selection.windows_selection_evidence,
                                    selection.bounds.is_some(),
                                ));
                            }
                            trace_selection_provider(
                                "helper-provider",
                                control.source_window,
                                control.source_process_id,
                                provider.window,
                                provider.process_id,
                            );
                            HelperCaptureResult::Selection {
                                selection,
                                source_window: control.source_window.0 as isize,
                                process_id: control.source_process_id,
                                provider_window: provider.window.0 as isize,
                                provider_process_id: provider.process_id,
                            }
                        }
                        Ok(None) => HelperCaptureResult::Empty,
                        Err(_) => HelperCaptureResult::Error,
                    }
                };
                let mut output = writer.lock().map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "helper output lock poisoned")
                })?;
                write_helper_frame(&mut *output, &HelperEvent::Result { request_id, result })?;
                output.flush()?;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        pump_sta_messages();
    }
    let _ = reader.join();
    Ok(())
}

struct CaptureEngine {
    // Rust drops fields in declaration order. Every COM interface must be
    // released before OleUninitialize runs, so the apartment guard stays last.
    automation: IUIAutomation,
    provider: Cell<CaptureProvider>,
    deadline: Cell<Instant>,
    // Per-executable UIA success streak. After enough consecutive misses for
    // an app, later selections use the native accessibility route first
    // instead of re-paying the full UIA retry budget; periodically re-probed
    // so an app that starts working again (e.g. a document pane finishes
    // loading) recovers.
    app_route_cache: RefCell<HashMap<String, AppRouteState>>,
    // Retains the user's original clipboard only while a document renderer can
    // still publish one delayed, source-owned write after a successful restore.
    // The helper is long-lived, so this safely spans the next mouse gesture.
    clipboard_recovery: RefCell<Option<ClipboardRecovery>>,
    observation: RefCell<Option<SelectionObservation>>,
    _apartment: ComApartment,
}

/// A short-lived baseline taken after the first click has reached the host.
/// Only explicit ranges are retained; no names, values or document text.
struct SelectionObservation {
    source_window: isize,
    source_process_id: u32,
    generation: u64,
    captured_at: Instant,
    uia: Vec<(IUIAutomationElement, Vec<IUIAutomationTextRange>)>,
    native: HashMap<isize, (u32, u32)>,
    word: HashMap<isize, (i32, i32, i32)>,
}

#[derive(Debug, Clone, Copy)]
struct AppRouteState {
    consecutive_uia_misses: u8,
    probed_at: Instant,
}

struct ClipboardRecovery {
    snapshot: clipboard::WindowsClipboardSnapshot,
    source_process_id: u32,
    capture_profile: ClipboardCaptureProfile,
    restored_sequence: u32,
    recorded_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardRecoveryDecision {
    Drop,
    RestoreOriginal,
}

struct CaptureTarget {
    element: IUIAutomationElement,
    process_id: u32,
    source_window: HWND,
    provider_window: HWND,
    provider_process_id: u32,
    source_app: SourceApplication,
    /// DocumentRange is only trustworthy on a focused or root provider. A
    /// point hit can be a child decoration whose document range is unrelated
    /// to the current drag.
    allow_document_range: bool,
}

/// Identity of the object that actually supplied a selection. It is passed
/// only through the helper protocol and never emitted to renderer code.
#[derive(Debug, Clone, Copy, Default)]
struct CaptureProvider {
    window: HWND,
    process_id: u32,
}

enum CaptureTargetLookup {
    Found(CaptureTarget),
    Retryable,
    Stop,
}

enum AccessibilityCapture {
    Selection(SelectionPayload),
    Protected,
    NotFound,
}

enum TextPatternSearch {
    Found(Vec<(IUIAutomationTextPattern, IUIAutomationElement)>),
    Protected,
    NotFound,
}

/// Keep the existing preference for the highest document provider, or the
/// first non-document provider if there is no document. Double-clicks may
/// additionally try the remaining candidates if that preference is stale.
fn order_text_pattern_candidates<T>(mut candidates: Vec<(T, bool)>, alternatives: bool) -> Vec<T> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let preferred = candidates
        .iter()
        .rposition(|(_, is_document)| *is_document)
        .unwrap_or(0);
    let (preferred, _) = candidates.remove(preferred);
    let mut ordered = vec![preferred];
    if alternatives {
        ordered.extend(candidates.into_iter().map(|(candidate, _)| candidate));
    }
    ordered
}

fn first_double_click_candidate<T>(
    candidates: impl IntoIterator<Item = T>,
    mut capture: impl FnMut(T) -> AccessibilityCapture,
) -> AccessibilityCapture {
    for candidate in candidates {
        match capture(candidate) {
            AccessibilityCapture::NotFound => continue,
            result => return result,
        }
    }
    AccessibilityCapture::NotFound
}

impl CaptureEngine {
    fn initialize() -> Result<Self, SelectionError> {
        let apartment = ComApartment::initialize()?;
        let automation = unsafe {
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
        }
        .map_err(|_| SelectionError::NativeInitializationFailed)?;
        Ok(Self {
            automation,
            provider: Cell::new(CaptureProvider::default()),
            deadline: Cell::new(Instant::now() + CAPTURE_ENGINE_BUDGET),
            app_route_cache: RefCell::new(HashMap::new()),
            clipboard_recovery: RefCell::new(None),
            observation: RefCell::new(None),
            _apartment: apartment,
        })
    }

    fn observation_matches(
        &self,
        observation: &SelectionObservation,
        request: CaptureRequest,
        control: &CaptureControl,
    ) -> bool {
        request.trigger == SelectionTrigger::DoubleClick
            && request.generation.is_some_and(|generation| {
                WindowsSelectionObservationContext {
                    source_root: observation.source_window,
                    process_id: observation.source_process_id,
                    generation: observation.generation,
                }
                .matches_double_click(
                    WindowsSelectionObservationContext {
                        source_root: root_window(control.source_window).0 as isize,
                        process_id: control.source_process_id,
                        generation,
                    },
                    request.observation_generation,
                    observation.captured_at.elapsed(),
                    Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }) + 100),
                )
            })
            && root_window(unsafe { GetForegroundWindow() }) == root_window(control.source_window)
            && !control.is_cancelled()
    }

    fn observe_selection(&self, request: CaptureRequest, control: &CaptureControl) {
        // This path is intentionally separate from capture(): even a configured
        // clipboard host receives no synthetic keys for a simple click.
        self.deadline
            .set(Instant::now() + Duration::from_millis(100));
        *self.observation.borrow_mut() = Some(SelectionObservation {
            source_window: root_window(control.source_window).0 as isize,
            source_process_id: control.source_process_id,
            generation: request.observation_generation.unwrap_or_default(),
            captured_at: Instant::now(),
            uia: Vec::new(),
            native: HashMap::new(),
            word: HashMap::new(),
        });
        let mut parents = None;
        let mut source_app = None;
        if process_image_path(control.source_process_id)
            .is_some_and(|image| word_application_executable(&executable_name(&image)))
        {
            let application = source_application(control.source_process_id, control.source_window);
            let _ = self.capture_word_com_selection(request, control, application);
        }
        if !control.is_cancelled() && Instant::now() < self.deadline.get() {
            let _ =
                self.capture_native_text_control(request, control, &mut parents, &mut source_app);
        }
        let target =
            self.acquire_capture_target(request, control, &mut parents, &mut source_app, false);
        if let CaptureTargetLookup::Found(target) = target {
            if !self.target_is_non_text_action(&target, control) {
                let walkers = [
                    unsafe { self.automation.ControlViewWalker() }.ok(),
                    unsafe { self.automation.RawViewWalker() }.ok(),
                ];
                let mut visited_runtime_ids = HashSet::new();
                let mut visited_without_runtime_id = Vec::<IUIAutomationElement>::new();
                for walker in walkers.into_iter().flatten() {
                    let mut element = target.element.clone();
                    for _ in 0..MAX_UIA_ANCESTORS {
                        if control.is_cancelled() || Instant::now() >= self.deadline.get() {
                            break;
                        }
                        if unsafe { element.CurrentIsPassword() }
                            .ok()
                            .is_some_and(|value| value.as_bool())
                        {
                            self.observation.borrow_mut().take();
                            return;
                        }
                        let runtime_id = element_runtime_id(&element);
                        let duplicate = runtime_id.as_ref().map_or_else(
                            || {
                                visited_without_runtime_id.iter().any(|previous| {
                                    unsafe { self.automation.CompareElements(previous, &element) }
                                        .ok()
                                        .is_some_and(|same| same.as_bool())
                                })
                            },
                            |runtime_id| !visited_runtime_ids.insert(runtime_id.clone()),
                        );
                        if !duplicate {
                            if let Ok(pattern) = unsafe {
                                element.GetCurrentPatternAs::<IUIAutomationTextPattern>(
                                    UIA_TextPatternId,
                                )
                            } {
                                if let Some(ranges) =
                                    snapshot_uia_selection(&pattern, control, self.deadline.get())
                                {
                                    if let Some(observation) =
                                        self.observation.borrow_mut().as_mut()
                                    {
                                        observation.uia.push((element.clone(), ranges));
                                    }
                                }
                            }
                            if runtime_id.is_none() {
                                visited_without_runtime_id.push(element.clone());
                            }
                        }
                        element = match unsafe { walker.GetParentElement(&element) } {
                            Ok(parent) => parent,
                            Err(_) => break,
                        };
                    }
                }
            }
        }
        if control.is_cancelled() {
            self.observation.borrow_mut().take();
        }
        trace_selection_capture(
            "selection-observed",
            &request,
            control.source_window.0 as isize,
            control.source_process_id,
        );
    }

    fn uia_selection_changed(
        &self,
        owner: &IUIAutomationElement,
        range: &IUIAutomationTextRange,
        request: CaptureRequest,
        control: &CaptureControl,
    ) -> bool {
        let observation = self.observation.borrow();
        let Some(observation) = observation
            .as_ref()
            .filter(|observation| self.observation_matches(observation, request, control))
        else {
            return false;
        };
        let Some((_, previous)) = observation.uia.iter().find(|(element, _)| {
            unsafe { self.automation.CompareElements(element, owner) }
                .ok()
                .is_some_and(|same| same.as_bool())
        }) else {
            return false;
        };
        if previous.is_empty() {
            return true;
        }
        // A failed cross-document endpoint comparison is unknown, not a change.
        previous.iter().all(|old| {
            let start = unsafe {
                range.CompareEndpoints(
                    TextPatternRangeEndpoint_Start,
                    old,
                    TextPatternRangeEndpoint_Start,
                )
            };
            let end = unsafe {
                range.CompareEndpoints(
                    TextPatternRangeEndpoint_End,
                    old,
                    TextPatternRangeEndpoint_End,
                )
            };
            matches!((start, end), (Ok(start), Ok(end)) if start != 0 || end != 0)
        })
    }

    fn capture(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
    ) -> Result<Option<SelectionPayload>, SelectionError> {
        // The helper process services many requests with one COM apartment.
        // Reset the per-request budget before any executable-specific fast
        // path, including Word's native object model probe.
        self.deadline.set(Instant::now() + CAPTURE_ENGINE_BUDGET);
        self.provider.set(CaptureProvider::default());
        let executable_key = process_image_path(control.source_process_id)
            .map(|path| executable_name(&path))
            .filter(|name| !name.is_empty());
        if request.trigger != SelectionTrigger::Manual
            && executable_key
                .as_deref()
                .is_some_and(screenshot_application_executable)
        {
            trace_selection_capture(
                "rejected-screenshot-host",
                &request,
                control.source_window.0 as isize,
                control.source_process_id,
            );
            self.observation.borrow_mut().take();
            return Ok(None);
        }
        if request.observation_only {
            self.observe_selection(request, control);
            return Ok(None);
        }
        let word_host = executable_key
            .as_deref()
            .is_some_and(word_application_executable);
        let pdf_host = executable_key
            .as_deref()
            .is_some_and(pdf_accessibility_first_application);
        let mut source_app = None;
        if request.trigger != SelectionTrigger::Manual {
            let mut parents = None;
            match self.acquire_capture_target(
                request,
                control,
                &mut parents,
                &mut source_app,
                request.trigger == SelectionTrigger::Keyboard,
            ) {
                CaptureTargetLookup::Found(target)
                    if self.target_is_non_text_action(&target, control) =>
                {
                    trace_selection_capture(
                        "rejected-non-text-control",
                        &request,
                        control.source_window.0 as isize,
                        control.source_process_id,
                    );
                    return Ok(None);
                }
                CaptureTargetLookup::Stop => return Ok(None),
                _ => {}
            }
        }

        // Word exposes the authoritative live range through its native object
        // model. Query it before UIA so a document/title-like TextPattern can
        // never win the race and become the returned selection.
        if word_host {
            let application = resolve_cached_source_app(
                &mut source_app,
                control.source_process_id,
                control.source_window,
            );
            if let Some(application) = application {
                trace_selection_route(
                    "word-com-priority-attempt",
                    root_window(control.source_window),
                    control.source_process_id,
                );
                if let Some(selection) =
                    self.capture_word_com_selection(request, control, application)
                {
                    self.record_provider(control.source_window, control.source_process_id);
                    trace_selection_route(
                        "word-com-priority-success",
                        root_window(control.source_window),
                        control.source_process_id,
                    );
                    return Ok(Some(selection));
                }
                trace_selection_route(
                    "word-com-priority-failed",
                    root_window(control.source_window),
                    control.source_process_id,
                );
            }
        }
        // TextLens follows the same non-destructive order as Cherry's native
        // hook: query UI Automation and legacy accessibility only. Document
        // hosts get a short MSAA `accSelection` probe first because Office and
        // PDF surfaces often expose it before UIA publishes a text range.
        let abbreviate_uia_retries = request.trigger != SelectionTrigger::DoubleClick
            && (pdf_host
                || executable_key
                    .as_deref()
                    .is_some_and(|key| self.should_abbreviate_uia_retries_for_key(key)));
        let document_accessibility_probe = executable_key
            .as_deref()
            .and_then(document_accessibility_probe_budget);
        let result = self.capture_inner(
            request,
            control,
            abbreviate_uia_retries,
            document_accessibility_probe,
            pdf_host,
        );
        if let (Some(key), Ok(selection)) = (executable_key.as_deref(), &result) {
            self.record_uia_outcome(
                key,
                abbreviate_uia_retries,
                selection.as_ref().map(|payload| payload.method),
            );
        }
        result
    }

    fn record_provider(&self, window: HWND, process_id: u32) {
        if process_id != 0 {
            self.provider.set(CaptureProvider { window, process_id });
        }
    }

    fn capture_provider(&self, control: &CaptureControl) -> CaptureProvider {
        let provider = self.provider.get();
        if provider.process_id != 0 {
            provider
        } else {
            CaptureProvider {
                window: control.source_window,
                process_id: control.source_process_id,
            }
        }
    }

    /// Look up whether `key` (an executable basename) should temporarily
    /// abbreviate delayed UIA retries after repeated misses. A periodic full
    /// re-probe prevents an application from being permanently classified
    /// while its accessibility tree is still loading.
    fn should_abbreviate_uia_retries_for_key(&self, key: &str) -> bool {
        self.app_route_cache.borrow().get(key).is_some_and(|state| {
            adaptive_route_should_abbreviate_uia_retries(
                state.consecutive_uia_misses,
                state.probed_at.elapsed(),
            )
        })
    }

    /// Update the adaptive-route streak for `key` after a completed capture.
    fn record_uia_outcome(
        &self,
        key: &str,
        retries_abbreviated: bool,
        method: Option<SelectionMethod>,
    ) {
        match adaptive_route_update(retries_abbreviated, method) {
            AdaptiveRouteUpdate::Leave => {}
            AdaptiveRouteUpdate::Clear => {
                self.app_route_cache.borrow_mut().remove(key);
            }
            AdaptiveRouteUpdate::Extend => {
                let mut cache = self.app_route_cache.borrow_mut();
                let state = cache.entry(key.to_owned()).or_insert(AppRouteState {
                    consecutive_uia_misses: 0,
                    probed_at: Instant::now(),
                });
                state.consecutive_uia_misses = state.consecutive_uia_misses.saturating_add(1);
                state.probed_at = Instant::now();
                if cache.len() > MAX_APP_ROUTE_CACHE_ENTRIES {
                    // Best-effort cache only; a blunt reset is simpler than an
                    // LRU and self-heals within one more miss streak.
                    cache.clear();
                }
            }
        }
    }

    fn capture_inner(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
        abbreviate_uia_retries: bool,
        document_accessibility_probe: Option<Duration>,
        pdf_host: bool,
    ) -> Result<Option<SelectionPayload>, SelectionError> {
        // A reused lane gets a fresh bounded budget for every request.
        self.deadline.set(Instant::now() + CAPTURE_ENGINE_BUDGET);
        let mut process_parents = None;
        let mut source_app = None;
        // Configured direct-copy hosts must receive Ctrl+C while their PDF or
        // canvas selection is still the active range. A failed clipboard
        // transaction has already restored the user's state and completed its
        // bounded retries. End it here so known-broken UIA/MSAA providers do
        // not occupy the isolated helper until the two-second engine timeout.
        if request.trigger != SelectionTrigger::DoubleClick
            && request.capture_strategy == SelectionCaptureStrategy::Clipboard
        {
            let foreground = unsafe { GetForegroundWindow() };
            let process_id = if control.source_process_id != 0 {
                control.source_process_id
            } else {
                window_process_id(foreground)
            };
            let source_window = if control.source_window.0 != ptr::null_mut() {
                control.source_window
            } else {
                foreground
            };
            if let Some(application) =
                resolve_cached_source_app(&mut source_app, process_id, source_window)
            {
                if let Some(selection) = self.capture_clipboard(
                    request,
                    process_id,
                    source_window,
                    application,
                    &mut process_parents,
                    control,
                )? {
                    self.record_provider(source_window, process_id);
                    return Ok(Some(selection));
                }
            }
            return Ok(None);
        }

        // Native edit controls expose their active range through EM_GETSEL but
        // frequently do not implement UIA TextPattern or IAccessible. Query
        // this read-only path before the broader accessibility walk so a
        // classic editor can respond on the first frame without any clipboard
        // mutation.
        {
            match self.capture_native_text_control(
                request,
                control,
                &mut process_parents,
                &mut source_app,
            ) {
                AccessibilityCapture::Selection(selection) => return Ok(Some(selection)),
                AccessibilityCapture::Protected => return Ok(None),
                AccessibilityCapture::NotFound => {}
            }

            if let Some(probe_budget) = document_accessibility_probe
                .filter(|_| request.trigger != SelectionTrigger::DoubleClick)
            {
                match self.capture_document_accessibility(
                    request,
                    control,
                    &mut process_parents,
                    &mut source_app,
                    probe_budget,
                ) {
                    AccessibilityCapture::Selection(selection) => return Ok(Some(selection)),
                    AccessibilityCapture::Protected => return Ok(None),
                    AccessibilityCapture::NotFound => {}
                }
            }
        }

        let retry_started = Instant::now();
        // Chromium/Electron validation may need a system-wide process-tree
        // snapshot. Capture it lazily and at most once across UIA retries.
        // A miss streak only suppresses the second delayed UIA attempt. It
        // must never permanently remove UIA from the route: many providers
        // publish their selection lazily, and a later gesture may be the first
        // one for which the focused element is usable again.
        let uia_retry_delays: &[Duration] = if abbreviate_uia_retries {
            &ACCESSIBILITY_RETRY_DELAYS[..1]
        } else {
            &ACCESSIBILITY_RETRY_DELAYS
        };
        for (attempt, retry_delay) in uia_retry_delays.iter().copied().enumerate() {
            let remaining = retry_delay.saturating_sub(retry_started.elapsed());
            if !remaining.is_zero() {
                thread::sleep(remaining);
            }
            if control.is_cancelled() {
                return Ok(None);
            }
            // Never skip the first attempt; only bail out of later ones once
            // slow third-party UIA providers have already eaten the budget.
            if attempt > 0 && retry_started.elapsed() >= UIA_PHASE_BUDGET {
                break;
            }
            if !matches!(
                request.trigger,
                SelectionTrigger::Keyboard | SelectionTrigger::Manual
            ) {
                // ElementFromPoint can land on a selection adornment,
                // scrollbar, or renderer child at mouse-up. Probe the
                // release point first and then the press point; both are
                // still tied to the same generation and source context.
                let mut points = vec![request.current];
                if let Some(start) = request.start.filter(|point| *point != request.current) {
                    points.push(start);
                }
                for point in points {
                    let target = match self.acquire_capture_target_at(
                        request,
                        control,
                        &mut process_parents,
                        &mut source_app,
                        false,
                        point,
                    ) {
                        CaptureTargetLookup::Found(target) => target,
                        CaptureTargetLookup::Retryable => continue,
                        CaptureTargetLookup::Stop => return Ok(None),
                    };
                    if request.trigger != SelectionTrigger::Manual
                        && self.target_is_non_text_action(&target, control)
                    {
                        // Explorer marks icons as selected objects. Their labels
                        // are not text selections, even when an accessibility
                        // provider exposes the label through a text pattern.
                        return Ok(None);
                    }
                    match self.capture_target_selection(&target, request, control)? {
                        AccessibilityCapture::Selection(selection) => return Ok(Some(selection)),
                        AccessibilityCapture::Protected => return Ok(None),
                        AccessibilityCapture::NotFound => {}
                    }
                }
            }

            let focused_target = match self.acquire_capture_target(
                request,
                control,
                &mut process_parents,
                &mut source_app,
                true,
            ) {
                CaptureTargetLookup::Found(target) => Some(target),
                CaptureTargetLookup::Retryable => None,
                CaptureTargetLookup::Stop => return Ok(None),
            };
            if let Some(target) = focused_target {
                match self.capture_target_selection(&target, request, control)? {
                    AccessibilityCapture::Selection(selection) => return Ok(Some(selection)),
                    AccessibilityCapture::Protected => return Ok(None),
                    AccessibilityCapture::NotFound => {}
                }
            }

            let root_target = match self.acquire_capture_target_from_foreground_window(
                control,
                &mut process_parents,
                &mut source_app,
            ) {
                CaptureTargetLookup::Found(target) => Some(target),
                CaptureTargetLookup::Retryable => None,
                CaptureTargetLookup::Stop => return Ok(None),
            };
            if let Some(target) = root_target {
                match self.capture_target_selection(&target, request, control)? {
                    AccessibilityCapture::Selection(selection) => return Ok(Some(selection)),
                    AccessibilityCapture::Protected => return Ok(None),
                    AccessibilityCapture::NotFound => {}
                }
            }
        }

        if request.trigger == SelectionTrigger::DoubleClick
            || Instant::now() >= self.deadline.get()
            || control.is_cancelled()
        {
            return Ok(None);
        }
        // Legacy IAccessible is the final non-destructive accessibility
        // fallback for all hosts. It expands coverage for classic Win32,
        // Office grids, Java, and custom controls without changing the source
        // application's state.
        // The initial PDF MSAA probe already covered the same point/window
        // providers. Skipping this duplicate late walk removes a noticeable
        // delay for canvas readers; a bounded empty-capture retry gives the
        // provider another chance after it commits the range.
        if !(pdf_host && document_accessibility_probe.is_some()) {
            match self.capture_foreground_accessible(
                request,
                control,
                &mut process_parents,
                &mut source_app,
            ) {
                AccessibilityCapture::Selection(selection) => return Ok(Some(selection)),
                AccessibilityCapture::Protected => return Ok(None),
                AccessibilityCapture::NotFound => {}
            }
        }

        // Word's native object model is the most authoritative source when
        // UIA exposes a title-like range or a renderer child. Keep this
        // provider strictly executable-gated and only call it after the
        // non-destructive UIA/MSAA probes have failed.
        if source_app.as_ref().is_some_and(|(_, application)| {
            matches!(
                executable_name(&application.bundle_id).as_str(),
                "winword.exe" | "word.exe"
            )
        }) {
            let application = source_app
                .as_ref()
                .map(|(_, application)| application.clone())
                .expect("source_app was checked above");
            let source_window = root_window(control.source_window);
            trace_selection_route(
                "word-com-attempt",
                source_window,
                window_process_id(source_window),
            );
            if let Some(selection) = self.capture_word_com_selection(request, control, application)
            {
                self.record_provider(source_window, window_process_id(source_window));
                trace_selection_route(
                    "word-com-success",
                    source_window,
                    window_process_id(source_window),
                );
                return Ok(Some(selection));
            }
            trace_selection_route(
                "word-com-failed",
                source_window,
                window_process_id(source_window),
            );
        }

        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return Ok(None);
        }
        if request.capture_strategy != SelectionCaptureStrategy::Auto {
            return Ok(None);
        }
        let foreground = unsafe { GetForegroundWindow() };
        let process_id = if control.source_process_id != 0 {
            control.source_process_id
        } else {
            window_process_id(foreground)
        };
        let source_window = if control.source_window.0 != ptr::null_mut() {
            control.source_window
        } else {
            foreground
        };
        let Some(source_app) =
            resolve_cached_source_app(&mut source_app, process_id, source_window)
        else {
            return Ok(None);
        };
        let selection = self.capture_clipboard(
            request,
            process_id,
            source_window,
            source_app,
            &mut process_parents,
            control,
        )?;
        if selection.is_some() {
            self.record_provider(source_window, process_id);
        }
        Ok(selection)
    }

    fn reconcile_clipboard_recovery(
        &self,
        request: CaptureRequest,
        process_id: u32,
        source_window: HWND,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        control: &CaptureControl,
    ) -> bool {
        let recovery = self.clipboard_recovery.borrow_mut().take();
        let Some(recovery) = recovery else {
            return false;
        };
        if recovery.recorded_at.elapsed() > CLIPBOARD_RECOVERY_WINDOW {
            trace_selection_route("clipboard-recovery-expired", source_window, process_id);
            return false;
        }
        if !clipboard_owner_pid_matches_target(
            process_id,
            recovery.source_process_id,
            process_parents
                .as_ref()
                .and_then(|parents| parents.as_ref()),
        ) {
            trace_selection_route(
                "clipboard-recovery-source-changed",
                source_window,
                process_id,
            );
            return false;
        }

        let current_sequence = clipboard::windows_clipboard_sequence();
        let decision = clipboard_recovery_decision(
            recovery.restored_sequence,
            current_sequence,
            request.clipboard_sequence_at_start,
            clipboard_recovery_owner_is_acceptable(
                recovery.capture_profile,
                process_id,
                process_parents,
            ),
        );
        if decision != ClipboardRecoveryDecision::RestoreOriginal {
            // Keep the ledger alive while the clipboard is still exactly the
            // sequence produced by our previous restore. A PDF renderer may
            // publish one delayed source-owned format immediately after this
            // probe; retaining the snapshot lets the next guarded snapshot
            // repair that race without ever overwriting a newer user copy.
            if current_sequence == recovery.restored_sequence {
                self.clipboard_recovery.borrow_mut().replace(recovery);
            }
            trace_selection_route(
                "clipboard-recovery-preserved-user-state",
                source_window,
                process_id,
            );
            return false;
        }
        if control.is_cancelled()
            || !clipboard_capture_context_is_valid_with_lazy_parents(
                source_window,
                process_id,
                process_parents,
                control,
            )
            || !copy_shortcut_is_safe_to_inject(control)
        {
            trace_selection_route("clipboard-recovery-cancelled", source_window, process_id);
            return false;
        }
        if recovery.snapshot.restore_if_unchanged(current_sequence) {
            control.report_phase(HelperPhase::ClipboardRestored { process_id });
            // The renderer can publish another delayed format after this
            // repair. Keep the same original snapshot under a fresh sequence
            // boundary so the guarded capture snapshot can reconcile it too.
            self.arm_clipboard_recovery(recovery.snapshot, process_id, recovery.capture_profile);
            trace_selection_route(
                "clipboard-recovery-restored-late-source-write",
                source_window,
                process_id,
            );
            true
        } else {
            trace_selection_route(
                "clipboard-recovery-restore-raced",
                source_window,
                process_id,
            );
            false
        }
    }

    fn arm_clipboard_recovery(
        &self,
        snapshot: clipboard::WindowsClipboardSnapshot,
        source_process_id: u32,
        capture_profile: ClipboardCaptureProfile,
    ) {
        let restored_sequence = clipboard::windows_clipboard_sequence();
        self.clipboard_recovery
            .borrow_mut()
            .replace(ClipboardRecovery {
                snapshot,
                source_process_id,
                capture_profile,
                restored_sequence,
                recorded_at: Instant::now(),
            });
    }

    fn capture_clipboard(
        &self,
        request: CaptureRequest,
        process_id: u32,
        source_window: HWND,
        source_app: SourceApplication,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        control: &CaptureControl,
    ) -> Result<Option<SelectionPayload>, SelectionError> {
        if !request.may_inject_copy() {
            trace_selection_capture(
                "rejected-unverified-clipboard-double-click",
                &request,
                source_window.0 as isize,
                process_id,
            );
            return Ok(None);
        }
        let image_path = source_app.bundle_id.clone();
        if request.trigger != SelectionTrigger::Manual
            && !known_copy_compatibility_application(&image_path)
        {
            let mut application = None;
            let target = self.acquire_capture_target(
                request,
                control,
                process_parents,
                &mut application,
                request.trigger == SelectionTrigger::Keyboard,
            );
            let text_context = match target {
                CaptureTargetLookup::Found(target)
                    if !self.target_is_non_text_action(&target, control) =>
                {
                    let walker = unsafe { self.automation.ControlViewWalker() }.ok();
                    let mut element = Some(target.element);
                    let mut text_context = false;
                    for _ in 0..MAX_UIA_ANCESTORS {
                        if control.is_cancelled() || Instant::now() >= self.deadline.get() {
                            break;
                        }
                        let Some(current) = element.take() else {
                            break;
                        };
                        let kind = unsafe { current.CurrentControlType() }.ok();
                        if kind == Some(UIA_EditControlTypeId)
                            || kind == Some(UIA_DocumentControlTypeId)
                        {
                            text_context = true;
                            break;
                        }
                        element = walker
                            .as_ref()
                            .and_then(|walker| unsafe { walker.GetParentElement(&current) }.ok());
                    }
                    text_context
                }
                _ => false,
            };
            if !text_context {
                trace_selection_capture(
                    "rejected-clipboard-unverified-text-context",
                    &request,
                    source_window.0 as isize,
                    process_id,
                );
                return Ok(None);
            }
        }
        let clipboard_profile = clipboard_capture_profile(&image_path);
        let password_state = if clipboard_profile.uses_native_password_probe() {
            Some(focused_native_control_is_password(source_window))
        } else {
            focused_element_is_password(&self.automation)
        };
        if process_id == 0
            || process_id == control.textlens_process_id
            || prohibited_clipboard_application(&image_path)
            || password_state_blocks_clipboard(
                password_state,
                known_copy_compatibility_application(&image_path),
            )
        {
            trace_selection_route("clipboard-blocked", source_window, process_id);
            return Ok(None);
        }

        // Exact HWND/PID and same-executable checks cover the normal path.
        // Building a ToolHelp process snapshot on every drag can take a
        // visible amount of time on busy systems, so only pay for it when a
        // multi-process renderer actually needs ancestry correlation.
        if !clipboard_capture_context_is_valid_with_lazy_parents(
            source_window,
            process_id,
            process_parents,
            control,
        ) {
            trace_selection_route("clipboard-context-invalid", source_window, process_id);
            return Ok(None);
        }
        self.reconcile_clipboard_recovery(
            request,
            process_id,
            source_window,
            process_parents,
            control,
        );
        if control.is_cancelled() || !copy_shortcut_is_safe_to_inject(control) {
            trace_selection_route("clipboard-recovery-interrupted", source_window, process_id);
            return Ok(None);
        }
        // A delayed PDF format can arrive between OleGetClipboard and the
        // sequence check. Reconcile the retained previous transaction once,
        // then take a fresh snapshot. Never loop indefinitely and never
        // replace a clipboard update that is not source-owned.
        let (snapshot, baseline_sequence) = {
            let mut snapshot = None;
            for attempt in 0..2 {
                let Some(candidate) = clipboard::snapshot_windows_clipboard() else {
                    trace_selection_route("clipboard-snapshot-failed", source_window, process_id);
                    return Ok(None);
                };
                let candidate_sequence = candidate.sequence();
                if clipboard::windows_clipboard_sequence() == candidate_sequence {
                    snapshot = Some((candidate, candidate_sequence));
                    break;
                }
                trace_selection_route("clipboard-snapshot-raced", source_window, process_id);
                if attempt == 0 {
                    if !self.reconcile_clipboard_recovery(
                        request,
                        process_id,
                        source_window,
                        process_parents,
                        control,
                    ) {
                        return Ok(None);
                    }
                    if control.is_cancelled() {
                        return Ok(None);
                    }
                }
            }
            let Some(snapshot) = snapshot else {
                return Ok(None);
            };
            snapshot
        };
        let clipboard_deadline = Instant::now()
            .checked_add(clipboard_profile.phase_budget())
            .unwrap_or_else(Instant::now)
            .min(self.deadline.get());

        // The original clipboard sequence is the freshness boundary. Do not
        // clear it before Ctrl+C: Acrobat's protected PDF renderer can drop a
        // just-created selection while handling that foreign clipboard write.
        // We accept only a later sequence and restore the snapshot after the
        // capture, so old clipboard content can neither leak into a result nor
        // be lost to the user.
        if control.is_cancelled()
            || !clipboard_capture_context_is_valid_with_lazy_parents(
                source_window,
                process_id,
                process_parents,
                control,
            )
            || !copy_shortcut_is_safe_to_inject(control)
            || Instant::now() >= clipboard_deadline
        {
            trace_selection_route("clipboard-user-shortcut-active", source_window, process_id);
            return Ok(None);
        }

        if !post_copy_shortcut() {
            trace_selection_route(
                "clipboard-ctrl-c-injection-failed",
                source_window,
                process_id,
            );
            return Ok(None);
        }
        trace_selection_route("clipboard-ctrl-c-posted", source_window, process_id);
        control.report_phase(HelperPhase::ClipboardInjected { process_id });
        let mut copied_sequence = wait_for_clipboard_change(
            baseline_sequence,
            source_window,
            process_id,
            clipboard_deadline,
            process_parents,
            control,
            clipboard_profile,
            clipboard_profile.copy_poll_attempts(),
            clipboard_profile.poll_interval(),
            clipboard_profile.required_sequence_stable_polls(),
        );
        // PDF canvases can silently swallow synthetic chords. Use a small,
        // profile-specific dispatch budget while the clipboard is exactly
        // untouched. Every round revalidates foreground, cancellation and
        // physical key state so real Ctrl+C/Ctrl+V always takes precedence.
        let mut copy_dispatches = 1usize;
        while copied_sequence.is_none()
            && copy_dispatches < clipboard_profile.copy_dispatch_attempts()
            && clipboard_profile.retries_copy_after_timeout()
            && clipboard::windows_clipboard_sequence() == baseline_sequence
            && clipboard_capture_context_is_valid_with_lazy_parents(
                source_window,
                process_id,
                process_parents,
                control,
            )
            && copy_shortcut_is_safe_to_inject(control)
            && Instant::now() < clipboard_deadline
        {
            thread::sleep(clipboard_profile.copy_retry_delay());
            if clipboard::windows_clipboard_sequence() == baseline_sequence
                && clipboard_capture_context_is_valid_with_lazy_parents(
                    source_window,
                    process_id,
                    process_parents,
                    control,
                )
                && copy_shortcut_is_safe_to_inject(control)
                && Instant::now() < clipboard_deadline
                && post_copy_shortcut()
            {
                copy_dispatches = copy_dispatches.saturating_add(1);
                trace_selection_route("clipboard-ctrl-c-retry-posted", source_window, process_id);
                control.report_phase(HelperPhase::ClipboardInjected { process_id });
                copied_sequence = wait_for_clipboard_change(
                    baseline_sequence,
                    source_window,
                    process_id,
                    clipboard_deadline,
                    process_parents,
                    control,
                    clipboard_profile,
                    clipboard_profile.copy_poll_attempts(),
                    clipboard_profile.poll_interval(),
                    clipboard_profile.required_sequence_stable_polls(),
                );
            } else {
                break;
            }
        }
        let Some(copied_sequence) = copied_sequence else {
            let restored = restore_interrupted_clipboard_if_safe(
                &snapshot,
                baseline_sequence,
                process_id,
                process_parents,
                control,
                source_window,
                clipboard_profile,
            );
            if restored {
                self.arm_clipboard_recovery(snapshot, process_id, clipboard_profile);
            }
            trace_selection_route("clipboard-empty", source_window, process_id);
            return Ok(None);
        };
        // Keep the phase trace aligned with the freshness boundary. The
        // sequence can advance before CF_UNICODETEXT is readable, especially
        // in Acrobat and delayed-format PDF viewers.
        control.report_phase(HelperPhase::ClipboardSequenceChanged { process_id });
        let mut captured_text = wait_for_fresh_clipboard_text(
            copied_sequence,
            clipboard_profile.text_ready_budget(),
            clipboard_profile.text_settle_delay(),
            clipboard_deadline,
            source_window,
            process_id,
            process_parents,
            control,
            clipboard_profile,
            clipboard_profile.poll_interval(),
        );
        // Acrobat can acknowledge Ctrl+C with an OLE-only or incomplete
        // format set and never finish rendering text for that transaction.
        // Establish the latest synthetic sequence as a new freshness boundary
        // and copy once more. The original user snapshot remains the only
        // state restored after either attempt.
        if captured_text.is_none()
            && clipboard_profile.retries_copy_after_unreadable_text()
            && clipboard_capture_context_is_valid_with_lazy_parents(
                source_window,
                process_id,
                process_parents,
                control,
            )
            && copy_shortcut_is_safe_to_inject(control)
            && Instant::now() < clipboard_deadline
        {
            let retry_baseline = clipboard::windows_clipboard_sequence();
            thread::sleep(clipboard_profile.copy_retry_delay());
            if clipboard::windows_clipboard_sequence() == retry_baseline
                && clipboard_capture_context_is_valid_with_lazy_parents(
                    source_window,
                    process_id,
                    process_parents,
                    control,
                )
                && copy_shortcut_is_safe_to_inject(control)
                && Instant::now() < clipboard_deadline
                && post_copy_shortcut()
            {
                trace_selection_route("clipboard-text-retry-posted", source_window, process_id);
                control.report_phase(HelperPhase::ClipboardInjected { process_id });
                if let Some(retry_sequence) = wait_for_clipboard_change(
                    retry_baseline,
                    source_window,
                    process_id,
                    clipboard_deadline,
                    process_parents,
                    control,
                    clipboard_profile,
                    clipboard_profile.copy_poll_attempts(),
                    clipboard_profile.poll_interval(),
                    clipboard_profile.required_sequence_stable_polls(),
                ) {
                    control.report_phase(HelperPhase::ClipboardSequenceChanged { process_id });
                    captured_text = wait_for_fresh_clipboard_text(
                        retry_sequence,
                        clipboard_profile.text_ready_budget(),
                        clipboard_profile.text_settle_delay(),
                        clipboard_deadline,
                        source_window,
                        process_id,
                        process_parents,
                        control,
                        clipboard_profile,
                        clipboard_profile.poll_interval(),
                    );
                }
            }
        }
        let Some((text, copied_sequence)) = captured_text else {
            let restored = restore_interrupted_clipboard_if_safe(
                &snapshot,
                baseline_sequence,
                process_id,
                process_parents,
                control,
                source_window,
                clipboard_profile,
            );
            if restored {
                self.arm_clipboard_recovery(snapshot, process_id, clipboard_profile);
            }
            trace_selection_route("clipboard-text-not-ready", source_window, process_id);
            return Ok(None);
        };

        control.report_phase(HelperPhase::ClipboardTextReady { process_id });

        if control.is_cancelled()
            || !clipboard_capture_context_is_valid_with_lazy_parents(
                source_window,
                process_id,
                process_parents,
                control,
            )
            || !clipboard_owner_is_acceptable_for_capture(
                clipboard_profile,
                process_id,
                process_parents,
            )
        {
            if snapshot.restore_if_unchanged(copied_sequence) {
                control.report_phase(HelperPhase::ClipboardRestored { process_id });
            }
            trace_selection_route("clipboard-finalize-invalid", source_window, process_id);
            return Ok(None);
        }

        trace_selection_route("clipboard-read-success", source_window, process_id);
        // Sequence equality makes this restore safe even if the pointer has
        // moved away: a real user copy changes the sequence and wins. Keeping
        // the restore independent of foreground state prevents an aborted PDF
        // capture from leaving stale text that later selections can consume.
        let restored = snapshot.restore_if_unchanged(copied_sequence);
        if restored {
            control.report_phase(HelperPhase::ClipboardRestored { process_id });
        }
        trace_selection_route(
            if restored {
                "clipboard-restore-success"
            } else {
                "clipboard-restore-raced"
            },
            source_window,
            process_id,
        );
        if restored {
            self.arm_clipboard_recovery(snapshot, process_id, clipboard_profile);
        }
        if control.is_cancelled() {
            return Ok(None);
        }
        let Some(text) = Some(text).filter(|text| {
            captured_text_is_usable(text)
                && !selection_text_matches_source_identity(text, &source_app, source_window)
                && !selection_text_matches_known_host_label(text, &source_app)
                && !accessibility_text_looks_like_document_chrome(text, &source_app)
        }) else {
            return Ok(None);
        };

        Ok(Some(SelectionPayload {
            text,
            source_app,
            bounds: None,
            start_top: None,
            start_bottom: None,
            end_top: None,
            end_bottom: None,
            mouse: SelectionMouse {
                start: request.start.map(raw_selection_point),
                end: request.end.map(raw_selection_point),
                current: raw_selection_point(request.current),
            },
            direction: direction_from_points(
                request.start.map(raw_selection_point),
                request.end.map(raw_selection_point),
            ),
            is_fullscreen: window_is_fullscreen(source_window),
            method: SelectionMethod::Clipboard,
            trigger: request.trigger,
            timestamp_ms: timestamp_ms(),
            sequence: 0,
            windows_selection_evidence: None,
        }))
    }

    fn capture_word_com_selection(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
        source_app: SourceApplication,
    ) -> Option<SelectionPayload> {
        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return None;
        }
        let source_root = root_window(control.source_window);
        let source_process_id = control.source_process_id;
        if source_root.0 == ptr::null_mut() || source_process_id == 0 {
            return None;
        }
        if matches!(
            request.trigger,
            SelectionTrigger::DoubleClick | SelectionTrigger::Drag | SelectionTrigger::ShiftClick
        ) && window_class_name(window_at_point(request.current)).as_deref() != Some("_WwG")
        {
            return None;
        }

        // Word's native object model is attached to different HWNDs across
        // versions and view modes. Probe the point/focus/root candidates in a
        // bounded list instead of assuming the mouse-down root owns OBJID_NATIVEOM.
        let mut windows = accessible_window_candidates(source_root, request.current);
        windows.extend([source_root, control.source_window, unsafe {
            GetForegroundWindow()
        }]);
        let mut seen = HashSet::new();
        for source_window in windows {
            if source_window.0 == ptr::null_mut()
                || !seen.insert(source_window.0 as isize)
                || window_process_id(source_window) != source_process_id
                || !capture_context_still_valid(control, source_window, source_process_id)
            {
                continue;
            }

            let mut raw_dispatch = ptr::null_mut();
            if unsafe {
                AccessibleObjectFromWindow(
                    source_window,
                    OBJID_NATIVEOM.0 as u32,
                    &IDispatch::IID,
                    &mut raw_dispatch,
                )
            }
            .is_err()
                || raw_dispatch.is_null()
            {
                continue;
            }
            // SAFETY: AccessibleObjectFromWindow returned one owned IDispatch
            // reference for the requested Word window.
            let native_object = unsafe { IDispatch::from_raw(raw_dispatch) };
            let Some(selection) = word_selection_dispatch(&native_object) else {
                continue;
            };
            let Some(range) = com_property_dispatch(&selection, w!("Range")) else {
                continue;
            };
            let signature = com_property_i32(&range, w!("Start"))
                .zip(com_property_i32(&range, w!("End")))
                .zip(com_property_i32(&range, w!("StoryType")))
                .map(|((start, end), story)| (start, end, story));
            if request.observation_only {
                if let (Some(observation), Some(signature)) =
                    (self.observation.borrow_mut().as_mut(), signature)
                {
                    observation.word.insert(source_window.0 as isize, signature);
                }
                continue;
            }
            if signature.is_some_and(|(start, end, _)| {
                end <= start || i64::from(end) - i64::from(start) > 1_000_000
            }) || (request.trigger != SelectionTrigger::Manual && signature.is_none())
            {
                continue;
            }
            if request.trigger == SelectionTrigger::DoubleClick {
                let changed = signature.is_some_and(|signature| {
                    signature.1 > signature.0
                        && self
                            .observation
                            .borrow()
                            .as_ref()
                            .is_some_and(|observation| {
                                self.observation_matches(observation, request, control)
                                    && observation
                                        .word
                                        .get(&(source_window.0 as isize))
                                        .is_some_and(|old| {
                                            old.2 == signature.2 && *old != signature
                                        })
                            })
                });
                if !changed || com_property_i32(&selection, w!("Type")) != Some(2) {
                    trace_selection_capture(
                        "rejected-word-range-unchanged-or-unobserved",
                        &request,
                        source_root.0 as isize,
                        source_process_id,
                    );
                    continue;
                }
            }
            let Some(text) = com_property_text(&range, w!("Text")) else {
                continue;
            };
            let text = normalize_captured_text(&text);
            if !captured_text_is_usable(&text)
                || selection_text_matches_source_identity(&text, &source_app, source_root)
                || selection_text_matches_known_host_label(&text, &source_app)
            {
                continue;
            }

            return Some(SelectionPayload {
                text,
                source_app,
                bounds: None,
                start_top: None,
                start_bottom: None,
                end_top: None,
                end_bottom: None,
                mouse: SelectionMouse {
                    start: request.start.map(raw_selection_point),
                    end: request.end.map(raw_selection_point),
                    current: raw_selection_point(request.current),
                },
                direction: direction_from_points(
                    request.start.map(raw_selection_point),
                    request.end.map(raw_selection_point),
                ),
                is_fullscreen: window_is_fullscreen(source_root),
                method: SelectionMethod::Accessibility,
                trigger: request.trigger,
                timestamp_ms: timestamp_ms(),
                sequence: 0,
                windows_selection_evidence: (request.trigger == SelectionTrigger::DoubleClick)
                    .then_some(WindowsSelectionEvidence::WordRangeChanged),
            });
        }
        None
    }

    /// Give document hosts a short MSAA window before UIA. The full fallback
    /// remains available later in the request, so a slow provider never makes
    /// the initial toolbar response wait for the whole capture budget.
    fn capture_document_accessibility(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        source_app: &mut Option<(u32, SourceApplication)>,
        probe_budget: Duration,
    ) -> AccessibilityCapture {
        let original_deadline = self.deadline.get();
        let probe_deadline = Instant::now()
            .checked_add(probe_budget)
            .unwrap_or_else(Instant::now);
        if probe_deadline < original_deadline {
            self.deadline.set(probe_deadline);
        }
        let result =
            self.capture_foreground_accessible(request, control, process_parents, source_app);
        self.deadline.set(original_deadline);
        result
    }

    /// Read the active range from a classic Win32 edit control without
    /// changing focus, selection, keyboard state, or the clipboard.
    ///
    /// `EM_GETSEL`, `WM_GETTEXTLENGTH`, and `WM_GETTEXT` are system messages,
    /// so USER32 marshals their pointer arguments when the target belongs to a
    /// different process. Every message uses `SendMessageTimeoutW` to keep a
    /// hung legacy window from blocking the isolated capture lane.
    fn capture_native_text_control(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        source_app: &mut Option<(u32, SourceApplication)>,
    ) -> AccessibilityCapture {
        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return AccessibilityCapture::NotFound;
        }
        let foreground = unsafe { GetForegroundWindow() };
        let process_id = window_process_id(foreground);
        if foreground.0 == ptr::null_mut()
            || process_id == 0
            || process_id == control.textlens_process_id
            || !capture_control_foreground_is_related(
                control,
                foreground,
                process_id,
                process_parents,
            )
        {
            return AccessibilityCapture::NotFound;
        }
        let identity_process_id = if control.source_process_id != 0 {
            control.source_process_id
        } else {
            process_id
        };
        let identity_window = if control.source_window.0 != ptr::null_mut() {
            control.source_window
        } else {
            foreground
        };
        let Some(application) =
            resolve_cached_source_app(source_app, identity_process_id, identity_window)
        else {
            return AccessibilityCapture::NotFound;
        };

        for window in accessible_window_candidates(foreground, request.current)
            .into_iter()
            .take(NATIVE_TEXT_CONTROL_PROBE_LIMIT)
        {
            if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                return AccessibilityCapture::NotFound;
            }
            let window_process_id = window_process_id(window);
            if window_process_id == 0
                || !process_belongs_to_capture_source(
                    window_process_id,
                    process_id,
                    process_parents,
                )
            {
                continue;
            }
            if !native_text_control_matches_request(window, request) {
                continue;
            }
            let Some(offsets) = read_native_text_control_offsets(window) else {
                continue;
            };
            if request.observation_only {
                if let Some(observation) = self.observation.borrow_mut().as_mut() {
                    observation.native.insert(window.0 as isize, offsets);
                }
                continue;
            }
            let changed = self
                .observation
                .borrow()
                .as_ref()
                .is_some_and(|observation| {
                    self.observation_matches(observation, request, control)
                        && observation
                            .native
                            .get(&(window.0 as isize))
                            .is_some_and(|previous| *previous != offsets)
                });
            if request.trigger == SelectionTrigger::DoubleClick && !changed {
                trace_selection_capture(
                    "rejected-native-range-unchanged-or-unobserved",
                    &request,
                    identity_window.0 as isize,
                    identity_process_id,
                );
                continue;
            }
            let Some(text) = read_native_text_control_selection(window) else {
                continue;
            };
            // Selection may change while bounded cross-process reads run.
            if read_native_text_control_offsets(window) != Some(offsets) {
                continue;
            }
            let mut selection = accessibility_selection_payload(
                text,
                None,
                application.clone(),
                identity_window,
                request,
            );
            if request.trigger == SelectionTrigger::DoubleClick {
                selection.windows_selection_evidence =
                    Some(WindowsSelectionEvidence::NativeRangeChanged);
            }
            self.record_provider(window, window_process_id);
            return AccessibilityCapture::Selection(selection);
        }
        AccessibilityCapture::NotFound
    }

    fn acquire_capture_target(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        source_app: &mut Option<(u32, SourceApplication)>,
        use_focused: bool,
    ) -> CaptureTargetLookup {
        self.acquire_capture_target_at(
            request,
            control,
            process_parents,
            source_app,
            use_focused,
            request.current,
        )
    }

    fn acquire_capture_target_at(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        source_app: &mut Option<(u32, SourceApplication)>,
        use_focused: bool,
        point: RawPoint,
    ) -> CaptureTargetLookup {
        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return CaptureTargetLookup::Stop;
        }
        let foreground = unsafe { GetForegroundWindow() };
        let process_id = window_process_id(foreground);
        if foreground.0 == ptr::null_mut()
            || process_id == 0
            || process_id == control.textlens_process_id
            || !capture_control_foreground_is_related(
                control,
                foreground,
                process_id,
                process_parents,
            )
        {
            return CaptureTargetLookup::Stop;
        }
        let element = match (use_focused, request.trigger) {
            (true, _) | (_, SelectionTrigger::Keyboard | SelectionTrigger::Manual) => {
                match unsafe { self.automation.GetFocusedElement() } {
                    Ok(element) => element,
                    Err(_) => return CaptureTargetLookup::Retryable,
                }
            }
            _ => match unsafe {
                self.automation.ElementFromPoint(POINT {
                    x: point.x,
                    y: point.y,
                })
            } {
                Ok(element) => element,
                Err(_) => return CaptureTargetLookup::Retryable,
            },
        };
        self.capture_target_for_element(
            element,
            foreground,
            process_id,
            use_focused
                || matches!(
                    request.trigger,
                    SelectionTrigger::Keyboard | SelectionTrigger::Manual
                ),
            control,
            process_parents,
            source_app,
        )
    }

    /// UIA providers for several document viewers expose their TextPattern on
    /// the foreground root, while ElementFromPoint and GetFocusedElement only
    /// return a drawing surface. This mirrors the upstream selection-hook
    /// handle probe without changing application state.
    fn acquire_capture_target_from_foreground_window(
        &self,
        control: &CaptureControl,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        source_app: &mut Option<(u32, SourceApplication)>,
    ) -> CaptureTargetLookup {
        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return CaptureTargetLookup::Stop;
        }
        let foreground = unsafe { GetForegroundWindow() };
        let process_id = window_process_id(foreground);
        if foreground.0 == ptr::null_mut()
            || process_id == 0
            || process_id == control.textlens_process_id
            || !capture_control_foreground_is_related(
                control,
                foreground,
                process_id,
                process_parents,
            )
        {
            return CaptureTargetLookup::Stop;
        }
        let element = match unsafe { self.automation.ElementFromHandle(foreground) } {
            Ok(element) => element,
            Err(_) => return CaptureTargetLookup::Retryable,
        };
        self.capture_target_for_element(
            element,
            foreground,
            process_id,
            true,
            control,
            process_parents,
            source_app,
        )
    }

    fn capture_target_for_element(
        &self,
        element: IUIAutomationElement,
        foreground: HWND,
        process_id: u32,
        allow_document_range: bool,
        control: &CaptureControl,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        source_app: &mut Option<(u32, SourceApplication)>,
    ) -> CaptureTargetLookup {
        let element_process_id = match unsafe { element.CurrentProcessId() } {
            Ok(value) if value > 0 => value as u32,
            _ => return CaptureTargetLookup::Retryable,
        };
        if element_process_id == control.textlens_process_id {
            return CaptureTargetLookup::Stop;
        }
        // Chromium/Electron accessibility providers and their native child
        // HWNDs commonly run in renderer PIDs rather than the foreground
        // browser PID. Accept only descendants of that foreground process;
        // unrelated providers still fail closed.
        let provider_window = unsafe { element.CurrentNativeWindowHandle() }
            .ok()
            .filter(|window| window.0 != ptr::null_mut())
            .unwrap_or(foreground);
        let element_window_process_id = window_process_id(provider_window);
        if !uia_processes_belong_to_target(
            process_id,
            element_process_id,
            element_window_process_id,
            process_parents,
        ) {
            return CaptureTargetLookup::Retryable;
        }
        let password_state = unsafe { element.CurrentIsPassword() }
            .ok()
            .map(|value| value.as_bool());
        if password_state == Some(true) {
            return CaptureTargetLookup::Stop;
        }
        // Keep application identity anchored to the original mouse-down
        // process. Chromium/CEF and Office helpers can become foreground
        // between mouse-up and the COM query; using their executable name for
        // routing or title filtering makes a valid document selection look
        // like an unrelated renderer result.
        let identity_process_id = if control.source_process_id != 0 {
            control.source_process_id
        } else {
            process_id
        };
        let identity_window = if control.source_window.0 != ptr::null_mut() {
            control.source_window
        } else {
            foreground
        };
        let source_app = match source_app {
            Some((cached_process_id, application)) if *cached_process_id == identity_process_id => {
                application.clone()
            }
            slot => {
                let application = source_application(identity_process_id, identity_window);
                *slot = Some((identity_process_id, application.clone()));
                application
            }
        };
        CaptureTargetLookup::Found(CaptureTarget {
            element,
            process_id,
            source_window: foreground,
            provider_window,
            provider_process_id: element_process_id,
            source_app,
            allow_document_range,
        })
    }

    fn capture_target_selection(
        &self,
        target: &CaptureTarget,
        request: CaptureRequest,
        control: &CaptureControl,
    ) -> Result<AccessibilityCapture, SelectionError> {
        if request.trigger != SelectionTrigger::Manual
            && self.target_is_non_text_action(target, control)
        {
            trace_selection_capture(
                "rejected-non-text-control",
                &request,
                target.source_window.0 as isize,
                target.process_id,
            );
            return Ok(AccessibilityCapture::NotFound);
        }
        match self.capture_accessibility(
            &target.element,
            target.source_window,
            target.source_app.clone(),
            request,
            control,
            target.allow_document_range,
        )? {
            AccessibilityCapture::Selection(selection)
                if capture_context_still_valid(
                    control,
                    target.source_window,
                    target.process_id,
                ) =>
            {
                self.record_provider(target.provider_window, target.provider_process_id);
                Ok(AccessibilityCapture::Selection(selection))
            }
            AccessibilityCapture::Selection(_) => {
                // A transient foreground/renderer transition should make this
                // probe stale, not make the whole gesture terminal. The next
                // point or focused ancestor may still expose the same range.
                Ok(AccessibilityCapture::NotFound)
            }
            other => Ok(other),
        }
    }

    fn target_is_non_text_action(&self, target: &CaptureTarget, control: &CaptureControl) -> bool {
        let walkers = [
            unsafe { self.automation.ControlViewWalker() }.ok(),
            unsafe { self.automation.RawViewWalker() }.ok(),
        ];
        let mut found_item = false;
        for walker in walkers.into_iter().flatten() {
            let mut element = target.element.clone();
            for _ in 0..MAX_UIA_ANCESTORS {
                if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                    return true;
                }
                if let Ok(kind) = unsafe { element.CurrentControlType() } {
                    if kind == UIA_EditControlTypeId || kind == UIA_DocumentControlTypeId {
                        // Editors and document content may live in a list
                        // item (file rename, chat message, HTML list). They
                        // still need an explicit selected text range below.
                        return false;
                    }
                    if kind == UIA_ButtonControlTypeId
                        || kind == UIA_CheckBoxControlTypeId
                        || kind == UIA_RadioButtonControlTypeId
                        || kind == UIA_MenuItemControlTypeId
                        || kind == UIA_TabItemControlTypeId
                    {
                        return true;
                    }
                    if kind == UIA_ListItemControlTypeId || kind == UIA_TreeItemControlTypeId {
                        // Keep looking for a containing text document. Plain
                        // shell/file items have no such text context.
                        found_item = true;
                    }
                }
                element = match unsafe { walker.GetParentElement(&element) } {
                    Ok(parent) => parent,
                    Err(_) => break,
                };
            }
        }
        found_item
    }

    /// Fast native accessibility fallback for known document/Office hosts.
    ///
    /// Cherry's Windows hook reaches `AccessibleObjectFromWindow` after UIA.
    /// Keeping the call in the isolated helper process preserves the same
    /// failure boundary as the UIA path while allowing PDF readers, Excel/PPT,
    /// and older Win32 controls to answer through `accSelection`.
    fn capture_foreground_accessible(
        &self,
        request: CaptureRequest,
        control: &CaptureControl,
        process_parents: &mut Option<Option<HashMap<u32, u32>>>,
        source_app: &mut Option<(u32, SourceApplication)>,
    ) -> AccessibilityCapture {
        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return AccessibilityCapture::NotFound;
        }
        let foreground = unsafe { GetForegroundWindow() };
        let process_id = window_process_id(foreground);
        if foreground.0 == ptr::null_mut()
            || process_id == 0
            || process_id == control.textlens_process_id
            || !capture_control_foreground_is_related(
                control,
                foreground,
                process_id,
                process_parents,
            )
        {
            return AccessibilityCapture::NotFound;
        }
        let identity_process_id = if control.source_process_id != 0 {
            control.source_process_id
        } else {
            process_id
        };
        let identity_window = if control.source_window.0 != ptr::null_mut() {
            control.source_window
        } else {
            foreground
        };
        let Some(application) =
            resolve_cached_source_app(source_app, identity_process_id, identity_window)
        else {
            return AccessibilityCapture::NotFound;
        };
        if focused_element_is_password(&self.automation) == Some(true) {
            return AccessibilityCapture::Protected;
        }
        // `AccessibleObjectFromPoint` is the most useful MSAA entry point for
        // canvas controls: the returned child often carries the live
        // selection even when the HWND itself exposes only a frame object.
        // Probe the release point first and the press point second so a
        // scrollbar/overlay at mouse-up does not hide a valid range.
        for point in accessible_point_candidates(request)
            .into_iter()
            .take(ACCESSIBLE_POINT_PROBE_LIMIT)
        {
            let point_window = window_at_point(point);
            let point_process_id = window_process_id(point_window);
            if point_process_id == 0
                || !process_belongs_to_capture_source(point_process_id, process_id, process_parents)
            {
                continue;
            }
            match self.capture_accessible_point(
                point,
                identity_window,
                application.clone(),
                request,
            ) {
                AccessibilityCapture::Selection(selection) => {
                    self.record_provider(point_window, point_process_id);
                    return AccessibilityCapture::Selection(selection);
                }
                AccessibilityCapture::Protected => return AccessibilityCapture::Protected,
                AccessibilityCapture::NotFound => {}
            }
        }

        let probe_limit = if pdf_accessibility_first_application(&application.bundle_id) {
            PDF_ACCESSIBLE_WINDOW_PROBE_LIMIT
        } else {
            ACCESSIBLE_WINDOW_PROBE_LIMIT
        };
        for accessible_window in accessible_window_candidates(foreground, request.current)
            .into_iter()
            .take(probe_limit)
        {
            if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                return AccessibilityCapture::NotFound;
            }
            // The point window can be a renderer child or an unrelated overlay.
            // Keep this native fallback scoped to the foreground process; UIA
            // remains responsible for validated multi-process providers.
            if !process_belongs_to_capture_source(
                window_process_id(accessible_window),
                process_id,
                process_parents,
            ) {
                continue;
            }
            match self.capture_accessible_window(
                accessible_window,
                identity_window,
                application.clone(),
                request,
            ) {
                AccessibilityCapture::Selection(selection) => {
                    self.record_provider(accessible_window, window_process_id(accessible_window));
                    return AccessibilityCapture::Selection(selection);
                }
                AccessibilityCapture::Protected => return AccessibilityCapture::Protected,
                AccessibilityCapture::NotFound => {}
            }
        }
        AccessibilityCapture::NotFound
    }

    fn capture_accessible_point(
        &self,
        point: RawPoint,
        source_window: HWND,
        source_app: SourceApplication,
        request: CaptureRequest,
    ) -> AccessibilityCapture {
        let mut accessible = None;
        let mut child = VARIANT::default();
        if unsafe {
            AccessibleObjectFromPoint(
                POINT {
                    x: point.x,
                    y: point.y,
                },
                &mut accessible,
                &mut child,
            )
        }
        .is_err()
        {
            return AccessibilityCapture::NotFound;
        }
        let Some(accessible) = accessible else {
            return AccessibilityCapture::NotFound;
        };
        let child = OwnedVariant(child);

        // Prefer an explicit accSelection from the point provider or one of
        // its immediate MSAA containers. Office and custom document surfaces
        // often expose the hit-tested text as a child while the live selected
        // range belongs to the parent canvas.
        if let Some((text, bounds)) = accessible_selection_data_from_point_ancestors(
            &accessible,
            request.trigger == SelectionTrigger::Manual,
        ) {
            let selection = accessibility_selection_payload(
                text,
                bounds,
                source_app.clone(),
                source_window,
                request,
            );
            if legacy_accessibility_selection_is_plausible(&selection, source_window) {
                return AccessibilityCapture::Selection(selection);
            }
        }

        // Some providers return the selected object as VT_UNKNOWN/VT_DISPATCH
        // in the child VARIANT and expose accValue only on that object.
        if let Some(child_accessible) = accessible_from_variant(&child.0) {
            if let Some((text, bounds)) = accessible_selection_data(
                &child_accessible,
                request.trigger == SelectionTrigger::Manual,
            ) {
                let selection = accessibility_selection_payload(
                    text,
                    bounds,
                    source_app.clone(),
                    source_window,
                    request,
                );
                if legacy_accessibility_selection_is_plausible(&selection, source_window) {
                    return AccessibilityCapture::Selection(selection);
                }
            }
        }

        if request.trigger != SelectionTrigger::Manual {
            return AccessibilityCapture::NotFound;
        }
        // Last-resort point fallback for explicit captures. It
        // is deliberately geometry-gated: without a child rectangle, a
        // window-level accName/accValue is too easy to mistake for selected
        // text. The normal UIA/MSAA selection paths always run first.
        let Some((text, Some(bounds))) = accessible_object_data(&accessible, &child.0) else {
            return AccessibilityCapture::NotFound;
        };
        if !selection_point_near_bounds(raw_selection_point(point), bounds) {
            return AccessibilityCapture::NotFound;
        }
        let selection =
            accessibility_selection_payload(text, Some(bounds), source_app, source_window, request);
        if legacy_accessibility_selection_is_plausible(&selection, source_window) {
            AccessibilityCapture::Selection(selection)
        } else {
            AccessibilityCapture::NotFound
        }
    }

    fn capture_accessible_window(
        &self,
        accessible_window: HWND,
        source_window: HWND,
        source_app: SourceApplication,
        request: CaptureRequest,
    ) -> AccessibilityCapture {
        // Native controls disagree on whether their selection is attached to
        // OBJID_CLIENT or OBJID_WINDOW. Query both object identities, but stop
        // at the first validated selected range.
        for object_id in [OBJID_CLIENT, OBJID_WINDOW] {
            let mut raw_accessible = ptr::null_mut();
            if unsafe {
                AccessibleObjectFromWindow(
                    accessible_window,
                    object_id.0 as u32,
                    &IAccessible::IID,
                    &mut raw_accessible,
                )
            }
            .is_err()
                || raw_accessible.is_null()
            {
                continue;
            }

            let accessible = unsafe { IAccessible::from_raw(raw_accessible) };
            let Some((text, bounds)) =
                accessible_selection_data(&accessible, request.trigger == SelectionTrigger::Manual)
            else {
                continue;
            };
            let selection = accessibility_selection_payload(
                text,
                bounds,
                source_app.clone(),
                source_window,
                request,
            );
            if legacy_accessibility_selection_is_plausible(&selection, source_window) {
                return AccessibilityCapture::Selection(selection);
            }
        }
        AccessibilityCapture::NotFound
    }

    fn capture_accessibility(
        &self,
        focused: &IUIAutomationElement,
        source_window: HWND,
        source_app: SourceApplication,
        request: CaptureRequest,
        control: &CaptureControl,
        allow_document_range: bool,
    ) -> Result<AccessibilityCapture, SelectionError> {
        let patterns =
            match self.find_text_pattern(focused, control, &source_app, source_window, request) {
                TextPatternSearch::Found(patterns) => patterns,
                TextPatternSearch::Protected => return Ok(AccessibilityCapture::Protected),
                TextPatternSearch::NotFound => {
                    // A number of Win32, Java, and Office controls expose their
                    // current selection through the legacy accessibility pattern
                    // but do not implement UIA TextPattern. Keep this fallback
                    // bounded and only enter it after the fast TextPattern path.
                    return self.capture_legacy_or_document_range(
                        focused,
                        source_window,
                        source_app,
                        request,
                        control,
                        allow_document_range,
                    );
                }
            };
        if request.trigger == SelectionTrigger::DoubleClick {
            return Ok(first_double_click_candidate(
                patterns,
                |(pattern, owner)| {
                    self.capture_double_click_text_range(
                        &pattern,
                        &owner,
                        source_window,
                        source_app.clone(),
                        request,
                        control,
                    )
                },
            ));
        }
        let Some((pattern, _owner)) = patterns.into_iter().next() else {
            return Ok(AccessibilityCapture::NotFound);
        };
        let ranges = match unsafe { pattern.GetSelection() } {
            Ok(ranges) => ranges,
            Err(_) => {
                return self.capture_legacy_or_document_range(
                    focused,
                    source_window,
                    source_app,
                    request,
                    control,
                    allow_document_range,
                );
            }
        };
        let range_count = unsafe { ranges.Length() }.unwrap_or(0);
        if range_count <= 0 || range_count > MAX_UIA_SELECTION_RANGES {
            return self.capture_legacy_or_document_range(
                focused,
                source_window,
                source_app,
                request,
                control,
                allow_document_range,
            );
        }

        let mut texts = Vec::new();
        let mut total_text_chars = 0usize;
        let mut total_bounding_values = 0usize;
        let mut rectangles = Vec::new();
        let mut bounds_valid = true;
        for index in 0..range_count {
            if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                return Ok(AccessibilityCapture::NotFound);
            }
            let Ok(range) = (unsafe { ranges.GetElement(index) }) else {
                continue;
            };
            let separator_chars = usize::from(!texts.is_empty());
            let Some((remaining, request_limit)) =
                windows_text_budget(total_text_chars, separator_chars != 0)
            else {
                return Ok(AccessibilityCapture::NotFound);
            };
            if let Ok(text) = unsafe { range.GetText(request_limit) } {
                let text = text.to_string();
                if !text.trim().is_empty() {
                    let text_chars = text.chars().count();
                    if text_chars > remaining {
                        return Ok(AccessibilityCapture::NotFound);
                    }
                    total_text_chars += separator_chars + text_chars;
                    texts.push(text);
                }
            }
            if bounds_valid {
                match bounding_rectangle_values(&range) {
                    Ok(values) => {
                        let next_values = total_bounding_values.checked_add(values.len());
                        if next_values.is_some_and(|total| total <= MAX_TOTAL_BOUNDING_VALUES) {
                            total_bounding_values = next_values.unwrap_or_default();
                            rectangles.extend(rectangles_from_values(&values));
                        } else {
                            bounds_valid = false;
                            rectangles.clear();
                        }
                    }
                    Err(_) => {
                        // Text is still useful when a provider returns broken,
                        // enormous or temporarily unavailable rectangles. The
                        // toolbar falls back to the physical mouse-up point.
                        bounds_valid = false;
                        rectangles.clear();
                    }
                }
            }
        }
        if texts.is_empty() {
            return self.capture_legacy_or_document_range(
                focused,
                source_window,
                source_app,
                request,
                control,
                allow_document_range,
            );
        }
        let text = normalize_captured_text(&texts.join("\n"));

        let mouse_start = request.start.map(raw_selection_point);
        let mouse_end = request.end.map(raw_selection_point);
        let mouse_current = raw_selection_point(request.current);
        let direction = direction_from_points(mouse_start, mouse_end);
        let bounds = union_selection_bounds(&rectangles);
        let (start_top, start_bottom, end_top, end_bottom) =
            endpoint_points(&rectangles, direction);

        let selection = SelectionPayload {
            text,
            source_app: source_app.clone(),
            bounds,
            start_top,
            start_bottom,
            end_top,
            end_bottom,
            mouse: SelectionMouse {
                start: mouse_start,
                end: mouse_end,
                current: mouse_current,
            },
            direction,
            is_fullscreen: window_is_fullscreen(source_window),
            method: SelectionMethod::Accessibility,
            trigger: request.trigger,
            timestamp_ms: timestamp_ms(),
            sequence: 0,
            windows_selection_evidence: None,
        };
        // Word and some document providers expose the window/document title
        // through TextPattern.GetSelection even when a real range exists on a
        // higher ancestor. Reject that identity value here so the caller can
        // continue to the next provider or the clipboard fallback.
        if !accessibility_selection_payload_is_plausible(&selection, source_window) {
            trace_selection_capture(
                "uia-candidate-rejected-untrusted-text",
                &request,
                source_window.0 as isize,
                window_process_id(source_window),
            );
            return self.capture_legacy_or_document_range(
                focused,
                source_window,
                source_app,
                request,
                control,
                allow_document_range,
            );
        }
        Ok(AccessibilityCapture::Selection(selection))
    }

    fn capture_double_click_text_range(
        &self,
        pattern: &IUIAutomationTextPattern,
        owner: &IUIAutomationElement,
        source_window: HWND,
        source_app: SourceApplication,
        request: CaptureRequest,
        control: &CaptureControl,
    ) -> AccessibilityCapture {
        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return AccessibilityCapture::NotFound;
        }
        let Ok(ranges) = (unsafe { pattern.GetSelection() }) else {
            return AccessibilityCapture::NotFound;
        };
        let count = unsafe { ranges.Length() }.unwrap_or(0);
        if count <= 0 || count > MAX_UIA_SELECTION_RANGES {
            return AccessibilityCapture::NotFound;
        }
        for index in 0..count {
            if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                return AccessibilityCapture::NotFound;
            }
            let Ok(range) = (unsafe { ranges.GetElement(index) }) else {
                continue;
            };
            let Some((remaining, limit)) = windows_text_budget(0, false) else {
                return AccessibilityCapture::NotFound;
            };
            let Ok(text) = (unsafe { range.GetText(limit) }) else {
                continue;
            };
            let text = normalize_captured_text(&text.to_string());
            if text.chars().count() > remaining {
                continue;
            }
            let rectangles = bounding_rectangle_values(&range)
                .ok()
                .map(|values| rectangles_from_values(&values))
                .unwrap_or_default();
            let at_point = windows_double_click_matches_text_range(
                &text,
                &rectangles,
                raw_selection_point(request.current),
            );
            // A no-geometry provider is accepted only after the same provider
            // changed its explicit selection since this gesture's first click.
            // RangeFromPoint alone is deliberately insufficient: it snaps to
            // the nearest text even when the pointer is on blank space.
            let changed = rectangles.is_empty()
                && self.uia_selection_changed(owner, &range, request, control);
            if !at_point && !changed {
                trace_selection_capture(
                    if rectangles.is_empty() {
                        "rejected-uia-range-unobserved-or-unchanged"
                    } else {
                        "rejected-pointer-outside-text"
                    },
                    &request,
                    source_window.0 as isize,
                    control.source_process_id,
                );
                continue;
            }
            let mut selection = accessibility_selection_payload(
                text,
                union_selection_bounds(&rectangles),
                source_app.clone(),
                source_window,
                request,
            );
            selection.windows_selection_evidence = Some(if at_point {
                WindowsSelectionEvidence::UiaRangeAtPoint
            } else {
                WindowsSelectionEvidence::UiaRangeChanged
            });
            (
                selection.start_top,
                selection.start_bottom,
                selection.end_top,
                selection.end_bottom,
            ) = endpoint_points(&rectangles, selection.direction);
            if selection.has_verified_windows_double_click()
                && accessibility_selection_payload_is_plausible(&selection, source_window)
            {
                return AccessibilityCapture::Selection(selection);
            }
        }
        AccessibilityCapture::NotFound
    }

    /// Resolve explicit selection providers before consulting DocumentRange.
    /// A few PDF viewers expose a non-empty document range even while their
    /// selection provider is temporarily empty; treating that range as the
    /// user's selection can yield a filename or the whole document. The
    /// document-range fallback is therefore the final bounded option.
    fn capture_legacy_or_document_range(
        &self,
        focused: &IUIAutomationElement,
        source_window: HWND,
        source_app: SourceApplication,
        request: CaptureRequest,
        control: &CaptureControl,
        allow_document_range: bool,
    ) -> Result<AccessibilityCapture, SelectionError> {
        if request.trigger == SelectionTrigger::DoubleClick {
            return Ok(AccessibilityCapture::NotFound);
        }
        let legacy = self.capture_legacy_accessibility(
            focused,
            source_window,
            source_app.clone(),
            request,
            control,
        );
        if !matches!(legacy, AccessibilityCapture::NotFound) || !allow_document_range {
            return Ok(legacy);
        }
        self.capture_direct_document_range(focused, source_window, &source_app, request, control)
    }

    fn capture_direct_document_range(
        &self,
        focused: &IUIAutomationElement,
        source_window: HWND,
        source_app: &SourceApplication,
        request: CaptureRequest,
        control: &CaptureControl,
    ) -> Result<AccessibilityCapture, SelectionError> {
        // DocumentRange represents a document, not an explicit selection.
        // Its active-end attribute alone cannot authorize automatic capture.
        if request.trigger != SelectionTrigger::Manual
            || Instant::now() >= self.deadline.get()
            || control.is_cancelled()
        {
            return Ok(AccessibilityCapture::NotFound);
        }
        if unsafe { focused.CurrentIsPassword() }
            .ok()
            .is_some_and(|value| value.as_bool())
        {
            return Ok(AccessibilityCapture::Protected);
        }
        let pattern = match unsafe {
            focused.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        } {
            Ok(pattern) => pattern,
            Err(_) => return Ok(AccessibilityCapture::NotFound),
        };

        // Leave normal explicit selections to the main TextPattern path. The
        // document-range branch exists specifically for providers that hide
        // their selected range from GetSelection.
        // A provider may return a non-empty range array whose ranges contain
        // no text yet (the selection is committed asynchronously). Only a
        // genuinely non-empty selection should suppress the DocumentRange
        // fallback; checking Length alone made PDF/Office providers disappear
        // during that short transition window.
        if text_pattern_has_nonempty_selection(&pattern, control, self.deadline.get()) {
            return Ok(AccessibilityCapture::NotFound);
        }
        let range = match unsafe { pattern.DocumentRange() } {
            Ok(range) => range,
            Err(_) => return Ok(AccessibilityCapture::NotFound),
        };
        let selection_active =
            unsafe { range.GetAttributeValue(UIA_SelectionActiveEndAttributeId) }
                .ok()
                .map(OwnedVariant)
                .is_some_and(|value| variant_selection_active_end_is_set(&value.0));
        if !selection_active || Instant::now() >= self.deadline.get() || control.is_cancelled() {
            return Ok(AccessibilityCapture::NotFound);
        }

        let Some((remaining, request_limit)) = windows_text_budget(0, false) else {
            return Ok(AccessibilityCapture::NotFound);
        };
        let text = match unsafe { range.GetText(request_limit) } {
            Ok(text) => normalize_captured_text(&text.to_string()),
            Err(_) => return Ok(AccessibilityCapture::NotFound),
        };
        let text_chars = text.chars().count();
        if text.trim().is_empty()
            || text_chars > remaining
            || (!matches!(
                request.trigger,
                SelectionTrigger::Keyboard | SelectionTrigger::Manual
            ) && text_chars > DOCUMENT_RANGE_FALLBACK_MAX_CHARS)
        {
            return Ok(AccessibilityCapture::NotFound);
        }
        let bounds = if Instant::now() >= self.deadline.get() || control.is_cancelled() {
            None
        } else {
            bounding_rectangle_values(&range)
                .ok()
                .map(|values| rectangles_from_values(&values))
                .and_then(|rectangles| union_selection_bounds(&rectangles))
        };
        let selection = accessibility_selection_payload(
            text,
            bounds,
            source_app.clone(),
            source_window,
            request,
        );
        if document_range_selection_is_plausible(&selection, request, source_window) {
            Ok(AccessibilityCapture::Selection(selection))
        } else {
            Ok(AccessibilityCapture::NotFound)
        }
    }

    fn capture_legacy_accessibility(
        &self,
        focused: &IUIAutomationElement,
        source_window: HWND,
        source_app: SourceApplication,
        request: CaptureRequest,
        control: &CaptureControl,
    ) -> AccessibilityCapture {
        let walkers = [
            unsafe { self.automation.ControlViewWalker() }.ok(),
            unsafe { self.automation.RawViewWalker() }.ok(),
        ];
        for walker in walkers.into_iter().flatten() {
            let mut element = focused.clone();
            for _ in 0..MAX_UIA_ANCESTORS {
                if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                    return AccessibilityCapture::NotFound;
                }
                let pattern = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                        UIA_LegacyIAccessiblePatternId,
                    )
                };
                if let Ok(pattern) = pattern {
                    // The native IAccessible bridge is what Cherry uses for
                    // legacy Office controls. Prefer it before the UIA
                    // element-array interpretation because it preserves the
                    // host's selected child/value semantics.
                    if let Ok(accessible) = unsafe { pattern.GetIAccessible() } {
                        if let Some((text, bounds)) = accessible_selection_data(
                            &accessible,
                            request.trigger == SelectionTrigger::Manual,
                        ) {
                            let selection = accessibility_selection_payload(
                                text,
                                bounds,
                                source_app.clone(),
                                source_window,
                                request,
                            );
                            if legacy_accessibility_selection_is_plausible(
                                &selection,
                                source_window,
                            ) {
                                return AccessibilityCapture::Selection(selection);
                            }
                        }
                    }
                    if request.trigger != SelectionTrigger::Manual {
                        element = match unsafe { walker.GetParentElement(&element) } {
                            Ok(parent) => parent,
                            Err(_) => break,
                        };
                        continue;
                    }
                    let Ok(selected) = (unsafe { pattern.GetCurrentSelection() }) else {
                        element = match unsafe { walker.GetParentElement(&element) } {
                            Ok(parent) => parent,
                            Err(_) => break,
                        };
                        continue;
                    };
                    let count = unsafe { selected.Length() }.unwrap_or(0);
                    if count <= 0 || count > MAX_UIA_SELECTION_RANGES {
                        element = match unsafe { walker.GetParentElement(&element) } {
                            Ok(parent) => parent,
                            Err(_) => break,
                        };
                        continue;
                    }

                    let mut texts = Vec::new();
                    let mut rectangles = Vec::new();
                    for index in 0..count {
                        if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                            return AccessibilityCapture::NotFound;
                        }
                        let Ok(selected_element) = (unsafe { selected.GetElement(index) }) else {
                            continue;
                        };
                        if unsafe { selected_element.CurrentIsPassword() }
                            .ok()
                            .is_some_and(|value| value.as_bool())
                        {
                            return AccessibilityCapture::Protected;
                        }

                        // CurrentValue belongs to the LegacyIAccessible pattern,
                        // not IUIAutomationElement. Office grids commonly expose
                        // the cell text through that pattern while CurrentName is
                        // the useful fallback for controls that only expose a name.
                        let value = unsafe {
                            selected_element
                                .GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                                    UIA_LegacyIAccessiblePatternId,
                                )
                        }
                        .ok()
                        .and_then(|pattern| unsafe { pattern.CurrentValue() }.ok())
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                        let name = unsafe { selected_element.CurrentName() }
                            .ok()
                            .map(|name| name.to_string())
                            .unwrap_or_default();
                        let text = if !value.trim().is_empty() {
                            value
                        } else {
                            name
                        };
                        if !text.trim().is_empty() {
                            texts.push(text);
                        }

                        if let Ok(rectangle) =
                            unsafe { selected_element.CurrentBoundingRectangle() }
                        {
                            let rectangle = SelectionBounds {
                                x: f64::from(rectangle.left),
                                y: f64::from(rectangle.top),
                                width: f64::from(rectangle.right - rectangle.left),
                                height: f64::from(rectangle.bottom - rectangle.top),
                            };
                            if selection_bounds_are_reasonable(rectangle) {
                                rectangles.push(rectangle);
                            }
                        }
                    }
                    if !texts.is_empty() {
                        let text = normalize_captured_text(&texts.join("\n"));
                        let mouse_start = request.start.map(raw_selection_point);
                        let mouse_end = request.end.map(raw_selection_point);
                        let mouse_current = raw_selection_point(request.current);
                        let direction = direction_from_points(mouse_start, mouse_end);
                        let bounds = union_selection_bounds(&rectangles);
                        let (start_top, start_bottom, end_top, end_bottom) =
                            endpoint_points(&rectangles, direction);
                        let selection = SelectionPayload {
                            text,
                            source_app: source_app.clone(),
                            bounds,
                            start_top,
                            start_bottom,
                            end_top,
                            end_bottom,
                            mouse: SelectionMouse {
                                start: mouse_start,
                                end: mouse_end,
                                current: mouse_current,
                            },
                            direction,
                            is_fullscreen: window_is_fullscreen(source_window),
                            method: SelectionMethod::Accessibility,
                            trigger: request.trigger,
                            timestamp_ms: timestamp_ms(),
                            sequence: 0,
                            windows_selection_evidence: None,
                        };
                        if legacy_accessibility_selection_is_plausible(&selection, source_window) {
                            return AccessibilityCapture::Selection(selection);
                        }
                    }
                }
                element = match unsafe { walker.GetParentElement(&element) } {
                    Ok(parent) => parent,
                    Err(_) => break,
                };
            }
        }
        AccessibilityCapture::NotFound
    }

    fn find_text_pattern(
        &self,
        focused: &IUIAutomationElement,
        control: &CaptureControl,
        source_app: &SourceApplication,
        source_window: HWND,
        request: CaptureRequest,
    ) -> TextPatternSearch {
        let walkers = [
            unsafe { self.automation.ControlViewWalker() }.ok(),
            unsafe { self.automation.RawViewWalker() }.ok(),
        ];
        let mut visited_runtime_ids = HashSet::new();
        let mut visited_without_runtime_id = Vec::<IUIAutomationElement>::new();
        let mut candidates = Vec::new();
        for walker in walkers.into_iter().flatten() {
            let mut element = focused.clone();
            for _ in 0..MAX_UIA_ANCESTORS {
                if Instant::now() >= self.deadline.get() || control.is_cancelled() {
                    return TextPatternSearch::NotFound;
                }
                // Runtime IDs turn the Control/Raw View de-duplication into a
                // constant-time local lookup. CompareElements remains only as
                // a fail-safe for providers which do not expose a runtime ID;
                // the old all-pairs COM comparison became very slow on deep
                // Chromium accessibility trees.
                let runtime_id = element_runtime_id(&element);
                let duplicate = runtime_id.as_ref().map_or_else(
                    || {
                        visited_without_runtime_id.iter().any(|previous| {
                            unsafe { self.automation.CompareElements(previous, &element) }
                                .ok()
                                .is_some_and(|same| same.as_bool())
                        })
                    },
                    |runtime_id| !visited_runtime_ids.insert(runtime_id.clone()),
                );
                if !duplicate {
                    // An explicit password node blocks every capture path.
                    // Some Chromium/WebView2 structural ancestors do not
                    // expose this optional property; that must not prevent a
                    // safe UIA TextPattern read from a known non-password leaf.
                    match unsafe { element.CurrentIsPassword() }
                        .ok()
                        .map(|value| value.as_bool())
                    {
                        Some(true) => return TextPatternSearch::Protected,
                        Some(false) | None => {}
                    }
                    if let Ok(pattern) = unsafe {
                        element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                    } {
                        // A leaf can advertise TextPattern while returning an
                        // empty selection; its parent document/provider may
                        // still own the live range. Verify the pattern before
                        // stopping the ancestor walk so those hosts reach the
                        // actual selected-text provider.
                        let has_selection = text_pattern_has_nonempty_selection(
                            &pattern,
                            control,
                            self.deadline.get(),
                        );
                        if has_selection
                            && !text_pattern_candidate_is_host_identity(
                                &pattern,
                                source_app,
                                source_window,
                                self.deadline.get(),
                            )
                        {
                            let is_document =
                                unsafe { element.CurrentControlType() }.ok().is_some_and(
                                    |control_type| control_type == UIA_DocumentControlTypeId,
                                );
                            candidates.push(((pattern, element.clone()), is_document));
                        }
                    }
                    if runtime_id.is_none() {
                        visited_without_runtime_id.push(element.clone());
                    }
                }
                element = match unsafe { walker.GetParentElement(&element) } {
                    Ok(parent) => parent,
                    Err(_) => break,
                };
            }
        }
        let candidates = order_text_pattern_candidates(
            candidates,
            request.trigger == SelectionTrigger::DoubleClick,
        );
        if candidates.is_empty() {
            TextPatternSearch::NotFound
        } else {
            TextPatternSearch::Found(candidates)
        }
    }
}

fn snapshot_uia_selection(
    pattern: &IUIAutomationTextPattern,
    control: &CaptureControl,
    deadline: Instant,
) -> Option<Vec<IUIAutomationTextRange>> {
    let ranges = unsafe { pattern.GetSelection() }.ok()?;
    let count = unsafe { ranges.Length() }.ok()?;
    if !(0..=16).contains(&count) {
        return None;
    }
    let mut snapshot = Vec::new();
    for index in 0..count {
        if control.is_cancelled() || Instant::now() >= deadline {
            return None;
        }
        let range = unsafe { ranges.GetElement(index) }.ok()?;
        snapshot.push(unsafe { range.Clone() }.ok()?);
    }
    (!control.is_cancelled() && Instant::now() < deadline).then_some(snapshot)
}

/// Verify that a TextPattern selection actually contains text before treating
/// the element as the owner. Some PDF/Office/Chromium child nodes publish an
/// empty range that would otherwise stop the ancestor search at a decoration.
/// The probe is deliberately small and bounded; full text is read only after
/// this function chooses the provider.
fn text_pattern_has_nonempty_selection(
    pattern: &IUIAutomationTextPattern,
    control: &CaptureControl,
    deadline: Instant,
) -> bool {
    let ranges = match unsafe { pattern.GetSelection() } {
        Ok(ranges) => ranges,
        Err(_) => return false,
    };
    let range_count = unsafe { ranges.Length() }.unwrap_or(0);
    if range_count <= 0 || range_count > MAX_UIA_SELECTION_RANGES {
        return false;
    }
    for index in 0..range_count.min(4) {
        if Instant::now() >= deadline || control.is_cancelled() {
            return false;
        }
        let Ok(range) = (unsafe { ranges.GetElement(index) }) else {
            continue;
        };
        if unsafe { range.GetText(UIA_SELECTION_TEXT_PROBE_LIMIT) }
            .ok()
            .is_some_and(|text| !text.to_string().trim().is_empty())
        {
            return true;
        }
    }
    false
}

fn text_pattern_candidate_is_host_identity(
    pattern: &IUIAutomationTextPattern,
    source_app: &SourceApplication,
    source_window: HWND,
    deadline: Instant,
) -> bool {
    let Some(text) = text_pattern_candidate_text(pattern, deadline) else {
        return true;
    };
    if selection_text_matches_source_identity(&text, source_app, source_window)
        || selection_text_matches_known_host_label(&text, source_app)
        || accessibility_text_looks_like_document_chrome(&text, source_app)
    {
        return true;
    }
    false
}

fn text_pattern_candidate_text(
    pattern: &IUIAutomationTextPattern,
    deadline: Instant,
) -> Option<String> {
    let ranges = unsafe { pattern.GetSelection() }.ok()?;
    let range_count = unsafe { ranges.Length() }.ok()?;
    if range_count <= 0 || range_count > MAX_UIA_SELECTION_RANGES {
        return None;
    }
    for index in 0..range_count.min(4) {
        if Instant::now() >= deadline {
            return None;
        }
        let range = unsafe { ranges.GetElement(index) }.ok()?;
        let text = unsafe { range.GetText(UIA_SELECTION_TEXT_PROBE_LIMIT) }
            .ok()?
            .to_string();
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

fn accessibility_selection_payload(
    text: String,
    bounds: Option<SelectionBounds>,
    source_app: SourceApplication,
    source_window: HWND,
    request: CaptureRequest,
) -> SelectionPayload {
    let text = normalize_captured_text(&text);
    let mouse_start = request.start.map(raw_selection_point);
    let mouse_end = request.end.map(raw_selection_point);
    let mouse_current = raw_selection_point(request.current);
    let direction = direction_from_points(mouse_start, mouse_end);
    let rectangles = bounds.into_iter().collect::<Vec<_>>();
    let bounds = union_selection_bounds(&rectangles);
    let (start_top, start_bottom, end_top, end_bottom) = endpoint_points(&rectangles, direction);

    SelectionPayload {
        text,
        source_app,
        bounds,
        start_top,
        start_bottom,
        end_top,
        end_bottom,
        mouse: SelectionMouse {
            start: mouse_start,
            end: mouse_end,
            current: mouse_current,
        },
        direction,
        is_fullscreen: window_is_fullscreen(source_window),
        method: SelectionMethod::Accessibility,
        trigger: request.trigger,
        timestamp_ms: timestamp_ms(),
        sequence: 0,
        windows_selection_evidence: None,
    }
}

/// Reject accessibility values that describe the host window rather than the
/// selected range. This is particularly important for Word/PDF providers:
/// their legacy `accName` and UIA `CurrentName` can be the document filename
/// or the application title even when no text range is exposed.
fn accessibility_selection_payload_is_plausible(
    selection: &SelectionPayload,
    source_window: HWND,
) -> bool {
    if !captured_text_is_usable(&selection.text) {
        return false;
    }
    if selection_text_matches_source_identity(&selection.text, &selection.source_app, source_window)
    {
        return false;
    }
    // Legacy/MSAA document providers occasionally return the application
    // label (for example "Microsoft Word" or "Adobe Acrobat") as an
    // accSelection value. A genuine short selection can happen to have the
    // same text, so apply this extra filter only when the provider gave us no
    // range geometry to corroborate it.
    if selection_text_matches_known_host_label(&selection.text, &selection.source_app)
        && (selection.bounds.is_none()
            || pdf_accessibility_first_application(&selection.source_app.bundle_id))
    {
        return false;
    }
    if accessibility_text_looks_like_document_chrome(&selection.text, &selection.source_app) {
        return false;
    }

    if let Some(bounds) = selection.bounds {
        let end = selection.mouse.end.unwrap_or(selection.mouse.current);
        let geometry_matches = selection_point_near_bounds(end, bounds)
            || selection_point_near_bounds(selection.mouse.current, bounds);
        if !geometry_matches {
            // Geometry is advisory for MSAA. PDF page coordinates, DPI
            // virtualization, and Office child windows frequently return a
            // valid selected string with a rectangle in a different coordinate
            // space. `deliver_captured_selection` will discard only the bad
            // rectangle and retain the text at the physical release point.
        }
    }

    // `accSelection`/TextPattern are explicit selected-text APIs. Host
    // title/app-name values were rejected above; a mismatched rectangle must
    // not turn an otherwise valid text capture into an empty result.
    true
}

/// Reject values that cannot be a usable text selection before they can block
/// a later provider or clipboard fallback. Normalization removes ordinary
/// control separators; replacement characters indicate that a provider or a
/// legacy clipboard decoder already lost source text and must not surface as a
/// translated result.
fn captured_text_is_usable(text: &str) -> bool {
    !text.trim().is_empty()
        && !text.contains('\u{fffd}')
        && text
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

/// PDF canvas providers sometimes return the accessibility object's class or
/// view name from their explicit selection API. These short ASCII identifiers
/// are not user text: accepting them prevents the clipboard fallback from
/// reading the actual selected range. Keep the heuristic restricted to known
/// document hosts so ordinary editors can still select terms such as "View".
fn accessibility_text_looks_like_document_chrome(
    text: &str,
    source_app: &SourceApplication,
) -> bool {
    if !pdf_accessibility_first_application(&source_app.bundle_id) {
        return false;
    }
    let value = text.trim();
    if value.is_empty() || value.len() > 64 || !value.is_ascii() {
        return false;
    }
    let normalized = normalize_selection_identity(value);
    if matches!(
        normalized.as_str(),
        "avpageview"
            | "pageview"
            | "pdfpageview"
            | "documentview"
            | "documentcanvas"
            | "pagecanvas"
            | "rootwebarea"
            | "webview"
            | "contentpane"
    ) {
        return true;
    }
    let compact_identifier = value
        .chars()
        .all(|character| character.is_ascii_alphanumeric());
    let mixed_case = value
        .chars()
        .any(|character| character.is_ascii_uppercase())
        && value
            .chars()
            .any(|character| character.is_ascii_lowercase());
    compact_identifier
        && mixed_case
        && value.len() >= 6
        && ["view", "pane", "canvas", "control", "window", "frame"]
            .iter()
            .any(|suffix| normalized.ends_with(suffix))
}

fn document_range_selection_is_plausible(
    selection: &SelectionPayload,
    request: CaptureRequest,
    source_window: HWND,
) -> bool {
    if !accessibility_selection_payload_is_plausible(selection, source_window) {
        return false;
    }
    if matches!(
        request.trigger,
        SelectionTrigger::Keyboard | SelectionTrigger::Manual
    ) {
        return true;
    }
    let Some(bounds) = selection.bounds else {
        // A mouse-driven DocumentRange without geometry is indistinguishable
        // from a window/document label. Explicit UIA/MSAA selection APIs can
        // still return text without bounds; this stricter rule is only for
        // the ambiguous DocumentRange fallback.
        return false;
    };
    let release = request.end.unwrap_or(request.current);
    let release = raw_selection_point(release);
    let start = request.start.map(raw_selection_point);
    selection_point_near_bounds(release, bounds)
        && start.is_none_or(|point| selection_point_near_bounds(point, bounds))
}

/// MSAA does not distinguish a selected text range from a window/document
/// label when it returns an un-geometrized value. The shared identity filters
/// therefore remain mandatory, but an explicit accSelection BSTR is still
/// useful for providers that do not expose a usable MSAA rectangle.
fn legacy_accessibility_selection_is_plausible(
    selection: &SelectionPayload,
    source_window: HWND,
) -> bool {
    // `accSelection` is an explicit selection provider even when it returns a
    // bare BSTR without a child rectangle. Keep those values usable for PDF
    // readers that expose the selected text but no geometry, while the
    // identity filters above continue to reject document titles and host
    // labels.
    accessibility_selection_payload_is_plausible(selection, source_window)
}

fn selection_point_near_bounds(point: SelectionPoint, bounds: SelectionBounds) -> bool {
    point.x >= bounds.x - ACCESSIBILITY_SELECTION_POINTER_TOLERANCE
        && point.x <= bounds.x + bounds.width + ACCESSIBILITY_SELECTION_POINTER_TOLERANCE
        && point.y >= bounds.y - ACCESSIBILITY_SELECTION_POINTER_TOLERANCE
        && point.y <= bounds.y + bounds.height + ACCESSIBILITY_SELECTION_POINTER_TOLERANCE
}

fn selection_text_matches_source_identity(
    text: &str,
    source_app: &SourceApplication,
    source_window: HWND,
) -> bool {
    if text.chars().count() > 512 {
        return false;
    }
    let normalized_text = normalize_selection_identity(text);
    if normalized_text.is_empty() {
        return false;
    }

    let mut candidates = Vec::with_capacity(4);
    candidates.push(source_app.name.clone());
    candidates.push(executable_name(&source_app.bundle_id));
    if let Some(title) = window_title(source_window) {
        if window_title_component_matches(&title, &normalized_text) {
            return true;
        }
        candidates.push(title);
    }

    candidates.into_iter().any(|candidate| {
        let candidate = normalize_selection_identity(&candidate);
        !candidate.is_empty() && candidate == normalized_text
    })
}

fn selection_text_matches_known_host_label(text: &str, source_app: &SourceApplication) -> bool {
    if text.chars().count() > 96 {
        return false;
    }
    let normalized_text = normalize_selection_identity(text);
    if normalized_text.is_empty() {
        return false;
    }
    let aliases: &[&str] = match executable_name(&source_app.bundle_id).as_str() {
        "winword.exe" | "word.exe" => &["word", "microsoftword"],
        "excel.exe" => &["excel", "microsoftexcel"],
        "powerpnt.exe" | "powerpoint.exe" => &["powerpoint", "microsoftpowerpoint"],
        "acrobat.exe" | "acrord32.exe" | "acrocef.exe" | "rdrcef.exe" => &[
            "acrobat",
            "acrobatreader",
            "acrobatpro",
            "acrobatprodc",
            "adobeacrobat",
            "adobeacrobatreader",
            "adobeacrobatpro",
            "adobeacrobatprodc",
        ],
        "foxitreader.exe" | "foxitpdfreader.exe" => &["foxitreader", "foxitpdfreader"],
        "sumatrapdf.exe" => &["sumatrapdf"],
        "wps.exe" | "et.exe" | "wpp.exe" | "wpspdf.exe" => &["wps", "wpsoffice", "kingsoftoffice"],
        "soffice.exe" | "soffice.bin" | "libreoffice.exe" => &["libreoffice", "openoffice"],
        "emeditor.exe" => &["emeditor"],
        _ => &[],
    };
    aliases.iter().any(|alias| normalized_text == *alias)
}

fn normalize_selection_identity(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn window_title_component_matches(title: &str, normalized_text: &str) -> bool {
    let normalized_title = normalize_selection_identity(title);
    if normalized_title == normalized_text {
        return true;
    }
    [" - ", " | ", " -- ", " : ", " @ ", " / ", "—", "–", "｜"]
        .into_iter()
        .flat_map(|separator| title.split(separator))
        .map(|component| {
            component.trim_matches(|character: char| {
                matches!(character, '*' | '\u{2022}' | '\u{00b7}' | ' ' | '\t')
            })
        })
        .map(normalize_selection_identity)
        .any(|component| {
            !component.is_empty()
                && (component == normalized_text
                    || document_title_stem(&component) == normalized_text
                    || component == document_title_stem(normalized_text)
                    || document_title_stem(&component) == document_title_stem(normalized_text))
        })
}

/// A provider may return either `report.pdf` or `report` while the native
/// window title uses the other form. Treat common document extensions as the
/// same identity so these labels cannot masquerade as a selected word.
fn document_title_stem(value: &str) -> String {
    const EXTENSIONS: &[&str] = &[
        ".pdf", ".doc", ".docx", ".dot", ".dotx", ".rtf", ".txt", ".md", ".odt", ".wps", ".xlsx",
        ".xls", ".ppt", ".pptx",
    ];
    EXTENSIONS
        .iter()
        .find_map(|extension| value.strip_suffix(extension))
        .unwrap_or(value)
        .to_owned()
}

fn accessible_selection_data(
    accessible: &IAccessible,
    allow_object_selection: bool,
) -> Option<(String, Option<SelectionBounds>)> {
    let selection = unsafe { accessible.accSelection().ok()? };
    let selection = OwnedVariant(selection);
    if !allow_object_selection && variant_type(&selection.0) != VT_BSTR.0 {
        return None;
    }
    accessible_selection_variant_data(accessible, &selection.0)
}

fn accessible_selection_data_from_point_ancestors(
    accessible: &IAccessible,
    allow_object_selection: bool,
) -> Option<(String, Option<SelectionBounds>)> {
    let mut current = Some(accessible.clone());
    for _ in 0..MAX_ACCESSIBLE_POINT_ANCESTORS {
        let candidate = current?;
        if let Some(selection) = accessible_selection_data(&candidate, allow_object_selection) {
            return Some(selection);
        }
        current = unsafe { candidate.accParent() }
            .ok()
            .and_then(|parent| parent.cast::<IAccessible>().ok());
    }
    None
}

fn accessible_selection_variant_data(
    accessible: &IAccessible,
    selection: &VARIANT,
) -> Option<(String, Option<SelectionBounds>)> {
    let selection_type = variant_type(selection);
    match selection_type {
        // A few PDF and Office providers return the selected text directly as
        // a BSTR. It has no geometry, so the caller must still apply the
        // source-title/host-label plausibility filters before accepting it.
        value if value == VT_BSTR.0 => accessible_selection_bstr_data(selection),
        value if value == VT_DISPATCH.0 || value == VT_UNKNOWN.0 => {
            let selected = accessible_from_variant(selection)?;
            let child = variant_child_self();
            accessible_object_data(&selected, &child)
        }
        value if value == VT_I4.0 => accessible_object_data(accessible, selection),
        value if value & VT_ARRAY.0 != 0 => {
            accessible_selection_array_data(accessible, selection, value & !VT_ARRAY.0)
        }
        _ => None,
    }
}

fn accessible_selection_array_data(
    accessible: &IAccessible,
    selection: &VARIANT,
    element_type: u16,
) -> Option<(String, Option<SelectionBounds>)> {
    if element_type != VT_VARIANT.0
        && element_type != VT_I4.0
        && element_type != VT_BSTR.0
        && element_type != VT_UNKNOWN.0
        && element_type != VT_DISPATCH.0
    {
        return None;
    }
    let array = variant_array(selection)?;
    if unsafe { SafeArrayGetDim(array) } != 1 {
        return None;
    }
    let lower = unsafe { SafeArrayGetLBound(array, 1) }.ok()?;
    let upper = unsafe { SafeArrayGetUBound(array, 1) }.ok()?;
    if upper < lower {
        return None;
    }
    let length = usize::try_from(i64::from(upper) - i64::from(lower) + 1).ok()?;
    if length == 0 || length > MAX_ACCESSIBLE_SELECTION_ITEMS {
        return None;
    }

    let mut texts = Vec::new();
    let mut rectangles = Vec::new();
    for offset in 0..length {
        let index = lower.checked_add(i32::try_from(offset).ok()?)?;
        let item_data = if element_type == VT_VARIANT.0 {
            let mut item = VARIANT::default();
            unsafe { SafeArrayGetElement(array, &index, (&mut item as *mut VARIANT).cast()) }
                .ok()
                .and_then(|_| {
                    let item = OwnedVariant(item);
                    accessible_selection_variant_data(accessible, &item.0)
                })
        } else if element_type == VT_I4.0 {
            let mut child_id = 0i32;
            unsafe { SafeArrayGetElement(array, &index, (&mut child_id as *mut i32).cast()) }
                .ok()
                .and_then(|_| {
                    let child = variant_child(child_id);
                    accessible_object_data(accessible, &child)
                })
        } else if element_type == VT_BSTR.0 {
            let mut item = BSTR::default();
            unsafe { SafeArrayGetElement(array, &index, (&mut item as *mut BSTR).cast()) }
                .ok()
                .and_then(|_| {
                    let text = String::from_utf16(&item).ok()?;
                    (!text.trim().is_empty()).then_some((text, None))
                })
        } else {
            // SAFEARRAY elements of VT_UNKNOWN/VT_DISPATCH are returned as
            // owned interface pointers by SafeArrayGetElement. Convert the
            // pointer into IUnknown so its reference is released exactly once
            // when this iteration ends, then query the IAccessible surface.
            let mut raw: *mut c_void = ptr::null_mut();
            unsafe { SafeArrayGetElement(array, &index, (&mut raw as *mut *mut c_void).cast()) }
                .ok()
                .and_then(|_| {
                    if raw.is_null() {
                        return None;
                    }
                    let unknown = unsafe { IUnknown::from_raw(raw) };
                    let selected = unknown.cast::<IAccessible>().ok()?;
                    let child = variant_child_self();
                    accessible_object_data(&selected, &child)
                })
        };
        if let Some((text, bounds)) = item_data {
            texts.push(text);
            if let Some(bounds) = bounds {
                rectangles.push(bounds);
            }
        }
    }
    if texts.is_empty() {
        return None;
    }
    Some((texts.join("\n"), union_selection_bounds(&rectangles)))
}

fn accessible_selection_bstr_data(
    selection: &VARIANT,
) -> Option<(String, Option<SelectionBounds>)> {
    let bstr: &BSTR = unsafe {
        // VARIANT owns this BSTR. Borrow it through the ManuallyDrop wrapper
        // so the surrounding OwnedVariant remains responsible for VariantClear.
        &*(&selection.Anonymous.Anonymous.Anonymous.bstrVal as *const std::mem::ManuallyDrop<BSTR>
            as *const BSTR)
    };
    let text = String::from_utf16(bstr).ok()?;
    (!text.trim().is_empty()).then_some((text, None))
}

fn read_native_text_control_offsets(window: HWND) -> Option<(u32, u32)> {
    if window.0 == ptr::null_mut() || window.is_invalid() {
        return None;
    }
    let class_name = window_class_name(window)?;
    if !is_native_text_control_class(&class_name) {
        return None;
    }

    if native_text_control_is_password(window) {
        return None;
    }

    let mut start = 0u32;
    let mut end = 0u32;
    let selection_read = unsafe {
        SendMessageTimeoutW(
            window,
            EM_GETSEL_MESSAGE,
            WPARAM((&mut start as *mut u32) as usize),
            LPARAM((&mut end as *mut u32) as isize),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            NATIVE_TEXT_CONTROL_MESSAGE_TIMEOUT_MS,
            None,
        )
    };
    (selection_read.0 != 0).then_some((start, end))
}

fn read_native_text_control_selection(window: HWND) -> Option<String> {
    let (start, end) = read_native_text_control_offsets(window)?;
    if end <= start {
        return None;
    }
    let text_length = send_native_message(window, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0))?;
    let max_chars = windows_text_budget(0, false)?.0;
    if text_length == 0 || text_length > max_chars {
        return None;
    }
    let capacity = text_length.checked_add(1)?;
    let mut text = vec![0u16; capacity];
    let copied = send_native_message(
        window,
        WM_GETTEXT,
        WPARAM(capacity),
        LPARAM(text.as_mut_ptr() as isize),
    )?;
    let copied = copied.min(text_length).min(text.len().saturating_sub(1));
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    native_selection_text_from_utf16(&text, copied, start, end)
}

fn native_text_control_is_password(window: HWND) -> bool {
    if window.0 == ptr::null_mut() || window.is_invalid() {
        return false;
    }
    let Some(class_name) = window_class_name(window) else {
        return false;
    };
    if !is_native_text_control_class(&class_name) {
        return false;
    }

    // The style query is supplemented by EM_GETPASSWORDCHAR because a few
    // framework wrappers do not mirror ES_PASSWORD on the HWND that receives
    // the edit messages.
    let style = unsafe { GetWindowLongPtrW(window, GWL_STYLE) };
    style & ES_PASSWORD as isize != 0
        || send_native_message(window, EM_GETPASSWORDCHAR_MESSAGE, WPARAM(0), LPARAM(0))
            .is_some_and(|password_char| password_char != 0)
}

fn native_text_control_matches_request(window: HWND, request: CaptureRequest) -> bool {
    if matches!(
        request.trigger,
        SelectionTrigger::Keyboard | SelectionTrigger::Manual
    ) {
        return true;
    }
    let mut rectangle = RECT::default();
    if unsafe { GetWindowRect(window, &mut rectangle) }.is_err() {
        return false;
    }
    let tolerance = NATIVE_TEXT_CONTROL_POINTER_TOLERANCE;
    request.current.x >= rectangle.left.saturating_sub(tolerance)
        && request.current.x <= rectangle.right.saturating_add(tolerance)
        && request.current.y >= rectangle.top.saturating_sub(tolerance)
        && request.current.y <= rectangle.bottom.saturating_add(tolerance)
}

fn native_selection_text_from_utf16(
    text: &[u16],
    copied: usize,
    start: usize,
    end: usize,
) -> Option<String> {
    let copied = copied.min(text.len());
    if start >= end || end > copied {
        return None;
    }
    let selected = String::from_utf16(&text[start..end]).ok()?;
    (!selected.trim().is_empty()).then_some(selected)
}

fn send_native_message(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<usize> {
    let mut result = 0usize;
    let status = unsafe {
        SendMessageTimeoutW(
            window,
            message,
            wparam,
            lparam,
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            NATIVE_TEXT_CONTROL_MESSAGE_TIMEOUT_MS,
            Some(&mut result as *mut usize),
        )
    };
    (status.0 != 0).then_some(result)
}

fn window_class_name(window: HWND) -> Option<String> {
    let mut buffer = [0u16; 128];
    let length = unsafe { GetClassNameW(window, &mut buffer) };
    (length > 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
}

fn is_native_text_control_class(class_name: &str) -> bool {
    let class_name = class_name.trim().to_ascii_lowercase();
    class_name == "edit"
        || class_name.starts_with("richedit")
        || class_name.starts_with("windowsforms10.edit")
        || class_name.starts_with("thunderrt6textbox")
        || class_name.starts_with("tedit")
        || class_name.starts_with("tmemo")
}

fn accessible_object_data(
    accessible: &IAccessible,
    child: &VARIANT,
) -> Option<(String, Option<SelectionBounds>)> {
    let name = unsafe { accessible.get_accName(child) }
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.trim().is_empty());
    let value = unsafe { accessible.get_accValue(child) }
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.trim().is_empty());
    // `accName` is often the cell/document label, while `accValue` is the
    // actual editable or selected content. Prefer the value so a Word/PDF
    // window title cannot win merely because it is populated first.
    let text = value.or(name)?;

    let mut x = 0;
    let mut y = 0;
    let mut width = 0;
    let mut height = 0;
    let bounds = unsafe { accessible.accLocation(&mut x, &mut y, &mut width, &mut height, child) }
        .ok()
        .map(|_| SelectionBounds {
            x: f64::from(x),
            y: f64::from(y),
            width: f64::from(width),
            height: f64::from(height),
        })
        .filter(|bounds| selection_bounds_are_reasonable(*bounds));

    Some((text, bounds))
}

fn variant_type(value: &VARIANT) -> u16 {
    unsafe { value.Anonymous.Anonymous.vt.0 }
}

fn variant_bool_is_true(value: &VARIANT) -> bool {
    variant_type(value) == VT_BOOL.0
        && unsafe { value.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 }
}

fn variant_selection_active_end_is_set(value: &VARIANT) -> bool {
    // UIA_SelectionActiveEndAttributeId is returned as the non-zero
    // TextRangeSelectionActiveEnd enum (Start/End). Empty, mixed, and
    // unsupported values must not turn a whole DocumentRange into a user
    // selection.
    variant_type(value) == VT_I4.0 && unsafe { value.Anonymous.Anonymous.Anonymous.lVal != 0 }
}

fn variant_array(value: &VARIANT) -> Option<*mut SAFEARRAY> {
    let array = unsafe { value.Anonymous.Anonymous.Anonymous.parray };
    (!array.is_null()).then_some(array)
}

fn variant_dispatch(value: &VARIANT) -> Option<&IDispatch> {
    let dispatch: &Option<IDispatch> = unsafe {
        &*(&value.Anonymous.Anonymous.Anonymous.pdispVal
            as *const std::mem::ManuallyDrop<Option<IDispatch>>
            as *const Option<IDispatch>)
    };
    dispatch.as_ref()
}

fn variant_unknown(value: &VARIANT) -> Option<&IUnknown> {
    let unknown: &Option<IUnknown> = unsafe {
        &*(&value.Anonymous.Anonymous.Anonymous.punkVal
            as *const std::mem::ManuallyDrop<Option<IUnknown>>
            as *const Option<IUnknown>)
    };
    unknown.as_ref()
}

fn accessible_from_variant(value: &VARIANT) -> Option<IAccessible> {
    let kind = variant_type(value);
    match kind {
        kind if kind == VT_DISPATCH.0 => variant_dispatch(value)?.cast().ok(),
        kind if kind == VT_UNKNOWN.0 => variant_unknown(value)?.cast().ok(),
        _ => None,
    }
}

fn variant_child_self() -> VARIANT {
    variant_child(0)
}

fn variant_child(child_id: i32) -> VARIANT {
    let mut child = VARIANT::default();
    unsafe {
        // The generated VARIANT bindings expose the payload through a
        // ManuallyDrop-backed union. Write through raw pointers instead of
        // assigning through that union, which is rejected by current Rust.
        ptr::write(ptr::addr_of_mut!((*child.Anonymous.Anonymous).vt), VT_I4);
        ptr::write(
            ptr::addr_of_mut!((*child.Anonymous.Anonymous).Anonymous.lVal),
            child_id,
        );
    }
    child
}

struct OwnedVariant(VARIANT);

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

fn capture_control_foreground_is_related(
    control: &CaptureControl,
    foreground: HWND,
    foreground_process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if foreground.0 == ptr::null_mut()
        || foreground_process_id == 0
        || control.source_process_id == 0
    {
        return false;
    }
    if foreground == control.source_window && foreground_process_id == control.source_process_id {
        return true;
    }
    // A source can legitimately hand selection ownership to a renderer child
    // or a documented companion process. Its own parent (for example
    // explorer.exe, which launches many Electron apps) must never be treated
    // as part of the source application.
    process_belongs_to_capture_source(
        foreground_process_id,
        control.source_process_id,
        process_parents,
    )
}

/// Whether `candidate_process_id` can provide a range for `source_process_id`.
/// Only descendants are accepted through ancestry. The reverse direction would
/// make a common launcher such as Explorer appear to be part of every app it
/// starts, which lets desktop UIA/MSAA values leak into application captures.
fn process_belongs_to_capture_source(
    candidate_process_id: u32,
    source_process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if candidate_process_id == 0 || source_process_id == 0 {
        return false;
    }
    if candidate_process_id == source_process_id
        || processes_share_executable(candidate_process_id, source_process_id)
    {
        return true;
    }
    process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref()
        .is_some_and(|parents| {
            process_descends_from(candidate_process_id, source_process_id, parents)
        })
}

fn process_ids_are_related_with_parents(
    left_process_id: u32,
    right_process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if left_process_id == 0 || right_process_id == 0 {
        return false;
    }
    if left_process_id == right_process_id
        || processes_share_executable(left_process_id, right_process_id)
    {
        return true;
    }
    process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref()
        .is_some_and(|parents| {
            process_descends_from(left_process_id, right_process_id, parents)
                || process_descends_from(right_process_id, left_process_id, parents)
        })
}

fn capture_context_still_valid(
    control: &CaptureControl,
    source_window: HWND,
    process_id: u32,
) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0 == ptr::null_mut() || process_id == 0 || control.is_cancelled() {
        return false;
    }
    let foreground_process_id = window_process_id(foreground);
    let mut process_parents = None;
    (foreground == source_window && foreground_process_id == process_id)
        || process_belongs_to_capture_source(
            foreground_process_id,
            control.source_process_id,
            &mut process_parents,
        )
}

fn foreground_interaction_decision(
    window_valid: bool,
    foreground_process_id: u32,
    own_process_id: u32,
    has_mouse_down: bool,
    mouse_down_on_self: bool,
    has_pending_capture: bool,
) -> ForegroundInteractionDecision {
    if !window_valid || foreground_process_id == 0 || foreground_process_id == own_process_id {
        ForegroundInteractionDecision::Ignore
    } else if has_mouse_down && !mouse_down_on_self {
        // Foreground activation is often delivered between the low-level Down
        // and Up callbacks. Preserve the gesture even when a shell host or
        // Chromium/Electron replaces the root HWND; the helper validates the
        // final foreground process and pointer target before returning text.
        ForegroundInteractionDecision::PreserveMouseGesture
    } else if has_pending_capture {
        ForegroundInteractionDecision::PreservePendingCapture
    } else {
        ForegroundInteractionDecision::Dismiss
    }
}

fn correlate_pending_capture_with_foreground(
    pending: &mut Option<PendingCapture>,
    foreground_root: isize,
    foreground_process_id: u32,
    generation: u64,
) {
    let Some(pending) = pending.as_mut() else {
        return;
    };
    if foreground_root == 0 || foreground_process_id == 0 {
        return;
    }
    // Keep the HWND/PID sampled during mouse-down as the source identity. The
    // foreground callback can point at a renderer child, an owned popup, or a
    // newly-created sibling window; replacing the original identity here
    // loses the executable and application-family information that the helper
    // needs to choose the right accessibility route.
    let source_root_window = pending.source_root_window;
    let source_process_id = pending.source_process_id;
    let source_context_missing = source_root_window == 0 || source_process_id == 0;
    if !source_context_missing
        && !capture_windows_are_related_with_parents(
            source_root_window,
            source_process_id,
            foreground_root,
            foreground_process_id,
            &mut pending.process_parents,
        )
    {
        pending.due = Instant::now() + FOREGROUND_SETTLE_RETRY;
        return;
    }
    if source_context_missing {
        pending.source_root_window = foreground_root;
        pending.source_process_id = foreground_process_id;
    }
    if pending
        .request
        .generation
        .is_none_or(|current| generation >= current)
    {
        pending.request.generation = Some(generation);
    }
    pending.due = Instant::now();
}

fn capture_windows_are_related(
    source_root_window: isize,
    source_process_id: u32,
    foreground_root_window: isize,
    foreground_process_id: u32,
) -> bool {
    let mut process_parents = None;
    capture_windows_are_related_with_parents(
        source_root_window,
        source_process_id,
        foreground_root_window,
        foreground_process_id,
        &mut process_parents,
    )
}

fn capture_windows_are_related_with_parents(
    source_root_window: isize,
    source_process_id: u32,
    foreground_root_window: isize,
    foreground_process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if source_root_window == 0 || foreground_root_window == 0 {
        return source_process_id != 0
            && foreground_process_id != 0
            && source_process_id == foreground_process_id;
    }
    if source_root_window == foreground_root_window
        || (source_process_id != 0 && source_process_id == foreground_process_id)
    {
        return true;
    }
    // WindowFromPoint and the foreground window can belong to different
    // cooperating processes even when the user is still inside one app. This
    // is common for PDF/CEF hosts and is cheaper and more reliable than
    // requiring a process-tree snapshot that may be unavailable under UIPI.
    if source_process_id != 0
        && foreground_process_id != 0
        && processes_share_executable(source_process_id, foreground_process_id)
    {
        return true;
    }
    let Some(parents) = process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref()
    else {
        return false;
    };
    process_descends_from(source_process_id, foreground_process_id, &parents)
        || process_descends_from(foreground_process_id, source_process_id, &parents)
}

fn pending_foreground_decision(
    foreground_valid: bool,
    foreground_process_id: u32,
    own_process_id: u32,
    source_related: bool,
    before_expiry: bool,
) -> PendingForegroundDecision {
    if foreground_valid && foreground_process_id != 0 && foreground_process_id != own_process_id {
        // `source_related` is useful correlation metadata, but it is not a
        // safe reason to hold a capture until expiry. Multi-process hosts can
        // legitimately expose the selected range from a renderer/child HWND
        // that is not discoverable from WindowFromPoint. The isolated helper
        // receives the original source context and performs the authoritative
        // process-tree check before returning text. A changed application is
        // therefore rejected there without imposing a 260 ms delay on valid
        // selections.
        let _ = source_related;
        PendingForegroundDecision::Capture
    } else if before_expiry {
        PendingForegroundDecision::Wait
    } else {
        PendingForegroundDecision::Expire
    }
}

/// Adaptive per-application routing: after repeated misses, keep the first
/// UIA probe but omit its delayed retry. A streak older than
/// `ADAPTIVE_ROUTE_REPROBE_INTERVAL` restores the full sequence, so an app
/// that starts exposing UIA (for example after a document pane loads) is not
/// classified forever.
fn adaptive_route_should_abbreviate_uia_retries(
    consecutive_misses: u8,
    since_probe: Duration,
) -> bool {
    consecutive_misses >= ADAPTIVE_ROUTE_MISS_THRESHOLD
        && since_probe < ADAPTIVE_ROUTE_REPROBE_INTERVAL
}

enum AdaptiveRouteUpdate {
    Clear,
    Extend,
    Leave,
}

/// Classify a completed capture for the adaptive-route cache. A genuine
/// UIA-backed selection clears the streak; anything else extends it — unless
/// this round used an abbreviated UIA sequence, which says nothing new about
/// whether the delayed retry would have worked and so must leave the existing
/// streak and its re-probe timer alone.
fn adaptive_route_update(
    retries_abbreviated: bool,
    method: Option<SelectionMethod>,
) -> AdaptiveRouteUpdate {
    match method {
        Some(SelectionMethod::Accessibility) => AdaptiveRouteUpdate::Clear,
        _ if retries_abbreviated => AdaptiveRouteUpdate::Leave,
        _ => AdaptiveRouteUpdate::Extend,
    }
}

fn resolve_cached_source_app(
    cache: &mut Option<(u32, SourceApplication)>,
    process_id: u32,
    window: HWND,
) -> Option<SourceApplication> {
    if process_id == 0 {
        return None;
    }
    match cache {
        Some((cached_process_id, application)) if *cached_process_id == process_id => {
            Some(application.clone())
        }
        slot => {
            let application = source_application(process_id, window);
            *slot = Some((process_id, application.clone()));
            Some(application)
        }
    }
}

fn focused_element_is_password(automation: &IUIAutomation) -> Option<bool> {
    let element = unsafe { automation.GetFocusedElement() }.ok()?;
    unsafe { element.CurrentIsPassword() }
        .ok()
        .map(|value| value.as_bool())
}

/// Unknown password state must not silently enable the synthetic-copy path.
/// Only profiles known to copy safely may proceed when the probe fails.
fn password_state_blocks_clipboard(state: Option<bool>, copy_compatibility_allowed: bool) -> bool {
    match state {
        Some(is_password) => is_password,
        None => !copy_compatibility_allowed,
    }
}

fn uia_processes_belong_to_target(
    target_process_id: u32,
    element_process_id: u32,
    element_window_process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if target_process_id == 0 || element_process_id == 0 {
        return false;
    }
    if element_process_id == target_process_id
        && (element_window_process_id == 0 || element_window_process_id == target_process_id)
    {
        return true;
    }
    let related_by_tree = process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref()
        .is_some_and(|parents| {
            uia_processes_belong_to_target_with_parents(
                target_process_id,
                element_process_id,
                element_window_process_id,
                parents,
            )
        });
    related_by_tree
        || (processes_share_executable(target_process_id, element_process_id)
            && (element_window_process_id == 0
                || processes_share_executable(target_process_id, element_window_process_id)))
}

fn uia_processes_belong_to_target_with_parents(
    target_process_id: u32,
    element_process_id: u32,
    element_window_process_id: u32,
    parents: &HashMap<u32, u32>,
) -> bool {
    let related = |process_id| {
        process_id == target_process_id
            || process_descends_from(process_id, target_process_id, parents)
    };
    related(element_process_id)
        && (element_window_process_id == 0 || related(element_window_process_id))
}

fn processes_share_executable(left_process_id: u32, right_process_id: u32) -> bool {
    process_image_path(left_process_id)
        .zip(process_image_path(right_process_id))
        .is_some_and(|(left, right)| executables_share_application_family(&left, &right))
}

/// Same executable name, or members of the same multi-process application
/// family (WPS CEF hosts, Acrobat CEF helpers, WeChat AppEx, QQ NT helpers,
/// ...).
fn executables_share_application_family(left_image: &str, right_image: &str) -> bool {
    let left = executable_name(left_image);
    let right = executable_name(right_image);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    left == right
        || (wps_suite_process(&left) && wps_suite_process(&right))
        || (docbox_suite_process(&left) && docbox_suite_process(&right))
        || (acrobat_suite_process(&left) && acrobat_suite_process(&right))
        || (wechat_suite_process(&left) && wechat_suite_process(&right))
        || (qq_suite_process(&left) && qq_suite_process(&right))
}

/// WPS Office ships several cooperating processes; ElementFromPoint often
/// lands on the CEF plugin host while the foreground window stays on wps.exe.
fn wps_suite_process(executable: &str) -> bool {
    matches!(
        executable,
        "wps.exe"
            | "wpsoffice.exe"
            | "et.exe"
            | "wpp.exe"
            | "wpspdf.exe"
            | "promecefpluginhost.exe"
            | "ksolaunch.exe"
            | "wpscloudsvr.exe"
            | "wpscenter.exe"
    )
}

fn docbox_suite_process(executable: &str) -> bool {
    matches!(
        executable,
        "docbox.exe" | "docboxrenderer.exe" | "docboxhelper.exe" | "docboxweb.exe"
    )
}

/// Acrobat Reader uses separate renderer/browser processes for PDF pages. Do
/// not include unrelated Adobe background services; only these view processes
/// are allowed to share the selection source context.
fn acrobat_suite_process(executable: &str) -> bool {
    matches!(
        executable,
        "acrobat.exe" | "acrord32.exe" | "acrocef.exe" | "rdrcef.exe"
    ) || executable.starts_with("acrocef_")
        || executable.starts_with("rdrcef_")
}

fn wechat_suite_process(executable: &str) -> bool {
    matches!(
        executable,
        "wechat.exe" | "weixin.exe" | "wechatappex.exe" | "wxwork.exe" | "wxworkweb.exe"
    )
}

fn qq_suite_process(executable: &str) -> bool {
    matches!(executable, "qq.exe" | "qqnt.exe" | "tim.exe")
}

fn office_accessibility_first_application(image_path: &str) -> bool {
    matches!(
        executable_name(image_path).as_str(),
        "excel.exe"
            | "winword.exe"
            | "powerpnt.exe"
            | "et.exe"
            | "wpp.exe"
            | "wps.exe"
            | "scalc.exe"
            | "simpress.exe"
            | "soffice.exe"
            | "soffice.bin"
            | "libreoffice.exe"
    )
}

fn word_application_executable(executable: &str) -> bool {
    matches!(executable, "winword.exe" | "word.exe")
}

/// PDF readers commonly render text in a custom canvas. Give their native
/// MSAA probe priority before running the broader UIA traversal.
fn pdf_accessibility_first_application(image_path: &str) -> bool {
    let executable = executable_name(image_path);
    acrobat_suite_process(&executable)
        || matches!(
            executable.as_str(),
            "foxitreader.exe"
                | "foxitpdfreader.exe"
                | "sumatrapdf.exe"
                | "pdfxedit.exe"
                | "pdfxchangeeditor.exe"
                | "pdfxcview.exe"
                | "pdfgear.exe"
                | "drawboardpdf.exe"
                | "nitropdf.exe"
                | "okular.exe"
                | "mupdf.exe"
                | "evince.exe"
                | "sioyek.exe"
                | "wpspdf.exe"
                | "docbox.exe"
        )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardCaptureProfile {
    General,
    PdfCanvas,
    Acrobat,
}

impl ClipboardCaptureProfile {
    fn phase_budget(self) -> Duration {
        match self {
            Self::General => CLIPBOARD_PHASE_BUDGET,
            Self::PdfCanvas | Self::Acrobat => PDF_CLIPBOARD_PHASE_BUDGET,
        }
    }

    fn poll_interval(self) -> Duration {
        match self {
            Self::General => CLIPBOARD_POLL_INTERVAL,
            Self::PdfCanvas | Self::Acrobat => PDF_CLIPBOARD_POLL_INTERVAL,
        }
    }

    fn copy_poll_attempts(self) -> usize {
        match self {
            Self::General => CLIPBOARD_POLL_ATTEMPTS,
            Self::PdfCanvas => PDF_CLIPBOARD_POLL_ATTEMPTS,
            Self::Acrobat => ACROBAT_CLIPBOARD_POLL_ATTEMPTS,
        }
    }

    fn copy_dispatch_attempts(self) -> usize {
        match self {
            Self::General => 1,
            Self::PdfCanvas => PDF_COPY_DISPATCH_ATTEMPTS,
            Self::Acrobat => ACROBAT_COPY_DISPATCH_ATTEMPTS,
        }
    }

    fn copy_retry_delay(self) -> Duration {
        match self {
            Self::General => Duration::ZERO,
            Self::PdfCanvas => PDF_COPY_RETRY_DELAY,
            Self::Acrobat => ACROBAT_COPY_RETRY_DELAY,
        }
    }

    fn text_ready_budget(self) -> Duration {
        match self {
            Self::General => GENERAL_CLIPBOARD_TEXT_READY_BUDGET,
            Self::PdfCanvas => PDF_CLIPBOARD_TEXT_READY_BUDGET,
            Self::Acrobat => ACROBAT_CLIPBOARD_TEXT_READY_BUDGET,
        }
    }

    fn text_settle_delay(self) -> Duration {
        match self {
            Self::General => GENERAL_CLIPBOARD_TEXT_SETTLE_DELAY,
            Self::PdfCanvas => PDF_CLIPBOARD_TEXT_SETTLE_DELAY,
            Self::Acrobat => ACROBAT_CLIPBOARD_TEXT_SETTLE_DELAY,
        }
    }

    fn requires_target_clipboard_owner(self) -> bool {
        matches!(self, Self::General)
    }

    fn accepts_clipboard_owner(self, owner_matches_target: bool) -> bool {
        !self.requires_target_clipboard_owner() || owner_matches_target
    }

    fn uses_native_password_probe(self) -> bool {
        !matches!(self, Self::General)
    }

    fn required_sequence_stable_polls(self) -> usize {
        match self {
            Self::General => CLIPBOARD_STABLE_POLLS,
            // The first source-owned change is already validated by the
            // clipboard owner and text checks below. Waiting for repeated
            // unchanged polls here adds 15ms+ to every PDF selection.
            Self::PdfCanvas | Self::Acrobat => 0,
        }
    }

    fn retries_copy_after_timeout(self) -> bool {
        !matches!(self, Self::General)
    }

    fn retries_copy_after_unreadable_text(self) -> bool {
        matches!(self, Self::Acrobat)
    }

    fn allows_ole_delayed_text(self) -> bool {
        matches!(self, Self::Acrobat)
    }
}

fn clipboard_capture_profile(image_path: &str) -> ClipboardCaptureProfile {
    let executable = executable_name(image_path);
    if acrobat_suite_process(&executable) {
        ClipboardCaptureProfile::Acrobat
    } else if docbox_suite_process(&executable) || pdf_accessibility_first_application(image_path) {
        ClipboardCaptureProfile::PdfCanvas
    } else {
        ClipboardCaptureProfile::General
    }
}

/// Document and Office surfaces commonly expose stale or window-level UIA
/// names. Their native accessibility route is tried first, while browsers and
/// general editors remain UIA-first.
fn document_accessibility_probe_budget(image_path: &str) -> Option<Duration> {
    if pdf_accessibility_first_application(image_path) {
        Some(PDF_ACCESSIBILITY_PROBE_BUDGET)
    } else if office_accessibility_first_application(image_path) {
        Some(OFFICE_ACCESSIBILITY_PROBE_BUDGET)
    } else {
        None
    }
}

fn executable_name(image_path: &str) -> String {
    image_path
        .rsplit(|character| character == '\\' || character == '/')
        .next()
        .unwrap_or(image_path)
        .trim()
        .to_ascii_lowercase()
}

fn capture_strategy_for_application(
    settings: &SelectionCaptureSettings,
    image_path: &str,
) -> SelectionCaptureStrategy {
    let image_path = image_path.trim().replace('/', "\\").to_ascii_lowercase();
    let executable = executable_name(&image_path);
    let configured = settings
        .applications
        .iter()
        .find(|rule| {
            let application = rule
                .application
                .trim()
                .replace('/', "\\")
                .to_ascii_lowercase();
            application == executable
                || application == image_path
                || image_path.ends_with(&format!("\\{application}"))
        })
        .map(|rule| rule.strategy);
    if let Some(strategy) = configured {
        return strategy;
    }

    // Acrobat creates version-specific renderer executables such as
    // AcroCEF_Renderer.exe. Let those inherit the explicit AcroCEF/RdrCEF
    // rule while preserving an exact user rule when one exists above.
    let renderer_rule = if executable.starts_with("acrocef_") {
        Some("acrocef.exe")
    } else if executable.starts_with("rdrcef_") {
        Some("rdrcef.exe")
    } else {
        None
    };
    let inherited = renderer_rule.and_then(|application| {
        settings
            .applications
            .iter()
            .find(|rule| rule.application.trim().eq_ignore_ascii_case(application))
            .map(|rule| rule.strategy)
    });
    if let Some(strategy) = inherited {
        return strategy;
    }

    // Existing installations can legitimately have an older or hand-edited
    // empty rule list. Keep the global default non-destructive for ordinary
    // applications, but make known PDF/canvas hosts use the same guarded copy
    // transaction as a visible `clipboard` rule. An explicit user rule above
    // always wins, including an explicit `selection-hook` opt-out.
    if known_copy_compatibility_application(&image_path) {
        SelectionCaptureStrategy::Clipboard
    } else {
        settings.default_strategy
    }
}

fn known_copy_compatibility_application(image_path: &str) -> bool {
    let executable = executable_name(image_path);
    pdf_accessibility_first_application(image_path)
        || docbox_suite_process(&executable)
        || matches!(executable.as_str(), "emeditor.exe")
}

/// How long to wait after mouse-up before the first capture attempt.
///
/// Multi-paragraph document surfaces frequently publish their AX range just
/// after mouse-up, so wait for the host to commit that range before querying
/// native accessibility.
fn capture_settle_delay_for_request(request: &CaptureRequest) -> Duration {
    let distance_delay = request
        .start
        .map(|start| {
            let end = request.end.unwrap_or(request.current);
            let dx = i64::from(end.x) - i64::from(start.x);
            let dy = i64::from(end.y) - i64::from(start.y);
            let distance_squared = dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy));
            // A vertical distance is not evidence that this is a PDF. Keep
            // the first probe inside one or two frames for browsers, editors
            // and Office; the adaptive per-application route handles hosts
            // that genuinely need the bounded document accessibility probe.
            capture_settle_delay_for_distance(distance_squared).min(CAPTURE_SETTLE_MEDIUM_DELAY)
        })
        .unwrap_or(CAPTURE_SETTLE_DELAY);
    let press_delay = if request.press_duration_ms >= SLOW_PRESS_MS
        && request_vertical_distance(request)
            .is_some_and(|distance| distance >= LATE_RETRY_SLOW_PRESS_VERTICAL_DISTANCE)
    {
        CAPTURE_SETTLE_SLOW_PRESS_DELAY
    } else {
        CAPTURE_SETTLE_DELAY
    };
    distance_delay.max(press_delay)
}

/// Known PDF canvases expose MSAA selection shortly after mouse-up. Give them
/// a first attempt within one frame while preserving the more conservative
/// settle profile for Office, browsers, editors, and unknown applications.
fn capture_settle_delay_for_application(
    request: &CaptureRequest,
    image_path: Option<&str>,
) -> Duration {
    let delay = capture_settle_delay_for_request(request);
    if image_path.is_some_and(known_copy_compatibility_application) {
        delay.min(PDF_CAPTURE_SETTLE_DELAY)
    } else {
        delay
    }
}

fn capture_settle_delay_for_distance(distance_squared: i64) -> Duration {
    if distance_squared >= CAPTURE_SETTLE_LONG_DISTANCE_SQUARED {
        CAPTURE_SETTLE_LONG_DELAY
    } else if distance_squared >= CAPTURE_SETTLE_MEDIUM_DISTANCE_SQUARED {
        CAPTURE_SETTLE_MEDIUM_DELAY
    } else {
        CAPTURE_SETTLE_DELAY
    }
}

/// Mouse-driven selections may finish committing after the first attempt
/// (especially multi-paragraph PDF). Keyboard/manual keep single-shot.
fn should_retry_empty_capture(request: &CaptureRequest) -> bool {
    !request.observation_only
        && matches!(
            request.trigger,
            SelectionTrigger::Drag | SelectionTrigger::ShiftClick | SelectionTrigger::DoubleClick
        )
}

fn request_vertical_distance(request: &CaptureRequest) -> Option<i64> {
    let start = request.start?;
    let end = request.end.unwrap_or(request.current);
    Some((i64::from(end.y) - i64::from(start.y)).abs())
}

/// Long / slow multi-paragraph drags often need one bounded late retry after
/// PDF/Office providers finish exposing their native selection range.
fn long_document_selection_needs_late_retry(request: &CaptureRequest) -> bool {
    if !matches!(request.trigger, SelectionTrigger::Drag) {
        return false;
    }
    request_vertical_distance(request)
        .is_some_and(|distance| distance >= LATE_RETRY_VERTICAL_DISTANCE)
        || (request.press_duration_ms >= SLOW_PRESS_MS
            && request_vertical_distance(request)
                .is_some_and(|distance| distance >= LATE_RETRY_SLOW_PRESS_VERTICAL_DISTANCE))
}

fn max_empty_capture_retries(request: &CaptureRequest) -> usize {
    if long_document_selection_needs_late_retry(request) {
        EMPTY_CAPTURE_RETRY_DELAYS.len()
    } else {
        1
    }
}

fn empty_capture_retry_delay(request: &CaptureRequest, empty_attempt: u8) -> Duration {
    if request.capture_strategy == SelectionCaptureStrategy::Clipboard {
        return CLIPBOARD_EMPTY_CAPTURE_RETRY_DELAY;
    }
    let index = empty_attempt.saturating_sub(1) as usize;
    EMPTY_CAPTURE_RETRY_DELAYS.get(index).copied().unwrap_or(
        *EMPTY_CAPTURE_RETRY_DELAYS
            .last()
            .unwrap_or(&Duration::from_millis(200)),
    )
}

/// Whether a mouse message should drop a queued capture. Scroll must not —
/// multi-paragraph PDF selections commonly emit wheel events after mouse-up.
fn mouse_message_clears_pending_capture(message: u32) -> bool {
    !matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL)
}

fn raw_selection_point(point: RawPoint) -> SelectionPoint {
    SelectionPoint {
        x: f64::from(point.x),
        y: f64::from(point.y),
    }
}

fn current_cursor_position() -> RawPoint {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_ok() {
        RawPoint {
            x: point.x,
            y: point.y,
        }
    } else {
        RawPoint { x: 0, y: 0 }
    }
}

fn window_at_point(point: RawPoint) -> HWND {
    let window = unsafe {
        WindowFromPoint(POINT {
            x: point.x,
            y: point.y,
        })
    };
    if window.is_invalid() {
        unsafe { GetForegroundWindow() }
    } else {
        window
    }
}

fn accessible_point_candidates(request: CaptureRequest) -> Vec<RawPoint> {
    let mut points = Vec::with_capacity(2);
    points.push(request.current);
    if let Some(start) = request.start.filter(|point| *point != request.current) {
        points.push(start);
    }
    points
}

fn accessible_window_candidates(source_window: HWND, point: RawPoint) -> Vec<HWND> {
    let mut candidates = Vec::with_capacity(16);
    let mut seen = HashSet::new();

    // Office exposes the selection on different HWNDs depending on whether
    // the pointer is over the grid/canvas, an active child, or the top-level
    // frame. Probe the pointer window first, then the foreground/focus chain.
    let point_window = window_at_point(point);
    push_accessible_window_candidate(&mut candidates, &mut seen, point_window);
    push_accessible_window_candidate(&mut candidates, &mut seen, source_window);
    push_accessible_window_candidate(&mut candidates, &mut seen, root_window(point_window));

    let point_thread_id = unsafe { GetWindowThreadProcessId(point_window, None) };
    let source_thread_id = unsafe { GetWindowThreadProcessId(source_window, None) };
    for thread_id in [source_thread_id, point_thread_id]
        .into_iter()
        .filter(|thread_id| *thread_id != 0)
        .collect::<HashSet<_>>()
    {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetGUIThreadInfo(thread_id, &mut info) }.is_ok() {
            for window in [info.hwndFocus, info.hwndCaret, info.hwndActive] {
                push_accessible_window_candidate(&mut candidates, &mut seen, window);
            }
        }
    }

    // Walk a short parent chain for native Office child windows. The limit is
    // intentionally small because this is a compatibility fallback only.
    let initial = candidates.clone();
    for window in initial {
        let mut current = window;
        for _ in 0..8 {
            let Some(parent) = (unsafe { GetParent(current) }).ok() else {
                break;
            };
            if parent.0 == ptr::null_mut() || parent == current {
                break;
            }
            push_accessible_window_candidate(&mut candidates, &mut seen, parent);
            current = parent;
        }
    }
    candidates
}

/// Avoid a synchronous UIA focus query on PDF renderers. Their accessibility
/// providers can block while a selection is being committed, which prevents
/// the synthetic copy from being sent at all. Native password edits remain
/// protected without crossing the UIA/COM boundary.
fn focused_native_control_is_password(source_window: HWND) -> bool {
    if source_window.0 == ptr::null_mut() || source_window.is_invalid() {
        return false;
    }
    let thread_id = unsafe { GetWindowThreadProcessId(source_window, None) };
    if thread_id == 0 {
        return false;
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(thread_id, &mut info) }.is_err() {
        return false;
    }
    [info.hwndFocus, info.hwndCaret]
        .into_iter()
        .any(native_text_control_is_password)
}

fn prohibited_clipboard_application(image_path: &str) -> bool {
    screenshot_application_executable(&executable_name(image_path))
        || matches!(
            executable_name(image_path).as_str(),
            "cmd.exe"
                | "powershell.exe"
                | "pwsh.exe"
                | "wt.exe"
                | "windowsterminal.exe"
                | "openconsole.exe"
                | "conhost.exe"
                | "1password.exe"
                | "bitwarden.exe"
                | "keepass.exe"
                | "keepassxc.exe"
                | "dashlane.exe"
                | "enpass.exe"
                | "lastpass.exe"
                | "protonpass.exe"
                | "mstsc.exe"
                | "teamviewer.exe"
                | "anydesk.exe"
                | "rustdesk.exe"
                | "todesk.exe"
                | "sunloginclient.exe"
                | "parsecd.exe"
        )
}

fn screenshot_application_executable(executable: &str) -> bool {
    matches!(
        executable,
        "pixpin.exe" | "snipaste.exe" | "snippingtool.exe" | "screenclippinghost.exe"
    )
}

fn copy_shortcut_is_safe_to_inject(control: &CaptureControl) -> bool {
    !control.is_cancelled()
        && !key_is_down(VK_CONTROL.0)
        && !key_is_down(VK_C.0)
        && !key_is_down(VK_X.0)
        && !key_is_down(VK_V.0)
        && !key_is_down(VK_MENU.0)
        && !key_is_down(VK_LWIN.0)
        && !key_is_down(VK_RWIN.0)
}

fn clipboard_recovery_decision(
    restored_sequence: u32,
    current_sequence: u32,
    gesture_sequence: Option<u32>,
    source_owns_current_clipboard: bool,
) -> ClipboardRecoveryDecision {
    if current_sequence == restored_sequence {
        return ClipboardRecoveryDecision::Drop;
    }
    // A sequence already observed at mouse-down belongs to the user or an
    // application action that completed before this gesture. It must never be
    // overwritten by a stale recovery snapshot.
    if gesture_sequence == Some(current_sequence) {
        return ClipboardRecoveryDecision::Drop;
    }
    // Only repair a change that occurred after this drag began, is still owned
    // by the same source application, and follows our previous restoration.
    if gesture_sequence == Some(restored_sequence) && source_owns_current_clipboard {
        ClipboardRecoveryDecision::RestoreOriginal
    } else {
        ClipboardRecoveryDecision::Drop
    }
}

fn post_copy_shortcut() -> bool {
    let key = |virtual_key, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let mut inputs = Vec::with_capacity(4);
    // VK_CONTROL produces the conventional left-control chord. Sending
    // VK_RCONTROL without KEYEVENTF_EXTENDEDKEY is interpreted inconsistently
    // by Acrobat and some custom document canvases.
    inputs.push(key(VK_CONTROL, Default::default()));
    inputs.push(key(VK_C, Default::default()));
    inputs.push(key(VK_C, KEYEVENTF_KEYUP));
    inputs.push(key(VK_CONTROL, KEYEVENTF_KEYUP));
    let inserted = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if inserted as usize == inputs.len() {
        true
    } else {
        let releases = [key(VK_C, KEYEVENTF_KEYUP), key(VK_CONTROL, KEYEVENTF_KEYUP)];
        let _ = unsafe { SendInput(&releases, std::mem::size_of::<INPUT>() as i32) };
        false
    }
}

fn clipboard_capture_context_is_valid(
    source_window: HWND,
    process_id: u32,
    process_parents: Option<&HashMap<u32, u32>>,
    control: &CaptureControl,
) -> bool {
    if control.is_cancelled()
        || process_id == 0
        || process_id == control.source_process_id && control.source_process_id == 0
    {
        return false;
    }
    clipboard_source_is_foreground(source_window, process_id, process_parents)
}

fn clipboard_source_is_foreground(
    source_window: HWND,
    process_id: u32,
    process_parents: Option<&HashMap<u32, u32>>,
) -> bool {
    if process_id == 0 {
        return false;
    }
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_pid = window_process_id(foreground);
    if foreground.0 == ptr::null_mut() || foreground_pid == 0 {
        return false;
    }
    foreground == source_window
        || foreground_pid == process_id
        || processes_share_executable(foreground_pid, process_id)
        || process_parents
            .is_some_and(|parents| process_descends_from(foreground_pid, process_id, parents))
}

fn clipboard_source_is_foreground_with_lazy_parents(
    source_window: HWND,
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if clipboard_source_is_foreground(source_window, process_id, None) {
        return true;
    }
    if process_parents.is_some() {
        return false;
    }
    let parents = process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref();
    clipboard_source_is_foreground(source_window, process_id, parents)
}

/// Keep ordinary single-process captures off the ToolHelp snapshot path.
/// Renderers that genuinely move selection ownership to a child process retry
/// the same validation with a cached ancestry map once, then reuse it for the
/// remainder of this capture.
fn clipboard_capture_context_is_valid_with_lazy_parents(
    source_window: HWND,
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
    control: &CaptureControl,
) -> bool {
    if clipboard_capture_context_is_valid(source_window, process_id, None, control) {
        return true;
    }
    if process_parents.is_some() {
        return false;
    }
    let parents = process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref();
    clipboard_capture_context_is_valid(source_window, process_id, parents, control)
}

fn clipboard_owner_matches_target(
    process_id: u32,
    process_parents: Option<&HashMap<u32, u32>>,
) -> bool {
    let Ok(owner) = (unsafe { windows::Win32::System::DataExchange::GetClipboardOwner() }) else {
        return true;
    };
    let owner_pid = window_process_id(owner);
    clipboard_owner_pid_matches_target(owner_pid, process_id, process_parents)
}

fn clipboard_owner_matches_target_with_lazy_parents(
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if clipboard_owner_matches_target(process_id, None) {
        return true;
    }
    if process_parents.is_some() {
        return false;
    }
    let parents = process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref();
    clipboard_owner_matches_target(process_id, parents)
}

/// General applications must publish the copied data from their own process
/// family. PDF renderers are allowed to use an OLE/system broker for delayed
/// clipboard formats; freshness, foreground continuity, cancellation, and
/// physical shortcut checks still guard every accepted update.
fn clipboard_owner_is_acceptable_for_capture(
    clipboard_profile: ClipboardCaptureProfile,
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if clipboard_profile.requires_target_clipboard_owner() {
        return clipboard_profile.accepts_clipboard_owner(
            clipboard_owner_matches_target_with_lazy_parents(process_id, process_parents),
        );
    }
    // PDF/document profiles may publish delayed formats through an OLE or
    // shell broker. Keep accepting ownerless/unknown owners, but reject a
    // positively identified foreign executable (e.g. a clipboard manager
    // racing the injected Ctrl+C).
    let target_family =
        clipboard_owner_matches_target_with_lazy_parents(process_id, process_parents);
    let owner_pid = clipboard_owner_process_id();
    let owner_image = owner_pid.and_then(process_image_path);
    pdf_clipboard_owner_is_acceptable(target_family, owner_pid, owner_image.as_deref())
}

fn clipboard_owner_process_id() -> Option<u32> {
    let owner = unsafe { windows::Win32::System::DataExchange::GetClipboardOwner() }.ok()?;
    if owner.0.is_null() || owner.is_invalid() {
        return None;
    }
    let owner_pid = window_process_id(owner);
    (owner_pid != 0).then_some(owner_pid)
}

fn is_system_clipboard_broker_image(image_path: &str) -> bool {
    matches!(
        executable_name(image_path).as_str(),
        "explorer.exe" | "rdpclip.exe" | "dllhost.exe"
    )
}

/// PDF profiles accept ownerless/unknown owners and system brokers, but not a
/// foreign application that happened to write the clipboard during capture.
fn pdf_clipboard_owner_is_acceptable(
    target_family: bool,
    owner_pid: Option<u32>,
    broker_image: Option<&str>,
) -> bool {
    if target_family || owner_pid.is_none() {
        return true;
    }
    broker_image.map_or(true, is_system_clipboard_broker_image)
}

fn clipboard_recovery_owner_is_acceptable(
    clipboard_profile: ClipboardCaptureProfile,
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if !clipboard_profile.requires_target_clipboard_owner() {
        return true;
    }
    clipboard_owner_is_known_target_with_lazy_parents(process_id, process_parents)
}

/// Recovery is allowed to overwrite only a clipboard owner we can positively
/// associate with the source process. The broader capture check intentionally
/// permits owner-less clipboard implementations, but that would be unsafe for
/// restoring a retained user snapshot.
fn clipboard_owner_is_known_target(
    process_id: u32,
    process_parents: Option<&HashMap<u32, u32>>,
) -> bool {
    let Ok(owner) = (unsafe { windows::Win32::System::DataExchange::GetClipboardOwner() }) else {
        return false;
    };
    let owner_pid = window_process_id(owner);
    owner_pid != 0 && clipboard_owner_pid_matches_target(owner_pid, process_id, process_parents)
}

fn clipboard_owner_is_known_target_with_lazy_parents(
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
) -> bool {
    if clipboard_owner_is_known_target(process_id, None) {
        return true;
    }
    if process_parents.is_some() {
        return false;
    }
    let parents = process_parents
        .get_or_insert_with(process_parent_map)
        .as_ref();
    clipboard_owner_is_known_target(process_id, parents)
}

fn clipboard_owner_pid_matches_target(
    owner_pid: u32,
    process_id: u32,
    process_parents: Option<&HashMap<u32, u32>>,
) -> bool {
    owner_pid == 0
        || owner_pid == process_id
        || processes_share_executable(owner_pid, process_id)
        || process_parents.is_some_and(|parents| {
            process_descends_from(owner_pid, process_id, parents)
                || process_descends_from(process_id, owner_pid, parents)
        })
}

fn wait_for_clipboard_change(
    original_sequence: u32,
    source_window: HWND,
    process_id: u32,
    deadline: Instant,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
    control: &CaptureControl,
    clipboard_profile: ClipboardCaptureProfile,
    max_attempts: usize,
    poll_interval: Duration,
    required_stable_polls: usize,
) -> Option<u32> {
    let mut stable_sequence = None;
    let mut stable_polls = 0usize;
    for _ in 0..max_attempts {
        thread::sleep(poll_interval);
        if Instant::now() >= deadline
            || control.is_cancelled()
            || !clipboard_capture_context_is_valid_with_lazy_parents(
                source_window,
                process_id,
                process_parents,
                control,
            )
        {
            return None;
        }
        let sequence = clipboard::windows_clipboard_sequence();
        if sequence == original_sequence {
            continue;
        }
        if !clipboard_owner_is_acceptable_for_capture(
            clipboard_profile,
            process_id,
            process_parents,
        ) || !copy_shortcut_is_safe_to_inject(control)
        {
            return None;
        }
        if required_stable_polls == 0 {
            return Some(sequence);
        }
        if stable_sequence != Some(sequence) {
            stable_sequence = Some(sequence);
            stable_polls = 0;
            continue;
        }
        stable_polls = stable_polls.saturating_add(1);
        if stable_polls >= required_stable_polls {
            return Some(sequence);
        }
    }
    None
}

/// Complete an interrupted synthetic-copy transaction when it has already
/// advanced the clipboard. Keyboard cancellation is deliberately excluded:
/// the user's own Ctrl+C/Ctrl+V must never be overwritten by a retained
/// TextLens snapshot. Mouse/foreground supersession can still restore a
/// synthetic write while the same source application remains foreground,
/// which prevents the next gesture from seeing an abandoned selection as its
/// baseline.
fn restore_interrupted_clipboard_if_safe(
    snapshot: &clipboard::WindowsClipboardSnapshot,
    baseline_sequence: u32,
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
    control: &CaptureControl,
    source_window: HWND,
    clipboard_profile: ClipboardCaptureProfile,
) -> bool {
    if control.cancel_reason() == Some(HelperCancelReason::UserKeyboard) {
        trace_selection_route(
            "clipboard-interrupted-user-keyboard",
            source_window,
            process_id,
        );
        return false;
    }
    let current_sequence = clipboard::windows_clipboard_sequence();
    if current_sequence == baseline_sequence
        || !clipboard_source_is_foreground_with_lazy_parents(
            source_window,
            process_id,
            process_parents,
        )
        || !clipboard_recovery_owner_is_acceptable(clipboard_profile, process_id, process_parents)
        || !copy_shortcut_is_safe_to_inject(control)
    {
        return false;
    }
    let restored = snapshot.restore_if_unchanged(current_sequence);
    if restored {
        control.report_phase(HelperPhase::ClipboardRestored { process_id });
    }
    trace_selection_route(
        if restored {
            "clipboard-interrupted-restore-success"
        } else {
            "clipboard-interrupted-restore-raced"
        },
        source_window,
        process_id,
    );
    restored
}

/// Reads only text published by the just-injected copy transaction.
///
/// PDF readers often advance the clipboard sequence before CF_UNICODETEXT is
/// available, then add delayed formats on a later sequence. Poll the actual
/// text for a short bounded window, always keeping the source process, active
/// gesture and key-state checks intact. General applications also retain a
/// strict clipboard-owner check; PDF profiles permit delayed OLE/system
/// publishers while still rejecting every pre-baseline clipboard value.
fn wait_for_fresh_clipboard_text(
    mut sequence: u32,
    ready_budget: Duration,
    settle_delay: Duration,
    phase_deadline: Instant,
    source_window: HWND,
    process_id: u32,
    process_parents: &mut Option<Option<HashMap<u32, u32>>>,
    control: &CaptureControl,
    clipboard_profile: ClipboardCaptureProfile,
    poll_interval: Duration,
) -> Option<(String, u32)> {
    let deadline = Instant::now()
        .checked_add(ready_budget)
        .unwrap_or_else(Instant::now)
        .min(phase_deadline);
    let mut readable_candidate: Option<(u32, Instant, String)> = None;
    while Instant::now() < deadline {
        if !clipboard_capture_context_is_valid_with_lazy_parents(
            source_window,
            process_id,
            process_parents,
            control,
        ) || !clipboard_owner_is_acceptable_for_capture(
            clipboard_profile,
            process_id,
            process_parents,
        ) || !copy_shortcut_is_safe_to_inject(control)
        {
            return None;
        }
        let current_sequence = clipboard::windows_clipboard_sequence();
        if current_sequence != sequence {
            // The target can publish delayed clipboard formats through a new
            // sequence. A foreign update is rejected by the owner check above.
            sequence = current_sequence;
            readable_candidate = None;
        }
        let now = Instant::now();
        if readable_candidate
            .as_ref()
            .is_some_and(|(candidate_sequence, since, _)| {
                clipboard_text_candidate_is_settled(
                    Some((*candidate_sequence, *since)),
                    sequence,
                    now,
                    settle_delay,
                )
            })
        {
            return readable_candidate
                .take()
                .map(|(_, _, text)| (text, sequence));
        }
        if readable_candidate.is_none() {
            if let Some(text) = clipboard::read_windows_clipboard_text(
                sequence,
                clipboard_profile.allows_ole_delayed_text(),
            )
            .map(|text| normalize_captured_text(&text))
            .filter(|text| captured_text_is_usable(text))
            {
                if settle_delay.is_zero() {
                    return Some((text, sequence));
                }
                readable_candidate = Some((sequence, now, text));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(poll_interval.min(remaining));
    }
    None
}

fn clipboard_text_candidate_is_settled(
    candidate: Option<(u32, Instant)>,
    sequence: u32,
    now: Instant,
    settle_delay: Duration,
) -> bool {
    candidate.is_some_and(|(candidate_sequence, since)| {
        candidate_sequence == sequence && now.saturating_duration_since(since) >= settle_delay
    })
}

fn push_accessible_window_candidate(
    candidates: &mut Vec<HWND>,
    seen: &mut HashSet<isize>,
    window: HWND,
) {
    if window.0 != ptr::null_mut() && !window.is_invalid() {
        let key = window.0 as isize;
        if seen.insert(key) {
            candidates.push(window);
        }
    }
}

fn root_window(window: HWND) -> HWND {
    if window.is_invalid() {
        return window;
    }
    let root = unsafe { GetAncestor(window, GA_ROOT) };
    if root.is_invalid() {
        window
    } else {
        root
    }
}

fn key_is_down(virtual_key: u16) -> bool {
    unsafe { GetAsyncKeyState(i32::from(virtual_key)) as u16 & 0x8000 != 0 }
}

fn current_modifiers() -> ModifierSnapshot {
    ModifierSnapshot {
        shift: key_is_down(VK_SHIFT.0),
        control: key_is_down(VK_CONTROL.0),
        alt: key_is_down(VK_MENU.0),
        windows: key_is_down(VK_LWIN.0) || key_is_down(VK_RWIN.0),
    }
}

fn is_modifier_virtual_key(virtual_key: u16) -> bool {
    matches!(
        virtual_key,
        key if key == VK_SHIFT.0
            || key == VK_LSHIFT.0
            || key == VK_RSHIFT.0
            || key == VK_CONTROL.0
            || key == VK_LCONTROL.0
            || key == VK_RCONTROL.0
            || key == VK_MENU.0
            || key == VK_LMENU.0
            || key == VK_RMENU.0
            || key == VK_LWIN.0
            || key == VK_RWIN.0
    )
}

fn hook_generation() -> &'static AtomicU64 {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    &GENERATION
}

fn trace_selection_capture(
    stage: &str,
    request: &CaptureRequest,
    source_root_window: isize,
    source_process_id: u32,
) {
    if !selection_trace_enabled() {
        return;
    }
    trace_selection_line(format!(
            "[selection] stage={stage} trigger={:?} observation={} root={source_root_window:#x} pid={source_process_id} generation={:?} first_click={:?}",
        request.trigger, request.observation_only, request.generation, request.observation_generation
    ));
}

/// Opt-in anonymous timing output for real-world host profiling. It contains
/// only a stage and duration so PDF/application text never enters diagnostics.
fn trace_selection_timing(stage: &str, duration: Duration) {
    if !selection_trace_enabled() {
        return;
    }
    trace_selection_line(format!(
        "[selection-timing] stage={stage} duration_ms={:.1}",
        duration.as_secs_f64() * 1_000.0
    ));
}

/// Opt-in route diagnostics for native compatibility paths. The trace carries
/// only a stage, HWND and PID; it never includes the selected text, window
/// title or clipboard contents.
fn trace_selection_route(stage: &str, source_window: HWND, process_id: u32) {
    if !selection_trace_enabled() {
        return;
    }
    trace_selection_line(format!(
        "[selection-route] stage={stage} hwnd={:#x} pid={process_id}",
        source_window.0 as isize
    ));
}

/// Provider diagnostics intentionally contain no selected text, title, or
/// clipboard state. They make a cross-process selection mismatch actionable
/// without retaining user content.
fn trace_selection_provider(
    stage: &str,
    source_window: HWND,
    source_process_id: u32,
    provider_window: HWND,
    provider_process_id: u32,
) {
    if !selection_trace_enabled() {
        return;
    }
    trace_selection_line(format!(
        "[selection-provider] stage={stage} source_hwnd={:#x} source_pid={source_process_id} provider_hwnd={:#x} provider_pid={provider_process_id}",
        source_window.0 as isize,
        provider_window.0 as isize,
    ));
}

fn selection_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let environment_enabled =
            std::env::var_os("TEXTLENS_SELECTION_TRACE").is_some_and(|value| {
                let value = value.to_string_lossy();
                value == "1" || value.eq_ignore_ascii_case("true")
            });
        environment_enabled
            || selection_trace_directory()
                .is_some_and(|directory| directory.join("selection-trace.enabled").is_file())
    })
}

fn selection_trace_directory() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|directory| std::path::PathBuf::from(directory).join("TextLens"))
}

fn trace_selection_line(line: String) {
    eprintln!("{line}");
    let Some(directory) = selection_trace_directory() else {
        return;
    };
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("selection-diagnostic.log");
    let Ok(mut output) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(output, "{} process={} {line}", timestamp_ms(), unsafe {
        GetCurrentProcessId()
    });
}

fn next_raw_input_sequence() -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    SEQUENCE.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
}

fn rectangles_from_values(values: &[f64]) -> Vec<SelectionBounds> {
    values
        .chunks_exact(4)
        .filter_map(|chunk| {
            let rectangle = SelectionBounds {
                x: chunk[0],
                y: chunk[1],
                width: chunk[2],
                height: chunk[3],
            };
            selection_bounds_are_reasonable(rectangle).then_some(rectangle)
        })
        .collect()
}

fn endpoint_points(
    rectangles: &[SelectionBounds],
    direction: SelectionDirection,
) -> (
    Option<SelectionPoint>,
    Option<SelectionPoint>,
    Option<SelectionPoint>,
    Option<SelectionPoint>,
) {
    let (Some(first), Some(last)) = (rectangles.first(), rectangles.last()) else {
        return (None, None, None, None);
    };
    let left = |rectangle: &SelectionBounds| {
        (
            SelectionPoint {
                x: rectangle.x,
                y: rectangle.y,
            },
            SelectionPoint {
                x: rectangle.x,
                y: rectangle.y + rectangle.height,
            },
        )
    };
    let right = |rectangle: &SelectionBounds| {
        (
            SelectionPoint {
                x: rectangle.x + rectangle.width,
                y: rectangle.y,
            },
            SelectionPoint {
                x: rectangle.x + rectangle.width,
                y: rectangle.y + rectangle.height,
            },
        )
    };
    let ((start_top, start_bottom), (end_top, end_bottom)) =
        if direction == SelectionDirection::Backward {
            (right(last), left(first))
        } else {
            (left(first), right(last))
        };
    (
        Some(start_top),
        Some(start_bottom),
        Some(end_top),
        Some(end_bottom),
    )
}

fn bounding_rectangle_values(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
) -> Result<Vec<f64>, SelectionError> {
    let array = unsafe { range.GetBoundingRectangles() }.map_err(|_| SelectionError::Internal)?;
    if array.is_null() {
        return Ok(Vec::new());
    }
    let array = OwnedSafeArray(array);
    if unsafe { SafeArrayGetDim(array.0) } != 1 {
        return Ok(Vec::new());
    }
    let lower = unsafe { SafeArrayGetLBound(array.0, 1) }.map_err(|_| SelectionError::Internal)?;
    let upper = unsafe { SafeArrayGetUBound(array.0, 1) }.map_err(|_| SelectionError::Internal)?;
    if upper < lower {
        return Ok(Vec::new());
    }
    let length = usize::try_from(i64::from(upper) - i64::from(lower) + 1)
        .map_err(|_| SelectionError::Internal)?;
    if length > MAX_BOUNDING_VALUES {
        return Err(SelectionError::Internal);
    }
    if length % 4 != 0 {
        return Err(SelectionError::Internal);
    }
    let mut data = ptr::null_mut::<c_void>();
    unsafe { SafeArrayAccessData(array.0, &mut data) }.map_err(|_| SelectionError::Internal)?;
    let access = SafeArrayAccess { array: array.0 };
    if data.is_null() || length == 0 {
        return Ok(Vec::new());
    }
    // UI Automation specifies VT_R8 for text range bounding rectangles.
    let values = unsafe { std::slice::from_raw_parts(data.cast::<f64>(), length) }.to_vec();
    drop(access);
    Ok(values)
}

fn element_runtime_id(element: &IUIAutomationElement) -> Option<Vec<i32>> {
    let array = unsafe { element.GetRuntimeId() }.ok()?;
    if array.is_null() {
        return None;
    }
    let array = OwnedSafeArray(array);
    if unsafe { SafeArrayGetDim(array.0) } != 1 {
        return None;
    }
    let lower = unsafe { SafeArrayGetLBound(array.0, 1) }.ok()?;
    let upper = unsafe { SafeArrayGetUBound(array.0, 1) }.ok()?;
    if upper < lower {
        return None;
    }
    let length = usize::try_from(i64::from(upper) - i64::from(lower) + 1).ok()?;
    if length == 0 || length > MAX_UIA_RUNTIME_ID_VALUES {
        return None;
    }
    let mut data = ptr::null_mut::<c_void>();
    unsafe { SafeArrayAccessData(array.0, &mut data) }.ok()?;
    let access = SafeArrayAccess { array: array.0 };
    if data.is_null() {
        return None;
    }
    // UI Automation specifies VT_I4 for element runtime IDs.
    let values = unsafe { std::slice::from_raw_parts(data.cast::<i32>(), length) }.to_vec();
    drop(access);
    Some(values)
}

struct OwnedSafeArray(*mut SAFEARRAY);

impl Drop for OwnedSafeArray {
    fn drop(&mut self) {
        let _ = unsafe { SafeArrayDestroy(self.0) };
    }
}

struct SafeArrayAccess {
    array: *mut SAFEARRAY,
}

impl Drop for SafeArrayAccess {
    fn drop(&mut self) {
        let _ = unsafe { SafeArrayUnaccessData(self.array) };
    }
}

fn source_application(process_id: u32, window: HWND) -> SourceApplication {
    let image = process_image_path(process_id);
    let name = image
        .as_deref()
        .and_then(|path| Path::new(path).file_stem())
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| window_title(window))
        .unwrap_or_else(|| format!("Process {process_id}"));
    SourceApplication {
        bundle_id: image.unwrap_or_else(|| format!("windows.pid.{process_id}")),
        name,
    }
}

fn com_property_dispatch(object: &IDispatch, name: windows::core::PCWSTR) -> Option<IDispatch> {
    let mut dispid = 0i32;
    let iid_null = GUID::zeroed();
    unsafe {
        object
            .GetIDsOfNames(&iid_null, &name, 1, 0, &mut dispid)
            .ok()?;
    }
    let params = DISPPARAMS::default();
    let mut value = VARIANT::default();
    let result = unsafe {
        object
            .Invoke(
                dispid,
                &iid_null,
                0,
                DISPATCH_PROPERTYGET,
                &params,
                Some(&mut value),
                None,
                None,
            )
            .ok()
    };
    if result.is_none() {
        return None;
    }
    let dispatch = variant_dispatch(&value)
        .cloned()
        .or_else(|| variant_unknown(&value).and_then(|unknown| unknown.cast().ok()));
    let _ = unsafe { VariantClear(&mut value) };
    dispatch
}

fn com_property_i32(object: &IDispatch, name: windows::core::PCWSTR) -> Option<i32> {
    let mut dispid = 0;
    let iid = GUID::zeroed();
    unsafe { object.GetIDsOfNames(&iid, &name, 1, 0, &mut dispid) }.ok()?;
    let mut value = OwnedVariant(VARIANT::default());
    unsafe {
        object.Invoke(
            dispid,
            &iid,
            0,
            DISPATCH_PROPERTYGET,
            &DISPPARAMS::default(),
            Some(&mut value.0),
            None,
            None,
        )
    }
    .ok()?;
    (variant_type(&value.0) == VT_I4.0)
        .then(|| unsafe { value.0.Anonymous.Anonymous.Anonymous.lVal })
}

fn word_selection_dispatch(native_object: &IDispatch) -> Option<IDispatch> {
    com_property_dispatch(native_object, w!("Selection")).or_else(|| {
        let application = com_property_dispatch(native_object, w!("Application"))?;
        com_property_dispatch(&application, w!("Selection"))
    })
}

fn com_property_text(object: &IDispatch, name: windows::core::PCWSTR) -> Option<String> {
    let mut dispid = 0i32;
    let iid_null = GUID::zeroed();
    unsafe {
        object
            .GetIDsOfNames(&iid_null, &name, 1, 0, &mut dispid)
            .ok()?;
    }
    let params = DISPPARAMS::default();
    let mut value = VARIANT::default();
    let result = unsafe {
        object
            .Invoke(
                dispid,
                &iid_null,
                0,
                DISPATCH_PROPERTYGET,
                &params,
                Some(&mut value),
                None,
                None,
            )
            .ok()
    };
    if result.is_none() {
        return None;
    }
    let text = if variant_type(&value) == VT_BSTR.0 {
        accessible_selection_bstr_data(&value).map(|(text, _)| text)
    } else {
        None
    };
    let _ = unsafe { VariantClear(&mut value) };
    text
}

fn normalize_captured_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut previous_was_cr = false;
    for character in text.chars() {
        match character {
            '\r' => {
                normalized.push('\n');
                previous_was_cr = true;
            }
            '\n' if previous_was_cr => {
                previous_was_cr = false;
            }
            '\n' => {
                normalized.push('\n');
                previous_was_cr = false;
            }
            '\0' => {
                previous_was_cr = false;
            }
            '\t' => {
                normalized.push('\t');
                previous_was_cr = false;
            }
            character if character.is_control() => {
                previous_was_cr = false;
            }
            character => {
                normalized.push(character);
                previous_was_cr = false;
            }
        }
    }
    normalized.trim().to_owned()
}

/// Best-effort: collapse the focused UIA text selection when it still equals `text`.
///
/// Never injects synthetic input. Returns `false` on any failure (no focused
/// text pattern, mismatch, password field, COM error).
pub fn clear_matching_text(bundle_id: Option<&str>, text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    clear_matching_text_inner(bundle_id, text).unwrap_or(false)
}

fn clear_matching_text_inner(bundle_id: Option<&str>, text: &str) -> Result<bool, SelectionError> {
    let _apartment = ComApartment::initialize()?;
    let automation =
        unsafe { CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| SelectionError::NativeInitializationFailed)?;

    let focused = match unsafe { automation.GetFocusedElement() } {
        Ok(element) => element,
        Err(_) => return Ok(false),
    };
    let own_process_id = unsafe { GetCurrentProcessId() };
    let element_process_id = match unsafe { focused.CurrentProcessId() } {
        Ok(value) if value > 0 => value as u32,
        _ => return Ok(false),
    };
    if element_process_id == own_process_id {
        return Ok(false);
    }
    if unsafe { focused.CurrentIsPassword() }
        .ok()
        .is_some_and(|value| value.as_bool())
    {
        return Ok(false);
    }

    let Some(pattern) = find_text_pattern_for_element(&automation, &focused) else {
        return Ok(false);
    };
    let ranges = match unsafe { pattern.GetSelection() } {
        Ok(ranges) => ranges,
        Err(_) => return Ok(false),
    };
    if unsafe { ranges.Length() }.unwrap_or(0) <= 0 {
        return Ok(false);
    }
    let range = match unsafe { ranges.GetElement(0) } {
        Ok(range) => range,
        Err(_) => return Ok(false),
    };
    let live_text = match unsafe { range.GetText(-1) } {
        Ok(value) => value.to_string(),
        Err(_) => return Ok(false),
    };

    let foreground = unsafe { GetForegroundWindow() };
    let process_id = {
        let foreground_pid = window_process_id(foreground);
        if foreground_pid != 0 {
            foreground_pid
        } else {
            element_process_id
        }
    };
    let source_app = source_application(process_id, foreground);
    if !super::should_clear_host_selection(
        text,
        &live_text,
        bundle_id,
        Some(source_app.bundle_id.as_str()),
    ) {
        return Ok(false);
    }

    collapse_text_range(&range)?;
    Ok(true)
}

fn find_text_pattern_for_element(
    automation: &IUIAutomation,
    focused: &IUIAutomationElement,
) -> Option<IUIAutomationTextPattern> {
    let walkers = [
        unsafe { automation.ControlViewWalker() }.ok(),
        unsafe { automation.RawViewWalker() }.ok(),
    ];
    let mut visited_runtime_ids = HashSet::new();
    let mut visited_without_runtime_id = Vec::<IUIAutomationElement>::new();
    for walker in walkers.into_iter().flatten() {
        let mut element = focused.clone();
        for _ in 0..MAX_UIA_ANCESTORS {
            let runtime_id = element_runtime_id(&element);
            let duplicate = runtime_id.as_ref().map_or_else(
                || {
                    visited_without_runtime_id.iter().any(|previous| {
                        unsafe { automation.CompareElements(previous, &element) }
                            .ok()
                            .is_some_and(|same| same.as_bool())
                    })
                },
                |runtime_id| !visited_runtime_ids.insert(runtime_id.clone()),
            );
            if !duplicate {
                match unsafe { element.CurrentIsPassword() }
                    .ok()
                    .map(|value| value.as_bool())
                {
                    Some(true) => return None,
                    Some(false) | None => {}
                }
                if let Ok(pattern) = unsafe {
                    element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                } {
                    return Some(pattern);
                }
                if runtime_id.is_none() {
                    visited_without_runtime_id.push(element.clone());
                }
            }
            element = match unsafe { walker.GetParentElement(&element) } {
                Ok(parent) => parent,
                Err(_) => break,
            };
        }
    }
    None
}

fn collapse_text_range(range: &IUIAutomationTextRange) -> Result<(), SelectionError> {
    // Move the End endpoint onto Start so the range length becomes 0, then
    // apply that collapsed caret as the active selection.
    unsafe {
        range
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                range,
                TextPatternRangeEndpoint_Start,
            )
            .map_err(|_| SelectionError::Internal)?;
        range.Select().map_err(|_| SelectionError::Internal)?;
    }
    Ok(())
}

fn process_image_path(process_id: u32) -> Option<String> {
    query_full_process_image_path(process_id)
        .or_else(|| process_executable_name_from_snapshot(process_id))
}

fn query_full_process_image_path(process_id: u32) -> Option<String> {
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let handle = OwnedHandle(handle);
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    String::from_utf16(&buffer[..length as usize]).ok()
}

/// QueryFullProcessImageName can be denied for Acrobat protected-mode and
/// sandbox renderer processes. ToolHelp still exposes the executable name in
/// those cases, which is sufficient for compatibility routing and process
/// family checks without reading any document or window content.
fn process_executable_name_from_snapshot(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let snapshot = OwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe { Process32FirstW(snapshot.0, &mut entry) }.ok()?;
    loop {
        if entry.th32ProcessID == process_id {
            return decode_process_executable_name(&entry.szExeFile);
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            return None;
        }
    }
}

fn decode_process_executable_name(buffer: &[u16]) -> Option<String> {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    (length > 0)
        .then(|| String::from_utf16(&buffer[..length]).ok())
        .flatten()
        .filter(|name| !name.trim().is_empty())
}

fn process_parent_map() -> Option<HashMap<u32, u32>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let snapshot = OwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe { Process32FirstW(snapshot.0, &mut entry) }.ok()?;

    let mut parents = HashMap::new();
    loop {
        if entry.th32ProcessID != 0 {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    Some(parents)
}

fn process_descends_from(
    mut process_id: u32,
    ancestor_process_id: u32,
    parents: &HashMap<u32, u32>,
) -> bool {
    if process_id == 0 || ancestor_process_id == 0 {
        return false;
    }
    for _ in 0..=MAX_PROCESS_ANCESTORS {
        if process_id == ancestor_process_id {
            return true;
        }
        let Some(parent_process_id) = parents.get(&process_id).copied() else {
            return false;
        };
        if parent_process_id == 0 || parent_process_id == process_id {
            return false;
        }
        process_id = parent_process_id;
    }
    false
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn window_title(window: HWND) -> Option<String> {
    if window.0 == ptr::null_mut() {
        return None;
    }
    let mut buffer = vec![0u16; 512];
    let length = unsafe { GetWindowTextW(window, &mut buffer) };
    (length > 0)
        .then(|| String::from_utf16_lossy(&buffer[..length as usize]))
        .filter(|title| !title.trim().is_empty())
}

fn window_process_id(window: HWND) -> u32 {
    if window.0 == ptr::null_mut() {
        return 0;
    }
    let mut process_id = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) };
    process_id
}

fn window_is_fullscreen(window: HWND) -> bool {
    if window.0 == ptr::null_mut() {
        return false;
    }
    let mut window_rectangle = RECT::default();
    if unsafe { GetWindowRect(window, &mut window_rectangle) }.is_err() {
        return false;
    }
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() {
        return unsafe { IsZoomed(window).as_bool() };
    }
    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool() {
        return unsafe { IsZoomed(window).as_bool() };
    }
    let monitor_rectangle = monitor_info.rcMonitor;
    (window_rectangle.left - monitor_rectangle.left).abs() <= 1
        && (window_rectangle.top - monitor_rectangle.top).abs() <= 1
        && (window_rectangle.right - monitor_rectangle.right).abs() <= 1
        && (window_rectangle.bottom - monitor_rectangle.bottom).abs() <= 1
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn strict_parent_timestamp_ms() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let wall_clock = timestamp_ms();
    let mut previous = LAST.load(Ordering::Acquire);
    loop {
        let next = wall_clock.max(previous.saturating_add(1));
        match LAST.compare_exchange_weak(previous, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return next,
            Err(actual) => previous = actual,
        }
    }
}

fn pump_sta_messages() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

struct HookThread {
    thread_id: u32,
    instance_id: u64,
    stop_requested: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl HookThread {
    fn start(inbox: Sender<WorkerMessage>) -> Result<Self, SelectionError> {
        let instance_id = next_hook_instance_id();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let published_thread_id = Arc::new(AtomicU32::new(0));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let hook_thread_id = published_thread_id.clone();
        let hook_stop_requested = stop_requested.clone();
        let join = thread::Builder::new()
            .name("textlens-selection-hooks".to_owned())
            .spawn(move || {
                hook_thread_main(
                    inbox,
                    ready_sender,
                    hook_thread_id,
                    hook_stop_requested,
                    instance_id,
                )
            })
            .map_err(|_| SelectionError::EventTapFailed)?;
        match ready_receiver.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                instance_id,
                stop_requested,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = reap_worker(join, WORKER_SHUTDOWN_GRACE);
                Err(error)
            }
            Err(_) => {
                // Never detach a hook thread after a startup timeout. The
                // stop flag covers the race before its message queue exists;
                // once its thread ID is published WM_QUIT promptly wakes the
                // timed message pump.
                stop_requested.store(true, Ordering::Release);
                let thread_id = published_thread_id.load(Ordering::Acquire);
                if thread_id != 0 {
                    let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
                }
                let _ = reap_worker(join, WORKER_SHUTDOWN_GRACE);
                let _ = clear_hook_inbox(instance_id);
                Err(SelectionError::EventTapFailed)
            }
        }
    }

    fn stop(&mut self) -> Result<(), SelectionError> {
        self.stop_requested.store(true, Ordering::Release);
        // WM_QUIT is only a prompt wake-up. The atomic flag is authoritative,
        // so a transient PostThreadMessage failure cannot leave join waiting
        // forever for a message notification.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(join) = self.join.take() {
            if !reap_worker(join, WORKER_SHUTDOWN_GRACE) {
                let _ = clear_hook_inbox(self.instance_id);
                return Err(SelectionError::Internal);
            }
        }
        let _ = clear_hook_inbox(self.instance_id);
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for HookThread {
    fn drop(&mut self) {
        if self.join.is_some() {
            let _ = self.stop();
        }
    }
}

fn hook_thread_main(
    inbox: Sender<WorkerMessage>,
    ready_sender: SyncSender<Result<u32, SelectionError>>,
    published_thread_id: Arc<AtomicU32>,
    stop_requested: Arc<AtomicBool>,
    instance_id: u64,
) {
    let exit_inbox = inbox.clone();
    // Calling PeekMessage creates this thread's message queue before its ID is
    // published to the worker, eliminating the PostThreadMessage startup race.
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    published_thread_id.store(thread_id, Ordering::Release);
    if stop_requested.load(Ordering::Acquire) {
        let _ = ready_sender.send(Err(SelectionError::EventTapFailed));
        return;
    }
    if !set_hook_inbox(instance_id, Some(inbox)) {
        let _ = ready_sender.send(Err(SelectionError::EventTapFailed));
        return;
    }

    let mut mouse_hook =
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_callback), None, 0) } {
            Ok(hook) => hook,
            Err(_) => {
                clear_hook_inbox(instance_id);
                let _ = ready_sender.send(Err(SelectionError::EventTapFailed));
                return;
            }
        };
    let mut keyboard_hook =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_callback), None, 0) } {
            Ok(hook) => hook,
            Err(_) => {
                let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
                clear_hook_inbox(instance_id);
                let _ = ready_sender.send(Err(SelectionError::EventTapFailed));
                return;
            }
        };
    let foreground_hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_event_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if stop_requested.load(Ordering::Acquire) {
        if !foreground_hook.is_invalid() {
            let _ = unsafe { UnhookWinEvent(foreground_hook) };
        }
        let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
        let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
        clear_hook_inbox(instance_id);
        let _ = ready_sender.send(Err(SelectionError::EventTapFailed));
        return;
    }
    let _ = ready_sender.send(Ok(thread_id));
    let mut reinstall_due = Instant::now() + HOOK_REINSTALL_INTERVAL;

    'message_loop: loop {
        if stop_requested.load(Ordering::Acquire) {
            break;
        }
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            if message.message == WM_QUIT || stop_requested.load(Ordering::Acquire) {
                break 'message_loop;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if Instant::now() >= reinstall_due {
            // Install the replacement pair before retiring the old pair. This
            // keeps the input path covered while Windows refreshes the hook.
            // Duplicate records from the short overlap are filtered in the
            // callbacks by the OS-provided low-level event timestamp.
            let replacement_mouse =
                match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_callback), None, 0) }
                {
                    Ok(hook) => hook,
                    Err(error) => {
                        eprintln!("[selection] failed to refresh Windows mouse hook: {error}");
                        break 'message_loop;
                    }
                };
            let replacement_keyboard = match unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_callback), None, 0)
            } {
                Ok(hook) => hook,
                Err(error) => {
                    let _ = unsafe { UnhookWindowsHookEx(replacement_mouse) };
                    eprintln!("[selection] failed to refresh Windows keyboard hook: {error}");
                    break 'message_loop;
                }
            };
            let old_mouse = mouse_hook;
            let old_keyboard = keyboard_hook;
            mouse_hook = replacement_mouse;
            keyboard_hook = replacement_keyboard;
            let _ = unsafe { UnhookWindowsHookEx(old_keyboard) };
            let _ = unsafe { UnhookWindowsHookEx(old_mouse) };
            arm_hook_refresh_window();
            reinstall_due = Instant::now() + HOOK_REINSTALL_INTERVAL;
        }
        let _ = unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                HOOK_HEALTH_INTERVAL.as_millis().min(u128::from(u32::MAX)) as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
    }
    if !foreground_hook.is_invalid() {
        let _ = unsafe { UnhookWinEvent(foreground_hook) };
    }
    let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
    let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
    clear_hook_inbox(instance_id);
    let _ = exit_inbox.send(WorkerMessage::HookExited { instance_id });
}

fn next_hook_instance_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::AcqRel)
}

fn hook_inbox() -> &'static Mutex<Option<(u64, Sender<WorkerMessage>)>> {
    static INBOX: OnceLock<Mutex<Option<(u64, Sender<WorkerMessage>)>>> = OnceLock::new();
    INBOX.get_or_init(|| Mutex::new(None))
}

fn set_hook_inbox(instance_id: u64, inbox: Option<Sender<WorkerMessage>>) -> bool {
    let Ok(mut slot) = hook_inbox().lock() else {
        return false;
    };
    *slot = inbox.map(|inbox| (instance_id, inbox));
    true
}

fn clear_hook_inbox(instance_id: u64) -> bool {
    let Ok(mut slot) = hook_inbox().lock() else {
        return false;
    };
    if slot
        .as_ref()
        .is_some_and(|(current, _)| *current == instance_id)
    {
        *slot = None;
    }
    true
}

fn enqueue_hook_input(input: RawInput) {
    let inbox = hook_inbox()
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|(_, inbox)| inbox.clone()));
    if let Some(inbox) = inbox {
        let _ = inbox.send(WorkerMessage::Raw(input));
    }
}

/// Hook refresh briefly leaves both the old and replacement low-level hooks in
/// the chain. Windows supplies the same `time` and payload to both callbacks;
/// scope deduplication to that short window so two genuine wheel events with
/// identical payloads in the same millisecond are never dropped.
fn hook_refresh_clock() -> &'static OnceLock<Instant> {
    static CLOCK: OnceLock<Instant> = OnceLock::new();
    &CLOCK
}

fn hook_refresh_window_ms() -> u64 {
    hook_refresh_clock()
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn hook_refresh_until_ms() -> &'static AtomicU64 {
    static UNTIL_MS: AtomicU64 = AtomicU64::new(0);
    &UNTIL_MS
}

fn hook_refresh_window_active() -> bool {
    let until = hook_refresh_until_ms().load(Ordering::Acquire);
    until != 0 && hook_refresh_window_ms() <= until
}

fn arm_hook_refresh_window() {
    hook_refresh_until_ms().store(
        hook_refresh_window_ms().saturating_add(100),
        Ordering::Release,
    );
}

fn is_duplicate_low_level_hook_event(
    kind: u64,
    message: u32,
    event_time: u32,
    first: u64,
    second: u64,
    extra: u64,
) -> bool {
    static LAST_FINGERPRINT: AtomicU64 = AtomicU64::new(0);
    let mut fingerprint = 0x9E37_79B9_7F4A_7C15_u64;
    for value in [
        kind,
        u64::from(message),
        u64::from(event_time),
        first,
        second,
        extra,
    ] {
        fingerprint ^= value.wrapping_add(0x9E37_79B9_7F4A_7C15);
        fingerprint = fingerprint
            .rotate_left(27)
            .wrapping_mul(0x94D0_49BB_1331_11EB);
    }
    let fingerprint = fingerprint.max(1);
    LAST_FINGERPRINT.swap(fingerprint, Ordering::AcqRel) == fingerprint
}

unsafe extern "system" fn mouse_hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // SAFETY: Windows supplies a valid MSLLHOOKSTRUCT for HC_ACTION during
        // this callback. We copy the point and retain no borrowed pointer.
        let message = wparam.0 as u32;
        if matches!(
            message,
            WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_MBUTTONDOWN
                | WM_XBUTTONDOWN
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        ) {
            let input = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let duplicate = hook_refresh_window_active()
                && is_duplicate_low_level_hook_event(
                    1,
                    message,
                    input.time,
                    input.pt.x as i64 as u64,
                    input.pt.y as i64 as u64,
                    u64::from(input.mouseData)
                        ^ (u64::from(input.flags) << 32)
                        ^ input.dwExtraInfo as u64,
                );
            if !duplicate {
                // Wheel is "soft" input: dismiss toolbar, but do not advance
                // the capture generation. Multi-paragraph PDF selections
                // commonly emit inertia wheel events after mouse-up, and a
                // valid accessibility read should not be discarded for that.
                let generation = if matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
                    hook_generation().load(Ordering::Acquire)
                } else {
                    hook_generation().fetch_add(1, Ordering::AcqRel) + 1
                };
                enqueue_hook_input(RawInput::Mouse {
                    sequence: next_raw_input_sequence(),
                    message,
                    point: RawPoint {
                        x: input.pt.x,
                        y: input.pt.y,
                    },
                    event_time: input.time,
                    generation,
                    modifiers: current_modifiers(),
                    timestamp_ms: strict_parent_timestamp_ms(),
                });
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        // SAFETY: Windows supplies a valid KBDLLHOOKSTRUCT for HC_ACTION during
        // this callback. Only scalar fields are copied into the queue.
        let message = wparam.0 as u32;
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
            let input = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            // The clipboard transaction uses standard injected Ctrl+C events
            // (matching selection-hook) so readers do not see an application-
            // specific marker. Ignore only that injected copy chord here; real
            // keyboard events, including manual Ctrl+C/V, still cancel capture.
            let injected_copy_key = input.flags.0 & LLKHF_INJECTED.0 != 0
                && matches!(
                    input.vkCode as u16,
                    key if key == VK_C.0 || key == VK_CONTROL.0 || key == VK_RCONTROL.0
                );
            if injected_copy_key {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            let duplicate = is_duplicate_low_level_hook_event(
                2,
                message,
                input.time,
                u64::from(input.vkCode),
                u64::from(input.scanCode),
                (u64::from(input.flags.0) << 32) ^ input.dwExtraInfo as u64,
            );
            if !duplicate {
                let generation = if is_modifier_virtual_key(input.vkCode as u16) {
                    hook_generation().load(Ordering::Acquire)
                } else {
                    hook_generation().fetch_add(1, Ordering::AcqRel) + 1
                };
                enqueue_hook_input(RawInput::Keyboard {
                    sequence: next_raw_input_sequence(),
                    message,
                    virtual_key: input.vkCode,
                    generation,
                    modifiers: current_modifiers(),
                    timestamp_ms: strict_parent_timestamp_ms(),
                });
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn foreground_event_callback(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    event: u32,
    window: HWND,
    _object_id: i32,
    _child_id: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND || window.0 == ptr::null_mut() {
        return;
    }
    enqueue_hook_input(RawInput::Foreground {
        sequence: next_raw_input_sequence(),
        window: window.0 as isize,
        // Foreground activation commonly lands between the mouse down/up that
        // moved focus from a TextLens result back to the source application.
        // Treating it as fresh user input cancels that legitimate capture.
        // Capture paths already validate the foreground HWND/PID during
        // and after capture, so unrelated programmatic focus changes still
        // fail closed without advancing the input generation here.
        generation: hook_generation().load(Ordering::Acquire),
        timestamp_ms: strict_parent_timestamp_ms(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_click_tries_other_patterns_after_preferred_document_misses() {
        let candidates = vec![("child", false), ("document", true)];
        let ordered = order_text_pattern_candidates(candidates.clone(), true);
        assert_eq!(ordered, vec!["document", "child"]);
        // Non-double-click captures keep the former single-provider priority.
        assert_eq!(
            order_text_pattern_candidates(candidates, false),
            vec!["document"]
        );

        let mut attempted = Vec::new();
        let result = first_double_click_candidate(ordered, |candidate| {
            attempted.push(candidate);
            if candidate == "child" {
                AccessibilityCapture::Selection(test_selection(SelectionTrigger::DoubleClick))
            } else {
                AccessibilityCapture::NotFound
            }
        });
        assert!(matches!(result, AccessibilityCapture::Selection(_)));
        assert_eq!(attempted, vec!["document", "child"]);
    }

    #[test]
    fn double_click_stops_on_protected_candidate() {
        let mut attempted = Vec::new();
        let result = first_double_click_candidate(["protected", "child"], |candidate| {
            attempted.push(candidate);
            AccessibilityCapture::Protected
        });
        assert!(matches!(result, AccessibilityCapture::Protected));
        assert_eq!(attempted, vec!["protected"]);
        assert!(matches!(
            first_double_click_candidate(std::iter::empty::<()>(), |_| unreachable!()),
            AccessibilityCapture::NotFound
        ));
    }

    #[test]
    fn text_pattern_preference_matches_previous_non_double_click_order() {
        assert_eq!(
            order_text_pattern_candidates(vec![("first", false), ("second", false)], false),
            vec!["first"]
        );
        assert_eq!(
            order_text_pattern_candidates(
                vec![("child", false), ("inner", true), ("outer", true)],
                true,
            ),
            vec!["outer", "child", "inner"]
        );
    }

    fn test_worker(event_sender: Sender<SelectionEvent>) -> SelectionWorker {
        let (inbox, _receiver) = mpsc::channel();
        SelectionWorker {
            inbox,
            event_sender: Arc::new(Mutex::new(event_sender)),
            own_process_id: 100,
            capture_settings: SelectionCaptureSettings::default(),
            capture_coordinator: None,
            hook_thread: None,
            desired_listening: false,
            hook_restart_due: None,
            hook_exit_check_due: None,
            hook_health_due: None,
            mouse_down: None,
            mouse_down_at: None,
            mouse_down_on_self: false,
            mouse_down_target_root: 0,
            mouse_down_target_window: 0,
            mouse_down_shift: false,
            mouse_down_unmodified: false,
            mouse_down_clipboard_sequence: None,
            last_mouse_up: None,
            last_click: None,
            keyboard_selection_key: None,
            pending_capture: None,
            last_automatic_fingerprint: None,
            recent_capture: None,
            last_raw_sequence: 0,
        }
    }

    fn test_selection(trigger: SelectionTrigger) -> SelectionPayload {
        SelectionPayload {
            text: "selected text".to_owned(),
            source_app: SourceApplication {
                bundle_id: r"C:\Windows\System32\notepad.exe".to_owned(),
                name: "Notepad".to_owned(),
            },
            bounds: None,
            start_top: None,
            start_bottom: None,
            end_top: None,
            end_bottom: None,
            mouse: SelectionMouse {
                start: Some(SelectionPoint { x: 10.0, y: 10.0 }),
                end: Some(SelectionPoint { x: 80.0, y: 10.0 }),
                current: SelectionPoint { x: 80.0, y: 10.0 },
            },
            direction: SelectionDirection::Forward,
            is_fullscreen: false,
            method: SelectionMethod::Accessibility,
            trigger,
            timestamp_ms: 1,
            sequence: 0,
            windows_selection_evidence: None,
        }
    }

    #[test]
    fn captured_text_normalization_preserves_unicode_and_layout() {
        assert_eq!(
            normalize_captured_text("  中文\r\n第二行\r第三行\0\u{7}\t  "),
            "中文\n第二行\n第三行"
        );
    }

    #[test]
    fn document_chrome_names_do_not_win_over_pdf_selection_fallback() {
        let pdf = SourceApplication {
            bundle_id: r"C:\Program Files\DocBox\docbox.exe".to_owned(),
            name: "DocBox".to_owned(),
        };
        let editor = SourceApplication {
            bundle_id: r"C:\Windows\notepad.exe".to_owned(),
            name: "Notepad".to_owned(),
        };

        assert!(accessibility_text_looks_like_document_chrome(
            "AVPageView",
            &pdf
        ));
        assert!(accessibility_text_looks_like_document_chrome(
            "PdfPageCanvas",
            &pdf
        ));
        assert!(!accessibility_text_looks_like_document_chrome(
            "Big Data processing",
            &pdf
        ));
        assert!(!accessibility_text_looks_like_document_chrome(
            "AVPageView",
            &editor
        ));

        let acrobat_selection = SelectionPayload {
            text: "Acrobat".to_owned(),
            source_app: SourceApplication {
                bundle_id: r"C:\Program Files\Adobe\Acrobat\Acrobat.exe".to_owned(),
                name: "Adobe Acrobat".to_owned(),
            },
            bounds: Some(SelectionBounds {
                x: 20.0,
                y: 20.0,
                width: 120.0,
                height: 24.0,
            }),
            start_top: None,
            start_bottom: None,
            end_top: None,
            end_bottom: None,
            mouse: SelectionMouse {
                start: None,
                end: None,
                current: SelectionPoint { x: 20.0, y: 20.0 },
            },
            direction: SelectionDirection::Forward,
            is_fullscreen: false,
            method: SelectionMethod::Accessibility,
            trigger: SelectionTrigger::Drag,
            timestamp_ms: 0,
            sequence: 0,
            windows_selection_evidence: None,
        };
        assert!(!accessibility_selection_payload_is_plausible(
            &acrobat_selection,
            HWND::default()
        ));
    }

    #[test]
    fn external_foreground_preserves_an_active_mouse_gesture_across_root_changes() {
        assert_eq!(
            foreground_interaction_decision(true, 200, 100, true, false, false),
            ForegroundInteractionDecision::PreserveMouseGesture
        );
    }

    #[test]
    fn foreground_events_ignore_textlens_and_dismiss_only_without_active_work() {
        assert_eq!(
            foreground_interaction_decision(true, 100, 100, true, false, false),
            ForegroundInteractionDecision::Ignore
        );
        assert_eq!(
            foreground_interaction_decision(false, 0, 100, true, false, false),
            ForegroundInteractionDecision::Ignore
        );
        assert_eq!(
            foreground_interaction_decision(true, 200, 100, false, false, false),
            ForegroundInteractionDecision::Dismiss
        );
        assert_eq!(
            foreground_interaction_decision(true, 200, 100, true, true, false),
            ForegroundInteractionDecision::Dismiss
        );
    }

    #[test]
    fn external_foreground_after_mouse_up_preserves_pending_capture() {
        assert_eq!(
            foreground_interaction_decision(true, 200, 100, false, false, true),
            ForegroundInteractionDecision::PreservePendingCapture
        );
    }

    #[test]
    fn foreground_after_mouse_up_keeps_the_original_capture_source() {
        let original_due = Instant::now() + Duration::from_millis(12);
        let mut pending = Some(PendingCapture {
            due: original_due,
            expires_at: Instant::now() + FOREGROUND_SETTLE_TIMEOUT,
            scheduled_at: Instant::now(),
            request: CaptureRequest {
                observation_generation: None,
                observation_only: false,
                trigger: SelectionTrigger::Drag,
                start: Some(RawPoint { x: 10, y: 10 }),
                end: Some(RawPoint { x: 80, y: 10 }),
                current: RawPoint { x: 80, y: 10 },
                generation: Some(7),
                clipboard_sequence_at_start: None,
                press_duration_ms: 0,
                capture_strategy: SelectionCaptureStrategy::SelectionHook,
            },
            source_root_window: 42,
            source_process_id: 100,
            process_parents: None,
            empty_attempt: 0,
        });

        correlate_pending_capture_with_foreground(&mut pending, 43, 100, 8);
        let pending = pending.expect("pending capture remains available");
        assert_eq!(pending.source_root_window, 42);
        assert_eq!(pending.source_process_id, 100);
        assert_eq!(pending.request.generation, Some(8));
        assert!(pending.due <= original_due);
    }

    #[test]
    fn older_foreground_generation_cannot_rewind_a_pending_capture() {
        let mut pending = Some(PendingCapture {
            due: Instant::now() + Duration::from_millis(12),
            expires_at: Instant::now() + FOREGROUND_SETTLE_TIMEOUT,
            scheduled_at: Instant::now(),
            request: CaptureRequest {
                observation_generation: None,
                observation_only: false,
                trigger: SelectionTrigger::Drag,
                start: Some(RawPoint { x: 10, y: 10 }),
                end: Some(RawPoint { x: 80, y: 10 }),
                current: RawPoint { x: 80, y: 10 },
                generation: Some(9),
                clipboard_sequence_at_start: None,
                press_duration_ms: 0,
                capture_strategy: SelectionCaptureStrategy::SelectionHook,
            },
            source_root_window: 42,
            source_process_id: 100,
            process_parents: None,
            empty_attempt: 0,
        });

        correlate_pending_capture_with_foreground(&mut pending, 43, 100, 8);
        assert_eq!(
            pending.expect("pending capture").request.generation,
            Some(9)
        );
    }

    #[test]
    fn foreground_fills_only_a_missing_capture_source() {
        let mut pending = Some(PendingCapture {
            due: Instant::now() + Duration::from_millis(12),
            expires_at: Instant::now() + FOREGROUND_SETTLE_TIMEOUT,
            scheduled_at: Instant::now(),
            request: CaptureRequest {
                observation_generation: None,
                observation_only: false,
                trigger: SelectionTrigger::Drag,
                start: Some(RawPoint { x: 10, y: 10 }),
                end: Some(RawPoint { x: 80, y: 10 }),
                current: RawPoint { x: 80, y: 10 },
                generation: Some(7),
                clipboard_sequence_at_start: None,
                press_duration_ms: 0,
                capture_strategy: SelectionCaptureStrategy::SelectionHook,
            },
            source_root_window: 0,
            source_process_id: 0,
            process_parents: None,
            empty_attempt: 0,
        });

        correlate_pending_capture_with_foreground(&mut pending, 43, 200, 8);
        let pending = pending.expect("pending capture remains available");
        assert_eq!(pending.source_root_window, 43);
        assert_eq!(pending.source_process_id, 200);
        assert_eq!(pending.request.generation, Some(8));
    }

    #[test]
    fn late_same_source_foreground_is_correlated_with_the_completed_capture() {
        let (event_sender, _event_receiver) = mpsc::channel();
        let mut worker = test_worker(event_sender);
        worker.recent_capture = Some(RecentCaptureContext {
            generation: 12,
            source_root_window: 42,
            source_process_id: 100,
            valid_until: Instant::now() + RECENT_CAPTURE_FOREGROUND_GRACE,
        });

        assert!(worker.recent_capture_matches_foreground(42, 100, 12));
        assert!(!worker.recent_capture_matches_foreground(42, 100, 13));
        assert!(worker.recent_capture.is_none());
    }

    #[test]
    fn pending_capture_does_not_wait_on_an_external_renderer_or_popup() {
        assert_eq!(
            pending_foreground_decision(true, 200, 100, true, true),
            PendingForegroundDecision::Capture
        );
        assert_eq!(
            pending_foreground_decision(true, 999, 100, false, true),
            PendingForegroundDecision::Capture
        );
    }

    #[test]
    fn pending_capture_waits_only_for_missing_or_textlens_foreground() {
        assert_eq!(
            pending_foreground_decision(false, 0, 100, false, true),
            PendingForegroundDecision::Wait
        );
        assert_eq!(
            pending_foreground_decision(true, 100, 100, false, true),
            PendingForegroundDecision::Wait
        );
        assert_eq!(
            pending_foreground_decision(false, 0, 100, false, false),
            PendingForegroundDecision::Expire
        );
        assert_eq!(
            pending_foreground_decision(true, 100, 100, false, false),
            PendingForegroundDecision::Expire
        );
    }

    #[test]
    fn helper_selection_result_emits_exactly_one_automatic_event() {
        let (event_sender, event_receiver) = mpsc::channel();
        let mut worker = test_worker(event_sender);
        let generation = hook_generation().load(Ordering::Acquire);
        let request = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 80, y: 10 }),
            current: RawPoint { x: 80, y: 10 },
            generation: Some(generation),
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        let selection = test_selection(SelectionTrigger::Drag);

        assert!(worker.deliver_captured_selection(request, selection.clone(), 42, 200));
        assert!(!worker.deliver_captured_selection(request, selection, 42, 200));
        assert!(matches!(
            event_receiver.recv_timeout(Duration::from_millis(50)),
            Ok(SelectionEvent::Selection(_))
        ));
        assert!(matches!(
            event_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[test]
    fn invalid_uia_bounds_fall_back_to_the_release_point_without_losing_text() {
        let (event_sender, event_receiver) = mpsc::channel();
        let mut worker = test_worker(event_sender);
        let generation = hook_generation().load(Ordering::Acquire);
        let request = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 80, y: 10 }),
            current: RawPoint { x: 80, y: 10 },
            generation: Some(generation),
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        let mut selection = test_selection(SelectionTrigger::Drag);
        selection.bounds = Some(SelectionBounds {
            x: 5_000.0,
            y: 5_000.0,
            width: 100.0,
            height: 20.0,
        });

        assert!(worker.deliver_captured_selection(request, selection, 42, 200));
        let SelectionEvent::Selection(selection) = event_receiver
            .recv_timeout(Duration::from_millis(50))
            .expect("selection event")
        else {
            panic!("expected selection event");
        };
        assert_eq!(selection.text, "selected text");
        assert_eq!(selection.bounds, None);
    }

    #[test]
    fn double_click_event_requires_verified_accessibility_text_selection() {
        let (event_sender, event_receiver) = mpsc::channel();
        let mut worker = test_worker(event_sender);
        let request = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::DoubleClick,
            start: Some(RawPoint { x: 80, y: 10 }),
            end: Some(RawPoint { x: 80, y: 10 }),
            current: RawPoint { x: 80, y: 10 },
            generation: Some(hook_generation().load(Ordering::Acquire)),
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::Clipboard,
        };
        let mut valid = test_selection(SelectionTrigger::DoubleClick);
        valid.windows_selection_evidence = Some(WindowsSelectionEvidence::UiaRangeAtPoint);
        valid.bounds = Some(SelectionBounds {
            x: 70.0,
            y: 0.0,
            width: 30.0,
            height: 20.0,
        });
        let mut missing_bounds = valid.clone();
        missing_bounds.bounds = None;
        let mut distant_bounds = valid.clone();
        distant_bounds.bounds.as_mut().expect("bounds").x = 500.0;
        let mut short_text = valid.clone();
        short_text.text = "字".to_owned();
        let mut copied = valid.clone();
        copied.method = SelectionMethod::Clipboard;
        let mut wrong_trigger = valid.clone();
        wrong_trigger.trigger = SelectionTrigger::Drag;
        let mut wrong_pointer = valid.clone();
        wrong_pointer.mouse.current.x = 79.0;
        for rejected in [
            missing_bounds,
            distant_bounds,
            short_text,
            copied,
            wrong_trigger,
            wrong_pointer,
        ] {
            assert!(!worker.deliver_captured_selection(request, rejected, 42, 200));
        }
        assert!(matches!(
            event_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(worker.deliver_captured_selection(request, valid, 42, 200));
        assert!(matches!(
            event_receiver.try_recv(),
            Ok(SelectionEvent::Selection(_))
        ));
    }

    #[test]
    fn changed_selection_without_geometry_can_emit_a_double_click_event() {
        let (event_sender, event_receiver) = mpsc::channel();
        let mut worker = test_worker(event_sender);
        let request = CaptureRequest {
            observation_generation: Some(7),
            observation_only: false,
            trigger: SelectionTrigger::DoubleClick,
            start: Some(RawPoint { x: 80, y: 10 }),
            end: Some(RawPoint { x: 80, y: 10 }),
            current: RawPoint { x: 80, y: 10 },
            generation: Some(hook_generation().load(Ordering::Acquire)),
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::Clipboard,
        };
        let mut selection = test_selection(SelectionTrigger::DoubleClick);
        selection.bounds = None;
        selection.windows_selection_evidence = Some(WindowsSelectionEvidence::UiaRangeChanged);
        assert!(worker.deliver_captured_selection(request, selection, 42, 200));
        assert!(matches!(
            event_receiver.try_recv(),
            Ok(SelectionEvent::Selection(SelectionPayload {
                bounds: None,
                ..
            }))
        ));
    }

    #[test]
    fn first_click_observation_never_emits_retries_or_injects_copy() {
        let (event_sender, event_receiver) = mpsc::channel();
        let mut worker = test_worker(event_sender);
        for strategy in [
            SelectionCaptureStrategy::SelectionHook,
            SelectionCaptureStrategy::Auto,
            SelectionCaptureStrategy::Clipboard,
        ] {
            let request = CaptureRequest {
                observation_only: true,
                observation_generation: Some(7),
                trigger: SelectionTrigger::DoubleClick,
                start: None,
                end: None,
                current: RawPoint { x: 80, y: 10 },
                generation: Some(hook_generation().load(Ordering::Acquire)),
                clipboard_sequence_at_start: None,
                press_duration_ms: 0,
                capture_strategy: strategy,
            };
            assert!(!request.may_inject_copy());
            assert!(!should_retry_empty_capture(&request));
            assert!(!worker.deliver_captured_selection(
                request,
                test_selection(SelectionTrigger::DoubleClick),
                42,
                200,
            ));
            assert!(!CaptureRequest {
                observation_only: false,
                ..request
            }
            .may_inject_copy());
        }
        assert!(matches!(
            event_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[test]
    fn adaptive_route_abbreviates_only_after_a_recent_miss_streak() {
        let fresh = Duration::ZERO;
        let stale = ADAPTIVE_ROUTE_REPROBE_INTERVAL;
        // Below the threshold every app keeps the full UIA retry sequence.
        assert!(!adaptive_route_should_abbreviate_uia_retries(0, fresh));
        assert!(!adaptive_route_should_abbreviate_uia_retries(
            ADAPTIVE_ROUTE_MISS_THRESHOLD - 1,
            fresh
        ));
        // At or past the threshold a recent streak omits only the delayed UIA
        // retry; it never removes the first UIA probe.
        assert!(adaptive_route_should_abbreviate_uia_retries(
            ADAPTIVE_ROUTE_MISS_THRESHOLD,
            fresh
        ));
        assert!(adaptive_route_should_abbreviate_uia_retries(u8::MAX, fresh));
        // A stale streak must restore the full retry sequence.
        assert!(!adaptive_route_should_abbreviate_uia_retries(
            u8::MAX,
            stale
        ));
    }

    #[test]
    fn adaptive_route_only_learns_from_rounds_that_actually_tried_uia() {
        // UIA produced the selection → the app works, drop any streak.
        assert!(matches!(
            adaptive_route_update(false, Some(SelectionMethod::Accessibility)),
            AdaptiveRouteUpdate::Clear
        ));
        // UIA was tried and missed → extend the streak.
        assert!(matches!(
            adaptive_route_update(false, None),
            AdaptiveRouteUpdate::Extend
        ));
        // A shortened retry sequence → no evidence either way about the
        // omitted delayed retry, so leave the streak/timer alone.
        assert!(matches!(
            adaptive_route_update(true, None),
            AdaptiveRouteUpdate::Leave
        ));
        // A shortened round that still reported Accessibility
        // can only mean UIA works, so it must still clear.
        assert!(matches!(
            adaptive_route_update(true, Some(SelectionMethod::Accessibility)),
            AdaptiveRouteUpdate::Clear
        ));
    }

    #[test]
    fn multi_process_suites_share_an_application_family() {
        assert!(executables_share_application_family(
            r"C:\Program Files\WPS Office\office6\wps.exe",
            r"C:\Program Files\WPS Office\office6\wps.exe",
        ));
        assert!(executables_share_application_family(
            r"C:\Program Files\WPS Office\office6\wps.exe",
            r"C:\Program Files\WPS Office\office6\promecefpluginhost.exe",
        ));
        assert!(executables_share_application_family(
            r"C:\WPS\et.exe",
            r"C:\WPS\wpspdf.exe",
        ));
        assert!(executables_share_application_family(
            r"C:\Program Files\Tencent\WeChat\WeChat.exe",
            r"C:\Program Files\Tencent\WeChat\WeChatAppEx.exe",
        ));
        assert!(executables_share_application_family(
            r"C:\Program Files\Tencent\QQNT\QQ.exe",
            r"C:\Program Files\Tencent\QQNT\QQNT.exe",
        ));
        assert!(wps_suite_process("wps.exe"));
        assert!(wps_suite_process("promecefpluginhost.exe"));
        assert!(docbox_suite_process("docbox.exe"));
        assert!(executables_share_application_family(
            r"C:\Program Files\DocBox\DocBox.exe",
            r"C:\Program Files\DocBox\DocBoxRenderer.exe",
        ));
        // Unrelated hosts must not collapse into a suite family.
        assert!(!executables_share_application_family(
            r"C:\Program Files\WPS Office\office6\wps.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        ));
        assert!(!executables_share_application_family(
            r"C:\Program Files\WPS Office\office6\wps.exe",
            r"C:\Program Files\Tencent\WeChat\WeChat.exe",
        ));
        assert!(!executables_share_application_family(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        ));
    }

    #[test]
    fn protected_process_snapshot_names_are_valid_routing_identities() {
        let mut buffer = [0u16; 32];
        let executable = "AcroCEF_Renderer.exe".encode_utf16().collect::<Vec<_>>();
        buffer[..executable.len()].copy_from_slice(&executable);
        assert_eq!(
            decode_process_executable_name(&buffer).as_deref(),
            Some("AcroCEF_Renderer.exe")
        );
        assert_eq!(decode_process_executable_name(&[]), None);
        assert_eq!(decode_process_executable_name(&[0; 4]), None);
        assert_eq!(
            clipboard_capture_profile(
                decode_process_executable_name(&buffer)
                    .as_deref()
                    .expect("snapshot executable")
            ),
            ClipboardCaptureProfile::Acrobat
        );
    }

    #[test]
    fn known_document_hosts_get_an_accessibility_probe_budget() {
        for path in [
            r"C:\Program Files\Adobe\Acrobat\acrobat.exe",
            r"C:\Program Files\Foxit Software\Foxit PDF Reader\FoxitPDFReader.exe",
            r"C:\Tools\SumatraPDF\SumatraPDF.exe",
            r"C:\Tools\PDF-XChange Editor\PDFXEdit.exe",
            r"C:\WPS\office6\wpspdf.exe",
            r"C:\Users\Administrator\AppData\Local\Programs\DocBox\DocBox.exe",
            r"C:\Program Files\Microsoft Office\root\Office16\WINWORD.EXE",
            r"C:\Program Files\Microsoft Office\root\Office16\POWERPNT.EXE",
            r"C:\Program Files\Microsoft Office\root\Office16\EXCEL.EXE",
        ] {
            assert!(
                document_accessibility_probe_budget(path).is_some(),
                "{path}"
            );
        }
        assert_eq!(
            document_accessibility_probe_budget(
                r"C:\Program Files\Google\Chrome\Application\chrome.exe"
            ),
            None
        );
        assert_eq!(
            document_accessibility_probe_budget(r"C:\Tools\SumatraPDF\SumatraPDF.exe"),
            Some(PDF_ACCESSIBILITY_PROBE_BUDGET)
        );
        assert_eq!(
            document_accessibility_probe_budget(
                r"C:\Program Files\Microsoft Office\root\Office16\EXCEL.EXE"
            ),
            Some(OFFICE_ACCESSIBILITY_PROBE_BUDGET)
        );
        assert_eq!(
            document_accessibility_probe_budget(
                r"C:\Users\Administrator\AppData\Local\Programs\DocBox\DocBox.exe"
            ),
            Some(PDF_ACCESSIBILITY_PROBE_BUDGET)
        );
    }

    #[test]
    fn clipboard_fallback_is_blocked_for_sensitive_hosts_and_recovers_late_source_writes() {
        assert!(prohibited_clipboard_application(
            r"C:\Windows\System32\cmd.exe"
        ));
        assert!(prohibited_clipboard_application(
            r"C:\Program Files\1Password\1Password.exe"
        ));
        assert_eq!(
            clipboard_recovery_decision(12, 13, Some(12), true),
            ClipboardRecoveryDecision::RestoreOriginal
        );
        // A clipboard update already visible when this drag began is a user
        // operation, even when the source application happens to own it.
        assert_eq!(
            clipboard_recovery_decision(12, 13, Some(13), true),
            ClipboardRecoveryDecision::Drop
        );
        assert_eq!(
            clipboard_recovery_decision(12, 13, Some(12), false),
            ClipboardRecoveryDecision::Drop
        );
        assert_eq!(
            clipboard_recovery_decision(12, 12, Some(12), true),
            ClipboardRecoveryDecision::Drop
        );
    }

    #[test]
    fn configured_direct_copy_rules_override_the_selection_hook_default() {
        let settings = SelectionCaptureSettings::default();
        assert_eq!(
            capture_strategy_for_application(
                &settings,
                r"C:\Program Files\Adobe\Acrobat\AcroCEF.exe"
            ),
            SelectionCaptureStrategy::Clipboard
        );
        assert_eq!(
            capture_strategy_for_application(&settings, r"C:\Program Files\EmEditor\EmEditor.exe"),
            SelectionCaptureStrategy::Clipboard
        );
        assert_eq!(
            capture_strategy_for_application(&settings, r"C:\Windows\System32\notepad.exe"),
            SelectionCaptureStrategy::SelectionHook
        );
        assert_eq!(
            capture_strategy_for_application(
                &settings,
                r"C:\Program Files\Adobe\Acrobat\AcroCEF_Renderer.exe"
            ),
            SelectionCaptureStrategy::Clipboard
        );

        let settings = SelectionCaptureSettings {
            default_strategy: SelectionCaptureStrategy::Auto,
            applications: vec![crate::models::SelectionCaptureRule {
                application: "acrobat.exe".to_owned(),
                strategy: SelectionCaptureStrategy::SelectionHook,
            }],
        };
        assert_eq!(
            capture_strategy_for_application(
                &settings,
                r"C:\Program Files\Adobe\Acrobat\Acrobat.exe"
            ),
            SelectionCaptureStrategy::SelectionHook
        );
        assert_eq!(
            capture_strategy_for_application(&settings, r"C:\Windows\System32\notepad.exe"),
            SelectionCaptureStrategy::Auto
        );

        let legacy_settings = SelectionCaptureSettings {
            default_strategy: SelectionCaptureStrategy::SelectionHook,
            applications: Vec::new(),
        };
        // Compatibility routing heals older/hand-edited settings that predate
        // the visible rules. Explicit rules above remain a user opt-out.
        assert_eq!(
            capture_strategy_for_application(
                &legacy_settings,
                r"C:\Program Files\Adobe\Acrobat\Acrobat.exe"
            ),
            SelectionCaptureStrategy::Clipboard
        );
        assert_eq!(
            capture_strategy_for_application(
                &legacy_settings,
                r"C:\Program Files\DocBox\DocBoxHelper.exe"
            ),
            SelectionCaptureStrategy::Clipboard
        );
        assert_eq!(
            capture_strategy_for_application(
                &legacy_settings,
                r"C:\Program Files\EmEditor\EmEditor.exe"
            ),
            SelectionCaptureStrategy::Clipboard
        );
        assert_eq!(
            capture_strategy_for_application(&legacy_settings, r"C:\Windows\notepad.exe"),
            SelectionCaptureStrategy::SelectionHook
        );
    }

    #[test]
    fn acrobat_and_docbox_use_fast_direct_copy_profiles() {
        let acrobat = clipboard_capture_profile(r"C:\Program Files\Adobe\Acrobat\Acrobat.exe");
        assert_eq!(acrobat, ClipboardCaptureProfile::Acrobat);
        assert_eq!(acrobat.poll_interval(), PDF_CLIPBOARD_POLL_INTERVAL);
        assert_eq!(
            acrobat.copy_poll_attempts(),
            ACROBAT_CLIPBOARD_POLL_ATTEMPTS
        );
        assert_eq!(
            acrobat.copy_dispatch_attempts(),
            ACROBAT_COPY_DISPATCH_ATTEMPTS
        );
        assert_eq!(acrobat.copy_retry_delay(), ACROBAT_COPY_RETRY_DELAY);
        assert_eq!(acrobat.phase_budget(), PDF_CLIPBOARD_PHASE_BUDGET);
        assert_eq!(
            acrobat.text_ready_budget(),
            ACROBAT_CLIPBOARD_TEXT_READY_BUDGET
        );
        assert_eq!(
            acrobat.text_settle_delay(),
            ACROBAT_CLIPBOARD_TEXT_SETTLE_DELAY
        );
        assert!(acrobat.text_ready_budget() > acrobat.text_settle_delay());
        assert_eq!(acrobat.required_sequence_stable_polls(), 0);
        assert!(acrobat.retries_copy_after_timeout());
        assert!(acrobat.retries_copy_after_unreadable_text());
        assert!(acrobat.allows_ole_delayed_text());
        assert!(!acrobat.requires_target_clipboard_owner());
        assert!(acrobat.accepts_clipboard_owner(false));
        assert!(acrobat.uses_native_password_probe());

        let docbox = clipboard_capture_profile(r"C:\Program Files\DocBox\DocBox.exe");
        assert_eq!(docbox, ClipboardCaptureProfile::PdfCanvas);
        assert_eq!(docbox.poll_interval(), PDF_CLIPBOARD_POLL_INTERVAL);
        assert_eq!(docbox.copy_poll_attempts(), PDF_CLIPBOARD_POLL_ATTEMPTS);
        assert_eq!(docbox.copy_dispatch_attempts(), PDF_COPY_DISPATCH_ATTEMPTS);
        assert_eq!(docbox.copy_retry_delay(), PDF_COPY_RETRY_DELAY);
        assert_eq!(docbox.text_ready_budget(), PDF_CLIPBOARD_TEXT_READY_BUDGET);
        assert_eq!(docbox.text_settle_delay(), PDF_CLIPBOARD_TEXT_SETTLE_DELAY);
        assert_eq!(docbox.required_sequence_stable_polls(), 0);
        assert!(docbox.retries_copy_after_timeout());
        assert!(!docbox.retries_copy_after_unreadable_text());
        assert!(!docbox.allows_ole_delayed_text());
        assert!(!docbox.requires_target_clipboard_owner());
        assert!(docbox.accepts_clipboard_owner(false));
        assert!(docbox.uses_native_password_probe());

        let foxit = clipboard_capture_profile(r"C:\Program Files\Foxit\FoxitReader.exe");
        assert_eq!(foxit, ClipboardCaptureProfile::PdfCanvas);

        let editor = clipboard_capture_profile(r"C:\Windows\notepad.exe");
        assert_eq!(editor, ClipboardCaptureProfile::General);
        assert_eq!(editor.poll_interval(), CLIPBOARD_POLL_INTERVAL);
        assert_eq!(editor.copy_poll_attempts(), CLIPBOARD_POLL_ATTEMPTS);
        assert_eq!(editor.copy_dispatch_attempts(), 1);
        assert_eq!(editor.copy_retry_delay(), Duration::ZERO);
        assert_eq!(editor.phase_budget(), CLIPBOARD_PHASE_BUDGET);
        assert_eq!(
            editor.text_ready_budget(),
            GENERAL_CLIPBOARD_TEXT_READY_BUDGET
        );
        assert_eq!(
            editor.text_settle_delay(),
            GENERAL_CLIPBOARD_TEXT_SETTLE_DELAY
        );
        assert_eq!(
            editor.required_sequence_stable_polls(),
            CLIPBOARD_STABLE_POLLS
        );
        assert!(!editor.retries_copy_after_timeout());
        assert!(!editor.retries_copy_after_unreadable_text());
        assert!(!editor.allows_ole_delayed_text());
        assert!(editor.requires_target_clipboard_owner());
        assert!(!editor.accepts_clipboard_owner(false));
        assert!(!editor.uses_native_password_probe());
    }

    #[test]
    fn clipboard_text_must_settle_before_the_snapshot_is_restored() {
        let now = Instant::now();
        let since = now - GENERAL_CLIPBOARD_TEXT_SETTLE_DELAY;
        assert!(clipboard_text_candidate_is_settled(
            Some((42, since)),
            42,
            now,
            GENERAL_CLIPBOARD_TEXT_SETTLE_DELAY
        ));
        assert!(!clipboard_text_candidate_is_settled(
            Some((42, now)),
            42,
            now,
            GENERAL_CLIPBOARD_TEXT_SETTLE_DELAY
        ));
        assert!(!clipboard_text_candidate_is_settled(
            Some((41, since)),
            42,
            now,
            GENERAL_CLIPBOARD_TEXT_SETTLE_DELAY
        ));
    }

    #[test]
    fn unknown_password_state_blocks_clipboard_unless_profile_is_copy_compatible() {
        assert!(password_state_blocks_clipboard(Some(true), true));
        assert!(!password_state_blocks_clipboard(Some(false), false));
        assert!(password_state_blocks_clipboard(None, false));
        assert!(!password_state_blocks_clipboard(None, true));
    }

    #[test]
    fn pdf_clipboard_owner_accepts_target_and_system_brokers_only() {
        assert!(pdf_clipboard_owner_is_acceptable(true, Some(42), None));
        assert!(pdf_clipboard_owner_is_acceptable(false, None, None));
        // Unknown owner image keeps the historical acceptance.
        assert!(pdf_clipboard_owner_is_acceptable(false, Some(42), None));
        assert!(pdf_clipboard_owner_is_acceptable(
            false,
            Some(42),
            Some(r"C:\Windows\explorer.exe")
        ));
        assert!(pdf_clipboard_owner_is_acceptable(
            false,
            Some(42),
            Some(r"C:\Windows\System32\dllhost.exe")
        ));
        assert!(!pdf_clipboard_owner_is_acceptable(
            false,
            Some(42),
            Some(r"C:\Apps\clipboard-manager.exe")
        ));
    }

    #[test]
    fn clipboard_owner_accepts_both_sides_of_a_document_process_family() {
        let parents = HashMap::from([(201, 200), (202, 201), (301, 300)]);
        // Acrobat's visible page may be an AcroCEF child while Acrobat.exe
        // owns the clipboard update.
        assert!(clipboard_owner_pid_matches_target(200, 202, Some(&parents)));
        assert!(clipboard_owner_pid_matches_target(202, 200, Some(&parents)));
        assert!(!clipboard_owner_pid_matches_target(
            300,
            202,
            Some(&parents)
        ));
    }

    #[test]
    fn office_hosts_use_native_accessibility_first() {
        for path in [
            r"C:\Program Files\Microsoft Office\root\Office16\EXCEL.EXE",
            r"C:\Program Files\Microsoft Office\root\Office16\POWERPNT.EXE",
            r"C:\Program Files\WPS Office\office6\et.exe",
            r"C:\Program Files\WPS Office\office6\wpp.exe",
            r"C:\Program Files\LibreOffice\program\scalc.exe",
            r"C:\Program Files\LibreOffice\program\simpress.exe",
        ] {
            assert!(office_accessibility_first_application(path), "{path}");
        }
        assert!(!office_accessibility_first_application(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe"
        ));
    }

    #[test]
    fn accessibility_point_probes_prioritize_mouse_up_without_duplicates() {
        let request = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 20 }),
            end: Some(RawPoint { x: 80, y: 20 }),
            current: RawPoint { x: 80, y: 20 },
            generation: Some(1),
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        assert_eq!(
            accessible_point_candidates(request),
            vec![RawPoint { x: 80, y: 20 }, RawPoint { x: 10, y: 20 }]
        );

        let click = CaptureRequest {
            start: Some(RawPoint { x: 80, y: 20 }),
            ..request
        };
        assert_eq!(
            accessible_point_candidates(click),
            vec![RawPoint { x: 80, y: 20 }]
        );
        assert_eq!(ACCESSIBLE_POINT_PROBE_LIMIT, 2);
        assert!(ACCESSIBLE_WINDOW_PROBE_LIMIT >= PDF_ACCESSIBLE_WINDOW_PROBE_LIMIT);
    }

    #[test]
    fn native_edit_fallback_accepts_only_text_controls_and_valid_ranges() {
        for class_name in [
            "Edit",
            "RichEdit20W",
            "RichEditD2DPT",
            "WindowsForms10.EDIT.app.0.2bf8098_r8_ad1",
            "ThunderRT6TextBox",
        ] {
            assert!(is_native_text_control_class(class_name), "{class_name}");
        }
        for class_name in ["Chrome_WidgetWin_1", "Scintilla", "ApplicationFrameWindow"] {
            assert!(!is_native_text_control_class(class_name), "{class_name}");
        }

        let text = "A中文B".encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            native_selection_text_from_utf16(&text, text.len(), 1, 3),
            Some("中文".to_owned())
        );
        assert_eq!(
            native_selection_text_from_utf16(&text, text.len(), 2, 2),
            None
        );
        assert_eq!(
            native_selection_text_from_utf16(&text, text.len(), 1, 5),
            None
        );
        assert_eq!(NATIVE_TEXT_CONTROL_PROBE_LIMIT, 6);
    }

    #[test]
    fn document_title_filters_cover_file_stems_and_product_labels() {
        assert!(window_title_component_matches(
            "* report.pdf - Adobe Acrobat Pro DC",
            "report"
        ));
        assert!(window_title_component_matches(
            "draft.docx - Word",
            "draft.docx"
        ));
        assert!(!window_title_component_matches(
            "research.pdf - Adobe Acrobat",
            "researcher"
        ));

        let acrobat = SourceApplication {
            bundle_id: r"C:\Program Files\Adobe\Acrobat\Acrobat.exe".to_owned(),
            name: "Acrobat".to_owned(),
        };
        assert!(selection_text_matches_known_host_label(
            "Adobe Acrobat Pro DC",
            &acrobat
        ));
        assert!(!captured_text_is_usable("\u{fffd}broken"));
        assert!(captured_text_is_usable("normal selected text"));
    }

    #[test]
    fn longer_drags_settle_longer_before_capture() {
        assert_eq!(capture_settle_delay_for_distance(0), CAPTURE_SETTLE_DELAY);
        assert_eq!(
            capture_settle_delay_for_distance(CAPTURE_SETTLE_MEDIUM_DISTANCE_SQUARED - 1),
            CAPTURE_SETTLE_DELAY
        );
        assert_eq!(
            capture_settle_delay_for_distance(CAPTURE_SETTLE_MEDIUM_DISTANCE_SQUARED),
            CAPTURE_SETTLE_MEDIUM_DELAY
        );
        assert_eq!(
            capture_settle_delay_for_distance(CAPTURE_SETTLE_LONG_DISTANCE_SQUARED),
            CAPTURE_SETTLE_LONG_DELAY
        );
        // Multi-paragraph vertical drag (~200 px) must use the long settle.
        assert_eq!(
            capture_settle_delay_for_distance(200 * 200),
            CAPTURE_SETTLE_LONG_DELAY
        );
        // Slow careful multi-line press lengthens settle after a meaningful
        // vertical movement; a slow same-line click remains on the fast path.
        let slow = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 20, y: 50 }),
            current: RawPoint { x: 20, y: 50 },
            generation: None,
            clipboard_sequence_at_start: None,
            press_duration_ms: SLOW_PRESS_MS,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        assert_eq!(
            capture_settle_delay_for_request(&slow),
            CAPTURE_SETTLE_SLOW_PRESS_DELAY
        );
        // A tall drag keeps the quick UIA probe; geometry alone must not route
        // browsers, editors or Office through a slower fallback route.
        let long = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 10, y: 200 }),
            current: RawPoint { x: 10, y: 200 },
            generation: None,
            clipboard_sequence_at_start: None,
            press_duration_ms: 100,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        assert_eq!(
            capture_settle_delay_for_request(&long),
            CAPTURE_SETTLE_MEDIUM_DELAY
        );
        assert_eq!(
            capture_settle_delay_for_application(&long, Some("AcroRd32.exe")),
            PDF_CAPTURE_SETTLE_DELAY
        );
        assert_eq!(
            capture_settle_delay_for_application(&long, Some("WINWORD.EXE")),
            CAPTURE_SETTLE_MEDIUM_DELAY
        );
        let long_same_line = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 320, y: 10 }),
            current: RawPoint { x: 320, y: 10 },
            generation: None,
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        assert_eq!(
            capture_settle_delay_for_request(&long_same_line),
            CAPTURE_SETTLE_MEDIUM_DELAY
        );
        assert!(!long_document_selection_needs_late_retry(&long_same_line));
    }

    #[test]
    fn mouse_driven_empty_captures_are_retried_and_scroll_preserves_pending() {
        let drag = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 80, y: 40 }),
            current: RawPoint { x: 80, y: 40 },
            generation: Some(1),
            clipboard_sequence_at_start: None,
            press_duration_ms: 200,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        assert!(should_retry_empty_capture(&drag));
        assert_eq!(max_empty_capture_retries(&drag), 1);
        let long = CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Drag,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 10, y: 220 }),
            current: RawPoint { x: 10, y: 220 },
            generation: Some(1),
            clipboard_sequence_at_start: None,
            press_duration_ms: 600,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        };
        assert!(long_document_selection_needs_late_retry(&long));
        assert_eq!(
            max_empty_capture_retries(&long),
            EMPTY_CAPTURE_RETRY_DELAYS.len()
        );
        assert_eq!(
            empty_capture_retry_delay(&drag, 1),
            EMPTY_CAPTURE_RETRY_DELAYS[0]
        );
        assert_eq!(
            empty_capture_retry_delay(&drag, 2),
            EMPTY_CAPTURE_RETRY_DELAYS[0]
        );
        let clipboard_drag = CaptureRequest {
            capture_strategy: SelectionCaptureStrategy::Clipboard,
            ..drag
        };
        assert_eq!(
            empty_capture_retry_delay(&clipboard_drag, 1),
            CLIPBOARD_EMPTY_CAPTURE_RETRY_DELAY
        );
        assert!(should_retry_empty_capture(&CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::DoubleClick,
            start: Some(RawPoint { x: 10, y: 10 }),
            end: Some(RawPoint { x: 10, y: 10 }),
            current: RawPoint { x: 10, y: 10 },
            generation: None,
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        }));
        assert!(!should_retry_empty_capture(&CaptureRequest {
            observation_generation: None,
            observation_only: false,
            trigger: SelectionTrigger::Keyboard,
            start: None,
            end: None,
            current: RawPoint { x: 0, y: 0 },
            generation: None,
            clipboard_sequence_at_start: None,
            press_duration_ms: 0,
            capture_strategy: SelectionCaptureStrategy::SelectionHook,
        }));
        assert!(!mouse_message_clears_pending_capture(WM_MOUSEWHEEL));
        assert!(!mouse_message_clears_pending_capture(WM_MOUSEHWHEEL));
        assert!(mouse_message_clears_pending_capture(WM_LBUTTONDOWN));
        assert!(mouse_message_clears_pending_capture(WM_LBUTTONUP));
    }

    #[test]
    fn parent_accepts_family_foreground_and_requires_current_generation() {
        // Exact HWND + generation are required for every non-destructive
        // accessibility capture.
        assert!(parent_capture_accepts(
            true, 200, 200, 200, 100, false, true, true, true
        ));
        assert!(!parent_capture_accepts(
            true, 200, 200, 200, 100, false, true, false, true
        ));
        // Sibling suite process (WPS CEF) without exact HWND.
        assert!(parent_capture_accepts(
            false, 9001, 9002, 9003, 100, true, true, true, true
        ));
        // Unrelated process still fails closed.
        assert!(!parent_capture_accepts(
            false, 300, 200, 200, 100, false, true, true, false
        ));
        // Never accept TextLens itself as the actual provider.
        assert!(!parent_capture_accepts(
            true, 200, 200, 100, 100, true, false, true, false
        ));
        // A provider outside the original process family cannot borrow the
        // source identity captured at mouse-down.
        assert!(!parent_capture_accepts(
            true, 200, 200, 300, 100, false, false, true, false
        ));
    }

    #[test]
    fn electron_uia_elements_must_stay_inside_the_foreground_process_tree() {
        let parents = HashMap::from([(101, 100), (102, 101), (201, 200)]);
        assert!(uia_processes_belong_to_target_with_parents(
            100, 101, 102, &parents
        ));
        assert!(uia_processes_belong_to_target_with_parents(
            100, 100, 101, &parents
        ));
        // A host parent is not a provider owned by its renderer child.
        assert!(!uia_processes_belong_to_target_with_parents(
            101, 100, 100, &parents
        ));
        assert!(!uia_processes_belong_to_target_with_parents(
            100, 201, 0, &parents
        ));
        assert!(!uia_processes_belong_to_target_with_parents(
            100, 101, 201, &parents
        ));
    }

    #[test]
    fn capture_source_accepts_renderer_children_but_not_the_launcher() {
        // 1 (Explorer) -> 100 (application) -> 101 (renderer)
        let mut parents = Some(Some(HashMap::from([(100, 1), (101, 100)])));
        assert!(process_belongs_to_capture_source(101, 100, &mut parents));
        assert!(!process_belongs_to_capture_source(100, 101, &mut parents));
        assert!(!process_belongs_to_capture_source(1, 100, &mut parents));
    }

    #[test]
    fn same_process_uia_fast_path_never_takes_a_process_snapshot() {
        let mut process_parents = None;
        assert!(uia_processes_belong_to_target(
            100,
            100,
            100,
            &mut process_parents
        ));
        assert!(process_parents.is_none());
    }

    #[test]
    fn accessibility_retries_and_ancestor_budget_match_windows_profiles() {
        assert_eq!(CAPTURE_SETTLE_DELAY, Duration::from_millis(12));
        assert_eq!(
            ACCESSIBILITY_RETRY_DELAYS,
            [Duration::ZERO, Duration::from_millis(28)]
        );
        assert_eq!(
            CAPTURE_SETTLE_DELAY + ACCESSIBILITY_RETRY_DELAYS[0],
            Duration::from_millis(12)
        );
        assert_eq!(
            CAPTURE_SETTLE_DELAY + ACCESSIBILITY_RETRY_DELAYS[1],
            Duration::from_millis(40)
        );
        assert_eq!(UIA_PHASE_BUDGET, Duration::from_millis(180));
        assert_eq!(MAX_UIA_ANCESTORS, 32);
        assert_eq!(MAX_UIA_SELECTION_RANGES, 256);
        assert_eq!(PDF_ACCESSIBLE_WINDOW_PROBE_LIMIT, 3);
        assert_eq!(PDF_ACCESSIBILITY_PROBE_BUDGET, Duration::from_millis(36));
        assert_eq!(OFFICE_ACCESSIBILITY_PROBE_BUDGET, Duration::from_millis(56));
        assert_eq!(ADAPTIVE_ROUTE_MISS_THRESHOLD, 2);
        assert_eq!(EMPTY_CAPTURE_RETRY_DELAYS, [Duration::from_millis(72)]);
        assert_eq!(CAPTURE_SETTLE_LONG_DELAY, Duration::from_millis(120));
        assert_eq!(CAPTURE_SETTLE_SLOW_PRESS_DELAY, Duration::from_millis(48));
        assert_eq!(PDF_CAPTURE_SETTLE_DELAY, Duration::from_millis(16));
        assert_eq!(LATE_RETRY_VERTICAL_DISTANCE, 64);
        assert_eq!(LATE_RETRY_SLOW_PRESS_VERTICAL_DISTANCE, 32);
        assert_eq!(SLOW_PRESS_MS, 350);
        assert_eq!(HELPER_MAINTENANCE_INTERVAL, Duration::from_secs(1));

        // The common path should reach its first UIA query within one frame.
        // Hosts without a provider get a separate, bounded clipboard phase.
        let first_uia_query = CAPTURE_SETTLE_DELAY + ACCESSIBILITY_RETRY_DELAYS[0];
        let final_uia_query = CAPTURE_SETTLE_DELAY
            + *ACCESSIBILITY_RETRY_DELAYS
                .last()
                .expect("the UIA retry schedule cannot be empty");
        assert!(first_uia_query <= Duration::from_millis(16));
        assert!(final_uia_query <= Duration::from_millis(100));
        assert!(ACCESSIBILITY_RETRY_DELAYS
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        assert!(CAPTURE_TASK_TIMEOUT > final_uia_query);
        assert!(CAPTURE_ENGINE_BUDGET < CAPTURE_TASK_TIMEOUT);
        assert!(CAPTURE_ENGINE_BUDGET >= UIA_PHASE_BUDGET);
        assert_eq!(CLIPBOARD_PHASE_BUDGET, Duration::from_millis(1_200));
        assert_eq!(CLIPBOARD_STABLE_POLLS, 2);
        assert!(CAPTURE_TASK_TIMEOUT >= CLIPBOARD_PHASE_BUDGET);
    }

    #[test]
    fn helper_protocol_round_trips_without_starting_the_test_harness_as_a_child() {
        let command = HelperCommand::Capture {
            request_id: 7,
            request: CaptureRequest {
                observation_generation: Some(17),
                observation_only: true,
                trigger: SelectionTrigger::Drag,
                start: Some(RawPoint { x: -20, y: 30 }),
                end: Some(RawPoint { x: 40, y: 50 }),
                current: RawPoint { x: 40, y: 50 },
                generation: Some(19),
                clipboard_sequence_at_start: None,
                press_duration_ms: 0,
                capture_strategy: SelectionCaptureStrategy::SelectionHook,
            },
            source_window: 123,
            source_process_id: 456,
            textlens_process_id: 789,
        };
        let mut bytes = Vec::new();
        write_helper_frame(&mut bytes, &command).expect("serialize helper command");
        let decoded = read_helper_frame::<HelperCommand>(&mut bytes.as_slice())
            .expect("read helper command")
            .expect("helper command frame");
        match decoded {
            HelperCommand::Capture {
                request_id,
                request,
                source_window,
                source_process_id,
                textlens_process_id,
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(request.current, RawPoint { x: 40, y: 50 });
                assert_eq!(request.generation, Some(19));
                assert_eq!(request.observation_generation, Some(17));
                assert!(request.observation_only);
                assert_eq!(source_window, 123);
                assert_eq!(source_process_id, 456);
                assert_eq!(textlens_process_id, 789);
            }
            _ => panic!("expected capture command"),
        }
    }

    #[test]
    fn helper_shutdown_cancels_any_active_request() {
        assert_eq!(
            helper_cancel_reason_from_code(helper_cancel_reason_code(HelperCancelReason::Shutdown)),
            Some(HelperCancelReason::Shutdown)
        );
        assert_eq!(
            helper_cancel_reason_from_code(helper_cancel_reason_code(
                HelperCancelReason::Superseded
            )),
            Some(HelperCancelReason::Superseded)
        );
        assert!(CAPTURE_ENGINE_BUDGET < CAPTURE_TASK_TIMEOUT);
    }

    #[test]
    fn helper_protocol_is_versioned() {
        let event = HelperEvent::Ready {
            protocol_version: SELECTION_HELPER_PROTOCOL_VERSION,
        };
        let mut bytes = Vec::new();
        write_helper_frame(&mut bytes, &event).expect("serialize ready event");
        assert!(matches!(
            read_helper_frame::<HelperEvent>(&mut bytes.as_slice())
                .expect("read ready event")
                .expect("ready event frame"),
            HelperEvent::Ready { protocol_version }
                if protocol_version == SELECTION_HELPER_PROTOCOL_VERSION
        ));
    }

    #[test]
    fn raw_input_sequence_is_strictly_monotonic() {
        let first = next_raw_input_sequence();
        let second = next_raw_input_sequence();
        assert!(second > first);
        let first_timestamp = strict_parent_timestamp_ms();
        let second_timestamp = strict_parent_timestamp_ms();
        assert!(second_timestamp > first_timestamp);
        assert!(MAX_MESSAGES_PER_TICK > 0);
    }

    #[test]
    fn modifier_keys_are_classified_without_async_state_races() {
        for key in [
            VK_SHIFT.0,
            VK_LSHIFT.0,
            VK_RSHIFT.0,
            VK_CONTROL.0,
            VK_LCONTROL.0,
            VK_RCONTROL.0,
            VK_MENU.0,
            VK_LMENU.0,
            VK_RMENU.0,
            VK_LWIN.0,
            VK_RWIN.0,
        ] {
            assert!(is_modifier_virtual_key(key));
        }
        // Caps Lock is user input, not a held shortcut modifier. It must
        // advance the capture generation and dismiss stale selections.
        assert!(!is_modifier_virtual_key(0x14));
        assert!(!is_modifier_virtual_key(VK_A.0));
        assert!(!is_modifier_virtual_key(VK_LEFT.0));
    }

    #[test]
    fn uia_rectangles_remain_physical_virtual_desktop_coordinates() {
        assert_eq!(
            rectangles_from_values(&[-2_400.0, 300.0, 150.0, 30.0]),
            vec![SelectionBounds {
                x: -2_400.0,
                y: 300.0,
                width: 150.0,
                height: 30.0,
            }]
        );
    }

    #[test]
    fn slow_worker_reaping_is_bounded() {
        let worker = thread::spawn(|| thread::sleep(Duration::from_millis(100)));
        let started = Instant::now();
        assert!(!reap_worker(worker, Duration::from_millis(5)));
        assert!(started.elapsed() < Duration::from_millis(80));
    }
}
