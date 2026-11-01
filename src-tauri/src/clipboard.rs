#[cfg(target_os = "macos")]
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
#[cfg(target_os = "macos")]
use objc2_foundation::NSString;

#[cfg(any(target_os = "windows", test))]
const WINDOWS_CLIPBOARD_ATTEMPTS: usize = 8;
#[cfg(any(target_os = "windows", test))]
const WINDOWS_CLIPBOARD_TEXT_LIMIT: usize = 1_000_000;
#[cfg(target_os = "windows")]
const WINDOWS_CLIPBOARD_SNAPSHOT_LIMIT: usize = 64 * 1024 * 1024;

#[cfg(target_os = "windows")]
pub(crate) struct WindowsClipboardSnapshot {
    formats: Vec<WindowsClipboardFormatSnapshot>,
    was_empty: bool,
    sequence: u32,
}

#[cfg(target_os = "windows")]
struct WindowsClipboardFormatSnapshot {
    format: u32,
    data: Vec<u8>,
    kind: WindowsClipboardFormatKind,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
enum WindowsClipboardFormatKind {
    GlobalMemory,
    EnhancedMetafile,
}

#[cfg(target_os = "windows")]
impl WindowsClipboardSnapshot {
    pub(crate) fn sequence(&self) -> u32 {
        self.sequence
    }

    /// Restores the complete OLE data object only while the synthetic copy is
    /// still the newest clipboard write. The snapshot owns materialized bytes
    /// rather than a third-party IDataObject proxy, so repeatedly restoring a
    /// PDF renderer's delayed formats cannot build a recursive OLE proxy chain.
    pub(crate) fn restore_if_unchanged(&self, expected_sequence: u32) -> bool {
        if !windows_clipboard_sequence_matches(expected_sequence) {
            return false;
        }
        restore_windows_clipboard_snapshot(self, expected_sequence)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn write_text(text: &str) -> Result<(), String> {
    let pasteboard = NSPasteboard::generalPasteboard();
    write_to_pasteboard(&pasteboard, text)
}

#[cfg(target_os = "windows")]
pub(crate) fn write_text(text: &str) -> Result<(), String> {
    write_to_windows_clipboard(text)
}

/// Materializes the current Windows clipboard, including an explicitly empty
/// clipboard. Keeping third-party IDataObject proxies across copy/restore
/// cycles can recurse through delayed OLE renderers and overflow the helper's
/// stack, so only self-contained formats are retained here.
#[cfg(target_os = "windows")]
pub(crate) fn snapshot_windows_clipboard() -> Option<WindowsClipboardSnapshot> {
    use std::{slice, thread, time::Duration};

    use windows::Win32::{
        Foundation::{GetLastError, SetLastError, HGLOBAL, WIN32_ERROR},
        Graphics::Gdi::{GetEnhMetaFileBits, HENHMETAFILE},
        System::{
            DataExchange::{
                CloseClipboard, CountClipboardFormats, EnumClipboardFormats, GetClipboardData,
                OpenClipboard,
            },
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::CF_ENHMETAFILE,
        },
    };

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    struct GlobalLockGuard(HGLOBAL);
    impl Drop for GlobalLockGuard {
        fn drop(&mut self) {
            let _ = unsafe { GlobalUnlock(self.0) };
        }
    }

    let sequence = unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
    retry_operation(
        WINDOWS_CLIPBOARD_ATTEMPTS,
        || unsafe { OpenClipboard(None) },
        |retry_index| {
            let delay_ms = 2_u64.saturating_mul((retry_index as u64) + 1).min(12);
            thread::sleep(Duration::from_millis(delay_ms));
        },
    )
    .ok()?;
    let _clipboard = ClipboardGuard;
    if !windows_clipboard_sequence_matches(sequence) {
        return None;
    }

    unsafe { SetLastError(WIN32_ERROR(0)) };
    let format_count = unsafe { CountClipboardFormats() };
    if format_count == 0 {
        if unsafe { GetLastError() }.0 != 0 {
            return None;
        }
        return Some(WindowsClipboardSnapshot {
            formats: Vec::new(),
            was_empty: true,
            sequence,
        });
    }

    let mut formats = Vec::with_capacity(format_count as usize);
    let mut total_bytes = 0usize;
    let mut format = 0u32;
    let mut accounting = WindowsClipboardSnapshotAccounting::default();
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if windows_clipboard_format_is_skipped(format) {
            accounting.observe(format, WindowsClipboardFormatOutcome::NotRestorable);
            continue;
        }

        let handle = unsafe { GetClipboardData(format) }.ok()?;
        let remaining = WINDOWS_CLIPBOARD_SNAPSHOT_LIMIT.saturating_sub(total_bytes);
        let (data, kind) = if format == u32::from(CF_ENHMETAFILE.0) {
            let metafile = HENHMETAFILE(handle.0);
            let size = unsafe { GetEnhMetaFileBits(metafile, None) } as usize;
            if size == 0 {
                accounting.observe(format, WindowsClipboardFormatOutcome::Unreadable);
                continue;
            }
            // Oversized formats are dropped individually: a huge format must
            // not abandon the whole restore snapshot (and with it the small
            // text format that keeps the user's clipboard intact). Pixel
            // formats are held to the fidelity rule below instead.
            if size > remaining {
                accounting.observe(format, WindowsClipboardFormatOutcome::Unreadable);
                continue;
            }
            let mut data = vec![0u8; size];
            if unsafe { GetEnhMetaFileBits(metafile, Some(&mut data)) } as usize != size {
                return None;
            }
            (data, WindowsClipboardFormatKind::EnhancedMetafile)
        } else {
            let global = HGLOBAL(handle.0);
            let size = unsafe { GlobalSize(global) };
            if size == 0 || size > remaining {
                accounting.observe(format, WindowsClipboardFormatOutcome::Unreadable);
                continue;
            }
            let pointer = unsafe { GlobalLock(global) }.cast::<u8>();
            if pointer.is_null() {
                return None;
            }
            let _lock = GlobalLockGuard(global);
            (
                unsafe { slice::from_raw_parts(pointer, size) }.to_vec(),
                WindowsClipboardFormatKind::GlobalMemory,
            )
        };
        total_bytes = total_bytes.saturating_add(data.len());
        accounting.observe(format, WindowsClipboardFormatOutcome::Captured);
        formats.push(WindowsClipboardFormatSnapshot { format, data, kind });
    }

    if !accounting.keeps_every_image() {
        // A screenshot already on the clipboard cannot be rebuilt from what we
        // captured. Our synthetic copy would EmptyClipboard and destroy the one
        // thing the user has not pasted yet, so the fallback is refused.
        trace_windows_clipboard_text_status(sequence, "snapshot-image-unrestorable");
        return None;
    }
    if formats.is_empty() || !windows_clipboard_sequence_matches(sequence) {
        return None;
    }
    windows_clipboard_sequence_matches(sequence).then_some(WindowsClipboardSnapshot {
        formats,
        was_empty: false,
        sequence,
    })
}

#[cfg(target_os = "windows")]
fn windows_clipboard_format_is_skipped(format: u32) -> bool {
    use windows::Win32::System::Ole::{
        CF_BITMAP, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT, CF_DSPTEXT, CF_GDIOBJFIRST,
        CF_GDIOBJLAST, CF_LOCALE, CF_METAFILEPICT, CF_OEMTEXT, CF_OWNERDISPLAY, CF_PALETTE,
        CF_PRIVATEFIRST, CF_PRIVATELAST, CF_TEXT,
    };

    let synthesized_or_unsupported = [
        CF_TEXT,
        CF_OEMTEXT,
        CF_LOCALE,
        CF_BITMAP,
        CF_PALETTE,
        CF_METAFILEPICT,
        CF_OWNERDISPLAY,
        CF_DSPTEXT,
        CF_DSPBITMAP,
        CF_DSPMETAFILEPICT,
        CF_DSPENHMETAFILE,
    ]
    .into_iter()
    .any(|candidate| format == u32::from(candidate.0));
    synthesized_or_unsupported
        || (u32::from(CF_PRIVATEFIRST.0)..=u32::from(CF_PRIVATELAST.0)).contains(&format)
        || (u32::from(CF_GDIOBJFIRST.0)..=u32::from(CF_GDIOBJLAST.0)).contains(&format)
}

/// Win32 formats that carry pixel content, by their fixed ids: CF_BITMAP 2,
/// CF_METAFILEPICT 3, CF_TIFF 6, CF_DIB 8, CF_PALETTE 9, CF_ENHMETAFILE 14,
/// CF_DIBV5 17, CF_OWNERDISPLAY 0x80, CF_DSPBITMAP 0x82, CF_DSPMETAFILEPICT
/// 0x83, CF_DSPENHMETAFILE 0x8E. The ids stay numeric so the fidelity rule
/// below is testable off Windows, and
/// `image_format_ids_match_the_win32_constants` pins them against the crate.
#[cfg(any(target_os = "windows", test))]
fn windows_clipboard_format_carries_image(format: u32) -> bool {
    matches!(
        format,
        2 | 3 | 6 | 8 | 9 | 14 | 17 | 0x80 | 0x82 | 0x83 | 0x8E
    )
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy)]
enum WindowsClipboardFormatOutcome {
    /// Listed by `windows_clipboard_format_is_skipped`: never materialized.
    NotRestorable,
    /// Readable in principle, but returned no bits or exceeded the budget.
    Unreadable,
    Captured,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
struct WindowsClipboardSnapshotAccounting {
    image_present: bool,
    image_captured: bool,
    image_unreadable: bool,
}

#[cfg(any(target_os = "windows", test))]
impl WindowsClipboardSnapshotAccounting {
    fn observe(&mut self, format: u32, outcome: WindowsClipboardFormatOutcome) {
        if !windows_clipboard_format_carries_image(format) {
            return;
        }
        self.image_present = true;
        match outcome {
            WindowsClipboardFormatOutcome::Captured => self.image_captured = true,
            WindowsClipboardFormatOutcome::Unreadable => self.image_unreadable = true,
            WindowsClipboardFormatOutcome::NotRestorable => {}
        }
    }

    /// Pixel content survives the restore once one pixel format was
    /// materialized and none that we could have read was dropped: Windows
    /// re-synthesizes CF_BITMAP from a restored CF_DIB, so a handle-based
    /// format in the skip list is not a loss by itself.
    fn keeps_every_image(&self) -> bool {
        !self.image_present || (self.image_captured && !self.image_unreadable)
    }
}

#[cfg(target_os = "windows")]
enum PreparedWindowsClipboardHandle {
    Global(Option<windows::Win32::Foundation::HGLOBAL>),
    EnhancedMetafile(Option<windows::Win32::Graphics::Gdi::HENHMETAFILE>),
}

#[cfg(target_os = "windows")]
impl PreparedWindowsClipboardHandle {
    fn raw(&self) -> windows::Win32::Foundation::HANDLE {
        use windows::Win32::Foundation::HANDLE;

        match self {
            Self::Global(Some(handle)) => HANDLE(handle.0),
            Self::EnhancedMetafile(Some(handle)) => HANDLE(handle.0),
            Self::Global(None) | Self::EnhancedMetafile(None) => HANDLE::default(),
        }
    }

    fn relinquish(&mut self) {
        match self {
            Self::Global(handle) => *handle = None,
            Self::EnhancedMetafile(handle) => *handle = None,
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for PreparedWindowsClipboardHandle {
    fn drop(&mut self) {
        use windows::{core::Free, Win32::Graphics::Gdi::DeleteEnhMetaFile};

        match self {
            Self::Global(handle) => {
                if let Some(mut handle) = handle.take() {
                    unsafe { handle.free() };
                }
            }
            Self::EnhancedMetafile(handle) => {
                if let Some(handle) = handle.take() {
                    let _ = unsafe { DeleteEnhMetaFile(Some(handle)) };
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn prepare_windows_clipboard_format(
    snapshot: &WindowsClipboardFormatSnapshot,
) -> Option<PreparedWindowsClipboardHandle> {
    use std::ptr;

    use windows::Win32::{
        Graphics::Gdi::SetEnhMetaFileBits,
        System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
    };

    match snapshot.kind {
        WindowsClipboardFormatKind::GlobalMemory => {
            let global = unsafe { GlobalAlloc(GMEM_MOVEABLE, snapshot.data.len()) }.ok()?;
            let pointer = unsafe { GlobalLock(global) }.cast::<u8>();
            if pointer.is_null() {
                let mut global = global;
                use windows::core::Free;
                unsafe { global.free() };
                return None;
            }
            unsafe {
                ptr::copy_nonoverlapping(snapshot.data.as_ptr(), pointer, snapshot.data.len())
            };
            let _ = unsafe { GlobalUnlock(global) };
            Some(PreparedWindowsClipboardHandle::Global(Some(global)))
        }
        WindowsClipboardFormatKind::EnhancedMetafile => {
            let metafile = unsafe { SetEnhMetaFileBits(&snapshot.data) };
            (!metafile.is_invalid()).then_some(PreparedWindowsClipboardHandle::EnhancedMetafile(
                Some(metafile),
            ))
        }
    }
}

#[cfg(target_os = "windows")]
fn restore_windows_clipboard_snapshot(
    snapshot: &WindowsClipboardSnapshot,
    expected_sequence: u32,
) -> bool {
    use std::{thread, time::Duration};

    use windows::{
        core::w,
        Win32::{
            Foundation::HWND,
            System::DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
            },
            UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
            },
        },
    };

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    struct ClipboardOwnerWindow(HWND);
    impl Drop for ClipboardOwnerWindow {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    let mut prepared = if snapshot.was_empty {
        Vec::new()
    } else {
        let Some(prepared) = snapshot
            .formats
            .iter()
            .map(|format| prepare_windows_clipboard_format(format).map(|handle| (format, handle)))
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        prepared
    };

    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("TextLens Clipboard Restore Owner"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    }
    .map(ClipboardOwnerWindow);
    let Ok(owner) = owner else {
        return false;
    };
    if retry_operation(
        WINDOWS_CLIPBOARD_ATTEMPTS,
        || unsafe { OpenClipboard(Some(owner.0)) },
        |retry_index| {
            let delay_ms = 2_u64.saturating_mul((retry_index as u64) + 1).min(12);
            thread::sleep(Duration::from_millis(delay_ms));
        },
    )
    .is_err()
    {
        return false;
    }
    let _clipboard = ClipboardGuard;
    if !windows_clipboard_sequence_matches(expected_sequence)
        || unsafe { EmptyClipboard() }.is_err()
    {
        return false;
    }

    for (format, handle) in &mut prepared {
        if unsafe { SetClipboardData(format.format, Some(handle.raw())) }.is_err() {
            return false;
        }
        handle.relinquish();
    }
    true
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_clipboard_sequence() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(target_os = "windows")]
pub(crate) fn read_windows_clipboard_text(
    expected_sequence: u32,
    allow_ole_delayed_rendering: bool,
) -> Option<String> {
    match read_windows_clipboard_text_win32(expected_sequence) {
        Ok(text) => Some(text),
        Err(status)
            if allow_ole_delayed_rendering && clipboard_read_can_fallback_to_ole(status) =>
        {
            match read_windows_clipboard_text_ole(expected_sequence) {
                Ok(text) => {
                    trace_windows_clipboard_text_status(expected_sequence, "ole-read-success");
                    Some(text)
                }
                Err(ole_status) => trace_windows_clipboard_text_miss(expected_sequence, ole_status),
            }
        }
        Err(status) => trace_windows_clipboard_text_miss(expected_sequence, status),
    }
}

#[cfg(target_os = "windows")]
fn clipboard_read_can_fallback_to_ole(status: &str) -> bool {
    matches!(
        status,
        "no-text-format"
            | "open-busy"
            | "data-unavailable"
            | "invalid-size"
            | "lock-failed"
            | "unicode-decode-failed"
    )
}

#[cfg(target_os = "windows")]
fn read_windows_clipboard_text_win32(expected_sequence: u32) -> Result<String, &'static str> {
    use std::slice;

    use windows::Win32::{
        Foundation::HGLOBAL,
        System::{
            DataExchange::{
                CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
            },
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::{CF_OEMTEXT, CF_TEXT, CF_UNICODETEXT},
        },
    };

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    struct GlobalLockGuard(HGLOBAL);
    impl Drop for GlobalLockGuard {
        fn drop(&mut self) {
            let _ = unsafe { GlobalUnlock(self.0) };
        }
    }

    if !windows_clipboard_sequence_matches(expected_sequence) {
        return Err("sequence-changed");
    }
    let format = if unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)) }.is_ok() {
        CF_UNICODETEXT
    } else if unsafe { IsClipboardFormatAvailable(u32::from(CF_TEXT.0)) }.is_ok() {
        CF_TEXT
    } else if unsafe { IsClipboardFormatAvailable(u32::from(CF_OEMTEXT.0)) }.is_ok() {
        CF_OEMTEXT
    } else {
        return Err("no-text-format");
    };

    // Selection capture owns the outer retry loop. Do not block one probe for
    // over 100 ms while a PDF renderer temporarily owns the clipboard: a
    // short failed probe lets that loop re-check cancellation, source focus,
    // and a newer clipboard sequence before trying again.
    if unsafe { OpenClipboard(None) }.is_err() {
        return Err("open-busy");
    }
    let _clipboard = ClipboardGuard;
    if !windows_clipboard_sequence_matches(expected_sequence) {
        return Err("sequence-raced-open");
    }

    let handle =
        unsafe { GetClipboardData(u32::from(format.0)) }.map_err(|_| "data-unavailable")?;
    let global = HGLOBAL(handle.0);
    if format == CF_UNICODETEXT {
        let (text, nonstandard_layout) =
            read_windows_unicode_hglobal(global).map_err(|error| match error {
                UnicodeHGlobalReadError::LockFailed => "lock-failed",
                UnicodeHGlobalReadError::RegionUnreadable
                | UnicodeHGlobalReadError::RegionInvalid
                | UnicodeHGlobalReadError::InvalidSize => "invalid-size",
                UnicodeHGlobalReadError::DecodeFailed => "unicode-decode-failed",
            })?;
        if nonstandard_layout {
            trace_windows_clipboard_text_status(
                expected_sequence,
                "win32-nonstandard-layout-bounded",
            );
        }
        if !windows_clipboard_sequence_matches(expected_sequence) {
            return Err("sequence-raced-read");
        }
        if text.chars().count() > WINDOWS_CLIPBOARD_TEXT_LIMIT {
            return Err("text-over-limit");
        }
        return Ok(text);
    }
    let byte_length = unsafe { GlobalSize(global) };
    if byte_length == 0 {
        return Err("invalid-size");
    }
    let pointer = unsafe { GlobalLock(global) }.cast::<u8>();
    if pointer.is_null() {
        return Err("lock-failed");
    }
    let _lock = GlobalLockGuard(global);
    let inspected_bytes = byte_length.min(WINDOWS_CLIPBOARD_TEXT_LIMIT.saturating_mul(4));
    let bytes = unsafe { slice::from_raw_parts(pointer, inspected_bytes) };
    let codepage = if format == CF_TEXT {
        windows::Win32::Globalization::CP_ACP
    } else {
        windows::Win32::Globalization::CP_OEMCP
    };
    let text = decode_windows_clipboard_ansi(bytes, codepage).ok_or("ansi-decode-failed")?;
    if !windows_clipboard_sequence_matches(expected_sequence) {
        return Err("sequence-raced-read");
    }
    if text.chars().count() > WINDOWS_CLIPBOARD_TEXT_LIMIT {
        return Err("text-over-limit");
    }
    Ok(text)
}

/// Acrobat can expose CF_UNICODETEXT through an OLE IDataObject whose
/// GetClipboardData handle reports a zero GlobalSize. Request one materialized
/// STGMEDIUM and release it immediately; retaining the proxy would recreate
/// the recursive delayed-rendering chain that clipboard snapshots avoid.
#[cfg(target_os = "windows")]
fn read_windows_clipboard_text_ole(expected_sequence: u32) -> Result<String, &'static str> {
    use windows::Win32::System::{
        Com::{DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL},
        Ole::{OleGetClipboard, ReleaseStgMedium, CF_UNICODETEXT},
    };

    struct StgMediumGuard(STGMEDIUM);
    impl Drop for StgMediumGuard {
        fn drop(&mut self) {
            unsafe { ReleaseStgMedium(&mut self.0) };
        }
    }

    if !windows_clipboard_sequence_matches(expected_sequence) {
        return Err("ole-sequence-changed");
    }
    let data_object = unsafe { OleGetClipboard() }.map_err(|_| "ole-get-clipboard-failed")?;
    if !windows_clipboard_sequence_matches(expected_sequence) {
        return Err("ole-sequence-raced-object");
    }
    let format = FORMATETC {
        cfFormat: CF_UNICODETEXT.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let medium = unsafe { data_object.GetData(&format) }.map_err(|_| "ole-get-data-failed")?;
    let medium = StgMediumGuard(medium);
    if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
        return Err("ole-invalid-medium");
    }
    let global = unsafe { medium.0.u.hGlobal };
    let (text, nonstandard_layout) =
        read_windows_unicode_hglobal(global).map_err(|error| match error {
            UnicodeHGlobalReadError::LockFailed => "ole-lock-failed",
            UnicodeHGlobalReadError::RegionUnreadable => "ole-region-unreadable",
            UnicodeHGlobalReadError::RegionInvalid => "ole-region-invalid",
            UnicodeHGlobalReadError::InvalidSize => "ole-invalid-size",
            UnicodeHGlobalReadError::DecodeFailed => "ole-unicode-decode-failed",
        })?;
    if nonstandard_layout {
        trace_windows_clipboard_text_status(expected_sequence, "ole-nonstandard-layout-bounded");
    }
    if !windows_clipboard_sequence_matches(expected_sequence) {
        return Err("ole-sequence-raced-read");
    }
    if text.chars().count() > WINDOWS_CLIPBOARD_TEXT_LIMIT {
        return Err("ole-text-over-limit");
    }
    Ok(text)
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnicodeHGlobalReadError {
    LockFailed,
    RegionUnreadable,
    RegionInvalid,
    InvalidSize,
    DecodeFailed,
}

#[cfg(target_os = "windows")]
fn read_windows_unicode_hglobal(
    global: windows::Win32::Foundation::HGLOBAL,
) -> Result<(String, bool), UnicodeHGlobalReadError> {
    use std::slice;

    use windows::Win32::System::Memory::{
        GlobalLock, GlobalSize, GlobalUnlock, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
        PAGE_GUARD, PAGE_NOACCESS,
    };

    struct GlobalLockGuard(windows::Win32::Foundation::HGLOBAL);
    impl Drop for GlobalLockGuard {
        fn drop(&mut self) {
            let _ = unsafe { GlobalUnlock(self.0) };
        }
    }

    let pointer = unsafe { GlobalLock(global) }.cast::<u8>();
    if pointer.is_null() {
        return Err(UnicodeHGlobalReadError::LockFailed);
    }
    let _lock = GlobalLockGuard(global);
    let maximum_bytes = WINDOWS_CLIPBOARD_TEXT_LIMIT
        .saturating_add(1)
        .saturating_mul(std::mem::size_of::<u16>());
    let mut region = MEMORY_BASIC_INFORMATION::default();
    let queried = unsafe {
        VirtualQuery(
            Some(pointer.cast()),
            &mut region,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if queried < std::mem::size_of::<MEMORY_BASIC_INFORMATION>()
        || region.State != MEM_COMMIT
        || region.Protect.0 & (PAGE_GUARD.0 | PAGE_NOACCESS.0) != 0
    {
        return Err(UnicodeHGlobalReadError::RegionUnreadable);
    }
    let Some(region_byte_length) = bounded_readable_region_length(
        region.BaseAddress as usize,
        region.RegionSize,
        pointer as usize,
        maximum_bytes,
    ) else {
        return Err(UnicodeHGlobalReadError::RegionInvalid);
    };
    let reported_byte_length = unsafe { GlobalSize(global) };
    let nonstandard_layout = reported_byte_length < std::mem::size_of::<u16>()
        || reported_byte_length % std::mem::size_of::<u16>() != 0
        || pointer as usize % std::mem::align_of::<u16>() != 0;
    let reported_boundary = if reported_byte_length < std::mem::size_of::<u16>() {
        region_byte_length
    } else {
        reported_byte_length.saturating_add(reported_byte_length % 2)
    };
    let byte_length = region_byte_length.min(reported_boundary).min(maximum_bytes);
    if byte_length < std::mem::size_of::<u16>() {
        return Err(UnicodeHGlobalReadError::InvalidSize);
    }
    let bytes = unsafe { slice::from_raw_parts(pointer, byte_length) };
    let text = decode_windows_clipboard_unicode_bytes(bytes, WINDOWS_CLIPBOARD_TEXT_LIMIT)
        .ok_or(UnicodeHGlobalReadError::DecodeFailed)?;
    Ok((text, nonstandard_layout))
}

#[cfg(any(target_os = "windows", test))]
fn bounded_readable_region_length(
    region_base: usize,
    region_size: usize,
    pointer: usize,
    maximum: usize,
) -> Option<usize> {
    let offset = pointer.checked_sub(region_base)?;
    let available = region_size.checked_sub(offset)?;
    (available > 0 && maximum > 0).then_some(available.min(maximum))
}

#[cfg(target_os = "windows")]
fn trace_windows_clipboard_text_miss(sequence: u32, status: &'static str) -> Option<String> {
    trace_windows_clipboard_text_status(sequence, status);
    None
}

#[cfg(target_os = "windows")]
fn trace_windows_clipboard_text_status(sequence: u32, status: &'static str) {
    use std::{
        cell::RefCell,
        io::Write,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    let trace_directory = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|directory| directory.join("TextLens"));
    let environment_enabled = std::env::var_os("TEXTLENS_SELECTION_TRACE").is_some_and(|value| {
        let value = value.to_string_lossy();
        value == "1" || value.eq_ignore_ascii_case("true")
    });
    if !environment_enabled
        && !trace_directory
            .as_ref()
            .is_some_and(|directory| directory.join("selection-trace.enabled").is_file())
    {
        return;
    }
    thread_local! {
        static LAST_STATUS: RefCell<Option<(u32, &'static str)>> = const { RefCell::new(None) };
    }
    LAST_STATUS.with(|last| {
        let current = Some((sequence, status));
        if *last.borrow() != current {
            let line = format!("[selection-clipboard] sequence={sequence} status={status}");
            eprintln!("{line}");
            if let Some(path) = trace_directory
                .as_ref()
                .map(|directory| directory.join("selection-diagnostic.log"))
            {
                if let Ok(mut output) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    let process_id =
                        unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
                    let _ = writeln!(output, "{timestamp} process={process_id} {line}");
                }
            }
            *last.borrow_mut() = current;
        }
    });
}

#[cfg(target_os = "windows")]
fn windows_clipboard_sequence_matches(expected_sequence: u32) -> bool {
    windows_clipboard_sequence_is_stable(expected_sequence, unsafe {
        windows::Win32::System::DataExchange::GetClipboardSequenceNumber()
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn write_text(_text: &str) -> Result<(), String> {
    Err("当前平台不支持系统剪贴板".to_owned())
}

#[cfg(target_os = "macos")]
fn write_to_pasteboard(pasteboard: &NSPasteboard, text: &str) -> Result<(), String> {
    pasteboard.clearContents();
    let value = NSString::from_str(text);
    // SAFETY: `NSPasteboardTypeString` is an AppKit-provided immutable global
    // whose address is valid for the lifetime of the process.
    let string_type = unsafe { NSPasteboardTypeString };
    pasteboard
        .setString_forType(&value, string_type)
        .then_some(())
        .ok_or_else(|| "无法写入系统剪贴板".to_owned())
}

#[cfg(target_os = "windows")]
fn write_to_windows_clipboard(text: &str) -> Result<(), String> {
    use std::{mem::size_of, ptr, thread, time::Duration};

    use windows::{
        core::{w, Free},
        Win32::{
            Foundation::{GetLastError, SetLastError, HANDLE, HGLOBAL, HWND, WIN32_ERROR},
            System::{
                DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
                Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
            },
            UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WINDOW_STYLE,
            },
        },
    };

    const CF_UNICODETEXT: u32 = 13;

    struct ClipboardGuard;

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            // SAFETY: A guard is constructed only after OpenClipboard succeeds,
            // and exactly one guard owns that open operation.
            let _ = unsafe { CloseClipboard() };
        }
    }

    struct ClipboardOwnerWindow(HWND);

    impl Drop for ClipboardOwnerWindow {
        fn drop(&mut self) {
            // SAFETY: The window was created by this thread and is destroyed
            // only after the clipboard guard has closed the clipboard.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    struct OwnedGlobalMemory(Option<HGLOBAL>);

    impl OwnedGlobalMemory {
        fn relinquish(&mut self) {
            self.0 = None;
        }
    }

    impl Drop for OwnedGlobalMemory {
        fn drop(&mut self) {
            if let Some(mut handle) = self.0.take() {
                // SAFETY: This handle came from GlobalAlloc and ownership has
                // not been transferred to SetClipboardData.
                unsafe { handle.free() };
            }
        }
    }

    let wide = encode_windows_clipboard_text(text)?;
    let allocation_size = wide
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| "复制内容过长".to_owned())?;
    // SAFETY: The requested byte count was checked and GMEM_MOVEABLE is the
    // allocation form required by SetClipboardData.
    let global = unsafe { GlobalAlloc(GMEM_MOVEABLE, allocation_size) }
        .map_err(|error| format!("无法分配剪贴板内存：{error}"))?;
    let mut memory = OwnedGlobalMemory(Some(global));

    // SAFETY: `global` is a live allocation owned by `memory`.
    let destination = unsafe { GlobalLock(global) }.cast::<u16>();
    if destination.is_null() {
        return Err("无法锁定剪贴板内存".to_owned());
    }
    // SAFETY: The allocation is exactly `wide.len() * size_of::<u16>()`
    // bytes, the source is valid, and the regions cannot overlap.
    unsafe { ptr::copy_nonoverlapping(wide.as_ptr(), destination, wide.len()) };
    // GlobalUnlock returns zero both when the final lock is successfully
    // released and when it fails. Clear/read last-error to distinguish them.
    unsafe { SetLastError(WIN32_ERROR(0)) };
    if unsafe { GlobalUnlock(global) }.is_err() {
        let error = unsafe { GetLastError() };
        if error.0 != 0 {
            return Err(format!(
                "无法解锁剪贴板内存：{}",
                std::io::Error::from_raw_os_error(error.0 as i32)
            ));
        }
    }

    // EmptyClipboard requires a real owner HWND; opening with NULL would make
    // the subsequent SetClipboardData fail. A hidden built-in STATIC window
    // gives this synchronous operation an owner without surfacing UI.
    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("TextLens Clipboard Owner"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    }
    .map(ClipboardOwnerWindow)
    .map_err(|error| format!("无法创建剪贴板所有者窗口：{error}"))?;

    retry_operation(
        WINDOWS_CLIPBOARD_ATTEMPTS,
        || {
            // SAFETY: `owner` remains alive until after ClipboardGuard closes
            // the clipboard. A successful call creates exactly one guard.
            unsafe { OpenClipboard(Some(owner.0)) }
        },
        |retry_index| {
            let delay_ms = 4_u64.saturating_mul((retry_index as u64) + 1).min(20);
            thread::sleep(Duration::from_millis(delay_ms));
        },
    )
    .map_err(|error| format!("系统剪贴板正忙：{error}"))?;
    let _clipboard = ClipboardGuard;

    // Allocate and populate memory before clearing the clipboard so allocation
    // failures cannot destroy the user's existing clipboard contents.
    unsafe { EmptyClipboard() }.map_err(|error| format!("无法清空系统剪贴板：{error}"))?;
    // SAFETY: `global` is an unlocked GMEM_MOVEABLE allocation containing a
    // NUL-terminated UTF-16 string. On success Windows owns the allocation.
    unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0))) }
        .map_err(|error| format!("无法写入系统剪贴板：{error}"))?;
    memory.relinquish();
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn encode_windows_clipboard_text(text: &str) -> Result<Vec<u16>, String> {
    let mut encoded = Vec::with_capacity(text.len().saturating_add(1));
    for unit in text.encode_utf16() {
        if unit == 0 {
            return Err("复制内容包含不支持的空字符".to_owned());
        }
        encoded.push(unit);
    }
    encoded.push(0);
    Ok(encoded)
}

#[cfg(test)]
fn decode_windows_clipboard_text(units: &[u16], limit: usize) -> Option<String> {
    let inspected = units.len().min(limit.saturating_add(1));
    let end = units[..inspected].iter().position(|unit| *unit == 0)?;
    if end > limit {
        return None;
    }
    let text = String::from_utf16(&units[..end]).ok()?;
    (text.chars().count() <= limit).then_some(text)
}

#[cfg(any(target_os = "windows", test))]
fn decode_windows_clipboard_unicode_bytes(bytes: &[u8], limit: usize) -> Option<String> {
    let mut units = Vec::with_capacity((bytes.len() / 2).min(limit.saturating_add(1)));
    let mut terminated = false;
    for pair in bytes.chunks_exact(2).take(limit.saturating_add(1)) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            terminated = true;
            break;
        }
        units.push(unit);
    }
    if !terminated || units.len() > limit {
        return None;
    }
    let text = String::from_utf16(&units).ok()?;
    (text.chars().count() <= limit).then_some(text)
}

#[cfg(target_os = "windows")]
fn decode_windows_clipboard_ansi(bytes: &[u8], codepage: u32) -> Option<String> {
    use windows::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};

    let end = bytes.iter().position(|byte| *byte == 0)?;
    let bytes = &bytes[..end];
    if bytes.is_empty() {
        return Some(String::new());
    }

    // CF_TEXT is documented as an ANSI code-page format, but several PDF and
    // canvas renderers publish UTF-8 bytes there when CF_UNICODETEXT is not
    // ready yet. Treat a complete UTF-8 value as authoritative before asking
    // Windows to decode it with the active ANSI code page. On a Chinese ACP,
    // decoding UTF-8 as GBK produces the familiar garbled text even though the
    // original clipboard bytes are perfectly valid.
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_owned());
    }

    let mut flags = MB_ERR_INVALID_CHARS;
    let required = unsafe { MultiByteToWideChar(codepage, flags, bytes, None) };
    if required <= 0 {
        // Some older readers emit a byte sequence which is valid for the
        // system code page but is rejected by the strict conversion flag.
        flags = Default::default();
    }
    let required = unsafe { MultiByteToWideChar(codepage, flags, bytes, None) };
    if required <= 0 {
        return None;
    }
    let mut wide = vec![0u16; usize::try_from(required).ok()?];
    let written = unsafe { MultiByteToWideChar(codepage, flags, bytes, Some(&mut wide)) };
    if written <= 0 || written != required {
        return None;
    }
    String::from_utf16(&wide).ok()
}

#[cfg(any(target_os = "windows", test))]
fn windows_clipboard_sequence_is_stable(expected: u32, current: u32) -> bool {
    current == expected
}

#[cfg(any(target_os = "windows", test))]
fn retry_operation<T, E>(
    attempts: usize,
    mut operation: impl FnMut() -> Result<T, E>,
    mut before_retry: impl FnMut(usize),
) -> Result<T, E> {
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if attempt + 1 == attempts => return Err(error),
            Err(_) => before_retry(attempt),
        }
    }
    unreachable!("attempts is always at least one")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_clipboard_encoding_is_unicode_and_nul_terminated() {
        let encoded = encode_windows_clipboard_text("TextLens 中文 🍎\n第二行").unwrap();
        assert_eq!(encoded.last(), Some(&0));
        assert_eq!(
            String::from_utf16(&encoded[..encoded.len() - 1]).unwrap(),
            "TextLens 中文 🍎\n第二行"
        );
    }

    #[test]
    fn windows_clipboard_encoding_supports_empty_text() {
        assert_eq!(encode_windows_clipboard_text("").unwrap(), vec![0]);
    }

    #[test]
    fn windows_clipboard_encoding_rejects_embedded_nul() {
        assert_eq!(
            encode_windows_clipboard_text("before\0after").unwrap_err(),
            "复制内容包含不支持的空字符"
        );
    }

    #[test]
    fn windows_clipboard_decoding_requires_a_bounded_nul_terminated_value() {
        let encoded = "TextLens 中文 🍎"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        assert_eq!(
            decode_windows_clipboard_text(&encoded, WINDOWS_CLIPBOARD_TEXT_LIMIT).as_deref(),
            Some("TextLens 中文 🍎")
        );
        assert!(decode_windows_clipboard_text(&[b'a' as u16, b'b' as u16], 2).is_none());
        assert!(decode_windows_clipboard_text(&[b'a' as u16, b'b' as u16, 0], 1).is_none());
        assert!(decode_windows_clipboard_text(&[0xD800, 0], 2).is_none());
    }

    #[test]
    fn unicode_byte_reader_accepts_unaligned_data_and_odd_tail_padding() {
        let encoded = "A中"
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let mut unaligned = vec![0xFF];
        unaligned.extend_from_slice(&encoded);
        unaligned.push(0x7F);

        assert_eq!(
            decode_windows_clipboard_unicode_bytes(&unaligned[1..], 16).as_deref(),
            Some("A中")
        );
        assert!(decode_windows_clipboard_unicode_bytes(&encoded[..4], 16).is_none());
        assert!(decode_windows_clipboard_unicode_bytes(&encoded, 1).is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn ansi_clipboard_reader_preserves_nonstandard_utf8_pdf_text() {
        let mut bytes = "PDF \u{4e2d}\u{6587}".as_bytes().to_vec();
        bytes.push(0);

        assert_eq!(
            decode_windows_clipboard_ansi(&bytes, windows::Win32::Globalization::CP_ACP).as_deref(),
            Some("PDF \u{4e2d}\u{6587}")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn ole_fallback_is_limited_to_delayed_rendering_failures() {
        for status in [
            "no-text-format",
            "open-busy",
            "data-unavailable",
            "invalid-size",
            "lock-failed",
            "unicode-decode-failed",
        ] {
            assert!(clipboard_read_can_fallback_to_ole(status));
        }
        for status in [
            "sequence-changed",
            "sequence-raced-open",
            "sequence-raced-read",
            "ansi-decode-failed",
            "text-over-limit",
        ] {
            assert!(!clipboard_read_can_fallback_to_ole(status));
        }
    }

    #[test]
    fn zero_size_ole_reads_stay_inside_the_committed_region_and_text_limit() {
        assert_eq!(
            bounded_readable_region_length(1_000, 200, 1_040, 80),
            Some(80)
        );
        assert_eq!(
            bounded_readable_region_length(1_000, 200, 1_040, 500),
            Some(160)
        );
        assert_eq!(bounded_readable_region_length(1_000, 200, 999, 80), None);
        assert_eq!(bounded_readable_region_length(1_000, 200, 1_201, 80), None);
        assert_eq!(bounded_readable_region_length(1_000, 200, 1_040, 0), None);
    }

    #[test]
    fn windows_clipboard_restore_never_overwrites_a_newer_clipboard_write() {
        assert!(windows_clipboard_sequence_is_stable(42, 42));
        assert!(!windows_clipboard_sequence_is_stable(42, 43));
        assert!(windows_clipboard_sequence_is_stable(0, 0));
        assert!(!windows_clipboard_sequence_is_stable(0, 1));
    }

    #[test]
    fn clipboard_snapshot_refuses_to_drop_a_pixel_format() {
        let mut text_only = WindowsClipboardSnapshotAccounting::default();
        text_only.observe(13, WindowsClipboardFormatOutcome::Captured);
        assert!(text_only.keeps_every_image());

        // The readable DIB went over the budget, so the restore would leave
        // the screenshot behind nothing.
        let mut lost_dib = WindowsClipboardSnapshotAccounting::default();
        lost_dib.observe(2, WindowsClipboardFormatOutcome::NotRestorable);
        lost_dib.observe(8, WindowsClipboardFormatOutcome::Unreadable);
        assert!(!lost_dib.keeps_every_image());

        // Only handle-based pixel formats, none of them materialized.
        let mut bitmap_only = WindowsClipboardSnapshotAccounting::default();
        bitmap_only.observe(2, WindowsClipboardFormatOutcome::NotRestorable);
        assert!(!bitmap_only.keeps_every_image());

        // A restored CF_DIB is enough: Windows re-synthesizes CF_BITMAP.
        let mut dib_restored = WindowsClipboardSnapshotAccounting::default();
        dib_restored.observe(2, WindowsClipboardFormatOutcome::NotRestorable);
        dib_restored.observe(8, WindowsClipboardFormatOutcome::Captured);
        assert!(dib_restored.keeps_every_image());

        // A second pixel format we could have read but did not is still a loss.
        let mut partial_image = WindowsClipboardSnapshotAccounting::default();
        partial_image.observe(8, WindowsClipboardFormatOutcome::Captured);
        partial_image.observe(6, WindowsClipboardFormatOutcome::Unreadable);
        assert!(!partial_image.keeps_every_image());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn image_format_ids_match_the_win32_constants() {
        use windows::Win32::System::Ole::{
            CF_BITMAP, CF_DIB, CF_DIBV5, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT,
            CF_ENHMETAFILE, CF_HDROP, CF_LOCALE, CF_METAFILEPICT, CF_OWNERDISPLAY, CF_PALETTE,
            CF_TIFF, CF_UNICODETEXT,
        };

        for pixel_format in [
            CF_BITMAP,
            CF_METAFILEPICT,
            CF_TIFF,
            CF_DIB,
            CF_PALETTE,
            CF_ENHMETAFILE,
            CF_DIBV5,
            CF_OWNERDISPLAY,
            CF_DSPBITMAP,
            CF_DSPMETAFILEPICT,
            CF_DSPENHMETAFILE,
        ] {
            assert!(windows_clipboard_format_carries_image(u32::from(
                pixel_format.0
            )));
        }
        for text_format in [CF_UNICODETEXT, CF_HDROP, CF_LOCALE] {
            assert!(!windows_clipboard_format_carries_image(u32::from(
                text_format.0
            )));
        }
    }

    #[test]
    fn retry_operation_stops_after_success_without_an_extra_delay() {
        let mut calls = 0;
        let mut retries = Vec::new();
        let value = retry_operation(
            WINDOWS_CLIPBOARD_ATTEMPTS,
            || {
                calls += 1;
                if calls < 3 {
                    Err("busy")
                } else {
                    Ok("opened")
                }
            },
            |attempt| retries.push(attempt),
        )
        .unwrap();

        assert_eq!(value, "opened");
        assert_eq!(calls, 3);
        assert_eq!(retries, vec![0, 1]);
    }

    #[test]
    fn retry_operation_returns_the_last_error() {
        let mut calls = 0;
        let mut retries = 0;
        let error = retry_operation(
            3,
            || {
                calls += 1;
                Err::<(), _>(calls)
            },
            |_| retries += 1,
        )
        .unwrap_err();

        assert_eq!(error, 3);
        assert_eq!(calls, 3);
        assert_eq!(retries, 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn writes_unicode_text_to_an_isolated_pasteboard() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let expected = "TextLens 剪贴板测试 🍎\n第二行";

        write_to_pasteboard(&pasteboard, expected).unwrap();

        // SAFETY: See the matching use in `write_to_pasteboard`.
        let string_type = unsafe { NSPasteboardTypeString };
        let actual = pasteboard
            .stringForType(string_type)
            .expect("unique pasteboard should contain a string");
        assert_eq!(actual.to_string(), expected);
    }
}
