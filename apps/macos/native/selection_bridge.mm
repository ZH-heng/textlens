/**
 * Native macOS selection bridge for TextLens.
 *
 * The Accessibility traversal, text-range bounds helpers, input detection, and
 * complete pasteboard snapshot approach in this file are adapted from
 * selection-hook 2.0.2:
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
#include <cstring>
#include <deque>
#include <mutex>
#include <new>
#include <limits>
#include <string>
#include <thread>
#include <utility>
#include <vector>

namespace {

using Clock = std::chrono::steady_clock;

constexpr double kMinimumDragDistance = 4.0;
constexpr uint64_t kMaximumDragDurationMs = 15'000;
constexpr uint64_t kDoubleClickDurationMs = 500;
constexpr double kDoubleClickDistance = 4.0;
constexpr int64_t kSyntheticEventMarker = 0x544c4d41434f534cLL; // "TLMACOSL"
constexpr size_t kMaximumQueuedTasks = 64;

enum class Trigger {
    Drag,
    DoubleClick,
    ShiftClick,
    Keyboard,
    Manual,
};

enum class SelectionMethod {
    Accessibility,
    Clipboard,
};

struct SelectionInfo {
    std::string text;
    std::string bundleId;
    std::string appName;
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
};

struct PasteboardRepresentation {
    std::string type;
    std::vector<uint8_t> bytes;
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

static uint64_t MonotonicMilliseconds() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::milliseconds>(
        Clock::now().time_since_epoch()).count());
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
    NSString *bundle = application.bundleIdentifier;
    if (bundle == nil || bundle.length == 0) {
        return true;
    }
    if (!excludedBundleId.empty() && StringFromNSString(bundle) == excludedBundleId) {
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

static void StoreDetailedBounds(SelectionInfo &info, CGRect first, CGRect last) {
    info.hasStartTop = true;
    info.hasStartBottom = true;
    info.hasEndTop = true;
    info.hasEndBottom = true;
    info.startTop = CGPointMake(CGRectGetMinX(first), CGRectGetMinY(first));
    info.startBottom = CGPointMake(CGRectGetMinX(first), CGRectGetMaxY(first));
    info.endTop = CGPointMake(CGRectGetMaxX(last), CGRectGetMinY(last));
    info.endBottom = CGPointMake(CGRectGetMaxX(last), CGRectGetMaxY(last));

    CGFloat minimumX = std::min(CGRectGetMinX(first), CGRectGetMinX(last));
    CGFloat minimumY = std::min(CGRectGetMinY(first), CGRectGetMinY(last));
    CGFloat maximumX = std::max(CGRectGetMaxX(first), CGRectGetMaxX(last));
    CGFloat maximumY = std::max(CGRectGetMaxY(first), CGRectGetMaxY(last));
    info.bounds = CGRectMake(minimumX, minimumY, maximumX - minimumX, maximumY - minimumY);
    info.hasBounds = IsReasonableRect(info.bounds);
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

static bool ReadSelectionBounds(AXUIElementRef element, SelectionInfo &info) {
    if (element == nullptr) {
        return false;
    }

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

    CFRange firstRange = CFRangeMake(range.location, 1);
    CFRange lastRange = CFRangeMake(range.location + range.length - 1, 1);
    AXValueRef firstRangeValue = AXValueCreate(kAXValueTypeCFRange, &firstRange);
    AXValueRef lastRangeValue = AXValueCreate(kAXValueTypeCFRange, &lastRange);
    AXValueRef firstBoundsValue = nullptr;
    AXValueRef lastBoundsValue = nullptr;

    bool detailed = false;
    if (firstRangeValue != nullptr && lastRangeValue != nullptr) {
        AXError firstError = AXUIElementCopyParameterizedAttributeValue(
            element,
            kAXBoundsForRangeParameterizedAttribute,
            firstRangeValue,
            reinterpret_cast<CFTypeRef *>(&firstBoundsValue));
        AXError lastError = AXUIElementCopyParameterizedAttributeValue(
            element,
            kAXBoundsForRangeParameterizedAttribute,
            lastRangeValue,
            reinterpret_cast<CFTypeRef *>(&lastBoundsValue));

        CGRect firstRect = CGRectZero;
        CGRect lastRect = CGRectZero;
        if (firstError == kAXErrorSuccess && lastError == kAXErrorSuccess &&
            firstBoundsValue != nullptr && lastBoundsValue != nullptr &&
            AXValueGetValue(firstBoundsValue, kAXValueTypeCGRect, &firstRect) &&
            AXValueGetValue(lastBoundsValue, kAXValueTypeCGRect, &lastRect) &&
            IsReasonableRect(firstRect) && IsReasonableRect(lastRect)) {
            StoreDetailedBounds(info, firstRect, lastRect);
            detailed = true;
        }
    }

    if (firstBoundsValue != nullptr) CFRelease(firstBoundsValue);
    if (lastBoundsValue != nullptr) CFRelease(lastBoundsValue);
    if (firstRangeValue != nullptr) CFRelease(firstRangeValue);
    if (lastRangeValue != nullptr) CFRelease(lastRangeValue);

    AXValueRef aggregateBoundsValue = nullptr;
    AXError aggregateError = AXUIElementCopyParameterizedAttributeValue(
        element,
        kAXBoundsForRangeParameterizedAttribute,
        selectedRangeValue,
        reinterpret_cast<CFTypeRef *>(&aggregateBoundsValue));
    CGRect aggregateRect = CGRectZero;
    if (aggregateError == kAXErrorSuccess && aggregateBoundsValue != nullptr &&
        AXValueGetValue(aggregateBoundsValue, kAXValueTypeCGRect, &aggregateRect) &&
        IsReasonableRect(aggregateRect)) {
        if (detailed) {
            // Preserve precise first/last glyph corners, but expose the full
            // range rectangle for multiline selections.
            info.bounds = aggregateRect;
            info.hasBounds = true;
        } else {
            StoreAggregateBounds(info, aggregateRect);
        }
        detailed = true;
    }
    if (aggregateBoundsValue != nullptr) CFRelease(aggregateBoundsValue);

    CFRelease(selectedRangeValue);
    return detailed;
}

static bool ReadSelectedText(AXUIElementRef element, std::string &text) {
    if (element == nullptr) {
        return false;
    }

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

    CFTypeRef value = nullptr;
    error = AXUIElementCopyAttributeValue(element, kAXValueAttribute, &value);
    if (error != kAXErrorSuccess || value == nullptr) {
        return false;
    }
    if (CFGetTypeID(value) != CFStringGetTypeID()) {
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

static bool FindSelectionInTree(
    AXUIElementRef element,
    SelectionInfo &info,
    int depth,
    size_t &remainingElements) {
    if (element == nullptr || depth < 0 || remainingElements == 0) {
        return false;
    }
    --remainingElements;

    std::string text;
    if (ReadSelectedText(element, text)) {
        info.text = std::move(text);
        ReadSelectionBounds(element, info);
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
    for (CFIndex index = 0; index < count && !found && remainingElements > 0; ++index) {
        AXUIElementRef child = static_cast<AXUIElementRef>(
            const_cast<void *>(CFArrayGetValueAtIndex(children, index)));
        found = FindSelectionInTree(child, info, depth - 1, remainingElements);
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

static bool FindSelectionFromFocusedContext(AXUIElementRef element, SelectionInfo &info) {
    if (element == nullptr) {
        return false;
    }

    size_t remaining = 192;
    bool found = FindSelectionInTree(element, info, 4, remaining);
    if (found) {
        return true;
    }

    AXUIElementRef current = element;
    CFRetain(current);
    for (int level = 0; level < 10 && !found; ++level) {
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
        std::string text;
        if (ReadSelectedText(current, text)) {
            info.text = std::move(text);
            ReadSelectionBounds(current, info);
            found = true;
        }
    }
    if (current != nullptr) CFRelease(current);
    return found;
}

static bool ReadFullscreen(AXUIElementRef applicationElement) {
    AXUIElementRef window = CopyFocusedWindow(applicationElement);
    if (window == nullptr) {
        return false;
    }
    CFTypeRef value = nullptr;
    AXError error = AXUIElementCopyAttributeValue(window, CFSTR("AXFullScreen"), &value);
    bool fullscreen = error == kAXErrorSuccess && value != nullptr &&
        CFGetTypeID(value) == CFBooleanGetTypeID() &&
        CFBooleanGetValue(static_cast<CFBooleanRef>(value));
    if (value != nullptr) CFRelease(value);
    CFRelease(window);
    return fullscreen;
}

static bool ReadViaAccessibility(NSRunningApplication *application, SelectionInfo &info) {
    AXUIElementRef applicationElement = AXUIElementCreateApplication(application.processIdentifier);
    if (applicationElement == nullptr) {
        return false;
    }
    info.fullscreen = ReadFullscreen(applicationElement);

    AXUIElementRef focused = CopyFocusedElement(applicationElement);
    if (focused == nullptr) {
        focused = CopyFocusedWindow(applicationElement);
    }
    AXUIElementRef focusedWindow = CopyFocusedWindow(applicationElement);

    bool found = false;
    if (focused != nullptr) {
        found = FindSelectionFromFocusedContext(focused, info);
    }
    if (!found && focusedWindow != nullptr && (focused == nullptr || !CFEqual(focusedWindow, focused))) {
        size_t remaining = 256;
        found = FindSelectionInTree(focusedWindow, info, 5, remaining);
    }

    if (!found) {
        // Chromium/Electron often does not expose its AX tree until one of
        // these attributes is enabled. This is a best-effort, documented
        // compatibility step inherited from selection-hook.
        AXUIElementSetAttributeValue(applicationElement, CFSTR("AXEnhancedUserInterface"), kCFBooleanTrue);
        AXUIElementSetAttributeValue(applicationElement, CFSTR("AXManualAccessibility"), kCFBooleanTrue);
    }

    if (focused != nullptr) CFRelease(focused);
    if (focusedWindow != nullptr) CFRelease(focusedWindow);
    CFRelease(applicationElement);
    if (found) info.method = SelectionMethod::Accessibility;
    return found;
}

static PasteboardSnapshot SnapshotPasteboard(NSPasteboard *pasteboard) {
    PasteboardSnapshot snapshot;
    if (pasteboard == nil) {
        return snapshot;
    }
    snapshot.valid = true;
    NSArray<NSPasteboardItem *> *items = pasteboard.pasteboardItems;
    for (NSPasteboardItem *item in items) {
        PasteboardItemSnapshot itemSnapshot;
        for (NSPasteboardType type in item.types) {
            NSData *data = [item dataForType:type];
            if (data == nil) {
                // A promised/lazy representation cannot be reconstructed after
                // clearContents. Abort fallback instead of risking data loss.
                snapshot.valid = false;
                snapshot.items.clear();
                return snapshot;
            }
            PasteboardRepresentation representation;
            representation.type = StringFromNSString(type);
            const auto *bytes = static_cast<const uint8_t *>(data.bytes);
            if (data.length > 0 && bytes != nullptr) {
                representation.bytes.assign(bytes, bytes + data.length);
            }
            itemSnapshot.representations.push_back(std::move(representation));
        }
        if (item.types.count > 0 && itemSnapshot.representations.empty()) {
            snapshot.valid = false;
            snapshot.items.clear();
            return snapshot;
        }
        snapshot.items.push_back(std::move(itemSnapshot));
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
    // Fail closed: another writer already changed the pasteboard.
    if (pasteboard.changeCount != expectedChangeCount) {
        return false;
    }
    [pasteboard prepareForNewContentsWithOptions:NSPasteboardContentsCurrentHostOnly];
    if (snapshot.items.empty()) {
        return true;
    }

    NSMutableArray<NSPasteboardItem *> *items = [NSMutableArray arrayWithCapacity:snapshot.items.size()];
    for (const PasteboardItemSnapshot &itemSnapshot : snapshot.items) {
        NSPasteboardItem *item = [[NSPasteboardItem alloc] init];
        for (const PasteboardRepresentation &representation : itemSnapshot.representations) {
            NSString *type = NSStringFromString(representation.type);
            if (type == nil) {
                continue;
            }
            NSData *data = [NSData dataWithBytes:representation.bytes.data()
                                          length:representation.bytes.size()];
            [item setData:data forType:type];
        }
        if (item.types.count > 0) {
            [items addObject:item];
        }
    }
    BOOL wrote = items.count == 0 || [pasteboard writeObjects:items];
    return wrote == YES;
}

static bool PostCopyShortcut(pid_t processIdentifier) {
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
    CGEventSetIntegerValueField(keyDown, kCGEventSourceUserData, kSyntheticEventMarker);
    CGEventSetIntegerValueField(keyUp, kCGEventSourceUserData, kSyntheticEventMarker);
    CGEventPostToPid(processIdentifier, keyDown);
    std::this_thread::sleep_for(std::chrono::milliseconds(5));
    CGEventPostToPid(processIdentifier, keyUp);
    CFRelease(keyDown);
    CFRelease(keyUp);
    return true;
}

static bool FocusedElementIsProtected(NSRunningApplication *application) {
    if (application == nil) {
        return true;
    }
    AXUIElementRef applicationElement =
        AXUIElementCreateApplication(application.processIdentifier);
    if (applicationElement == nullptr) {
        return false;
    }

    AXUIElementRef focused = nullptr;
    AXError focusedError = AXUIElementCopyAttributeValue(
        applicationElement,
        kAXFocusedUIElementAttribute,
        reinterpret_cast<CFTypeRef *>(&focused));
    CFRelease(applicationElement);
    if (focusedError != kAXErrorSuccess || focused == nullptr) {
        return false;
    }

    bool isProtected = false;
    CFTypeRef subrole = nullptr;
    if (AXUIElementCopyAttributeValue(focused, kAXSubroleAttribute, &subrole) == kAXErrorSuccess &&
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

    CFRelease(focused);
    return isProtected;
}

static std::string LowercaseAscii(std::string value) {
    std::transform(value.begin(), value.end(), value.begin(), [](unsigned char character) {
        return static_cast<char>(std::tolower(character));
    });
    return value;
}

static bool MatchesBundleFamily(
    const std::string &bundleId,
    const std::string &family) {
    return bundleId == family ||
        (bundleId.size() > family.size() &&
         bundleId.compare(0, family.size(), family) == 0 &&
         bundleId[family.size()] == '.');
}

static bool IsOpenAISelectionApplication(
    const std::string &bundleId,
    const std::string &appName) {
    const std::string normalizedBundleId = LowercaseAscii(bundleId);
    if (MatchesBundleFamily(normalizedBundleId, "com.openai.codex") ||
        MatchesBundleFamily(normalizedBundleId, "com.openai.chatgpt") ||
        MatchesBundleFamily(normalizedBundleId, "com.openai.chat")) {
        return true;
    }

    const std::string normalizedAppName = LowercaseAscii(appName);
    return normalizedBundleId.rfind("com.openai.", 0) == 0 &&
        (normalizedAppName == "codex" || normalizedAppName == "chatgpt");
}

// CodeG: native AppKit shell + WebKit content panes. Input fields usually expose
// AXSelectedText; document/chat content panes often do not, so AX read fails and
// clipboard fallback is required (root cause of toolbar not appearing).
static bool IsCodeGSelectionApplication(
    const std::string &bundleId,
    const std::string &appName) {
    const std::string normalizedBundleId = LowercaseAscii(bundleId);
    if (MatchesBundleFamily(normalizedBundleId, "app.codeg") ||
        normalizedBundleId == "app.codeg") {
        return true;
    }
    const std::string normalizedAppName = LowercaseAscii(appName);
    return normalizedAppName == "codeg";
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

static bool ShouldUseClipboardFallback(
    const std::string &bundleId,
    const std::string &appName) {
    // Always compare in lowercase: bundle identifiers are usually lowercase
    // but channel builds and sideloaded packages occasionally differ in case.
    // Prefix list stays ASCII-lowercase so we avoid allocating on every entry.
    static const char *const compatibleApplicationPrefixes[] = {
        "com.apple.preview",
        "com.apple.safari",
        "com.google.chrome",
        "com.microsoft.edgemac",
        "org.mozilla.firefox",
        "com.adobe.reader",
        "com.adobe.acrobat.pro",
        "com.microsoft.word",
        "com.microsoft.excel",
        "com.microsoft.powerpoint",
        "com.apple.iwork.pages",
        "com.apple.iwork.numbers",
        "com.apple.iwork.keynote",
        "org.libreoffice.script",
        // Custom-rendered Chinese office and communication applications.
        "com.tencent.xinwechat",
        "com.tencent.qq",
        "com.tencent.tencentmeeting",
        "com.kingsoft.wpsoffice",
        "com.kingsoft.",
        "cn.wps.",
        "com.wps.",
        "com.alibaba.dingtalk",
        "com.bytedance.feishu",
        "com.larksuite.",
        // Other common custom-rendered communication and note applications.
        "com.microsoft.teams",
        "com.tinyspeck.slackmacgap",
        "com.hnc.discord",
        "ru.keepcoder.telegram",
        "notion.id",
        "md.obsidian",
        // Native shell + WebKit content (selection often missing from AX in
        // document panes; input fields still work via AX).
        "app.codeg",
    };
    const std::string normalizedBundleId = LowercaseAscii(bundleId);
    const std::string normalizedAppName = LowercaseAscii(appName);
    for (const char *prefix : compatibleApplicationPrefixes) {
        if (normalizedBundleId.rfind(prefix, 0) == 0) {
            return true;
        }
    }
    if (IsWpsSelectionApplication(normalizedBundleId, normalizedAppName)) {
        return true;
    }
    return IsOpenAISelectionApplication(bundleId, appName) ||
        IsCodeGSelectionApplication(bundleId, appName);
}

static std::mutex gPasteboardFallbackMutex;

static bool ReadViaClipboard(NSRunningApplication *application, SelectionInfo &info) {
    if (application == nil || application.processIdentifier == getpid() ||
        !application.active || FocusedElementIsProtected(application)) {
        return false;
    }

    std::lock_guard<std::mutex> processGuard(gPasteboardFallbackMutex);
    @autoreleasepool {
        NSPasteboard *pasteboard = [NSPasteboard generalPasteboard];
        if (pasteboard == nil) {
            return false;
        }
        NSInteger originalChangeCount = pasteboard.changeCount;
        PasteboardSnapshot snapshot = SnapshotPasteboard(pasteboard);
        if (!snapshot.valid || !PostCopyShortcut(application.processIdentifier)) {
            return false;
        }

        NSInteger copiedChangeCount = originalChangeCount;
        bool changed = false;
        // WPS and some Electron/custom-rendered applications publish copied
        // text asynchronously. Wait up to 500 ms, while still returning as
        // soon as the pasteboard changes.
        for (int attempt = 0; attempt < 50; ++attempt) {
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
            copiedChangeCount = pasteboard.changeCount;
            if (copiedChangeCount != originalChangeCount) {
                changed = true;
                break;
            }
        }

        std::string copiedText;
        bool copiedValueStable = changed && pasteboard.changeCount == copiedChangeCount;
        if (copiedValueStable) {
            NSString *string = [pasteboard stringForType:NSPasteboardTypeString];
            copiedText = StringFromNSString(string);
            copiedValueStable = pasteboard.changeCount == copiedChangeCount;
        }

        // Do not overwrite a clipboard change that occurred after our Cmd+C.
        // Otherwise restore every item and every captured representation,
        // including the originally-empty state.
        // expectedChangeCount = changeCount after our synthetic Cmd+C.
        if (copiedValueStable) {
            RestorePasteboard(pasteboard, snapshot, copiedChangeCount);
        }

        if (!copiedValueStable || IsBlank(copiedText)) {
            return false;
        }
        info.text = std::move(copiedText);
        info.method = SelectionMethod::Clipboard;
        return true;
    }
}

static bool CaptureSelection(
    const std::string &excludedBundleId,
    Trigger trigger,
    CGPoint mouseStart,
    bool hasMouseStart,
    CGPoint mouseEnd,
    bool hasMouseEnd,
    CGPoint currentMouse,
    SelectionInfo &info) {
    @autoreleasepool {
        if (!AXIsProcessTrusted()) {
            return false;
        }
        NSRunningApplication *application = FrontmostApplication();
        if (IsExcludedApplication(application, excludedBundleId)) {
            return false;
        }

        info.bundleId = StringFromNSString(application.bundleIdentifier);
        info.appName = StringFromNSString(application.localizedName);
        info.trigger = trigger;
        info.mouseStart = mouseStart;
        info.mouseEnd = mouseEnd;
        info.mouseCurrent = currentMouse;
        info.hasMouseStart = hasMouseStart;
        info.hasMouseEnd = hasMouseEnd;

        bool found = ReadViaAccessibility(application, info);
        if (!found) {
            // Give Chromium/Electron a brief chance to expose the AX tree after
            // the compatibility attributes were enabled above.
            std::this_thread::sleep_for(std::chrono::milliseconds(12));
            found = ReadViaAccessibility(application, info);
        }
        // The clipboard fallback posts a synthetic Cmd+C to the frontmost app
        // to read its selection. That replaces whatever the user just copied
        // (e.g. another translator's auto-copied OCR result), so only allow it
        // for an explicit user-triggered capture (capture shortcut / tray),
        // never for automatic drag/double-click/keyboard selection detection.
        if (!found && trigger == Trigger::Manual &&
            ShouldUseClipboardFallback(info.bundleId, info.appName)) {
            found = ReadViaClipboard(application, info);
        }
        if (!found || IsBlank(info.text)) {
            return false;
        }

        // Clipboard-only captures still report fullscreen state when AX can
        // expose the front window even if it cannot expose selected text.
        if (info.method == SelectionMethod::Clipboard) {
            AXUIElementRef appElement = AXUIElementCreateApplication(application.processIdentifier);
            if (appElement != nullptr) {
                info.fullscreen = ReadFullscreen(appElement);
                CFRelease(appElement);
            }
        }
        return true;
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
        @"method": info.method == SelectionMethod::Accessibility ? @"accessibility" : @"clipboard",
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

        AXUIElementRef focused = CopyFocusedElement(applicationElement);
        CFRelease(applicationElement);
        if (focused == nullptr) {
            return 0;
        }

        std::string current;
        if (!ReadSelectedText(focused, current) || current != expectedText) {
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
        keyboardSelectionPending = false;
        running.store(true, std::memory_order_release);

        try {
            workerThread = std::thread(&TextLensSelectionMonitor::workerMain, this);
            eventThread = std::thread(&TextLensSelectionMonitor::eventMain, this);
        } catch (...) {
            running.store(false, std::memory_order_release);
            taskCondition.notify_all();
            if (eventThread.joinable()) eventThread.join();
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
            if (eventThread.joinable()) eventThread.join();
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
        if (eventThread.joinable()) eventThread.join();
        if (workerThread.joinable()) workerThread.join();
        {
            std::lock_guard<std::mutex> taskLock(taskMutex);
            tasks.clear();
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
            info);
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
        if (type == kCGEventTapDisabledByTimeout || type == kCGEventTapDisabledByUserInput) {
            if (monitor->eventTap != nullptr) {
                CGEventTapEnable(monitor->eventTap, true);
            }
            return event;
        }
        if (CGEventGetIntegerValueField(event, kCGEventSourceUserData) == kSyntheticEventMarker) {
            return event;
        }
        monitor->handleEvent(type, event);
        return event;
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
        enqueueTask(std::move(task));
    }

    void enqueueDismiss(const char *reason, CGPoint point, int64_t targetPid) {
        Task task;
        task.kind = TaskKind::Dismiss;
        task.dismissReason = reason;
        task.mouseCurrent = point;
        task.targetPid = targetPid;
        enqueueTask(std::move(task));
    }

    void enqueueTask(Task task) {
        if (!running.load(std::memory_order_acquire)) {
            return;
        }
        {
            std::lock_guard<std::mutex> lock(taskMutex);
            if (tasks.size() >= kMaximumQueuedTasks) {
                auto dismiss = std::find_if(tasks.begin(), tasks.end(), [](const Task &queued) {
                    return queued.kind == TaskKind::Dismiss;
                });
                if (dismiss != tasks.end()) {
                    tasks.erase(dismiss);
                } else {
                    tasks.pop_front();
                }
            }
            if (task.kind == TaskKind::Dismiss && !tasks.empty() &&
                tasks.back().kind == TaskKind::Dismiss &&
                tasks.back().dismissReason == task.dismissReason) {
                tasks.back() = std::move(task);
            } else {
                tasks.push_back(std::move(task));
            }
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
                if (task.kind == TaskKind::Dismiss) {
                    emit(DismissJSON(task));
                    continue;
                }

                // Let the target application commit its selection after the
                // input event without ever blocking the event-tap callback.
                std::this_thread::sleep_for(std::chrono::milliseconds(12));
                SelectionInfo info;
                bool captured = false;
                {
                    std::lock_guard<std::mutex> captureLock(captureMutex);
                    captured = CaptureSelection(
                        excludedBundleId,
                        task.trigger,
                        task.mouseStart,
                        task.hasMouseStart,
                        task.mouseEnd,
                        task.hasMouseEnd,
                        task.mouseCurrent,
                        info);
                }
                if (captured && task.generation == captureGeneration.load(std::memory_order_acquire)) {
                    emit(SelectionJSON(info));
                }
            }
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

    std::atomic<bool> running{false};
    std::atomic<uint64_t> captureGeneration{0};
    std::mutex lifecycleMutex;
    std::mutex captureMutex;

    std::thread eventThread;
    std::thread workerThread;
    CFMachPortRef eventTap = nullptr;
    CFRunLoopSourceRef runLoopSource = nullptr;
    CFRunLoopRef eventRunLoop = nullptr;
    std::mutex runLoopMutex;

    std::mutex startupMutex;
    std::condition_variable startupCondition;
    bool startupReady = false;
    int32_t startupStatus = TEXTLENS_SELECTION_INTERNAL_ERROR;

    std::mutex taskMutex;
    std::condition_variable taskCondition;
    std::deque<Task> tasks;

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
