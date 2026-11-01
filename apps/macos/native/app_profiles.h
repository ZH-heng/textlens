#pragma once

#include <cstddef>
#include <string>

// Declarative application routing shared by the Objective-C++ bridge and the
// host policy tests. Keep this header free of Foundation/AppKit so it stays
// testable with a plain C++ compiler.
namespace textlens_app_profiles {

enum class ClipboardPolicy {
    Allowed,
    // Smart-Cmd+C terminals send SIGINT when nothing is selected; only
    // keyboard gestures (Cmd+A, shift+arrows) are guaranteed to leave a
    // selection behind.
    KeyboardOnly,
    Denied,
};

enum class AppleScriptDialect {
    None,
    Safari,
    Chromium,
};

enum class SurfaceGuard {
    None,
    // Canvas chat windows (WeChat image viewer) expose a bare AXWindow under
    // the cursor; a synthetic Cmd+C there copies the image instead of text.
    OpaqueWindow,
};

struct AppTraits {
    bool wps = false;
    bool documentOffice = false;
    ClipboardPolicy clipboard = ClipboardPolicy::Allowed;
    AppleScriptDialect applescript = AppleScriptDialect::None;
    SurfaceGuard surfaceGuard = SurfaceGuard::None;
};

enum class MatchKind {
    BundleExact,
    BundlePrefix,
    BundleContains,
    NameContains,
    NameEquals,
};

struct AppProfileRule {
    MatchKind kind;
    const char *value;
    AppTraits traits;
};

// Lowercases ASCII only; bundle ids and executable names use ASCII.
inline std::string LowercaseAscii(std::string value) {
    for (char &character : value) {
        if (character >= 'A' && character <= 'Z') {
            character = static_cast<char>(character - 'A' + 'a');
        }
    }
    return value;
}

inline const AppProfileRule kAppProfileRules[] = {
    // WPS / Kingsoft channel variants.
    {MatchKind::BundlePrefix, "com.kingsoft.", {true, true}},
    {MatchKind::BundlePrefix, "cn.wps.", {true, true}},
    {MatchKind::BundlePrefix, "com.wps.", {true, true}},
    {MatchKind::BundleContains, "wpsoffice", {true, true}},
    {MatchKind::BundleContains, "wps-office", {true, true}},
    {MatchKind::NameContains, "wps", {true, true}},
    {MatchKind::NameContains, "wpsoffice", {true, true}},
    {MatchKind::NameContains, "kingsoft", {true, true}},

    // Document / Office / PDF providers with slower AX publication.
    {MatchKind::BundlePrefix, "com.adobe.", {false, true}},
    {MatchKind::BundlePrefix, "com.microsoft.word", {false, true}},
    {MatchKind::BundlePrefix, "com.microsoft.excel", {false, true}},
    {MatchKind::BundlePrefix, "com.microsoft.powerpoint", {false, true}},
    {MatchKind::BundlePrefix, "com.apple.preview", {false, true}},
    {MatchKind::BundlePrefix, "com.apple.iwork.pages", {false, true}},
    {MatchKind::BundlePrefix, "com.apple.iwork.numbers", {false, true}},
    {MatchKind::BundlePrefix, "com.apple.iwork.keynote", {false, true}},
    {MatchKind::BundlePrefix, "org.libreoffice.", {false, true}},
    {MatchKind::BundlePrefix, "org.openoffice.", {false, true}},
    {MatchKind::NameContains, "acrobat", {false, true}},
    {MatchKind::NameContains, "preview", {false, true}},
    {MatchKind::NameContains, "libreoffice", {false, true}},
    {MatchKind::NameEquals, "word", {false, true}},
    {MatchKind::NameEquals, "excel", {false, true}},
    {MatchKind::NameEquals, "powerpoint", {false, true}},

    // Synthetic copy could lift secrets or files.
    {MatchKind::BundlePrefix, "com.1password.1password", {false, false, ClipboardPolicy::Denied}},
    {MatchKind::BundlePrefix, "com.agilebits.", {false, false, ClipboardPolicy::Denied}},
    {MatchKind::BundlePrefix, "org.keepassxc.keepassxc", {false, false, ClipboardPolicy::Denied}},
    {MatchKind::BundlePrefix, "com.bitwarden.desktop", {false, false, ClipboardPolicy::Denied}},
    {MatchKind::BundlePrefix, "com.apple.finder", {false, false, ClipboardPolicy::Denied}},

    // Smart-Cmd+C terminals.
    {MatchKind::BundlePrefix, "com.apple.terminal", {false, false, ClipboardPolicy::KeyboardOnly}},
    {MatchKind::BundlePrefix, "com.googlecode.iterm2", {false, false, ClipboardPolicy::KeyboardOnly}},
    {MatchKind::NameContains, "terminal", {false, false, ClipboardPolicy::KeyboardOnly}},
    {MatchKind::NameContains, "iterm", {false, false, ClipboardPolicy::KeyboardOnly}},

    // Browsers that answer a live DOM selection through Apple Events.
    {MatchKind::BundleExact, "com.apple.safari", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::Safari}},
    {MatchKind::BundleExact, "com.google.chrome", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::Chromium}},
    {MatchKind::BundleExact, "com.microsoft.edgemac", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::Chromium}},
    {MatchKind::BundleExact, "com.brave.browser", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::Chromium}},
    {MatchKind::BundleExact, "com.vivaldi.vivaldi", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::Chromium}},
    {MatchKind::BundleExact, "com.operasoftware.opera", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::Chromium}},
    {MatchKind::BundleExact, "company.thebrowser.arc", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::Chromium}},

    // Canvas chat hosts: require a trusted text window before synthesizing a copy.
    {MatchKind::BundleExact, "com.tencent.xinwechat", {false, false, ClipboardPolicy::Allowed,
        AppleScriptDialect::None, SurfaceGuard::OpaqueWindow}},
};

inline bool RuleMatches(
    const AppProfileRule &rule,
    const std::string &bundleId,
    const std::string &appName) {
    switch (rule.kind) {
        case MatchKind::BundleExact: return bundleId == rule.value;
        case MatchKind::BundlePrefix: return bundleId.rfind(rule.value, 0) == 0;
        case MatchKind::BundleContains: return bundleId.find(rule.value) != std::string::npos;
        case MatchKind::NameContains: return appName.find(rule.value) != std::string::npos;
        case MatchKind::NameEquals: return appName == rule.value;
    }
    return false;
}

inline AppTraits ResolveAppTraits(
    const std::string &bundleId,
    const std::string &appName) {
    const std::string normalizedBundle = LowercaseAscii(bundleId);
    const std::string normalizedName = LowercaseAscii(appName);
    AppTraits traits;
    for (const AppProfileRule &rule : kAppProfileRules) {
        if (!RuleMatches(rule, normalizedBundle, normalizedName)) {
            continue;
        }
        traits.wps = traits.wps || rule.traits.wps;
        traits.documentOffice = traits.documentOffice || rule.traits.documentOffice;
        if (rule.traits.clipboard == ClipboardPolicy::Denied ||
            (rule.traits.clipboard == ClipboardPolicy::KeyboardOnly &&
             traits.clipboard == ClipboardPolicy::Allowed)) {
            traits.clipboard = rule.traits.clipboard;
        }
        if (rule.traits.applescript != AppleScriptDialect::None) {
            traits.applescript = rule.traits.applescript;
        }
        if (rule.traits.surfaceGuard != SurfaceGuard::None) {
            traits.surfaceGuard = rule.traits.surfaceGuard;
        }
    }
    return traits;
}

}  // namespace textlens_app_profiles
