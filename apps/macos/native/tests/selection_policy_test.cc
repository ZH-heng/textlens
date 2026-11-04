#include "app_profiles.h"
#include "selection_policy.h"

#include <cassert>
#include <iostream>

using namespace textlens_selection_policy;
using namespace textlens_app_profiles;

namespace {

void ExpectDegenerateFragment() {
    assert(IsDegenerateFragment(""));
    assert(IsDegenerateFragment("   \n\t"));
    assert(IsDegenerateFragment("1. "));
    assert(IsDegenerateFragment("• "));
    assert(IsDegenerateFragment("—"));
    assert(IsDegenerateFragment("42"));
    assert(!IsDegenerateFragment("a"));
    assert(!IsDegenerateFragment("中"));
    assert(!IsDegenerateFragment("あ"));
    assert(!IsDegenerateFragment("Ж"));
    assert(!IsDegenerateFragment("word"));
    assert(!IsDegenerateFragment("1. Item"));
}

void ExpectPointNearBounds() {
    assert(PointNearBounds(0, 0, 0, 0, 100, 50, 0));
    assert(PointNearBounds(100, 50, 0, 0, 100, 50, 0));
    assert(PointNearBounds(-20, 60, 0, 0, 100, 50, 20));
    assert(!PointNearBounds(-21, 60, 0, 0, 100, 50, 20));
    assert(!PointNearBounds(0, 71, 0, 0, 100, 50, 20));
    assert(PointNearBounds(-100.5, -200.5, -120, -220, 30, 30, 0.6));
}

void ExpectOpaqueWindowDecision() {
    OpaqueWindowSignals signals;
    assert(!IsOpaqueWindowSurface(signals));

    signals.resolved = true;
    signals.reachedWindow = true;
    assert(IsOpaqueWindowSurface(signals));
    assert(AnchorIsOpaqueWindow(signals));

    signals.anchorWindowKnown = true;
    signals.trustedAnchorWindow = true;
    assert(!AnchorIsOpaqueWindow(signals));

    signals.trustedAnchorWindow = false;
    assert(AnchorIsOpaqueWindow(signals));

    signals.windowHasContentSurface = true;
    assert(!IsOpaqueWindowSurface(signals));
    assert(!AnchorIsOpaqueWindow(signals));

    signals.windowHasContentSurface = false;
    signals.complete = false;
    assert(!IsOpaqueWindowSurface(signals));
    assert(!AnchorIsOpaqueWindow(signals));
}

void ExpectClipboardSnapshotRestorable() {
    ClipboardSnapshotAccounting empty;
    assert(empty.IsRestorable());

    ClipboardSnapshotAccounting faithful;
    faithful.restorableRepresentations = 3;
    faithful.capturedRepresentations = 3;
    assert(faithful.IsRestorable());

    // One oversized representation skipped out of three: restoring would drop it.
    ClipboardSnapshotAccounting partial;
    partial.restorableRepresentations = 3;
    partial.capturedRepresentations = 2;
    assert(!partial.IsRestorable());

    // Nothing captured at all (promised/lazy clipboard).
    ClipboardSnapshotAccounting unreadable;
    unreadable.restorableRepresentations = 2;
    assert(!unreadable.IsRestorable());

    // An item we had to drop even though every other representation survived.
    ClipboardSnapshotAccounting hollowItem;
    hollowItem.restorableRepresentations = 2;
    hollowItem.capturedRepresentations = 2;
    hollowItem.itemsWithoutCapture = 1;
    assert(!hollowItem.IsRestorable());
}

void ExpectAppProfiles() {
    const AppTraits wps = ResolveAppTraits("com.kingsoft.wpsoffice.mac", "");
    assert(wps.wps);
    assert(wps.documentOffice);
    assert(wps.clipboard == ClipboardPolicy::Allowed);

    const AppTraits wpsByName = ResolveAppTraits("", "WPS Office");
    assert(wpsByName.wps);
    assert(wpsByName.documentOffice);

    const AppTraits acrobat = ResolveAppTraits("com.adobe.Acrobat", "");
    assert(acrobat.documentOffice);
    assert(!acrobat.wps);

    const AppTraits previewByName = ResolveAppTraits("", "Preview");
    assert(previewByName.documentOffice);

    const AppTraits finder = ResolveAppTraits("com.apple.finder", "Finder");
    assert(finder.clipboard == ClipboardPolicy::Denied);

    assert(ResolveAppTraits("com.depthpicture.PixPin", "").clipboard == ClipboardPolicy::Denied);
    assert(ResolveAppTraits("", "PixPin").clipboard == ClipboardPolicy::Denied);
    assert(ResolveAppTraits("com.example.editor", "PixPin Notes").clipboard ==
        ClipboardPolicy::Allowed);

    const AppTraits passwordManager = ResolveAppTraits("com.1password.1password", "");
    assert(passwordManager.clipboard == ClipboardPolicy::Denied);

    const AppTraits terminal = ResolveAppTraits("com.apple.Terminal", "Terminal");
    assert(terminal.clipboard == ClipboardPolicy::KeyboardOnly);

    const AppTraits itermByName = ResolveAppTraits("", "iTerm2");
    assert(itermByName.clipboard == ClipboardPolicy::KeyboardOnly);

    const AppTraits safari = ResolveAppTraits("com.apple.Safari", "Safari");
    assert(safari.applescript == AppleScriptDialect::Safari);

    const AppTraits chrome = ResolveAppTraits("com.google.Chrome", "Google Chrome");
    assert(chrome.applescript == AppleScriptDialect::Chromium);

    const AppTraits wechat = ResolveAppTraits("com.tencent.xinWeChat", "WeChat");
    assert(wechat.surfaceGuard == SurfaceGuard::OpaqueWindow);

    const AppTraits ordinary = ResolveAppTraits("com.example.editor", "Editor");
    assert(!ordinary.wps);
    assert(!ordinary.documentOffice);
    assert(ordinary.clipboard == ClipboardPolicy::Allowed);
    assert(ordinary.applescript == AppleScriptDialect::None);
    assert(ordinary.surfaceGuard == SurfaceGuard::None);
}

}  // namespace

int main() {
    ExpectDegenerateFragment();
    ExpectPointNearBounds();
    ExpectOpaqueWindowDecision();
    ExpectClipboardSnapshotRestorable();
    ExpectAppProfiles();
    std::cout << "selection policy tests passed\n";
    return 0;
}
