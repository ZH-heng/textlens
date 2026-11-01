#pragma once

#include <cstddef>
#include <cstdint>
#include <string>

// Pure selection predicates shared by the Objective-C++ bridge and the host
// test runner. Keep this file free of Foundation/AppKit so it stays testable
// with a plain C++ compiler.
namespace textlens_selection_policy {

inline bool CodePointIsLetterish(uint32_t codePoint) {
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

/// Mouse-triggered captures of tiny non-letter fragments ("1. ", "•", "—")
/// are rendering artifacts, not text the user tried to select.
inline bool IsDegenerateFragment(const std::string &text) {
    size_t start = 0;
    size_t end = text.size();
    while (start < end) {
        const unsigned char value = static_cast<unsigned char>(text[start]);
        if (value != ' ' && value != '\t' && value != '\n' && value != '\r' &&
            value != '\f' && value != '\v') {
            break;
        }
        ++start;
    }
    while (end > start) {
        const unsigned char value = static_cast<unsigned char>(text[end - 1]);
        if (value != ' ' && value != '\t' && value != '\n' && value != '\r' &&
            value != '\f' && value != '\v') {
            break;
        }
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
        ++codePoints;
        if (codePoints > 3) {
            return false;
        }
    }
    return !hasLetterish;
}

inline bool PointNearBounds(
    double pointX,
    double pointY,
    double originX,
    double originY,
    double width,
    double height,
    double tolerance) {
    return pointX >= originX - tolerance &&
        pointX <= originX + width + tolerance &&
        pointY >= originY - tolerance &&
        pointY <= originY + height + tolerance;
}

struct OpaqueWindowSignals {
    bool resolved = false;
    bool complete = true;
    bool reachedWindow = false;
    bool hasTextRole = false;
    bool hasContentRole = false;
    bool windowHasContentSurface = false;
    bool anchorWindowKnown = false;
    bool trustedAnchorWindow = false;
};

inline bool IsOpaqueWindowSurface(const OpaqueWindowSignals &signals) {
    return signals.resolved && signals.complete && signals.reachedWindow &&
        !signals.hasTextRole && !signals.hasContentRole &&
        !signals.windowHasContentSurface;
}

/// An opaque chain is still text when the anchor window already produced a
/// text capture. Unknown window ids stay conservative (reject the copy).
inline bool AnchorIsOpaqueWindow(const OpaqueWindowSignals &signals) {
    return IsOpaqueWindowSurface(signals) &&
        (!signals.anchorWindowKnown || !signals.trustedAnchorWindow);
}

}  // namespace textlens_selection_policy
