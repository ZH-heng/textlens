/**
 * Native macOS selection bridge for TextLens.
 *
 * The Accessibility traversal, text-range bounds helpers, and input detection
 * in this file are adapted from selection-hook 2.0.2:
 *
 * Copyright (c) 2025 0xfullex (https://github.com/0xfullex/selection-hook)
 * Licensed under the MIT License. See LICENSE.selection-hook.
 *
 * This implementation exposes a small C ABI and intentionally contains no
 * Node/N-API integration.
 */

#import "selection_bridge.h"

#import <ApplicationServices/ApplicationServices.h>
#import <AppKit/AppKit.h>
#import <Carbon/Carbon.h>
#import <Foundation/Foundation.h>

#include <algorithm>
#include <atomic>
#include <cctype>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <deque>
#include <mutex>
#include <new>
#include <limits>
#include <string>
#include <sys/stat.h>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

namespace {

using Clock = std::chrono::steady_clock;

constexpr double kMinimumDragDistance = 4.0;
constexpr uint64_t kMaximumDragDurationMs = 15'000;
constexpr uint64_t kDoubleClickDurationMs = 500;
constexpr double kDoubleClickDistance = 4.0;
constexpr size_t kMaximumQueuedTasks = 64;
// Bounds every AX round-trip to a single app so one slow/hung process can't
// stall the shared single-threaded capture worker for every other app.
constexpr float kAXMessagingTimeoutSeconds = 0.08f;
// Geometry is optional because the mouse-up point is always a placement
// fallback. Give range-bound queries a smaller budget so a correct text read
// is never held behind a slow PDF/Office layout calculation.
constexpr float kAXGeometryMessagingTimeoutSeconds = 0.03f;
// Selection capture stays below a perceptible delay even when a provider
// responds to individual AX requests but exposes a deep, sparse tree.
constexpr uint64_t kAXCaptureBudgetMs = 260;
constexpr uint64_t kDocumentAXCaptureBudgetMs = 300;
constexpr uint64_t kAXCompatibilityRetryDelayMs = 28;
constexpr uint64_t kAXLateSelectionRetryDelayMs = 24;
// Grace period for the single unconditional AX retry when the first read
// found nothing: the provider may not have committed the selection when the
// first read lands. This replaces the old unconditional 12ms pre-capture
// sleep, which taxed every successful fast-path capture.
constexpr uint64_t kAXSelectionCommitRetryDelayMs = 12;
// Proactive self-healing check for the event tap, mirroring Windows' hook
// health-check cadence; the reactive re-enable in eventTapCallback only
// fires on the next incoming event, which never arrives if the tap was
// disabled during a quiet period.
constexpr CFTimeInterval kEventTapHealthCheckIntervalSeconds = 5.0;
// Clipboard compatibility fallback: canvas-based apps (WPS/Qt documents,
// browser PDF plugins, some Electron views) expose no selected text over AX.
// A synthetic Cmd+C reads what the app itself would copy, then the original
// pasteboard is restored. The fallback is denylisted, secure-input aware and
// generation-cancellable so it only ever reads what the user just selected.
constexpr int64_t kSyntheticEventMarker = 0x544c4d41434f534cLL; // "TLMACOSL"
constexpr uint64_t kClipboardFallbackBudgetMs = 480;
constexpr uint64_t kClipboardPollIntervalMs = 12;
// Apps that ignore pid-targeted events get one retry posted at session level.
constexpr uint64_t kClipboardSessionRetryAfterMs = 200;
// Extra window after polling stopped for a slow or interrupted copy to land,
// so the restore can reclaim our chord instead of polluting the clipboard.
constexpr uint64_t kClipboardLateCopyClaimMs = 120;
constexpr uint64_t kClipboardMaxTextBytes = 2'000'000;
constexpr NSUInteger kClipboardSnapshotMaxRepresentationBytes = 16u << 20;
constexpr NSUInteger kClipboardSnapshotMaxTotalBytes = 32u << 20;
// Stability window (in poll ticks) required before reclaiming a non-text
// copy, so multi-step pasteboard writes finish inside the boundary.
constexpr NSInteger kClipboardNonTextStablePolls = 5;
// Canvas apps can keep their previous selection alive when the user clicks
// empty space, so a synthetic copy may return that old text unchanged. A
// result identical to the previous accepted capture, but captured far away
// from it, is treated as stale instead of a fresh selection.
constexpr double kStaleClipboardSelectionDistancePx = 48.0;
// Browser AppleScript layer: reads the live DOM selection via `osascript`
// without synthesizing input or touching the pasteboard. It sits between the
// AX path and the clipboard fallback. Safari exposes no AXSelectedText at
// all, so this is its only fast non-intrusive path; Chrome-kernel browsers
// need the user-facing "Allow JavaScript from Apple Events" switch, and a
// missing switch fails fast into the clipboard fallback. The layer is
// bounded by kAppleScriptProcessTimeoutMs.
constexpr uint64_t kAppleScriptProcessTimeoutMs = 200;

enum class Trigger {
    Drag,
    DoubleClick,
    ShiftClick,
    Keyboard,
    Manual,
};

static const char *TriggerTag(Trigger trigger) {
    switch (trigger) {
        case Trigger::Drag: return "drag";
        case Trigger::DoubleClick: return "doubleClick";
        case Trigger::ShiftClick: return "shiftClick";
        case Trigger::Keyboard: return "keyboard";
        case Trigger::Manual: return "manual";
    }
}

enum class SelectionMethod {
    Accessibility,
    Clipboard,
    AppleScript,
};

struct SelectionInfo {
    std::string text;
    std::string bundleId;
    std::string appName;
    // Kept private to the native bridge. Some AX providers report this value
    // as selected text when the actual range is unavailable.
    std::string windowTitle;
    SelectionMethod method = SelectionMethod::Accessibility;
    Trigger trigger = Trigger::Manual;
    bool fullscreen = false;

    bool hasBounds = false;
    CGRect bounds = CGRectZero;

    bool hasStartTop = false;
    bool hasStartBottom = false;
    bool hasEndTop = false;
    bool hasEndBottom = false;
    CGPoint startTop = CGPointZero;
    CGPoint startBottom = CGPointZero;
    CGPoint endTop = CGPointZero;
    CGPoint endBottom = CGPointZero;

    bool hasMouseStart = false;
    bool hasMouseEnd = false;
    CGPoint mouseStart = CGPointZero;
    CGPoint mouseEnd = CGPointZero;
    CGPoint mouseCurrent = CGPointZero;
};

enum class TaskKind {
    Capture,
    Dismiss,
};

struct Task {
    TaskKind kind = TaskKind::Dismiss;
    Trigger trigger = Trigger::Manual;
    std::string dismissReason;
    CGPoint mouseStart = CGPointZero;
    CGPoint mouseEnd = CGPointZero;
    CGPoint mouseCurrent = CGPointZero;
    bool hasMouseStart = false;
    bool hasMouseEnd = false;
    int64_t targetPid = 0;
    uint64_t generation = 0;
    uint64_t enqueuedAtMs = 0;
};

// Deferred clipboard/AppleScript capture: the AX pipeline ran on the capture
// worker and found nothing, so the (potentially multi-hundred-ms) fallback
// chain continues on its own thread without blocking the next gesture's AX
// read. `targetPid` identifies the app the AX half observed.
struct FallbackTask {
    SelectionInfo info;
    pid_t targetPid = 0;
    uint64_t generation = 0;
    bool clipboardAllowed = false;
    uint64_t enqueuedAtMs = 0;
};

static uint64_t MonotonicMilliseconds() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::milliseconds>(
        Clock::now().time_since_epoch()).count());
}

// Every AX request is tied to the originating input generation and a single
// wall-clock budget. AX providers can block independently even after their
// application element was configured with a messaging timeout, so checking at
// traversal boundaries is what keeps a stale PDF/Office read from delaying the
// next selection.
struct AccessibilityReadContext {
    const std::atomic<uint64_t> *generation = nullptr;
    uint64_t expectedGeneration = 0;
    uint64_t deadlineMs = 0;

    bool IsCurrent() const {
        return (generation == nullptr ||
                generation->load(std::memory_order_acquire) == expectedGeneration) &&
            MonotonicMilliseconds() < deadlineMs;
    }
};

static void ConfigureAccessibilityElement(AXUIElementRef element) {
    if (element != nullptr) {
        AXUIElementSetMessagingTimeout(element, kAXMessagingTimeoutSeconds);
    }
}

static bool SelectionTimingEnabled() {
    static const bool enabled = [] {
        const char *value = std::getenv("TEXTLENS_SELECTION_TRACE");
        if (value != nullptr &&
            (std::strcmp(value, "1") == 0 || std::strcmp(value, "true") == 0 ||
             std::strcmp(value, "TRUE") == 0)) {
            return true;
        }
        // File-based switch so a Finder-launched (packaged) app can enable
        // tracing without launchctl env propagation: the marker must exist
        // before the app starts.
        const char *home = std::getenv("HOME");
        if (home == nullptr) {
            return false;
        }
        const std::string marker =
            std::string(home) + "/Library/Logs/TextLens/selection-trace.enabled";
        struct stat info {};
        return ::stat(marker.c_str(), &info) == 0 && S_ISREG(info.st_mode);
    }();
    return enabled;
}

static void TraceSelectionTiming(const char *stage, uint64_t durationMs) {
    if (SelectionTimingEnabled()) {
        std::fprintf(
            stderr,
            "[selection-timing] stage=%s duration_ms=%llu\n",
            stage,
            static_cast<unsigned long long>(durationMs));
    }
}

// One line per clipboard-fallback gate decision; enables field diagnosis of
// "why did/didn't a synthetic copy fire" without changing default behavior.
// Mirrored to ~/Library/Logs/TextLens/selection-trace.log because a
// Finder-launched app's stderr is not visible. Never logs text content —
// lengths and reasons only.
static void TraceClipboardGate(const char *decision) {
    if (!SelectionTimingEnabled()) {
        return;
    }
    std::fprintf(stderr, "[selection-fallback] %s\n", decision);
    static std::mutex fileMutex;
    std::lock_guard<std::mutex> lock(fileMutex);
    const char *home = std::getenv("HOME");
    if (home == nullptr) {
        return;
    }
    const std::string directory = std::string(home) + "/Library/Logs/TextLens";
    ::mkdir(directory.c_str(), 0755);
    FILE *file = std::fopen((directory + "/selection-trace.log").c_str(), "a");
    if (file == nullptr) {
        return;
    }
    std::fprintf(
        file,
        "%llu %s\n",
        static_cast<unsigned long long>(MonotonicMilliseconds()),
        decision);
    std::fclose(file);
}

static uint64_t TimestampMilliseconds() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::system_clock::now().time_since_epoch()).count());
}

static std::string StringFromNSString(NSString *value) {
    if (value == nil) {
        return {};
    }
    NSData *data = [value dataUsingEncoding:NSUTF8StringEncoding allowLossyConversion:NO];
    if (data == nil) {
        return {};
    }
    return std::string(static_cast<const char *>(data.bytes), data.length);
}

static NSString *NSStringFromString(const std::string &value) {
    return [[NSString alloc] initWithBytes:value.data()
                                    length:value.size()
                                  encoding:NSUTF8StringEncoding];
}

static bool StringFromCFString(CFStringRef value, std::string &result) {
    if (value == nullptr) {
        return false;
    }
    CFIndex length = CFStringGetLength(value);
    if (length <= 0) {
        return false;
    }
    CFIndex maximum = CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8) + 1;
    if (maximum <= 1) {
        return false;
    }
    std::vector<char> buffer(static_cast<size_t>(maximum));
    if (!CFStringGetCString(value, buffer.data(), maximum, kCFStringEncodingUTF8)) {
        return false;
    }
    result.assign(buffer.data());
    return !result.empty();
}

static bool IsBlank(const std::string &value) {
    NSString *string = NSStringFromString(value);
    if (string == nil) {
        return true;
    }
    return [[string stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]] length] == 0;
}

// Window under a screen point, resolved through the window server. AX hit
// chains cannot separate WeChat's image viewer from a chat bubble (both report
// AXButton>AXGroup>AXWindow), and the viewer's window may not answer
// position/size queries, so compare window numbers instead: a drag that starts
// in the chat window and ends inside the image viewer must not synthesize
// Cmd+C, or WeChat copies the image with visible feedback.
static CGWindowID TopmostWindowNumberAtPoint(pid_t processIdentifier, CGPoint point) {
    CFArrayRef windows = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID);
    if (windows == nullptr) {
        return kCGNullWindowID;
    }
    CGWindowID result = kCGNullWindowID;
    const CFIndex count = CFArrayGetCount(windows);
    for (CFIndex index = 0; index < count; ++index) {
        NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(windows, index);
        NSNumber *owner = info[(__bridge NSString *)kCGWindowOwnerPID];
        NSNumber *number = info[(__bridge NSString *)kCGWindowNumber];
        NSNumber *alpha = info[(__bridge NSString *)kCGWindowAlpha];
        NSDictionary *bounds = info[(__bridge NSString *)kCGWindowBounds];
        if (owner == nil || number == nil || bounds == nil ||
            owner.intValue != processIdentifier) {
            continue;
        }
        if (alpha != nil && alpha.doubleValue < 0.01) {
            continue;
        }
        CGRect frame = CGRectZero;
        if (!CGRectMakeWithDictionaryRepresentation(
                (__bridge CFDictionaryRef)bounds, &frame)) {
            continue;
        }
        // Skip tooltips, status icons and other auxiliary windows.
        if (frame.size.width < 24.0 || frame.size.height < 24.0) {
            continue;
        }
        if (CGRectContainsPoint(CGRectInset(frame, -1.0, -1.0), point)) {
            result = number.unsignedIntValue;
            break;
        }
    }
    CFRelease(windows);
    return result;
}

static CGPoint CurrentMousePosition() {
    CGEventRef event = CGEventCreate(nullptr);
    if (event == nullptr) {
        return CGPointZero;
    }
    CGPoint point = CGEventGetLocation(event);
    CFRelease(event);
    return point;
}

static NSRunningApplication *FrontmostApplication() {
    return [NSWorkspace sharedWorkspace].frontmostApplication;
}

static bool IsExcludedApplication(
    NSRunningApplication *application,
    const std::string &excludedBundleId) {
    if (application == nil || application.processIdentifier == getpid()) {
        return true;
    }
    // A missing bundle id (some CLI-launched GUI processes, Wine/JVM-hosted
    // apps) no longer disqualifies an app outright; identity for matching
    // falls back to appName (localizedName) elsewhere in the pipeline.
    NSString *bundle = application.bundleIdentifier;
    if (bundle != nil && bundle.length > 0 && !excludedBundleId.empty() &&
        StringFromNSString(bundle) == excludedBundleId) {
        return true;
    }
    return false;
}

static bool IsReasonableRect(CGRect rect) {
    return std::isfinite(rect.origin.x) && std::isfinite(rect.origin.y) &&
        std::isfinite(rect.size.width) && std::isfinite(rect.size.height) &&
        rect.size.width > 0.0 && rect.size.height > 0.0 &&
        rect.size.width < 200'000.0 && rect.size.height < 200'000.0 &&
        std::abs(rect.origin.x) < 500'000.0 && std::abs(rect.origin.y) < 500'000.0;
}

static void StoreAggregateBounds(SelectionInfo &info, CGRect rect) {
    info.hasBounds = true;
    info.bounds = rect;
    info.hasStartTop = true;
    info.hasStartBottom = true;
    info.hasEndTop = true;
    info.hasEndBottom = true;
    info.startTop = CGPointMake(CGRectGetMinX(rect), CGRectGetMinY(rect));
    info.startBottom = CGPointMake(CGRectGetMinX(rect), CGRectGetMaxY(rect));
    info.endTop = CGPointMake(CGRectGetMaxX(rect), CGRectGetMinY(rect));
    info.endBottom = CGPointMake(CGRectGetMaxX(rect), CGRectGetMaxY(rect));
}

static bool ReadSelectionBounds(
    AXUIElementRef element,
    SelectionInfo &info,
    const AccessibilityReadContext &context) {
    if (element == nullptr || !context.IsCurrent()) {
        return false;
    }

    // The selection text is already validated at this point. Bounds only
    // improve placement, so cap this optional work much more aggressively
    // than the text read itself.
    AXUIElementSetMessagingTimeout(element, kAXGeometryMessagingTimeoutSeconds);

    AXValueRef selectedRangeValue = nullptr;
    AXError error = AXUIElementCopyAttributeValue(
        element,
        kAXSelectedTextRangeAttribute,
        reinterpret_cast<CFTypeRef *>(&selectedRangeValue));
    if (error != kAXErrorSuccess || selectedRangeValue == nullptr) {
        return false;
    }

    CFRange range = CFRangeMake(0, 0);
    bool hasRange = AXValueGetValue(selectedRangeValue, kAXValueTypeCFRange, &range) &&
        range.location >= 0 && range.length > 0 &&
        range.length <= std::numeric_limits<CFIndex>::max() - range.location;
    if (!hasRange) {
        CFRelease(selectedRangeValue);
        return false;
    }

    AXValueRef aggregateBoundsValue = nullptr;
    AXError aggregateError = context.IsCurrent()
        ? AXUIElementCopyParameterizedAttributeValue(
              element,
              kAXBoundsForRangeParameterizedAttribute,
              selectedRangeValue,
              reinterpret_cast<CFTypeRef *>(&aggregateBoundsValue))
        : kAXErrorFailure;
    CGRect aggregateRect = CGRectZero;
    if (aggregateError == kAXErrorSuccess && aggregateBoundsValue != nullptr &&
        AXValueGetValue(aggregateBoundsValue, kAXValueTypeCGRect, &aggregateRect) &&
        IsReasonableRect(aggregateRect)) {
        StoreAggregateBounds(info, aggregateRect);
    }
    if (aggregateBoundsValue != nullptr) CFRelease(aggregateBoundsValue);

    CFRelease(selectedRangeValue);
    return info.hasBounds;
}

// Reads the selection via the text-marker APIs used by web-content AX trees.
static bool ReadSelectedTextByMarkerRange(
    AXUIElementRef element,
    std::string &text,
    const AccessibilityReadContext &context) {
    if (element == nullptr || !context.IsCurrent()) {
        return false;
    }
    CFTypeRef markerRange = nullptr;
    AXError error = AXUIElementCopyAttributeValue(
        element,
        CFSTR("AXSelectedTextMarkerRange"),
        &markerRange);
    if (error != kAXErrorSuccess || markerRange == nullptr) {
        return false;
    }
    CFTypeRef markerText = nullptr;
    AXError textError = context.IsCurrent()
        ? AXUIElementCopyParameterizedAttributeValue(
              element,
              CFSTR("AXStringForTextMarkerRange"),
              markerRange,
              reinterpret_cast<CFTypeRef *>(&markerText))
        : kAXErrorFailure;
    CFRelease(markerRange);
    const bool success = textError == kAXErrorSuccess && markerText != nullptr &&
        CFGetTypeID(markerText) == CFStringGetTypeID() &&
        StringFromCFString(static_cast<CFStringRef>(markerText), text) && !IsBlank(text);
    if (markerText != nullptr) {
        CFRelease(markerText);
    }
    return success;
}

static bool ReadSelectedText(
    AXUIElementRef element,
    std::string &text,
    const AccessibilityReadContext &context) {
    if (element == nullptr || !context.IsCurrent()) {
        return false;
    }

    ConfigureAccessibilityElement(element);

    CFTypeRef selectedValue = nullptr;
    AXError error = AXUIElementCopyAttributeValue(element, kAXSelectedTextAttribute, &selectedValue);
    if (error == kAXErrorSuccess && selectedValue != nullptr) {
        bool success = CFGetTypeID(selectedValue) == CFStringGetTypeID() &&
            StringFromCFString(static_cast<CFStringRef>(selectedValue), text) && !IsBlank(text);
        CFRelease(selectedValue);
        if (success) {
            return true;
        }
    }

    if (!context.IsCurrent()) {
        return false;
    }

    // Chromium/Electron/WebKit expose web selections through text-marker
    // ranges even when kAXSelectedTextAttribute is unavailable (or reports
    // an empty string) on the focused element. Two AX calls read the exact
    // selected string, avoiding both the kAXValue whole-document read and
    // the bounded tree descent in the most common web scenario.
    if (ReadSelectedTextByMarkerRange(element, text, context)) {
        return true;
    }

    CFTypeRef value = nullptr;
    error = AXUIElementCopyAttributeValue(element, kAXValueAttribute, &value);
    if (error != kAXErrorSuccess || value == nullptr) {
        return false;
    }
    if (CFGetTypeID(value) != CFStringGetTypeID()) {
        CFRelease(value);
        return false;
    }

    if (!context.IsCurrent()) {
        CFRelease(value);
        return false;
    }

    AXValueRef rangeValue = nullptr;
    error = AXUIElementCopyAttributeValue(
        element,
        kAXSelectedTextRangeAttribute,
        reinterpret_cast<CFTypeRef *>(&rangeValue));
    if (error != kAXErrorSuccess || rangeValue == nullptr) {
        CFRelease(value);
        return false;
    }

    CFRange range = CFRangeMake(0, 0);
    CFIndex valueLength = CFStringGetLength(static_cast<CFStringRef>(value));
    bool validRange = AXValueGetValue(rangeValue, kAXValueTypeCFRange, &range) &&
        range.location >= 0 && range.length > 0 && range.location < valueLength;
    if (validRange && range.length > valueLength - range.location) {
        range.length = valueLength - range.location;
    }

    bool success = false;
    if (validRange && range.length > 0) {
        CFStringRef substring = CFStringCreateWithSubstring(
            kCFAllocatorDefault,
            static_cast<CFStringRef>(value),
            range);
        if (substring != nullptr) {
            success = StringFromCFString(substring, text) && !IsBlank(text);
            CFRelease(substring);
        }
    }

    CFRelease(rangeValue);
    CFRelease(value);
    return success;
}

// Forward declaration because the application routing table is declared with
// the other capture policy helpers below.
static bool IsDocumentOrOfficeSelectionApplication(
    const std::string &bundleId,
    const std::string &appName);

static void ClearSelectionGeometry(SelectionInfo &info) {
    info.hasBounds = false;
    info.bounds = CGRectZero;
    info.hasStartTop = false;
    info.hasStartBottom = false;
    info.hasEndTop = false;
    info.hasEndBottom = false;
    info.startTop = CGPointZero;
    info.startBottom = CGPointZero;
    info.endTop = CGPointZero;
    info.endBottom = CGPointZero;
}

static std::string NormalizeSelectionIdentity(const std::string &value) {
    std::string normalized;
    normalized.reserve(value.size());
    for (unsigned char character : value) {
        if (std::isspace(character) != 0) {
            continue;
        }
        normalized.push_back(static_cast<char>(std::tolower(character)));
    }
    return normalized;
}

static bool WindowTitleComponentMatches(
    const std::string &title,
    const std::string &normalizedText) {
    const std::string normalizedTitle = NormalizeSelectionIdentity(title);
    if (normalizedTitle == normalizedText) {
        return true;
    }
    static const char *const separators[] = {" - ", " | ", " -- "};
    for (const char *separator : separators) {
        size_t start = 0;
        const std::string delimiter(separator);
        while (start <= title.size()) {
            size_t end = title.find(delimiter, start);
            std::string component = title.substr(start, end == std::string::npos ? end : end - start);
            if (!component.empty() && NormalizeSelectionIdentity(component) == normalizedText) {
                return true;
            }
            if (end == std::string::npos) {
                break;
            }
            start = end + delimiter.size();
        }
    }
    return false;
}

static bool SelectionTextMatchesSourceIdentity(const SelectionInfo &info) {
    if (info.text.empty() || info.text.size() > 2048) {
        return false;
    }
    const std::string normalizedText = NormalizeSelectionIdentity(info.text);
    if (normalizedText.empty()) {
        return false;
    }

    for (const std::string &candidate : {info.appName, info.bundleId}) {
        if (!candidate.empty() && NormalizeSelectionIdentity(candidate) == normalizedText) {
            return true;
        }
        const size_t separator = candidate.find_last_of("./");
        if (separator != std::string::npos &&
            NormalizeSelectionIdentity(candidate.substr(separator + 1)) == normalizedText) {
            return true;
        }
    }
    return !info.windowTitle.empty() &&
        WindowTitleComponentMatches(info.windowTitle, normalizedText);
}

// A double-click selects the word under the cursor, so the reported
// selection must essentially contain the click. A larger tolerance would
// admit "clicked the blank right side of a line, got the adjacent unbroken
// token/formula" captures. Drags end exactly at the cursor and shift-clicks
// sit inside the selection, but providers round coordinates, so they keep
// the looser historical tolerance.
static CGFloat PointNearSelectionBoundsTolerance(Trigger trigger) {
    return trigger == Trigger::DoubleClick ? 20.0 : 56.0;
}

static bool PointNearSelectionBounds(CGPoint point, CGRect bounds, CGFloat tolerance) {
    return point.x >= CGRectGetMinX(bounds) - tolerance &&
        point.x <= CGRectGetMaxX(bounds) + tolerance &&
        point.y >= CGRectGetMinY(bounds) - tolerance &&
        point.y <= CGRectGetMaxY(bounds) + tolerance;
}

// A double-click on rendered-but-empty content (markdown bullets "• ", empty
// list markers "1. ", gutter fragments) yields a tiny "selection" the user
// never perceives as text. Mouse-triggered captures of such degenerate
// fragments — at most three characters and none of them letter-like — are
// dropped; keyboard/manual gestures stay deliberate and unaffected, and any
// CJK/kana/Hangul or Latin letter keeps the capture.
static bool CodePointIsLetterish(uint32_t codePoint) {
    return (codePoint >= 'a' && codePoint <= 'z') ||
        (codePoint >= 'A' && codePoint <= 'Z') ||
        (codePoint >= 0x0400 && codePoint <= 0x04FF) ||   // Cyrillic
        (codePoint >= 0x3040 && codePoint <= 0x30FF) ||   // kana
        (codePoint >= 0x3400 && codePoint <= 0x4DBF) ||   // CJK ext A
        (codePoint >= 0x4E00 && codePoint <= 0x9FFF) ||   // CJK unified
        (codePoint >= 0xAC00 && codePoint <= 0xD7AF) ||   // Hangul
        (codePoint >= 0xF900 && codePoint <= 0xFAFF) ||   // CJK compat
        (codePoint >= 0x20000 && codePoint <= 0x3FFFF);   // CJK ext B+
}

static bool SelectionTextIsDegenerateFragment(const std::string &text) {
    size_t start = 0;
    size_t end = text.size();
    while (start < end && std::isspace(static_cast<unsigned char>(text[start])) != 0) {
        ++start;
    }
    while (end > start && std::isspace(static_cast<unsigned char>(text[end - 1])) != 0) {
        --end;
    }
    if (end == start) {
        return true;
    }
    size_t codePoints = 0;
    bool hasLetterish = false;
    size_t index = start;
    while (index < end) {
        const unsigned char lead = static_cast<unsigned char>(text[index]);
        size_t length = 1;
        uint32_t codePoint = lead;
        if ((lead & 0xE0) == 0xC0 && index + 1 < end) {
            length = 2;
            codePoint = lead & 0x1F;
        } else if ((lead & 0xF0) == 0xE0 && index + 2 < end) {
            length = 3;
            codePoint = lead & 0x0F;
        } else if ((lead & 0xF8) == 0xF0 && index + 3 < end) {
            length = 4;
            codePoint = lead & 0x07;
        }
        for (size_t offset = 1; offset < length; ++offset) {
            codePoint = (codePoint << 6) |
                (static_cast<unsigned char>(text[index + offset]) & 0x3F);
        }
        if (CodePointIsLetterish(codePoint)) {
            hasLetterish = true;
        }
        index += length;
        codePoints += 1;
        if (codePoints > 3) {
            return false;
        }
    }
    return !hasLetterish;
}

static bool SelectionCandidateMatchesInput(const SelectionInfo &candidate) {
    if (SelectionTextMatchesSourceIdentity(candidate)) {
        return false;
    }
    if (candidate.trigger == Trigger::Keyboard || candidate.trigger == Trigger::Manual) {
        return true;
    }
    if (SelectionTextIsDegenerateFragment(candidate.text)) {
        return false;
    }
    if (!candidate.hasBounds) {
        // Some PDF and Office providers expose a real AXSelectedText range
        // but do not implement AXBoundsForRange. The text was read from an
        // explicit selection attribute and host/window-title values have
        // already been filtered above, so keep it rather than falling back to
        // a destructive fallback.
        return true;
    }
    const CGFloat tolerance = PointNearSelectionBoundsTolerance(candidate.trigger);
    return PointNearSelectionBounds(candidate.mouseCurrent, candidate.bounds, tolerance) ||
        (candidate.hasMouseEnd &&
         PointNearSelectionBounds(candidate.mouseEnd, candidate.bounds, tolerance));
}

static bool ReadValidatedSelectedText(
    AXUIElementRef element,
    SelectionInfo &info,
    const AccessibilityReadContext &context) {
    std::string text;
    if (!ReadSelectedText(element, text, context)) {
        return false;
    }

    SelectionInfo candidate = info;
    ClearSelectionGeometry(candidate);
    candidate.text = std::move(text);
    ReadSelectionBounds(element, candidate, context);
    if (!SelectionCandidateMatchesInput(candidate)) {
        return false;
    }
    info = std::move(candidate);
    return true;
}

static bool FindSelectionInTree(
    AXUIElementRef element,
    SelectionInfo &info,
    int depth,
    size_t &remainingElements,
    const AccessibilityReadContext &context) {
    if (element == nullptr || depth < 0 || remainingElements == 0 || !context.IsCurrent()) {
        return false;
    }
    --remainingElements;

    ConfigureAccessibilityElement(element);

    if (ReadValidatedSelectedText(element, info, context)) {
        return true;
    }

    if (depth == 0) {
        return false;
    }
    CFArrayRef children = nullptr;
    AXError error = AXUIElementCopyAttributeValue(
        element,
        kAXChildrenAttribute,
        reinterpret_cast<CFTypeRef *>(&children));
    if (error != kAXErrorSuccess || children == nullptr) {
        return false;
    }

    bool found = false;
    CFIndex count = std::min<CFIndex>(CFArrayGetCount(children), 64);
    for (CFIndex index = 0;
         index < count && !found && remainingElements > 0 && context.IsCurrent();
         ++index) {
        AXUIElementRef child = static_cast<AXUIElementRef>(
            const_cast<void *>(CFArrayGetValueAtIndex(children, index)));
        found = FindSelectionInTree(child, info, depth - 1, remainingElements, context);
    }
    CFRelease(children);
    return found;
}

static bool FindSelectionInChildren(
    AXUIElementRef element,
    SelectionInfo &info,
    int depth,
    size_t &remainingElements,
    const AccessibilityReadContext &context) {
    if (element == nullptr || depth <= 0 || remainingElements == 0 || !context.IsCurrent()) {
        return false;
    }

    ConfigureAccessibilityElement(element);
    CFArrayRef children = nullptr;
    AXError error = AXUIElementCopyAttributeValue(
        element,
        kAXChildrenAttribute,
        reinterpret_cast<CFTypeRef *>(&children));
    if (error != kAXErrorSuccess || children == nullptr) {
        return false;
    }

    bool found = false;
    CFIndex count = std::min<CFIndex>(CFArrayGetCount(children), 64);
    for (CFIndex index = 0;
         index < count && !found && remainingElements > 0 && context.IsCurrent();
         ++index) {
        AXUIElementRef child = static_cast<AXUIElementRef>(
            const_cast<void *>(CFArrayGetValueAtIndex(children, index)));
        found = FindSelectionInTree(child, info, depth - 1, remainingElements, context);
    }
    CFRelease(children);
    return found;
}

static AXUIElementRef CopyFocusedElement(AXUIElementRef applicationElement) {
    AXUIElementRef element = nullptr;
    if (applicationElement != nullptr) {
        AXUIElementCopyAttributeValue(
            applicationElement,
            kAXFocusedUIElementAttribute,
            reinterpret_cast<CFTypeRef *>(&element));
    }
    return element;
}

static AXUIElementRef CopyFocusedWindow(AXUIElementRef applicationElement) {
    AXUIElementRef window = nullptr;
    if (applicationElement != nullptr) {
        AXUIElementCopyAttributeValue(
            applicationElement,
            kAXFocusedWindowAttribute,
            reinterpret_cast<CFTypeRef *>(&window));
    }
    return window;
}

static void ReadWindowTitle(
    AXUIElementRef window,
    std::string &title,
    const AccessibilityReadContext &context) {
    if (window == nullptr || !context.IsCurrent()) {
        return;
    }
    AXUIElementSetMessagingTimeout(window, kAXGeometryMessagingTimeoutSeconds);
    CFTypeRef value = nullptr;
    if (AXUIElementCopyAttributeValue(window, kAXTitleAttribute, &value) != kAXErrorSuccess ||
        value == nullptr) {
        return;
    }
    if (context.IsCurrent() && CFGetTypeID(value) == CFStringGetTypeID()) {
        std::string candidate;
        if (StringFromCFString(static_cast<CFStringRef>(value), candidate) &&
            !IsBlank(candidate)) {
            title = std::move(candidate);
        }
    }
    CFRelease(value);
}

static bool FindSelectionFromFocusedContext(
    AXUIElementRef element,
    SelectionInfo &info,
    const AccessibilityReadContext &context) {
    if (element == nullptr || !context.IsCurrent()) {
        return false;
    }

    if (ReadValidatedSelectedText(element, info, context)) {
        return true;
    }

    AXUIElementRef current = element;
    CFRetain(current);
    bool found = false;
    for (int level = 0; level < 10 && !found && context.IsCurrent(); ++level) {
        ConfigureAccessibilityElement(current);
        AXUIElementRef parent = nullptr;
        AXError error = AXUIElementCopyAttributeValue(
            current,
            kAXParentAttribute,
            reinterpret_cast<CFTypeRef *>(&parent));
        CFRelease(current);
        current = nullptr;
        if (error != kAXErrorSuccess || parent == nullptr) {
            break;
        }
        current = parent;
        if (ReadValidatedSelectedText(current, info, context)) {
            found = true;
        }
    }
    if (current != nullptr) CFRelease(current);
    if (found || !context.IsCurrent()) {
        return found;
    }

    // Direct focused/parent reads cover native editors and most Office/PDF
    // applications. Only then descend into a bounded child tree for WebKit,
    // Chromium, and custom canvas accessibility hierarchies.
    size_t remaining = 96;
    return FindSelectionInChildren(element, info, 4, remaining, context);
}

static bool ReadFullscreen(
    AXUIElementRef applicationElement,
    const AccessibilityReadContext &context) {
    if (applicationElement == nullptr || !context.IsCurrent()) {
        return false;
    }
    AXUIElementRef window = CopyFocusedWindow(applicationElement);
    if (window == nullptr) {
        return false;
    }
    AXUIElementSetMessagingTimeout(window, kAXGeometryMessagingTimeoutSeconds);
    CFTypeRef value = nullptr;
    AXError error = AXUIElementCopyAttributeValue(window, CFSTR("AXFullScreen"), &value);
    bool fullscreen = context.IsCurrent() && error == kAXErrorSuccess && value != nullptr &&
        CFGetTypeID(value) == CFBooleanGetTypeID() &&
        CFBooleanGetValue(static_cast<CFBooleanRef>(value));
    if (value != nullptr) CFRelease(value);
    CFRelease(window);
    return fullscreen;
}

static bool ElementIsProtected(
    AXUIElementRef element,
    const AccessibilityReadContext &context) {
    if (element == nullptr || !context.IsCurrent()) {
        return true;
    }
    ConfigureAccessibilityElement(element);

    bool isProtected = false;
    CFTypeRef subrole = nullptr;
    if (AXUIElementCopyAttributeValue(element, kAXSubroleAttribute, &subrole) == kAXErrorSuccess &&
        subrole != nullptr) {
        isProtected = CFGetTypeID(subrole) == CFStringGetTypeID() &&
            CFStringCompare(
                static_cast<CFStringRef>(subrole),
                CFSTR("AXSecureTextField"),
                0) == kCFCompareEqualTo;
        CFRelease(subrole);
    }
    if (isProtected || !context.IsCurrent()) {
        return true;
    }

    CFTypeRef protectedContent = nullptr;
    if (AXUIElementCopyAttributeValue(element, CFSTR("AXProtectedContent"), &protectedContent) ==
            kAXErrorSuccess &&
        protectedContent != nullptr) {
        isProtected = CFGetTypeID(protectedContent) == CFBooleanGetTypeID() &&
            CFBooleanGetValue(static_cast<CFBooleanRef>(protectedContent));
        CFRelease(protectedContent);
    }
    return isProtected || !context.IsCurrent();
}

constexpr uint8_t kAXEnhancedUserInterfaceApplied = 1u << 0;
constexpr uint8_t kAXManualAccessibilityApplied = 1u << 1;
constexpr uint8_t kAXCompatibilityAttributesApplied =
    kAXEnhancedUserInterfaceApplied | kAXManualAccessibilityApplied;

static uint64_t ProcessLaunchToken(NSRunningApplication *application) {
    if (application == nil || application.launchDate == nil) {
        return 0;
    }
    NSTimeInterval seconds = application.launchDate.timeIntervalSince1970;
    if (!std::isfinite(seconds) || seconds <= 0.0) {
        return 0;
    }
    return static_cast<uint64_t>(std::llround(seconds * 1'000'000.0));
}

// Tracks which AX compatibility attributes were successfully written. The
// launch token prevents a recycled PID from inheriting another app's state,
// and each attribute is retried independently when its write fails.
class EnhancedUiCache {
public:
    uint8_t Attributes(pid_t pid, uint64_t launchToken) {
        // Without a launch identity, a PID-only hit is unsafe because macOS
        // can recycle PIDs. Retry the compatibility writes for such apps
        // instead of carrying state across process lifetimes.
        if (launchToken == 0) {
            return 0;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        auto it = appliedPids_.find(pid);
        if (it == appliedPids_.end() || it->second.launchToken != launchToken) {
            if (it != appliedPids_.end()) {
                appliedPids_.erase(it);
            }
            return 0;
        }
        return it->second.attributes;
    }

    void MarkApplied(pid_t pid, uint64_t launchToken, uint8_t attributes) {
        if (attributes == 0 || launchToken == 0) {
            return;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        auto it = appliedPids_.find(pid);
        if (it == appliedPids_.end() || it->second.launchToken != launchToken) {
            if (appliedPids_.size() >= kMaxEntries) {
                appliedPids_.clear();
            }
            appliedPids_[pid] = CachedProcessState{launchToken, attributes};
            return;
        }
        it->second.attributes |= attributes;
    }

private:
    struct CachedProcessState {
        uint64_t launchToken;
        uint8_t attributes;
    };

    static constexpr size_t kMaxEntries = 256;
    std::mutex mutex_;
    std::unordered_map<pid_t, CachedProcessState> appliedPids_;
};

// Reuses the per-application AXUIElement across captures. The read path, the
// clipboard focus probe and other helpers previously allocated a fresh
// element for every call; the cache keeps repeated captures of one app
// allocation-free. PIDs may be recycled: an element created for pid X always
// addresses whoever owns pid X now, which stays correct because every caller
// re-derives the element for the current frontmost pid.
class ApplicationElementCache {
public:
    AXUIElementRef CopyElement(pid_t pid) {
        std::lock_guard<std::mutex> lock(mutex_);
        auto it = elements_.find(pid);
        if (it != elements_.end()) {
            CFRetain(it->second);
            return it->second;
        }
        AXUIElementRef element = AXUIElementCreateApplication(pid);
        if (element == nullptr) {
            return nullptr;
        }
        if (elements_.size() >= kMaxEntries) {
            // FIFO eviction; entries are interchangeable, so no LRU needed.
            elements_.erase(order_.front());
            order_.pop_front();
        }
        elements_[pid] = element;
        order_.push_back(pid);
        CFRetain(element);
        return element;
    }

    ~ApplicationElementCache() {
        for (const auto &entry : elements_) {
            CFRelease(entry.second);
        }
    }

private:
    static constexpr size_t kMaxEntries = 16;
    std::mutex mutex_;
    std::unordered_map<pid_t, AXUIElementRef> elements_;
    std::deque<pid_t> order_;
};

static ApplicationElementCache gApplicationElementCache;

static bool ReadViaAccessibility(
    NSRunningApplication *application,
    SelectionInfo &info,
    uint8_t appliedAttributes,
    uint8_t &compatibilityAttributesJustApplied,
    const AccessibilityReadContext &context) {
    compatibilityAttributesJustApplied = 0;
    if (application == nil || !context.IsCurrent()) {
        return false;
    }
    AXUIElementRef applicationElement =
        gApplicationElementCache.CopyElement(application.processIdentifier);
    if (applicationElement == nullptr) {
        return false;
    }
    ConfigureAccessibilityElement(applicationElement);

    AXUIElementRef focused = CopyFocusedElement(applicationElement);
    if (focused == nullptr) {
        focused = CopyFocusedWindow(applicationElement);
    }
    AXUIElementRef focusedWindow = CopyFocusedWindow(applicationElement);
    ReadWindowTitle(focusedWindow, info.windowTitle, context);

    const bool protectedFocus = focused != nullptr && ElementIsProtected(focused, context);
    bool found = false;
    if (focused != nullptr && !protectedFocus) {
        found = FindSelectionFromFocusedContext(focused, info, context);
    }
    if (!found && !protectedFocus && context.IsCurrent() && focusedWindow != nullptr &&
        (focused == nullptr || !CFEqual(focusedWindow, focused))) {
        size_t remaining = 256;
        found = FindSelectionInTree(focusedWindow, info, 5, remaining, context);
    }

    if (!protectedFocus && !found && context.IsCurrent() &&
        appliedAttributes != kAXCompatibilityAttributesApplied) {
        // Chromium/Electron often does not expose its AX tree until one of
        // these attributes is enabled. Cache only successful writes so a
        // transient AX timeout does not permanently suppress compatibility
        // setup for that process.
        if ((appliedAttributes & kAXEnhancedUserInterfaceApplied) == 0 &&
            AXUIElementSetAttributeValue(
                applicationElement,
                CFSTR("AXEnhancedUserInterface"),
                kCFBooleanTrue) == kAXErrorSuccess) {
            compatibilityAttributesJustApplied |= kAXEnhancedUserInterfaceApplied;
        }
        if ((appliedAttributes & kAXManualAccessibilityApplied) == 0 &&
            AXUIElementSetAttributeValue(
                applicationElement,
                CFSTR("AXManualAccessibility"),
                kCFBooleanTrue) == kAXErrorSuccess) {
            compatibilityAttributesJustApplied |= kAXManualAccessibilityApplied;
        }
    }

    if (found && context.IsCurrent()) {
        info.fullscreen = ReadFullscreen(applicationElement, context);
        if (context.IsCurrent()) {
            info.method = SelectionMethod::Accessibility;
        } else {
            found = false;
        }
    } else if (!context.IsCurrent()) {
        found = false;
    }
    if (focused != nullptr) CFRelease(focused);
    if (focusedWindow != nullptr) CFRelease(focusedWindow);
    CFRelease(applicationElement);
    return found;
}

static std::string LowercaseAscii(std::string value) {
    std::transform(value.begin(), value.end(), value.begin(), [](unsigned char character) {
        return static_cast<char>(std::tolower(character));
    });
    return value;
}

static bool IsWpsSelectionApplication(
    const std::string &normalizedBundleId,
    const std::string &normalizedAppName) {
    // Regional / channel variants use several Kingsoft / WPS identifiers.
    if (normalizedBundleId.rfind("com.kingsoft.", 0) == 0 ||
        normalizedBundleId.rfind("cn.wps.", 0) == 0 ||
        normalizedBundleId.rfind("com.wps.", 0) == 0 ||
        normalizedBundleId.find("wpsoffice") != std::string::npos ||
        normalizedBundleId.find("wps-office") != std::string::npos) {
        return true;
    }
    // Fallback when bundle id is missing or atypical but the display name is
    // clearly a WPS product (Writer / Spreadsheets / Presentation / PDF).
    return normalizedAppName.find("wps") != std::string::npos ||
        normalizedAppName.find("wpsoffice") != std::string::npos ||
        normalizedAppName.find("kingsoft") != std::string::npos;
}

static bool IsDocumentOrOfficeSelectionApplication(
    const std::string &bundleId,
    const std::string &appName) {
    const std::string normalizedBundleId = LowercaseAscii(bundleId);
    const std::string normalizedAppName = LowercaseAscii(appName);
    if (IsWpsSelectionApplication(normalizedBundleId, normalizedAppName)) {
        return true;
    }
    static const char *const prefixes[] = {
        "com.adobe.",
        "com.microsoft.word",
        "com.microsoft.excel",
        "com.microsoft.powerpoint",
        "com.apple.preview",
        "com.apple.iwork.pages",
        "com.apple.iwork.numbers",
        "com.apple.iwork.keynote",
        "org.libreoffice.",
        "org.openoffice.",
    };
    for (const char *prefix : prefixes) {
        if (normalizedBundleId.rfind(prefix, 0) == 0) {
            return true;
        }
    }
    return normalizedAppName.find("acrobat") != std::string::npos ||
        normalizedAppName.find("preview") != std::string::npos ||
        normalizedAppName == "word" ||
        normalizedAppName == "excel" ||
        normalizedAppName == "powerpoint" ||
        normalizedAppName.find("libreoffice") != std::string::npos;
}

// Declared in <HIServices/SecureInput.h>; redeclared here because that SDK
// header is not present in every toolchain's module map. Password fields
// enable secure event input system-wide, which must disable the synthetic
// copy fallback even when the AX tree cannot identify the focused element.
extern "C" Boolean IsSecureEventInputEnabled(void);

struct PasteboardRepresentation {
    std::string type;
    // ARC-retained immutable bytes from the pasteboard. Retaining instead of
    // copying avoids duplicating up to the full snapshot budget per capture.
    NSData *data = nil;
};

struct PasteboardItemSnapshot {
    std::vector<PasteboardRepresentation> representations;
};

struct PasteboardSnapshot {
    // `valid` also represents an originally empty pasteboard. This distinction
    // is required to restore an empty clipboard after a synthetic Cmd+C.
    bool valid = false;
    std::vector<PasteboardItemSnapshot> items;
};

// Last accepted clipboard-fallback capture, used by the stale-model guard.
// Guarded by `gLastClipboardCaptureMutex`: the clipboard fallback runs on the
// dedicated fallback worker while the manual capture path still touches it
// from the caller's thread.
struct LastClipboardCapture {
    bool valid = false;
    std::string bundleId;
    std::string normalizedText;
    CGPoint location = CGPointZero;
};

static std::mutex gPasteboardFallbackMutex;
static std::mutex gLastClipboardCaptureMutex;

// Last WeChat window that produced a real text capture. The image viewer is a
// separate window; a shallow AX hit inside a window that already yielded text
// is a chat surface, not the viewer.
static std::atomic<CGWindowID> gLastWeChatTextWindow{kCGNullWindowID};

static PasteboardSnapshot SnapshotPasteboard(NSPasteboard *pasteboard) {
    PasteboardSnapshot snapshot;
    if (pasteboard == nil) {
        return snapshot;
    }
    snapshot.valid = true;
    NSArray<NSPasteboardItem *> *items = pasteboard.pasteboardItems;
    NSUInteger totalSnapshotBytes = 0;
    NSUInteger skippedRepresentations = 0;
    for (NSPasteboardItem *item in items) {
        PasteboardItemSnapshot itemSnapshot;
        for (NSPasteboardType type in item.types) {
            // Dynamic ("dyn.*") representations are derived forms of the
            // static ones and reading one can force a cross-process lazy
            // fetch from the source app. Skip them.
            if ([type hasPrefix:@"dyn."]) {
                continue;
            }
            NSData *data = [item dataForType:type];
            if (data == nil || data.length > kClipboardSnapshotMaxRepresentationBytes) {
                // A promised/lazy (or oversized) representation cannot be
                // faithfully reconstructed after clearContents. Skipping it
                // instead of aborting the whole fallback: the capture must
                // stay available regardless of the clipboard's HISTORY, and
                // the restore below keeps every readable representation.
                skippedRepresentations += 1;
                continue;
            }
            if (totalSnapshotBytes + data.length > kClipboardSnapshotMaxTotalBytes) {
                skippedRepresentations += 1;
                continue;
            }
            totalSnapshotBytes += data.length;
            PasteboardRepresentation representation;
            representation.type = StringFromNSString(type);
            representation.data = data;
            itemSnapshot.representations.push_back(std::move(representation));
        }
        if (itemSnapshot.representations.empty()) {
            // The item exposed only unreadable/lazy representations. Drop the
            // whole item rather than restoring a hollow shell for it.
            continue;
        }
        snapshot.items.push_back(std::move(itemSnapshot));
    }
    if (snapshot.items.empty() && items.count > 0) {
        // The original clipboard had content but none of it is restorable;
        // restoring would wipe the user's clipboard. Abort this fallback.
        snapshot.valid = false;
        TraceClipboardGate("skip: clipboard-unrestorable-all-lazy");
    } else if (skippedRepresentations > 0) {
        char line[96];
        std::snprintf(
            line,
            sizeof(line),
            "snapshot: %u representations skipped (restore partial)",
            static_cast<unsigned>(skippedRepresentations));
        TraceClipboardGate(line);
    }
    return snapshot;
}

static bool RestorePasteboard(
    NSPasteboard *pasteboard,
    const PasteboardSnapshot &snapshot,
    NSInteger expectedChangeCount) {
    if (pasteboard == nil || !snapshot.valid) {
        return false;
    }
    // Fail closed: another writer already changed the pasteboard. A user copy
    // that happened after ours wins and must never be overwritten.
    if (pasteboard.changeCount != expectedChangeCount) {
        return false;
    }
    [pasteboard prepareForNewContentsWithOptions:NSPasteboardContentsCurrentHostOnly];
    if (snapshot.items.empty()) {
        return true;
    }

    NSMutableArray<NSPasteboardItem *> *items =
        [NSMutableArray arrayWithCapacity:snapshot.items.size()];
    for (const PasteboardItemSnapshot &itemSnapshot : snapshot.items) {
        NSPasteboardItem *item = [[NSPasteboardItem alloc] init];
        for (const PasteboardRepresentation &representation : itemSnapshot.representations) {
            NSString *type = NSStringFromString(representation.type);
            if (type == nil || representation.data == nil) {
                continue;
            }
            [item setData:representation.data forType:type];
        }
        if (item.types.count > 0) {
            [items addObject:item];
        }
    }
    BOOL wrote = items.count == 0 || [pasteboard writeObjects:items];
    return wrote == YES;
}

static bool PostCopyShortcut(pid_t processIdentifier, bool toSession) {
    CGEventSourceRef source = CGEventSourceCreate(kCGEventSourceStateCombinedSessionState);
    CGEventRef keyDown = CGEventCreateKeyboardEvent(source, kVK_ANSI_C, true);
    CGEventRef keyUp = CGEventCreateKeyboardEvent(source, kVK_ANSI_C, false);
    if (source != nullptr) CFRelease(source);
    if (keyDown == nullptr || keyUp == nullptr) {
        if (keyDown != nullptr) CFRelease(keyDown);
        if (keyUp != nullptr) CFRelease(keyUp);
        return false;
    }
    CGEventSetFlags(keyDown, kCGEventFlagMaskCommand);
    CGEventSetFlags(keyUp, kCGEventFlagMaskCommand);
    // Mark both events so the event tap ignores them: an unmarked synthetic
    // chord would bump the capture generation and dismiss the toolbar as if
    // the user had typed.
    CGEventSetIntegerValueField(keyDown, kCGEventSourceUserData, kSyntheticEventMarker);
    CGEventSetIntegerValueField(keyUp, kCGEventSourceUserData, kSyntheticEventMarker);
    if (toSession) {
        CGEventPost(kCGSessionEventTap, keyDown);
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
        CGEventPost(kCGSessionEventTap, keyUp);
    } else {
        CGEventPostToPid(processIdentifier, keyDown);
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
        CGEventPostToPid(processIdentifier, keyUp);
    }
    CFRelease(keyDown);
    CFRelease(keyUp);
    return true;
}

// One bounded AX probe of the frontmost app's focused element, shared by all
// clipboard-fallback gates. Reading once — instead of every gate re-creating
// the application element and re-reading focus — removes up to half a dozen
// redundant round trips per capture and guarantees every gate reasons about
// the SAME element even if the app changes focus mid-flight. The short retry
// window covers the tree-build delay right after the compatibility
// attributes were written during this same capture.
struct ClipboardFocusProbe {
    AXUIElementRef focused = nullptr;   // retained; null when never readable
    std::string role;
    bool hasRole = false;
    bool isProtected = false;
    CFRange selectedRange = CFRangeMake(-1, -1);
    bool hasSelectedRange = false;

    ClipboardFocusProbe() = default;
    ClipboardFocusProbe(const ClipboardFocusProbe &) = delete;
    ClipboardFocusProbe &operator=(const ClipboardFocusProbe &) = delete;
    ClipboardFocusProbe(ClipboardFocusProbe &&other) noexcept
        : focused(std::exchange(other.focused, nullptr)),
          role(std::move(other.role)),
          hasRole(std::exchange(other.hasRole, false)),
          isProtected(std::exchange(other.isProtected, false)),
          selectedRange(std::exchange(other.selectedRange, CFRangeMake(-1, -1))),
          hasSelectedRange(std::exchange(other.hasSelectedRange, false)) {}
    ClipboardFocusProbe &operator=(ClipboardFocusProbe &&other) noexcept {
        if (this != &other) {
            if (focused != nullptr) {
                CFRelease(focused);
            }
            focused = std::exchange(other.focused, nullptr);
            role = std::move(other.role);
            hasRole = std::exchange(other.hasRole, false);
            isProtected = std::exchange(other.isProtected, false);
            selectedRange = std::exchange(other.selectedRange, CFRangeMake(-1, -1));
            hasSelectedRange = std::exchange(other.hasSelectedRange, false);
        }
        return *this;
    }
    ~ClipboardFocusProbe() {
        if (focused != nullptr) {
            CFRelease(focused);
        }
    }
};

static ClipboardFocusProbe ProbeClipboardFocus(NSRunningApplication *application) {
    ClipboardFocusProbe probe;
    if (application == nil) {
        return probe;
    }
    AXUIElementRef applicationElement =
        gApplicationElementCache.CopyElement(application.processIdentifier);
    if (applicationElement == nullptr) {
        return probe;
    }
    // Every AX round trip stays inside the shared capture budget: a provider
    // that answered the text read can still hang on these follow-up reads.
    ConfigureAccessibilityElement(applicationElement);
    for (int attempt = 0; attempt < 3; ++attempt) {
        if (attempt > 0) {
            std::this_thread::sleep_for(std::chrono::milliseconds(30));
        }
        AXUIElementRef focused = CopyFocusedElement(applicationElement);
        if (focused == nullptr) {
            continue;
        }
        ConfigureAccessibilityElement(focused);

        CFTypeRef role = nullptr;
        if (AXUIElementCopyAttributeValue(focused, kAXRoleAttribute, &role) ==
                kAXErrorSuccess &&
            role != nullptr) {
            probe.hasRole = CFGetTypeID(role) == CFStringGetTypeID() &&
                StringFromCFString(static_cast<CFStringRef>(role), probe.role);
            CFRelease(role);
        }

        bool isProtected = false;
        CFTypeRef subrole = nullptr;
        if (AXUIElementCopyAttributeValue(focused, kAXSubroleAttribute, &subrole) ==
                kAXErrorSuccess &&
            subrole != nullptr) {
            isProtected = CFGetTypeID(subrole) == CFStringGetTypeID() &&
                CFStringCompare(
                    static_cast<CFStringRef>(subrole),
                    CFSTR("AXSecureTextField"),
                    0) == kCFCompareEqualTo;
            CFRelease(subrole);
        }
        if (!isProtected) {
            CFTypeRef protectedContent = nullptr;
            if (AXUIElementCopyAttributeValue(
                    focused,
                    CFSTR("AXProtectedContent"),
                    &protectedContent) == kAXErrorSuccess &&
                protectedContent != nullptr) {
                isProtected = CFGetTypeID(protectedContent) == CFBooleanGetTypeID() &&
                    CFBooleanGetValue(static_cast<CFBooleanRef>(protectedContent));
                CFRelease(protectedContent);
            }
        }
        probe.isProtected = isProtected;

        AXValueRef rangeValue = nullptr;
        if (AXUIElementCopyAttributeValue(
                focused,
                kAXSelectedTextRangeAttribute,
                reinterpret_cast<CFTypeRef *>(&rangeValue)) == kAXErrorSuccess &&
            rangeValue != nullptr) {
            CFRange range = CFRangeMake(-1, -1);
            if (AXValueGetValue(rangeValue, kAXValueTypeCFRange, &range)) {
                probe.selectedRange = range;
                probe.hasSelectedRange = true;
            }
            CFRelease(rangeValue);
        }

        probe.focused = focused;   // the probe owns the element now
        break;
    }
    CFRelease(applicationElement);
    return probe;
}

static bool FocusProbeIsImage(const ClipboardFocusProbe &probe) {
    return probe.hasRole && probe.role == "AXImage";
}

// Result of probing the AX element under the cursor before a synthetic copy.
// Image viewers (WeChat 图片查看器) frequently hide the image behind an
// opaque container whose focused-element role cannot be read, so the
// focused-element gate never fires for them; a synthetic Cmd+C in that
// context copies the image with visible app feedback (the drag on an image
// reads as a Drag gesture). The hit test queries a different AX path
// (element-at-position) and is bounded by the geometry messaging timeout.
struct PointerHitTestOutcome {
    // element-at-position answered with an element (false = unsupported or
    // no element; the caller keeps today's behavior).
    bool resolved = false;
    // The hit element or one of its two closest ancestors is an AXImage.
    bool isImage = false;
    // The hit element IS the bare window/application — the app exposes no
    // content under the cursor at all (opaque canvas surface).
    bool bareWindowSurface = false;
    // The hit element is the menu bar; a text selection never ends there.
    bool isMenuBar = false;
    // The hit element or one of its two closest ancestors accepts text.
    bool hasTextRole = false;
    // An ancestor also hosts selectable content (chat split view / web area)
    // even when the exact hit point is not text. A start point in blank space
    // of a real content window still belongs to the same text selection.
    bool hasContentRole = false;
    // The chain reached the top-level window within the scan depth. WeChat's
    // image viewer reports hit -> groups -> AXWindow with no text or content
    // ancestor; a synthetic Cmd+C on that surface copies the viewed image.
    bool reachedWindow = false;
    // The window itself contains a real content surface even though the hit
    // chain did not pass through it (a shallow button inside the chat window).
    // Scanning the window separates that case from an opaque viewer window.
    bool windowHasContentSurface = false;
    // Frame of the top-level window containing the hit element. Comparing the
    // start and end frames separates a text selection (same window) from a
    // window drag (the press point now exposes the window behind).
    bool hasWindowFrame = false;
    CGRect windowFrame = CGRectZero;
    int axError = 0;
    std::string roleChain;
};

static bool IsTextLayerRole(const std::string &role) {
    return role == "AXStaticText" || role == "AXTextArea" ||
        role == "AXTextField" || role == "AXTextView";
}

static bool IsContentSurfaceRole(const std::string &role) {
    return role == "AXSplitGroup" || role == "AXWebArea";
}

static bool CopyAXWindowFrame(AXUIElementRef element, CGRect &frame) {
    if (element == nullptr) {
        return false;
    }
    AXUIElementRef window = nullptr;
    if (AXUIElementCopyAttributeValue(
            element,
            kAXWindowAttribute,
            reinterpret_cast<CFTypeRef *>(&window)) != kAXErrorSuccess ||
        window == nullptr) {
        return false;
    }
    AXUIElementSetMessagingTimeout(window, kAXGeometryMessagingTimeoutSeconds);
    AXValueRef positionValue = nullptr;
    AXValueRef sizeValue = nullptr;
    const bool copied =
        AXUIElementCopyAttributeValue(
            window,
            kAXPositionAttribute,
            reinterpret_cast<CFTypeRef *>(&positionValue)) == kAXErrorSuccess &&
        AXUIElementCopyAttributeValue(
            window,
            kAXSizeAttribute,
            reinterpret_cast<CFTypeRef *>(&sizeValue)) == kAXErrorSuccess;
    bool ok = false;
    if (copied && positionValue != nullptr && sizeValue != nullptr) {
        CGPoint position = CGPointZero;
        CGSize size = CGSizeZero;
        if (AXValueGetValue(positionValue, kAXValueTypeCGPoint, &position) &&
            AXValueGetValue(sizeValue, kAXValueTypeCGSize, &size)) {
            frame = CGRectMake(position.x, position.y, size.width, size.height);
            ok = true;
        }
    }
    if (positionValue != nullptr) {
        CFRelease(positionValue);
    }
    if (sizeValue != nullptr) {
        CFRelease(sizeValue);
    }
    CFRelease(window);
    return ok;
}

static bool SubtreeContainsContentSurface(
    AXUIElementRef element,
    int depth,
    size_t &remaining) {
    if (element == nullptr || depth < 0 || remaining == 0) {
        return false;
    }
    --remaining;
    AXUIElementSetMessagingTimeout(element, kAXGeometryMessagingTimeoutSeconds);
    CFArrayRef children = nullptr;
    if (AXUIElementCopyAttributeValue(
            element,
            kAXChildrenAttribute,
            reinterpret_cast<CFTypeRef *>(&children)) != kAXErrorSuccess ||
        children == nullptr) {
        return false;
    }
    bool found = false;
    const CFIndex count = std::min<CFIndex>(CFArrayGetCount(children), 32);
    for (CFIndex index = 0; index < count && !found && remaining > 0; ++index) {
        AXUIElementRef child = static_cast<AXUIElementRef>(
            const_cast<void *>(CFArrayGetValueAtIndex(children, index)));
        std::string role;
        CFTypeRef roleValue = nullptr;
        AXUIElementSetMessagingTimeout(child, kAXGeometryMessagingTimeoutSeconds);
        if (AXUIElementCopyAttributeValue(child, kAXRoleAttribute, &roleValue) ==
                kAXErrorSuccess &&
            roleValue != nullptr) {
            if (CFGetTypeID(roleValue) == CFStringGetTypeID()) {
                StringFromCFString(static_cast<CFStringRef>(roleValue), role);
            }
            CFRelease(roleValue);
        }
        if (IsContentSurfaceRole(role)) {
            found = true;
        } else if (depth > 0) {
            found = SubtreeContainsContentSurface(child, depth - 1, remaining);
        }
    }
    CFRelease(children);
    return found;
}

static PointerHitTestOutcome PointerHitTestAtPosition(
    NSRunningApplication *application,
    CGPoint point,
    bool includeWindowFrame = false,
    bool inspectWindowContent = false) {
    PointerHitTestOutcome outcome;
    if (application == nil) {
        return outcome;
    }
    AXUIElementRef applicationElement =
        gApplicationElementCache.CopyElement(application.processIdentifier);
    if (applicationElement == nullptr) {
        return outcome;
    }
    AXUIElementSetMessagingTimeout(applicationElement, kAXGeometryMessagingTimeoutSeconds);
    AXUIElementRef hit = nullptr;
    AXError error = AXUIElementCopyElementAtPosition(
        applicationElement, point.x, point.y, &hit);
    CFRelease(applicationElement);
    if (error != kAXErrorSuccess || hit == nullptr) {
        outcome.axError = static_cast<int>(error);
        return outcome;
    }
    outcome.resolved = true;
    if (includeWindowFrame && CopyAXWindowFrame(hit, outcome.windowFrame)) {
        outcome.hasWindowFrame = true;
    }

    static const char *const kRoleSeparator = ">";
    AXUIElementRef current = hit;
    CFRetain(current);
    // WeChat's image viewer hides the image below several wrapper groups; a
    // three-level scan stops before the window and cannot tell it apart from
    // a chat bubble. Deep-scan only the WeChat call sites (includeWindowFrame).
    const int scanDepth = includeWindowFrame ? 5 : 3;
    for (int level = 0; level < scanDepth && !outcome.isImage && current != nullptr; ++level) {
        std::string role;
        CFTypeRef roleValue = nullptr;
        if (AXUIElementCopyAttributeValue(current, kAXRoleAttribute, &roleValue) ==
                kAXErrorSuccess &&
            roleValue != nullptr) {
            if (CFGetTypeID(roleValue) == CFStringGetTypeID()) {
                StringFromCFString(static_cast<CFStringRef>(roleValue), role);
            }
            CFRelease(roleValue);
        }
        if (!outcome.roleChain.empty()) {
            outcome.roleChain += kRoleSeparator;
        }
        outcome.roleChain += role.empty() ? "?" : role;
        if (IsTextLayerRole(role)) {
            outcome.hasTextRole = true;
        }
        if (IsContentSurfaceRole(role)) {
            outcome.hasContentRole = true;
        }
        if (level == 0) {
            // Bare window surface: no content element under the cursor.
            outcome.bareWindowSurface =
                role == "AXWindow" || role == "AXApplication";
            outcome.isMenuBar = role == "AXMenuBar";
        }
        if (role == "AXImage") {
            outcome.isImage = true;
            break;
        }
        if (role == "AXWindow" || role == "AXApplication") {
            outcome.reachedWindow = true;
            // A shallow button also exists inside the chat window. Only call
            // the hit chain opaque when the containing window has no content
            // surface of its own; the image viewer window has none.
            if (inspectWindowContent && role == "AXWindow" &&
                !outcome.hasTextRole && !outcome.hasContentRole) {
                size_t remaining = 48;
                outcome.windowHasContentSurface =
                    SubtreeContainsContentSurface(current, 2, remaining);
            }
            break;
        }
        if (outcome.hasTextRole || outcome.hasContentRole) {
            break;
        }
        AXUIElementRef parent = nullptr;
        if (AXUIElementCopyAttributeValue(
                current,
                kAXParentAttribute,
                reinterpret_cast<CFTypeRef *>(&parent)) != kAXErrorSuccess ||
            parent == nullptr) {
            break;
        }
        AXUIElementRef previous = current;
        current = parent;
        if (previous != hit) {
            CFRelease(previous);
        }
    }
    if (current != nullptr && current != hit) {
        CFRelease(current);
    }
    CFRelease(hit);
    return outcome;
}

// True when a WeChat gesture's anchor sits on an opaque, text-less window
// surface: the hit chain reaches the top-level window through wrapper groups
// with no text or content ancestor, and the window itself has no content
// surface. The image viewer reports exactly this chain
// (AXButton > AXGroup > AXWindow); synthesizing Cmd+C there copies the viewed
// image with visible feedback. A shallow button inside the chat window still
// has the chat split view below the window and is not treated as opaque.
static bool PointerHitTestIsOpaqueWindowSurface(const PointerHitTestOutcome &outcome) {
    return outcome.resolved && outcome.reachedWindow &&
        !outcome.hasTextRole && !outcome.hasContentRole &&
        !outcome.windowHasContentSurface;
}

// Even an opaque chain is a text surface when the anchor window already
// produced a real text capture. WeChat's image viewer is a new, different
// window, so it never inherits that trust.
static bool WeChatAnchorIsOpaqueWindow(
    const PointerHitTestOutcome &outcome,
    CGWindowID anchorWindow) {
    if (!PointerHitTestIsOpaqueWindowSurface(outcome)) {
        return false;
    }
    if (anchorWindow == kCGNullWindowID) {
        return true;
    }
    return anchorWindow != gLastWeChatTextWindow.load(std::memory_order_relaxed);
}

// True when a TEXT-role focused element explicitly reports an empty selected
// range. Only text roles are trusted: generic containers and chat canvases
// (WeChat) report {0,0} regardless of the user's canvas selection, and
// trusting them killed WeChat text capture. Combined with the click-only
// trigger gate at the call site this keeps the editor "implicit copy on
// empty double-click" fix (JetBrains-style Cmd+C copies the current line)
// while canvas selections keep using the fallback.
static bool FocusProbeTextElementExplicitlyEmpty(const ClipboardFocusProbe &probe) {
    if (!probe.hasRole || !probe.hasSelectedRange) {
        return false;
    }
    const bool textRole = probe.role == "AXTextArea" ||
        probe.role == "AXTextField" ||
        probe.role == "AXTextView" ||
        probe.role == "AXStaticText";
    return textRole && probe.selectedRange.location == 0 && probe.selectedRange.length == 0;
}

// Apps where a synthetic Cmd+C is destructive or sensitive must never use the
// fallback; apps whose AX selection already works never reach it either.
// GPU-rendered terminals (kitty/Alacritty/Ghostty/WezTerm) implement Cmd+C as
// pure copy — a selection-less copy is a no-op there — so they are allowed.
enum class ClipboardFallbackPolicy {
    Allowed,
    // Smart-Cmd+C terminals send SIGINT when nothing is selected. Only
    // keyboard gestures (Cmd+A, shift+arrows) qualify: they always leave a
    // real selection behind, so the chord can only ever copy.
    KeyboardOnly,
    Denied,
};

static ClipboardFallbackPolicy ClipboardFallbackPolicyForApplication(
    const std::string &bundleId,
    const std::string &appName,
    Trigger trigger) {
    const std::string normalizedBundle = LowercaseAscii(bundleId);
    static const char *const deniedBundlePrefixes[] = {
        // Password managers: a synthetic copy could lift a secret into the
        // selection pipeline and on into an AI request.
        "com.1password.1password",
        "com.agilebits.",
        "org.keepassxc.keepassxc",
        "com.bitwarden.desktop",
        // Finder drag-select is an icon rubber-band; Cmd+C would copy files.
        "com.apple.finder",
    };
    for (const char *prefix : deniedBundlePrefixes) {
        if (normalizedBundle.rfind(prefix, 0) == 0) {
            return ClipboardFallbackPolicy::Denied;
        }
    }
    const std::string normalizedName = LowercaseAscii(appName);
    const bool smartInterruptTerminal =
        normalizedBundle.rfind("com.apple.terminal", 0) == 0 ||
        normalizedBundle.rfind("com.googlecode.iterm2", 0) == 0 ||
        normalizedName.find("terminal") != std::string::npos ||
        normalizedName.find("iterm") != std::string::npos;
    if (smartInterruptTerminal) {
        return trigger == Trigger::Keyboard
            ? ClipboardFallbackPolicy::KeyboardOnly
            : ClipboardFallbackPolicy::Denied;
    }
    return ClipboardFallbackPolicy::Allowed;
}

// True when the current pasteboard contents carry KNOWN non-text data
// (image/file) and no text type at all — the app answered the synthetic copy
// with an image/file payload (image viewers do this), so waiting longer
// cannot yield a text selection. Deliberately a whitelist: an app writing
// text under an unusual UTI must not be misread as non-text, so unknown
// type sets keep waiting for the full budget. Type-list inspection only:
// fetching data could lazily block on a provider.
static bool PasteboardChangeIsNonText(NSPasteboard *pasteboard) {
    NSArray<NSPasteboardType> *types = pasteboard.types;
    if (types.count == 0) {
        // Empty contents are the clear phase of a two-step write; keep
        // waiting for the string to land.
        return false;
    }
    bool hasKnownNonTextData = false;
    for (NSPasteboardType type in types) {
        if ([type isEqualToString:NSPasteboardTypeString] ||
            [type isEqualToString:NSPasteboardTypeRTF] ||
            [type isEqualToString:NSPasteboardTypeHTML] ||
            [type isEqualToString:@"public.utf16-plain-text"]) {
            return false;
        }
        if ([type isEqualToString:NSPasteboardTypeTIFF] ||
            [type isEqualToString:NSPasteboardTypePNG] ||
            [type isEqualToString:@"public.jpeg"] ||
            [type isEqualToString:@"public.file-url"] ||
            [type isEqualToString:@"NSFilesPromisePboardType"]) {
            hasKnownNonTextData = true;
        }
    }
    return hasKnownNonTextData;
}

// Trace lines carry the app + trigger so one log file can be attributed per
// application when hunting capture regressions.
static void TraceClipboardGateFor(const SelectionInfo &info, const char *detail) {
    char line[224];
    std::snprintf(
        line,
        sizeof(line),
        "app=%s trigger=%s %s",
        info.bundleId.c_str(),
        TriggerTag(info.trigger),
        detail);
    TraceClipboardGate(line);
}

// First characters of the captured text for the opt-in trace file only —
// enough to tell WHAT an empty-space copy returned (stale selection vs
// current line vs junk) without dumping user content. Newlines and control
// characters are masked.
static std::string TraceTextPrefix(const std::string &text) {
    NSString *raw = NSStringFromString(text);
    if (raw == nil) {
        return {};
    }
    NSString *oneLine =
        [raw stringByReplacingOccurrencesOfString:@"\n" withString:@" "];
    oneLine = [oneLine stringByReplacingOccurrencesOfString:@"\r" withString:@" "];
    if (oneLine.length > 24) {
        oneLine = [oneLine substringToIndex:24];
    }
    NSMutableString *clean = [oneLine mutableCopy];
    NSUInteger index = 0;
    while (index < clean.length) {
        if ([clean characterAtIndex:index] < 0x20 ||
            [clean characterAtIndex:index] == 0x7F) {
            [clean replaceCharactersInRange:NSMakeRange(index, 1) withString:@"·"];
        }
        index += 1;
    }
    return StringFromNSString(clean);
}

static bool CaptureViaClipboard(
    NSRunningApplication *application,
    SelectionInfo &info,
    LastClipboardCapture &lastClipboardCapture,
    const std::atomic<uint64_t> *captureGeneration,
    uint64_t expectedGeneration,
    uint64_t deadlineMs) {
    if (application == nil || application.processIdentifier == getpid() ||
        !application.active) {
        TraceClipboardGateFor(info, "skip: app-inactive-or-own-process");
        return false;
    }
    auto generationCurrent = [&]() {
        return captureGeneration == nullptr ||
            captureGeneration->load(std::memory_order_acquire) == expectedGeneration;
    };
    if (!generationCurrent()) {
        // Cheapest gate first: no AX work should happen for a gesture that
        // new input already superseded.
        TraceClipboardGateFor(info, "skip: superseded-by-new-input");
        return false;
    }

    ClipboardFocusProbe focus = ProbeClipboardFocus(application);
    if (focus.isProtected) {
        TraceClipboardGateFor(info, "skip: protected-focus");
        return false;
    }
    if (IsSecureEventInputEnabled()) {
        TraceClipboardGateFor(info, "skip: secure-event-input");
        return false;
    }
    if ((info.trigger == Trigger::DoubleClick || info.trigger == Trigger::ShiftClick) &&
        FocusProbeTextElementExplicitlyEmpty(focus)) {
        // A text-role element (editor text area) explicitly reports "nothing
        // selected": the chord could only produce an implicit copy (current
        // line). Drag/keyboard gestures never trust this signal — canvas apps
        // (WeChat) report {0,0} on containers that do not reflect the user's
        // canvas selection.
        TraceClipboardGateFor(info, "skip: text-element-empty-range-on-click");
        return false;
    }
    if (FocusProbeIsImage(focus)) {
        // Image-viewer context: the chord would copy the image (with visible
        // app feedback) instead of a text selection.
        TraceClipboardGateFor(info, "skip: focused-element-is-image");
        return false;
    }
    if (info.trigger == Trigger::Drag || info.trigger == Trigger::DoubleClick ||
        info.trigger == Trigger::ShiftClick) {
        // Mouse gestures carry a meaningful cursor position; keyboard
        // gestures keep the fallback because the cursor may be parked
        // anywhere while the user selects by keyboard.
        const bool isWeChat = info.bundleId == "com.tencent.xinWeChat";
        const bool inspectWindowContent = isWeChat &&
            (info.trigger == Trigger::DoubleClick ||
             info.trigger == Trigger::ShiftClick);
        const PointerHitTestOutcome hitTest = PointerHitTestAtPosition(
            application, info.mouseCurrent, isWeChat, inspectWindowContent);
        char hitLine[192];
        if (hitTest.resolved) {
            std::snprintf(
                hitLine,
                sizeof(hitLine),
                "pointer-hit-test: roles=%s text=%d content=%d window=%d surface=%d",
                hitTest.roleChain.c_str(),
                hitTest.hasTextRole ? 1 : 0,
                hitTest.hasContentRole ? 1 : 0,
                hitTest.reachedWindow ? 1 : 0,
                hitTest.windowHasContentSurface ? 1 : 0);
        } else {
            std::snprintf(
                hitLine,
                sizeof(hitLine),
                "pointer-hit-test: unavailable (axError=%d)",
                hitTest.axError);
        }
        TraceClipboardGate(hitLine);
        if (hitTest.isImage) {
            TraceClipboardGateFor(info, "skip: pointer-element-is-image");
            return false;
        }
        // WeChat's image viewer exposes no AX content under the cursor — the
        // hit test lands straight on the bare AXWindow — while its chat
        // window does expose content groups for text drags. A drag on that
        // bare surface can only be the image viewer (or empty canvas
        // background), where the chord copies the image with visible
        // feedback and never yields text.
        if (isWeChat && hitTest.bareWindowSurface) {
            TraceClipboardGateFor(info, "skip: wechat-bare-window-drag");
            return false;
        }
        // A text selection never ends on the menu bar; in WeChat this is a
        // window/title-bar drag (seen in traces) that copies chrome. Keep the
        // rule app-scoped so canvas hosts keep their existing fallback.
        if (isWeChat && hitTest.isMenuBar) {
            TraceClipboardGateFor(info, "skip: pointer-element-is-menu-bar");
            return false;
        }
        // A double-click or shift-click inside WeChat's image viewer lands on
        // the same opaque window surface as a drag. There is no text to copy;
        // the chord would copy the image with visible feedback.
        if (isWeChat &&
            (info.trigger == Trigger::DoubleClick || info.trigger == Trigger::ShiftClick)) {
            const CGWindowID anchorWindow = TopmostWindowNumberAtPoint(
                application.processIdentifier, info.mouseCurrent);
            char windowNumberLine[96];
            std::snprintf(
                windowNumberLine,
                sizeof(windowNumberLine),
                "pointer-window-number: point=%u trusted=%u",
                anchorWindow,
                gLastWeChatTextWindow.load(std::memory_order_relaxed));
            TraceClipboardGate(windowNumberLine);
            if (WeChatAnchorIsOpaqueWindow(hitTest, anchorWindow)) {
                TraceClipboardGateFor(info, "skip: wechat-opaque-window-click");
                return false;
            }
        }
        // WeChat's image viewer exposes no text under the cursor. The 0.4.1
        // gate only covered a bare AXWindow hit, but real traces also show
        // image-viewer and window drags landing on AXButton/AXGroup. The hit
        // test runs at the release point, which can legitimately land on the
        // chat input button for a text drag, so the end chain cannot separate
        // the two. Compare the drag start instead: a text selection starts on
        // text inside the same window, while a window drag leaves the press
        // point over the window behind.
        if (isWeChat && info.trigger == Trigger::Drag && info.hasMouseStart) {
            const PointerHitTestOutcome startHit =
                PointerHitTestAtPosition(application, info.mouseStart, true, true);
            char startLine[192];
            if (startHit.resolved) {
                std::snprintf(
                    startLine,
                    sizeof(startLine),
                    "pointer-hit-test-start: roles=%s text=%d content=%d window=%d surface=%d",
                    startHit.roleChain.c_str(),
                    startHit.hasTextRole ? 1 : 0,
                    startHit.hasContentRole ? 1 : 0,
                    startHit.reachedWindow ? 1 : 0,
                    startHit.windowHasContentSurface ? 1 : 0);
            } else {
                std::snprintf(
                    startLine,
                    sizeof(startLine),
                    "pointer-hit-test-start: unavailable (axError=%d)",
                    startHit.axError);
            }
            TraceClipboardGate(startLine);
            const CGWindowID startWindow = TopmostWindowNumberAtPoint(
                application.processIdentifier, info.mouseStart);
            const CGWindowID endWindow = TopmostWindowNumberAtPoint(
                application.processIdentifier, info.mouseCurrent);
            char windowNumberLine[128];
            std::snprintf(
                windowNumberLine,
                sizeof(windowNumberLine),
                "pointer-window-numbers: start=%u end=%u trusted=%u",
                startWindow,
                endWindow,
                gLastWeChatTextWindow.load(std::memory_order_relaxed));
            TraceClipboardGate(windowNumberLine);
            if (startHit.hasWindowFrame && hitTest.hasWindowFrame) {
                char windowLine[144];
                std::snprintf(
                    windowLine,
                    sizeof(windowLine),
                    "pointer-hit-test-window: start=%.0f,%.0f,%.0fx%.0f end=%.0f,%.0f,%.0fx%.0f",
                    startHit.windowFrame.origin.x,
                    startHit.windowFrame.origin.y,
                    startHit.windowFrame.size.width,
                    startHit.windowFrame.size.height,
                    hitTest.windowFrame.origin.x,
                    hitTest.windowFrame.origin.y,
                    hitTest.windowFrame.size.width,
                    hitTest.windowFrame.size.height);
                TraceClipboardGate(windowLine);
            }
            if (startHit.isImage) {
                TraceClipboardGateFor(info, "skip: wechat-image-drag-start");
                return false;
            }
            if (startHit.bareWindowSurface) {
                TraceClipboardGateFor(info, "skip: wechat-bare-window-drag-start");
                return false;
            }
            // The image viewer's press point reports AXButton > AXGroup >
            // AXWindow with no text or content ancestor, while a real text
            // selection starts on text or inside the chat split view. Window
            // numbers alone cannot see this: the drag starts and ends inside
            // the same viewer window. A window that already produced text is
            // trusted so shallow chat hits keep working.
            if (WeChatAnchorIsOpaqueWindow(startHit, startWindow)) {
                TraceClipboardGateFor(info, "skip: wechat-opaque-window-drag-start");
                return false;
            }
            // AX window frames are unavailable for these WeChat drags (the
            // viewer does not answer position/size queries), so compare the
            // window server's topmost window instead. A drag that leaves the
            // chat window and lands on the image viewer is not a text
            // selection; synthesizing Cmd+C there copies the image.
            if (startWindow != kCGNullWindowID && endWindow != kCGNullWindowID &&
                startWindow != endWindow) {
                TraceClipboardGateFor(info, "skip: wechat-cross-window-drag");
                return false;
            }
            if (startHit.hasWindowFrame && hitTest.hasWindowFrame) {
                constexpr CGFloat kWindowFrameTolerance = 1.0;
                const bool sameWindow =
                    std::fabs(startHit.windowFrame.origin.x - hitTest.windowFrame.origin.x) <=
                        kWindowFrameTolerance &&
                    std::fabs(startHit.windowFrame.origin.y - hitTest.windowFrame.origin.y) <=
                        kWindowFrameTolerance &&
                    std::fabs(startHit.windowFrame.size.width - hitTest.windowFrame.size.width) <=
                        kWindowFrameTolerance &&
                    std::fabs(startHit.windowFrame.size.height - hitTest.windowFrame.size.height) <=
                        kWindowFrameTolerance;
                if (!sameWindow) {
                    TraceClipboardGateFor(info, "skip: wechat-cross-window-drag");
                    return false;
                }
            }
        }
    }

    std::lock_guard<std::mutex> processGuard(gPasteboardFallbackMutex);
    @autoreleasepool {
        NSPasteboard *pasteboard = [NSPasteboard generalPasteboard];
        if (pasteboard == nil) {
            return false;
        }
        const NSInteger originalChangeCount = pasteboard.changeCount;
        PasteboardSnapshot snapshot = SnapshotPasteboard(pasteboard);
        if (!snapshot.valid) {
            return false;
        }
        if (!PostCopyShortcut(application.processIdentifier, false)) {
            return false;
        }

        const uint64_t startedAt = MonotonicMilliseconds();
        const uint64_t fallbackDeadlineMs = deadlineMs;
        NSInteger copiedChangeCount = originalChangeCount;
        bool changed = false;
        bool postedSessionRetry = false;
        NSInteger nonTextStablePolls = 0;
        std::string copiedText;
        while (generationCurrent() && MonotonicMilliseconds() < fallbackDeadlineMs) {
            std::this_thread::sleep_for(
                std::chrono::milliseconds(kClipboardPollIntervalMs));
            const NSInteger changeCount = pasteboard.changeCount;
            if (changeCount == originalChangeCount) {
                if (!postedSessionRetry &&
                    MonotonicMilliseconds() - startedAt >= kClipboardSessionRetryAfterMs) {
                    // Some apps ignore pid-targeted events. The generation
                    // check above still guarantees no user input happened,
                    // so the session-level chord reaches the same app.
                    postedSessionRetry = true;
                    PostCopyShortcut(application.processIdentifier, true);
                }
                continue;
            }
            changed = true;
            if (changeCount != copiedChangeCount) {
                // A fresh write arrived (first response or a follow-up step);
                // the stability clock restarts so the restore boundary below
                // lands on the FINAL contents.
                copiedChangeCount = changeCount;
                nonTextStablePolls = 0;
            }
            NSString *string = [pasteboard stringForType:NSPasteboardTypeString];
            copiedText = StringFromNSString(string);
            if (!IsBlank(copiedText)) {
                break;
            }
            if (PasteboardChangeIsNonText(pasteboard)) {
                // The app answered the chord with image/file data only.
                // Waiting longer cannot yield text, but some apps publish in
                // several steps; require a short stable window so the restore
                // below reclaims the final contents instead of racing a
                // mid-write boundary.
                nonTextStablePolls += 1;
                if (nonTextStablePolls >= kClipboardNonTextStablePolls) {
                    break;
                }
                continue;
            }
            // Providers such as PDF viewers clear-then-write in two steps;
            // keep polling for the string within the same budget.
        }
        if (!changed && generationCurrent()) {
            // The chord can also land after polling stopped: a slow app past
            // the deadline. Claim it briefly so the restore below reclaims
            // our copy instead of leaving it behind in the user's clipboard.
            // Skipped once new input arrived (generation changed): a landing
            // copy could then be the user's own Cmd+C, and restoring over it
            // would clobber their clipboard. Leaving our late copy behind is
            // the recoverable mistake; overwriting their copy is not.
            const uint64_t lateDeadlineMs =
                MonotonicMilliseconds() + kClipboardLateCopyClaimMs;
            while (MonotonicMilliseconds() < lateDeadlineMs && generationCurrent()) {
                std::this_thread::sleep_for(
                    std::chrono::milliseconds(kClipboardPollIntervalMs));
                if (pasteboard.changeCount != originalChangeCount) {
                    changed = true;
                    copiedChangeCount = pasteboard.changeCount;
                    NSString *string = [pasteboard stringForType:NSPasteboardTypeString];
                    copiedText = StringFromNSString(string);
                    break;
                }
            }
        }
        if (changed) {
            // `copiedChangeCount` stays the count our synthetic copy produced.
            // RestorePasteboard fail-closes unless the pasteboard still holds
            // exactly that write, so a real user copy made during the wait
            // (changeCount advanced) always wins and is never overwritten.
            const bool restored = RestorePasteboard(pasteboard, snapshot, copiedChangeCount);
            if (!restored) {
                TraceClipboardGateFor(
                    info, "reclaim: restore-boundary-missed (pasteboard moved on)");
            } else if (IsBlank(copiedText)) {
                TraceClipboardGateFor(info, "reclaim: non-text-copy");
            }
        }
        if (!changed) {
            TraceClipboardGateFor(info, "abort: chord-ignored-by-app");
            return false;
        }
        if (!generationCurrent() ||
            IsBlank(copiedText) || copiedText.size() > kClipboardMaxTextBytes) {
            TraceClipboardGateFor(info, "abort: interrupted-or-blank-oversized");
            return false;
        }
        SelectionInfo candidate = info;
        ClearSelectionGeometry(candidate);
        candidate.text = std::move(copiedText);
        candidate.method = SelectionMethod::Clipboard;
        if (SelectionTextMatchesSourceIdentity(candidate)) {
            TraceClipboardGateFor(info, "reject: text-matches-app-identity");
            return false;
        }
        if (SelectionTextIsDegenerateFragment(candidate.text)) {
            TraceClipboardGateFor(info, "reject: degenerate-fragment");
            return false;
        }
        const std::string normalizedCapture = NormalizeSelectionIdentity(candidate.text);
        bool stale = false;
        {
            std::lock_guard<std::mutex> lock(gLastClipboardCaptureMutex);
            if (lastClipboardCapture.valid &&
                lastClipboardCapture.bundleId == info.bundleId &&
                lastClipboardCapture.normalizedText == normalizedCapture &&
                std::hypot(
                    lastClipboardCapture.location.x - info.mouseCurrent.x,
                    lastClipboardCapture.location.y - info.mouseCurrent.y) >
                    kStaleClipboardSelectionDistancePx) {
                // Same text captured far from where it was originally selected:
                // the app kept its old model selection and the synthetic copy
                // returned it. Treat as stale instead of presenting a toolbar for
                // content the user did not just select.
                stale = true;
            } else {
                lastClipboardCapture.valid = true;
                lastClipboardCapture.bundleId = info.bundleId;
                lastClipboardCapture.normalizedText = normalizedCapture;
                lastClipboardCapture.location = info.mouseCurrent;
            }
        }
        if (stale) {
            TraceClipboardGateFor(info, "reject: stale-same-text-far-location");
            return false;
        }
        char okLine[256];
        std::snprintf(
            okLine,
            sizeof(okLine),
            "app=%s trigger=%s capture: ok bytes=%zu prefix=\"%s\"",
            info.bundleId.c_str(),
            TriggerTag(info.trigger),
            candidate.text.size(),
            TraceTextPrefix(candidate.text).c_str());
        TraceClipboardGate(okLine);
        info = std::move(candidate);
        return true;
    }
}

// ---------------------------------------------------------------------------
// Browser AppleScript fallback
// ---------------------------------------------------------------------------

// Browsers where reading the live DOM selection via AppleScript is worth a
// subprocess round trip before any synthetic-copy fallback. Safari is the
// main beneficiary: it never exposes AXSelectedText, so without this layer
// every Safari selection paid for a synthetic Cmd+C. Chrome-kernel browsers
// only answer when "Allow JavaScript from Apple Events" is enabled; when it
// is not, osascript fails fast and the clipboard fallback proceeds.
static bool IsAppleScriptSelectionBrowser(const std::string &bundleId) {
    static const char *const browsers[] = {
        "com.apple.Safari",
        "com.google.Chrome",
        "com.microsoft.edgemac",
        "com.brave.Browser",
        "com.vivaldi.Vivaldi",
        "com.operasoftware.Opera",
        "Company.TheBrowser.Arc",
    };
    for (const char *candidate : browsers) {
        if (bundleId == candidate) {
            return true;
        }
    }
    return false;
}

static std::string AppleScriptSelectionScript(const std::string &bundleId) {
    if (bundleId == "com.apple.Safari") {
        return "tell application \"Safari\" to do JavaScript "
               "\"window.getSelection().toString();\" in current tab of front window";
    }
    return "tell application id \"" + bundleId +
           "\" to tell active tab of front window to execute javascript "
           "\"window.getSelection().toString();\"";
}

// Runs /usr/bin/osascript with a hard timeout. A subprocess keeps
// NSAppleScript (not thread-safe) off our worker threads and bounds the
// worst case: the first Apple Event to a target app can block on a system
// Automation (TCC) consent prompt, which must never stall the capture
// pipeline — the timeout kills the client while the user decides.
static bool RunSelectionAppleScript(const std::string &script, std::string &output) {
    @autoreleasepool {
        NSTask *task = [[NSTask alloc] init];
        task.executableURL = [NSURL fileURLWithPath:@"/usr/bin/osascript"];
        NSString *scriptString = NSStringFromString(script);
        if (scriptString == nil) {
            return false;
        }
        task.arguments = @[ @"-e", scriptString ];
        NSPipe *outputPipe = [NSPipe pipe];
        task.standardOutput = outputPipe;
        task.standardError = [NSPipe pipe];
        task.standardInput = [NSPipe pipe];
        NSError *launchError = nil;
        if (![task launchAndReturnError:&launchError]) {
            return false;
        }
        __block NSData *data = nil;
        dispatch_semaphore_t finished = dispatch_semaphore_create(0);
        NSFileHandle *readHandle = outputPipe.fileHandleForReading;
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            data = [readHandle readDataToEndOfFile];
            dispatch_semaphore_signal(finished);
        });
        const bool completed = dispatch_semaphore_wait(
            finished,
            dispatch_time(
                DISPATCH_TIME_NOW,
                static_cast<int64_t>(kAppleScriptProcessTimeoutMs) * NSEC_PER_MSEC)) == 0;
        if (!completed) {
            [task terminate];
            [task waitUntilExit];
            return false;
        }
        [task waitUntilExit];
        NSString *result =
            [[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding];
        output = StringFromNSString(result);
        return task.terminationStatus == 0 && !IsBlank(output);
    }
}

// Non-intrusive browser capture: no synthetic keystrokes, no pasteboard
// access. Runs after AX failed and before the clipboard fallback. The result
// passes the same candidate validation as the AX path (identity match,
// degenerate fragments for mouse triggers, bounds proximity when available).
static bool CaptureViaAppleScript(NSRunningApplication *application, SelectionInfo &info) {
    if (application == nil || application.processIdentifier == getpid() ||
        !application.active) {
        return false;
    }
    if (!IsAppleScriptSelectionBrowser(info.bundleId)) {
        return false;
    }
    const uint64_t startedAt = MonotonicMilliseconds();
    std::string text;
    const bool ok = RunSelectionAppleScript(AppleScriptSelectionScript(info.bundleId), text) &&
        text.size() <= kClipboardMaxTextBytes;
    TraceSelectionTiming("applescript", MonotonicMilliseconds() - startedAt);
    if (!ok) {
        TraceClipboardGateFor(info, "applescript: failed-or-denied");
        return false;
    }
    SelectionInfo candidate = info;
    ClearSelectionGeometry(candidate);
    candidate.text = std::move(text);
    candidate.method = SelectionMethod::AppleScript;
    if (!SelectionCandidateMatchesInput(candidate)) {
        TraceClipboardGateFor(info, "reject: applescript-candidate-invalid");
        return false;
    }
    char okLine[256];
    std::snprintf(
        okLine,
        sizeof(okLine),
        "app=%s trigger=%s applescript-capture: ok bytes=%zu prefix=\"%s\"",
        info.bundleId.c_str(),
        TriggerTag(info.trigger),
        candidate.text.size(),
        TraceTextPrefix(candidate.text).c_str());
    TraceClipboardGate(okLine);
    info = std::move(candidate);
    return true;
}

// The AX half of a capture: bounded reads, compatibility attributes and one
// cancellable retry. Returns true when a selection was captured. When false,
// `fallbackAllowed` reports whether the fallback chain (AppleScript, then the
// synthetic-copy clipboard fallback) may still run for this app — false also
// covers "AX returned a blank capture", which deliberately never escalates to
// a fallback. Callers must fill `info`'s mouse fields before calling.
static bool CaptureViaAccessibilityPipeline(
    NSRunningApplication *application,
    SelectionInfo &info,
    Trigger trigger,
    EnhancedUiCache &enhancedUiCache,
    const std::atomic<uint64_t> *captureGeneration,
    uint64_t expectedGeneration,
    bool &fallbackAllowed) {
    fallbackAllowed = false;
    info.bundleId = StringFromNSString(application.bundleIdentifier);
    info.appName = StringFromNSString(application.localizedName);
    info.trigger = trigger;

    const pid_t pid = application.processIdentifier;
    const uint64_t launchToken = ProcessLaunchToken(application);
    const uint8_t appliedAttributes = enhancedUiCache.Attributes(pid, launchToken);
    uint8_t compatibilityAttributesJustApplied = 0;
    const bool documentOrOffice = IsDocumentOrOfficeSelectionApplication(
        info.bundleId,
        info.appName);
    const uint64_t budgetMs = documentOrOffice
        ? kDocumentAXCaptureBudgetMs
        : kAXCaptureBudgetMs;
    const AccessibilityReadContext context{
        captureGeneration,
        expectedGeneration,
        MonotonicMilliseconds() + budgetMs,
    };
    if (!context.IsCurrent()) return false;

    bool found = ReadViaAccessibility(
        application,
        info,
        appliedAttributes,
        compatibilityAttributesJustApplied,
        context);
    if (!found && context.IsCurrent()) {
        // One cancellable retry for every provider: the first read can land
        // before the app has committed the selection, Chromium/Electron needs
        // a short turn after its compatibility attributes are set, and
        // document canvases commonly publish their selection a frame after
        // mouse-up. The retry stays inside this request's existing AX budget.
        const uint64_t retryDelay = compatibilityAttributesJustApplied != 0
            ? kAXCompatibilityRetryDelayMs
            : documentOrOffice
                ? kAXLateSelectionRetryDelayMs
                : kAXSelectionCommitRetryDelayMs;
        const uint64_t retryAt = std::min(
            context.deadlineMs,
            MonotonicMilliseconds() + retryDelay);
        while (context.IsCurrent() && MonotonicMilliseconds() < retryAt) {
            std::this_thread::sleep_for(std::chrono::milliseconds(4));
        }
        if (context.IsCurrent()) {
            uint8_t retryJustApplied = 0;
            found = ReadViaAccessibility(
                application,
                info,
                static_cast<uint8_t>(appliedAttributes | compatibilityAttributesJustApplied),
                retryJustApplied,
                context);
            compatibilityAttributesJustApplied |= retryJustApplied;
        }
    }
    if (compatibilityAttributesJustApplied != 0) {
        enhancedUiCache.MarkApplied(pid, launchToken, compatibilityAttributesJustApplied);
    }
    if (found) {
        if (IsBlank(info.text)) {
            TraceClipboardGateFor(info, "abort: blank-ax-selection");
            return false;
        }
        char okLine[256];
        std::snprintf(
            okLine,
            sizeof(okLine),
            "app=%s trigger=%s ax-capture: ok bytes=%zu prefix=\"%s\"",
            info.bundleId.c_str(),
            TriggerTag(trigger),
            info.text.size(),
            TraceTextPrefix(info.text).c_str());
        TraceClipboardGate(okLine);
        return true;
    }
    if (!context.IsCurrent()) {
        return false;
    }
    const auto policy = ClipboardFallbackPolicyForApplication(
        info.bundleId,
        info.appName,
        trigger);
    if (policy == ClipboardFallbackPolicy::Denied) {
        TraceClipboardGate("skip: app-policy-denied");
        return false;
    }
    fallbackAllowed = true;
    return false;
}

// AppleScript first (non-intrusive, browser-only), then the synthetic-copy
// clipboard fallback. Shared by the synchronous manual capture path and the
// dedicated clipboard worker.
static bool CaptureViaFallbackChain(
    NSRunningApplication *application,
    SelectionInfo &info,
    LastClipboardCapture &lastClipboardCapture,
    const std::atomic<uint64_t> *captureGeneration,
    uint64_t expectedGeneration,
    bool clipboardAllowed) {
    if (CaptureViaAppleScript(application, info)) {
        return true;
    }
    if (!clipboardAllowed) {
        return false;
    }
    // Canvas-based apps (WPS documents, browser PDF plugins, some Electron
    // panes) publish no AX selected text. The synthetic copy reads what the
    // app itself would copy, then restores the original pasteboard contents.
    // The clipboard wait gets its own budget: the AX deadline above only
    // bounds AX messaging.
    const uint64_t clipboardDeadlineMs =
        MonotonicMilliseconds() + kClipboardFallbackBudgetMs;
    return CaptureViaClipboard(
        application,
        info,
        lastClipboardCapture,
        captureGeneration,
        expectedGeneration,
        clipboardDeadlineMs);
}

static bool CaptureSelection(
    const std::string &excludedBundleId,
    Trigger trigger,
    CGPoint mouseStart,
    bool hasMouseStart,
    CGPoint mouseEnd,
    bool hasMouseEnd,
    CGPoint currentMouse,
    SelectionInfo &info,
    EnhancedUiCache &enhancedUiCache,
    LastClipboardCapture &lastClipboardCapture,
    const std::atomic<uint64_t> *captureGeneration = nullptr,
    uint64_t expectedGeneration = 0) {
    @autoreleasepool {
        if (!AXIsProcessTrusted()) {
            return false;
        }
        NSRunningApplication *application = FrontmostApplication();
        if (IsExcludedApplication(application, excludedBundleId)) {
            return false;
        }

        info.mouseStart = mouseStart;
        info.mouseEnd = mouseEnd;
        info.mouseCurrent = currentMouse;
        info.hasMouseStart = hasMouseStart;
        info.hasMouseEnd = hasMouseEnd;

        bool fallbackAllowed = false;
        bool captured = CaptureViaAccessibilityPipeline(
            application,
            info,
            trigger,
            enhancedUiCache,
            captureGeneration,
            expectedGeneration,
            fallbackAllowed);
        if (!captured && fallbackAllowed) {
            captured = CaptureViaFallbackChain(
                application,
                info,
                lastClipboardCapture,
                captureGeneration,
                expectedGeneration,
                true);
        }
        if (!captured) {
            TraceClipboardGateFor(info, "abort: no-selection-found");
        } else if (info.bundleId == "com.tencent.xinWeChat" && hasMouseEnd &&
                   (trigger == Trigger::Drag || trigger == Trigger::DoubleClick ||
                    trigger == Trigger::ShiftClick)) {
            // Remember the window that actually yielded text. A later shallow
            // hit in this window is a chat surface, not the image viewer.
            const CGPoint anchor =
                trigger == Trigger::Drag && hasMouseStart ? mouseStart : mouseEnd;
            const CGWindowID window =
                TopmostWindowNumberAtPoint(application.processIdentifier, anchor);
            if (window != kCGNullWindowID) {
                gLastWeChatTextWindow.store(window, std::memory_order_relaxed);
            }
        }
        return captured;
    }
}

static NSDictionary *PointObject(CGPoint point) {
    return @{ @"x": @(point.x), @"y": @(point.y) };
}

static id OptionalPoint(bool present, CGPoint point) {
    return present ? PointObject(point) : [NSNull null];
}

static NSString *TriggerName(Trigger trigger) {
    switch (trigger) {
        case Trigger::Drag: return @"drag";
        case Trigger::DoubleClick: return @"doubleClick";
        case Trigger::ShiftClick: return @"shiftClick";
        case Trigger::Keyboard: return @"keyboard";
        case Trigger::Manual: return @"manual";
    }
}

static NSString *MethodName(SelectionMethod method) {
    switch (method) {
        case SelectionMethod::Accessibility: return @"accessibility";
        case SelectionMethod::Clipboard: return @"clipboard";
        case SelectionMethod::AppleScript: return @"applescript";
    }
}

static NSString *SelectionDirection(const SelectionInfo &info) {
    if (!info.hasMouseStart || !info.hasMouseEnd) {
        return @"unknown";
    }
    double deltaY = info.mouseEnd.y - info.mouseStart.y;
    double deltaX = info.mouseEnd.x - info.mouseStart.x;
    if (std::abs(deltaY) > 2.0) {
        return deltaY > 0 ? @"forward" : @"backward";
    }
    if (std::abs(deltaX) > 0.5) {
        return deltaX > 0 ? @"forward" : @"backward";
    }
    return @"unknown";
}

static std::string SerializeJSONObject(NSDictionary *object) {
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:object options:0 error:&error];
    if (data == nil || error != nil) {
        return {};
    }
    return std::string(static_cast<const char *>(data.bytes), data.length);
}

static std::string SelectionJSON(const SelectionInfo &info) {
    NSString *text = NSStringFromString(info.text);
    NSString *bundle = NSStringFromString(info.bundleId);
    NSString *name = NSStringFromString(info.appName);
    if (text == nil || bundle == nil) {
        return {};
    }

    id bounds = [NSNull null];
    if (info.hasBounds) {
        bounds = @{
            @"x": @(info.bounds.origin.x),
            @"y": @(info.bounds.origin.y),
            @"width": @(info.bounds.size.width),
            @"height": @(info.bounds.size.height),
        };
    }

    NSDictionary *object = @{
        @"type": @"selection",
        @"text": text,
        @"sourceApp": @{
            @"bundleId": bundle,
            @"name": name ?: bundle,
        },
        @"bounds": bounds,
        @"startTop": OptionalPoint(info.hasStartTop, info.startTop),
        @"startBottom": OptionalPoint(info.hasStartBottom, info.startBottom),
        @"endTop": OptionalPoint(info.hasEndTop, info.endTop),
        @"endBottom": OptionalPoint(info.hasEndBottom, info.endBottom),
        @"mouse": @{
            @"start": OptionalPoint(info.hasMouseStart, info.mouseStart),
            @"end": OptionalPoint(info.hasMouseEnd, info.mouseEnd),
            @"current": PointObject(info.mouseCurrent),
        },
        @"direction": SelectionDirection(info),
        @"isFullscreen": @(info.fullscreen),
        @"method": MethodName(info.method),
        @"trigger": TriggerName(info.trigger),
        @"timestampMs": @(TimestampMilliseconds()),
    };
    return SerializeJSONObject(object);
}

static std::string DismissJSON(const Task &task) {
    NSString *reason = NSStringFromString(task.dismissReason);
    if (reason == nil) {
        return {};
    }
    return SerializeJSONObject(@{
        @"type": @"dismiss",
        @"reason": reason,
        @"mouse": PointObject(task.mouseCurrent),
        @"targetPid": @(task.targetPid),
        @"timestampMs": @(TimestampMilliseconds()),
    });
}

static bool IsKeyboardSelectionKey(CGKeyCode keyCode) {
    switch (keyCode) {
        case kVK_LeftArrow:
        case kVK_RightArrow:
        case kVK_UpArrow:
        case kVK_DownArrow:
        case kVK_Home:
        case kVK_End:
        case kVK_PageUp:
        case kVK_PageDown:
            return true;
        default:
            return false;
    }
}

static bool IsModifierKey(CGKeyCode keyCode) {
    switch (keyCode) {
        case kVK_Shift:
        case kVK_RightShift:
        case kVK_Command:
        case kVK_RightCommand:
        case kVK_Control:
        case kVK_RightControl:
        case kVK_Option:
        case kVK_RightOption:
        case kVK_CapsLock:
        case kVK_Function:
            return true;
        default:
            return false;
    }
}

// Collapse the focused AX selection to a caret at the end of the former range
// when the selected text still exactly matches `expectedText`. Optional
// `requiredBundleId` rejects apps that are not the original host.
static uint8_t ClearMatchingTextOnMainThread(
    const std::string &requiredBundleId,
    const std::string &expectedText) {
    @autoreleasepool {
        if (expectedText.empty() || !AXIsProcessTrusted()) {
            return 0;
        }

        NSRunningApplication *application = FrontmostApplication();
        if (application == nil || application.processIdentifier == getpid()) {
            return 0;
        }

        std::string appBundleId = StringFromNSString(application.bundleIdentifier);
        if (!requiredBundleId.empty() && appBundleId != requiredBundleId) {
            return 0;
        }

        AXUIElementRef applicationElement =
            AXUIElementCreateApplication(application.processIdentifier);
        if (applicationElement == nullptr) {
            return 0;
        }
        AXUIElementSetMessagingTimeout(applicationElement, kAXMessagingTimeoutSeconds);

        AXUIElementRef focused = CopyFocusedElement(applicationElement);
        CFRelease(applicationElement);
        if (focused == nullptr) {
            return 0;
        }

        const AccessibilityReadContext context{
            nullptr,
            0,
            MonotonicMilliseconds() +
                static_cast<uint64_t>(kAXMessagingTimeoutSeconds * 1'000.0f),
        };
        std::string current;
        if (!ReadSelectedText(focused, current, context) || current != expectedText) {
            CFRelease(focused);
            return 0;
        }

        AXValueRef selectedRangeValue = nullptr;
        AXError error = AXUIElementCopyAttributeValue(
            focused,
            kAXSelectedTextRangeAttribute,
            reinterpret_cast<CFTypeRef *>(&selectedRangeValue));
        if (error != kAXErrorSuccess || selectedRangeValue == nullptr) {
            CFRelease(focused);
            return 0;
        }

        CFRange range = CFRangeMake(0, 0);
        bool hasRange = AXValueGetValue(selectedRangeValue, kAXValueTypeCFRange, &range) &&
            range.location >= 0 &&
            range.length > 0 &&
            range.length <= std::numeric_limits<CFIndex>::max() - range.location;
        CFRelease(selectedRangeValue);
        if (!hasRange) {
            CFRelease(focused);
            return 0;
        }

        // Caret at the end of the former selection (natural "deselect after read").
        CFRange collapsed = CFRangeMake(range.location + range.length, 0);
        AXValueRef collapsedValue = AXValueCreate(kAXValueTypeCFRange, &collapsed);
        if (collapsedValue == nullptr) {
            CFRelease(focused);
            return 0;
        }

        error = AXUIElementSetAttributeValue(
            focused,
            kAXSelectedTextRangeAttribute,
            collapsedValue);
        CFRelease(collapsedValue);
        CFRelease(focused);
        return error == kAXErrorSuccess ? 1 : 0;
    }
}

} // namespace

struct TextLensSelectionMonitor {
    explicit TextLensSelectionMonitor(
        std::string excludedBundle,
        TextLensSelectionEventCallback eventCallback,
        void *eventContext)
        : excludedBundleId(std::move(excludedBundle)),
          callback(eventCallback),
          callbackContext(eventContext) {}

    ~TextLensSelectionMonitor() {
        stop();
        std::lock_guard<std::mutex> lock(callbackMutex);
        callback = nullptr;
        callbackContext = nullptr;
    }

    int32_t start() {
        std::lock_guard<std::mutex> lifecycleLock(lifecycleMutex);
        if (running.load(std::memory_order_acquire)) {
            return TEXTLENS_SELECTION_ALREADY_RUNNING;
        }
        if (!AXIsProcessTrusted()) {
            return TEXTLENS_SELECTION_NOT_TRUSTED;
        }

        {
            std::lock_guard<std::mutex> startupLock(startupMutex);
            startupReady = false;
            startupStatus = TEXTLENS_SELECTION_INTERNAL_ERROR;
        }
        {
            std::lock_guard<std::mutex> runLoopLock(runLoopMutex);
            eventRunLoop = nullptr;
        }
        {
            std::lock_guard<std::mutex> taskLock(taskMutex);
            tasks.clear();
        }
        {
            std::lock_guard<std::mutex> dismissLock(dismissMutex);
            dismissTasks.clear();
        }
        keyboardSelectionPending = false;
        running.store(true, std::memory_order_release);

        try {
            workerThread = std::thread(&TextLensSelectionMonitor::workerMain, this);
            fallbackThread = std::thread(&TextLensSelectionMonitor::fallbackMain, this);
            dismissThread = std::thread(&TextLensSelectionMonitor::dismissMain, this);
            eventThread = std::thread(&TextLensSelectionMonitor::eventMain, this);
        } catch (...) {
            running.store(false, std::memory_order_release);
            taskCondition.notify_all();
            fallbackCondition.notify_all();
            dismissCondition.notify_all();
            if (eventThread.joinable()) eventThread.join();
            if (dismissThread.joinable()) dismissThread.join();
            if (fallbackThread.joinable()) fallbackThread.join();
            if (workerThread.joinable()) workerThread.join();
            return TEXTLENS_SELECTION_INTERNAL_ERROR;
        }

        int32_t status = TEXTLENS_SELECTION_INTERNAL_ERROR;
        {
            std::unique_lock<std::mutex> startupLock(startupMutex);
            startupCondition.wait(startupLock, [this] { return startupReady; });
            status = startupStatus;
        }
        if (status != TEXTLENS_SELECTION_OK) {
            running.store(false, std::memory_order_release);
            taskCondition.notify_all();
            fallbackCondition.notify_all();
            dismissCondition.notify_all();
            if (eventThread.joinable()) eventThread.join();
            if (dismissThread.joinable()) dismissThread.join();
            if (fallbackThread.joinable()) fallbackThread.join();
            if (workerThread.joinable()) workerThread.join();
        }
        return status;
    }

    int32_t stop() {
        std::lock_guard<std::mutex> lifecycleLock(lifecycleMutex);
        bool wasRunning = running.exchange(false, std::memory_order_acq_rel);
        {
            std::lock_guard<std::mutex> runLoopLock(runLoopMutex);
            if (eventRunLoop != nullptr) {
                CFRunLoopStop(eventRunLoop);
                CFRunLoopWakeUp(eventRunLoop);
            }
        }
        taskCondition.notify_all();
        fallbackCondition.notify_all();
        dismissCondition.notify_all();
        if (eventThread.joinable()) eventThread.join();
        if (dismissThread.joinable()) dismissThread.join();
        if (fallbackThread.joinable()) fallbackThread.join();
        if (workerThread.joinable()) workerThread.join();
        {
            std::lock_guard<std::mutex> taskLock(taskMutex);
            tasks.clear();
        }
        {
            std::lock_guard<std::mutex> fallbackLock(fallbackMutex);
            fallbackTasks.clear();
        }
        {
            std::lock_guard<std::mutex> dismissLock(dismissMutex);
            dismissTasks.clear();
        }
        return wasRunning ? TEXTLENS_SELECTION_OK : TEXTLENS_SELECTION_NOT_RUNNING;
    }

    bool captureCurrent(SelectionInfo &info) {
        std::lock_guard<std::mutex> captureLock(captureMutex);
        CGPoint mouse = CurrentMousePosition();
        return CaptureSelection(
            excludedBundleId,
            Trigger::Manual,
            mouse,
            false,
            mouse,
            false,
            mouse,
            info,
            enhancedUiCache,
            lastClipboardCapture);
    }

    static CGEventRef eventTapCallback(
        CGEventTapProxy,
        CGEventType type,
        CGEventRef event,
        void *context) {
        auto *monitor = static_cast<TextLensSelectionMonitor *>(context);
        if (monitor == nullptr || !monitor->running.load(std::memory_order_acquire)) {
            return event;
        }
        // Synthetic copy chords from the clipboard fallback carry this marker.
        // Without the filter they would bump the capture generation (cancelling
        // the very capture that posted them) and dismiss the toolbar.
        if (CGEventGetIntegerValueField(event, kCGEventSourceUserData) == kSyntheticEventMarker) {
            return event;
        }
        if (type == kCGEventTapDisabledByTimeout || type == kCGEventTapDisabledByUserInput) {
            if (monitor->eventTap != nullptr) {
                CGEventTapEnable(monitor->eventTap, true);
            }
            return event;
        }
        monitor->handleEvent(type, event);
        return event;
    }

    // Reactive re-enable in eventTapCallback only fires on the next incoming
    // event; if the tap is disabled with no further events arriving (e.g. a
    // sustained high event-rate burst tripped the OS-side rate limit),
    // capture would otherwise stay silent until app restart. This periodic
    // check gives the same self-healing Windows already has for its hook.
    static void HealthCheckTimerCallback(CFRunLoopTimerRef, void *context) {
        auto *monitor = static_cast<TextLensSelectionMonitor *>(context);
        if (monitor == nullptr || !monitor->running.load(std::memory_order_acquire)) {
            return;
        }
        if (monitor->eventTap != nullptr && !CGEventTapIsEnabled(monitor->eventTap)) {
            CGEventTapEnable(monitor->eventTap, true);
        }
    }

    void handleEvent(CGEventType type, CGEventRef event) {
        const int64_t targetPid = CGEventGetIntegerValueField(event, kCGEventTargetUnixProcessID);
        // Clicks/scrolls targeting TextLens itself must still dismiss an open
        // toolbar (e.g. re-select toolbar over the result webview). Runtime
        // ignores dismiss points that land on the no-activate toolbar window.
        // Never schedule selection capture for the own process.
        if (targetPid == getpid()) {
            if (type == kCGEventLeftMouseDown ||
                type == kCGEventRightMouseDown ||
                type == kCGEventOtherMouseDown) {
                const CGPoint point = CGEventGetLocation(event);
                captureGeneration.fetch_add(1, std::memory_order_acq_rel);
                enqueueDismiss("mouseDown", point, targetPid);
            } else if (type == kCGEventScrollWheel) {
                const CGPoint point = CGEventGetLocation(event);
                captureGeneration.fetch_add(1, std::memory_order_acq_rel);
                enqueueDismiss("scroll", point, targetPid);
            }
            return;
        }
        const CGPoint point = CGEventGetLocation(event);
        const uint64_t now = MonotonicMilliseconds();

        switch (type) {
            case kCGEventLeftMouseDown: {
                captureGeneration.fetch_add(1, std::memory_order_acq_rel);
                mouseDown = point;
                mouseDownTime = now;
                enqueueDismiss("mouseDown", point, targetPid);
                break;
            }
            case kCGEventRightMouseDown:
            case kCGEventOtherMouseDown:
                captureGeneration.fetch_add(1, std::memory_order_acq_rel);
                enqueueDismiss("mouseDown", point, targetPid);
                break;
            case kCGEventScrollWheel:
                captureGeneration.fetch_add(1, std::memory_order_acq_rel);
                enqueueDismiss("scroll", point, targetPid);
                break;
            case kCGEventLeftMouseUp: {
                uint64_t duration = now >= mouseDownTime ? now - mouseDownTime : 0;
                double distance = std::hypot(point.x - mouseDown.x, point.y - mouseDown.y);
                double previousDistance = std::hypot(point.x - lastMouseUp.x, point.y - lastMouseUp.y);
                int64_t clickCount = CGEventGetIntegerValueField(event, kCGMouseEventClickState);
                CGEventFlags flags = CGEventGetFlags(event);

                Trigger trigger = Trigger::Manual;
                bool shouldCapture = false;
                CGPoint start = mouseDown;
                if (duration <= kMaximumDragDurationMs && distance >= kMinimumDragDistance) {
                    trigger = Trigger::Drag;
                    shouldCapture = true;
                } else if (clickCount >= 2 ||
                    (lastClickWasValid && now - lastMouseUpTime <= kDoubleClickDurationMs &&
                     previousDistance <= kDoubleClickDistance)) {
                    trigger = Trigger::DoubleClick;
                    start = point;
                    shouldCapture = true;
                } else {
                    bool shiftOnly = (flags & kCGEventFlagMaskShift) != 0 &&
                        (flags & (kCGEventFlagMaskCommand | kCGEventFlagMaskControl |
                                  kCGEventFlagMaskAlternate)) == 0;
                    if (shiftOnly) {
                        trigger = Trigger::ShiftClick;
                        start = lastMouseUp;
                        shouldCapture = true;
                    }
                }

                lastClickWasValid = duration <= kDoubleClickDurationMs;
                lastMouseUp = point;
                lastMouseUpTime = now;
                if (shouldCapture) {
                    enqueueCapture(trigger, start, true, point, true, point);
                }
                break;
            }
            case kCGEventKeyDown: {
                CGKeyCode keyCode = static_cast<CGKeyCode>(
                    CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode));
                CGEventFlags flags = CGEventGetFlags(event);
                bool extendsWithShift = (flags & kCGEventFlagMaskShift) != 0 &&
                    IsKeyboardSelectionKey(keyCode);
                bool selectAll = (flags & kCGEventFlagMaskCommand) != 0 &&
                    (flags & (kCGEventFlagMaskControl | kCGEventFlagMaskAlternate)) == 0 &&
                    keyCode == kVK_ANSI_A;
                if (extendsWithShift || selectAll) {
                    keyboardSelectionPending = true;
                    keyboardSelectionKey = keyCode;
                }
                if (!IsModifierKey(keyCode)) {
                    captureGeneration.fetch_add(1, std::memory_order_acq_rel);
                    enqueueDismiss("keyDown", point, targetPid);
                }
                break;
            }
            case kCGEventKeyUp: {
                CGKeyCode keyCode = static_cast<CGKeyCode>(
                    CGEventGetIntegerValueField(event, kCGKeyboardEventKeycode));
                if (keyboardSelectionPending && keyCode == keyboardSelectionKey) {
                    keyboardSelectionPending = false;
                    enqueueCapture(Trigger::Keyboard, point, false, point, false, point);
                }
                break;
            }
            default:
                break;
        }
    }

    void enqueueCapture(
        Trigger trigger,
        CGPoint start,
        bool hasStart,
        CGPoint end,
        bool hasEnd,
        CGPoint current) {
        Task task;
        task.kind = TaskKind::Capture;
        task.trigger = trigger;
        task.mouseStart = start;
        task.mouseEnd = end;
        task.mouseCurrent = current;
        task.hasMouseStart = hasStart;
        task.hasMouseEnd = hasEnd;
        task.generation = captureGeneration.fetch_add(1, std::memory_order_acq_rel) + 1;
        task.enqueuedAtMs = MonotonicMilliseconds();
        enqueueTask(std::move(task));
    }

    void enqueueDismiss(const char *reason, CGPoint point, int64_t targetPid) {
        Task task;
        task.kind = TaskKind::Dismiss;
        task.dismissReason = reason;
        task.mouseCurrent = point;
        task.targetPid = targetPid;
        if (!running.load(std::memory_order_acquire)) {
            return;
        }
        {
            std::lock_guard<std::mutex> lock(dismissMutex);
            if (dismissTasks.size() >= kMaximumQueuedTasks) {
                dismissTasks.pop_front();
            }
            if (!dismissTasks.empty() &&
                dismissTasks.back().dismissReason == task.dismissReason) {
                dismissTasks.back() = std::move(task);
            } else {
                dismissTasks.push_back(std::move(task));
            }
        }
        dismissCondition.notify_one();
    }

    void enqueueTask(Task task) {
        if (!running.load(std::memory_order_acquire) || task.kind != TaskKind::Capture) {
            return;
        }
        {
            std::lock_guard<std::mutex> lock(taskMutex);
            // Capture is latest-wins. A task already executing may finish its
            // bounded AX transaction, but queued stale captures must never
            // delay the newest mouse-up or keyboard selection.
            tasks.clear();
            tasks.push_back(std::move(task));
        }
        taskCondition.notify_one();
    }

    void emit(const std::string &json) {
        if (json.empty() || !running.load(std::memory_order_acquire)) {
            return;
        }
        std::lock_guard<std::mutex> lock(callbackMutex);
        if (callback != nullptr && running.load(std::memory_order_acquire)) {
            callback(json.c_str(), callbackContext);
        }
    }

    void workerMain() {
        while (true) {
            Task task;
            {
                std::unique_lock<std::mutex> lock(taskMutex);
                taskCondition.wait(lock, [this] {
                    return !running.load(std::memory_order_acquire) || !tasks.empty();
                });
                if (!running.load(std::memory_order_acquire)) {
                    tasks.clear();
                    return;
                }
                task = std::move(tasks.front());
                tasks.pop_front();
            }

            @autoreleasepool {
                if (task.generation != captureGeneration.load(std::memory_order_acquire)) {
                    continue;
                }

                const uint64_t captureStartedAt = MonotonicMilliseconds();
                SelectionInfo info;
                info.mouseStart = task.mouseStart;
                info.mouseEnd = task.mouseEnd;
                info.mouseCurrent = task.mouseCurrent;
                info.hasMouseStart = task.hasMouseStart;
                info.hasMouseEnd = task.hasMouseEnd;
                bool captured = false;
                bool fallbackAllowed = false;
                NSRunningApplication *application = nil;
                {
                    std::lock_guard<std::mutex> captureLock(captureMutex);
                    application = AXIsProcessTrusted() ? FrontmostApplication() : nil;
                    if (application != nil &&
                        !IsExcludedApplication(application, excludedBundleId)) {
                        captured = CaptureViaAccessibilityPipeline(
                            application,
                            info,
                            task.trigger,
                            enhancedUiCache,
                            &captureGeneration,
                            task.generation,
                            fallbackAllowed);
                    }
                }
                const uint64_t completedAt = MonotonicMilliseconds();
                TraceSelectionTiming("capture", completedAt - captureStartedAt);
                TraceSelectionTiming(
                    "mouse-up-to-ax-complete",
                    completedAt - task.enqueuedAtMs);
                if (captured) {
                    if (task.generation == captureGeneration.load(std::memory_order_acquire)) {
                        emit(SelectionJSON(info));
                    }
                    continue;
                }
                if (!fallbackAllowed ||
                    task.generation != captureGeneration.load(std::memory_order_acquire)) {
                    continue;
                }
                // The multi-hundred-ms fallback chain must not hold the
                // capture worker (and with it the next gesture's AX read)
                // hostage: defer it to the dedicated clipboard worker. The
                // queue is latest-wins and every phase re-checks the
                // generation, so a superseded fallback discards itself.
                FallbackTask fallbackTask;
                fallbackTask.info = std::move(info);
                fallbackTask.targetPid = application.processIdentifier;
                fallbackTask.generation = task.generation;
                fallbackTask.clipboardAllowed = true;
                fallbackTask.enqueuedAtMs = task.enqueuedAtMs;
                enqueueFallbackTask(std::move(fallbackTask));
            }
        }
    }

    void enqueueFallbackTask(FallbackTask task) {
        if (!running.load(std::memory_order_acquire)) {
            return;
        }
        {
            std::lock_guard<std::mutex> lock(fallbackMutex);
            // Latest-wins, mirroring the capture queue: a stale fallback must
            // never delay the newest gesture's clipboard/AppleScript read.
            fallbackTasks.clear();
            fallbackTasks.push_back(std::move(task));
        }
        fallbackCondition.notify_one();
    }

    void fallbackMain() {
        while (true) {
            FallbackTask task;
            {
                std::unique_lock<std::mutex> lock(fallbackMutex);
                fallbackCondition.wait(lock, [this] {
                    return !running.load(std::memory_order_acquire) || !fallbackTasks.empty();
                });
                if (!running.load(std::memory_order_acquire)) {
                    fallbackTasks.clear();
                    return;
                }
                task = std::move(fallbackTasks.front());
                fallbackTasks.pop_front();
            }

            @autoreleasepool {
                if (task.generation != captureGeneration.load(std::memory_order_acquire)) {
                    continue;
                }
                // Re-resolve by pid (not frontmostApplication): the fallback
                // must read the same app the AX half observed. An app that
                // quit in between resolves to nil here, which aborts the
                // fallback quietly.
                NSRunningApplication *application = nil;
                for (NSRunningApplication *candidate in
                     [NSWorkspace sharedWorkspace].runningApplications) {
                    if (candidate.processIdentifier == task.targetPid) {
                        application = candidate;
                        break;
                    }
                }
                const uint64_t startedAt = MonotonicMilliseconds();
                SelectionInfo info = std::move(task.info);
                const bool captured = application != nil && CaptureViaFallbackChain(
                    application,
                    info,
                    lastClipboardCapture,
                    &captureGeneration,
                    task.generation,
                    task.clipboardAllowed);
                const uint64_t completedAt = MonotonicMilliseconds();
                TraceSelectionTiming("fallback-capture", completedAt - startedAt);
                TraceSelectionTiming(
                    "mouse-up-to-fallback-complete",
                    completedAt - task.enqueuedAtMs);
                const bool stillCurrent =
                    task.generation == captureGeneration.load(std::memory_order_acquire);
                if (captured && stillCurrent) {
                    emit(SelectionJSON(info));
                } else if (!captured && stillCurrent) {
                    TraceClipboardGateFor(info, "abort: no-selection-found");
                }
            }
        }
    }

    void dismissMain() {
        while (true) {
            Task task;
            {
                std::unique_lock<std::mutex> lock(dismissMutex);
                dismissCondition.wait(lock, [this] {
                    return !running.load(std::memory_order_acquire) || !dismissTasks.empty();
                });
                if (!running.load(std::memory_order_acquire)) {
                    dismissTasks.clear();
                    return;
                }
                task = std::move(dismissTasks.front());
                dismissTasks.pop_front();
            }
            emit(DismissJSON(task));
        }
    }

    void eventMain() {
        @autoreleasepool {
            CFRunLoopRef runLoop = CFRunLoopGetCurrent();
            CGEventMask eventMask =
                CGEventMaskBit(kCGEventLeftMouseDown) |
                CGEventMaskBit(kCGEventLeftMouseUp) |
                CGEventMaskBit(kCGEventRightMouseDown) |
                CGEventMaskBit(kCGEventOtherMouseDown) |
                CGEventMaskBit(kCGEventScrollWheel) |
                CGEventMaskBit(kCGEventKeyDown) |
                CGEventMaskBit(kCGEventKeyUp);

            eventTap = CGEventTapCreate(
                kCGSessionEventTap,
                kCGTailAppendEventTap,
                kCGEventTapOptionListenOnly,
                eventMask,
                &TextLensSelectionMonitor::eventTapCallback,
                this);
            if (eventTap != nullptr) {
                runLoopSource = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, eventTap, 0);
            }

            int32_t status = eventTap != nullptr && runLoopSource != nullptr
                ? TEXTLENS_SELECTION_OK
                : TEXTLENS_SELECTION_EVENT_TAP_FAILED;
            if (status == TEXTLENS_SELECTION_OK) {
                {
                    std::lock_guard<std::mutex> lock(runLoopMutex);
                    eventRunLoop = runLoop;
                }
                CFRunLoopAddSource(runLoop, runLoopSource, kCFRunLoopDefaultMode);
                CGEventTapEnable(eventTap, true);

                CFRunLoopTimerContext timerContext = {0, this, nullptr, nullptr, nullptr};
                healthTimer = CFRunLoopTimerCreate(
                    kCFAllocatorDefault,
                    CFAbsoluteTimeGetCurrent() + kEventTapHealthCheckIntervalSeconds,
                    kEventTapHealthCheckIntervalSeconds,
                    0,
                    0,
                    &TextLensSelectionMonitor::HealthCheckTimerCallback,
                    &timerContext);
                if (healthTimer != nullptr) {
                    CFRunLoopAddTimer(runLoop, healthTimer, kCFRunLoopDefaultMode);
                }
            }
            {
                std::lock_guard<std::mutex> lock(startupMutex);
                startupStatus = status;
                startupReady = true;
            }
            startupCondition.notify_all();

            if (status == TEXTLENS_SELECTION_OK && running.load(std::memory_order_acquire)) {
                CFRunLoopRun();
            }

            if (eventTap != nullptr) {
                CGEventTapEnable(eventTap, false);
            }
            if (healthTimer != nullptr) {
                CFRunLoopRemoveTimer(runLoop, healthTimer, kCFRunLoopDefaultMode);
                CFRelease(healthTimer);
                healthTimer = nullptr;
            }
            if (runLoopSource != nullptr) {
                CFRunLoopRemoveSource(runLoop, runLoopSource, kCFRunLoopDefaultMode);
                CFRelease(runLoopSource);
                runLoopSource = nullptr;
            }
            if (eventTap != nullptr) {
                CFRelease(eventTap);
                eventTap = nullptr;
            }
            {
                std::lock_guard<std::mutex> lock(runLoopMutex);
                eventRunLoop = nullptr;
            }
        }
    }

    std::string excludedBundleId;
    TextLensSelectionEventCallback callback = nullptr;
    void *callbackContext = nullptr;
    std::mutex callbackMutex;
    EnhancedUiCache enhancedUiCache;
    LastClipboardCapture lastClipboardCapture;

    std::atomic<bool> running{false};
    std::atomic<uint64_t> captureGeneration{0};
    std::mutex lifecycleMutex;
    std::mutex captureMutex;

    std::thread eventThread;
    std::thread workerThread;
    std::thread fallbackThread;
    std::thread dismissThread;
    CFMachPortRef eventTap = nullptr;
    CFRunLoopSourceRef runLoopSource = nullptr;
    CFRunLoopTimerRef healthTimer = nullptr;
    CFRunLoopRef eventRunLoop = nullptr;
    std::mutex runLoopMutex;

    std::mutex startupMutex;
    std::condition_variable startupCondition;
    bool startupReady = false;
    int32_t startupStatus = TEXTLENS_SELECTION_INTERNAL_ERROR;

    std::mutex taskMutex;
    std::condition_variable taskCondition;
    std::deque<Task> tasks;
    std::mutex fallbackMutex;
    std::condition_variable fallbackCondition;
    std::deque<FallbackTask> fallbackTasks;
    std::mutex dismissMutex;
    std::condition_variable dismissCondition;
    std::deque<Task> dismissTasks;

    CGPoint mouseDown = CGPointZero;
    CGPoint lastMouseUp = CGPointZero;
    uint64_t mouseDownTime = 0;
    uint64_t lastMouseUpTime = 0;
    bool lastClickWasValid = false;
    bool keyboardSelectionPending = false;
    CGKeyCode keyboardSelectionKey = 0;
};

extern "C" uint8_t textlens_accessibility_is_trusted(void) {
    return AXIsProcessTrusted() ? 1 : 0;
}

extern "C" uint8_t textlens_accessibility_request(void) {
    @autoreleasepool {
        const void *keys[] = { kAXTrustedCheckOptionPrompt };
        const void *values[] = { kCFBooleanTrue };
        CFDictionaryRef options = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys,
            values,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks);
        bool trusted = AXIsProcessTrustedWithOptions(options);
        if (options != nullptr) CFRelease(options);
        return trusted ? 1 : 0;
    }
}

extern "C" TextLensSelectionMonitor *textlens_selection_monitor_create(
    const char *excluded_bundle_id_utf8,
    TextLensSelectionEventCallback callback,
    void *context) {
    if (excluded_bundle_id_utf8 == nullptr || callback == nullptr) {
        return nullptr;
    }
    try {
        return new TextLensSelectionMonitor(excluded_bundle_id_utf8, callback, context);
    } catch (...) {
        return nullptr;
    }
}

extern "C" int32_t textlens_selection_monitor_start(TextLensSelectionMonitor *monitor) {
    if (monitor == nullptr) {
        return TEXTLENS_SELECTION_INVALID_ARGUMENT;
    }
    try {
        return monitor->start();
    } catch (...) {
        return TEXTLENS_SELECTION_INTERNAL_ERROR;
    }
}

extern "C" int32_t textlens_selection_monitor_stop(TextLensSelectionMonitor *monitor) {
    if (monitor == nullptr) {
        return TEXTLENS_SELECTION_INVALID_ARGUMENT;
    }
    try {
        return monitor->stop();
    } catch (...) {
        return TEXTLENS_SELECTION_INTERNAL_ERROR;
    }
}

extern "C" char *textlens_selection_monitor_capture_current(
    TextLensSelectionMonitor *monitor,
    int32_t *status_out) {
    if (status_out != nullptr) *status_out = TEXTLENS_SELECTION_INVALID_ARGUMENT;
    if (monitor == nullptr) {
        return nullptr;
    }
    if (!AXIsProcessTrusted()) {
        if (status_out != nullptr) *status_out = TEXTLENS_SELECTION_NOT_TRUSTED;
        return nullptr;
    }
    try {
        SelectionInfo info;
        if (!monitor->captureCurrent(info)) {
            if (status_out != nullptr) *status_out = TEXTLENS_SELECTION_OK;
            return nullptr;
        }
        std::string json = SelectionJSON(info);
        if (json.empty()) {
            if (status_out != nullptr) *status_out = TEXTLENS_SELECTION_INTERNAL_ERROR;
            return nullptr;
        }
        char *result = static_cast<char *>(std::malloc(json.size() + 1));
        if (result == nullptr) {
            if (status_out != nullptr) *status_out = TEXTLENS_SELECTION_INTERNAL_ERROR;
            return nullptr;
        }
        std::memcpy(result, json.data(), json.size());
        result[json.size()] = '\0';
        if (status_out != nullptr) *status_out = TEXTLENS_SELECTION_OK;
        return result;
    } catch (...) {
        if (status_out != nullptr) *status_out = TEXTLENS_SELECTION_INTERNAL_ERROR;
        return nullptr;
    }
}

extern "C" void textlens_selection_string_free(char *value) {
    std::free(value);
}

extern "C" uint8_t textlens_selection_clear_matching_text(
    const char *bundle_id_utf8,
    const char *text_utf8) {
    try {
        if (text_utf8 == nullptr || text_utf8[0] == '\0') {
            return 0;
        }
        std::string expectedText(text_utf8);
        std::string requiredBundleId;
        if (bundle_id_utf8 != nullptr && bundle_id_utf8[0] != '\0') {
            requiredBundleId.assign(bundle_id_utf8);
        }

        __block uint8_t result = 0;
        void (^work)(void) = ^{
            result = ClearMatchingTextOnMainThread(requiredBundleId, expectedText);
        };
        if ([NSThread isMainThread]) {
            work();
        } else {
            // AX write must run on the main thread; hop when called from a worker.
            dispatch_sync(dispatch_get_main_queue(), work);
        }
        return result;
    } catch (...) {
        return 0;
    }
}

extern "C" void textlens_selection_monitor_destroy(TextLensSelectionMonitor *monitor) {
    delete monitor;
}
