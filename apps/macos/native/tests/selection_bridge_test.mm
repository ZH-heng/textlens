// Include the bridge to exercise its real enqueue/serialization boundaries.
// No monitor is started: this test installs no input hooks, reads no AX data,
// and sends no synthetic keys or clipboard operations.
#include "../selection_bridge.mm"

#include <cassert>
#include <iostream>

static NSDictionary *Decode(const std::string &json) {
    NSData *data = [NSData dataWithBytes:json.data() length:json.size()];
    return [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
}

int main() {
    @autoreleasepool {
        std::atomic<uint64_t> generation{7};
        gCaptureRunning.store(true);
        const AccessibilityReadContext expiredAXStage{&generation, 7, 0};
        assert(!expiredAXStage.IsCurrent());
        assert(expiredAXStage.RequestIsCurrent());
        generation.store(8);
        assert(!expiredAXStage.RequestIsCurrent());
        generation.store(7);
        gCaptureRunning.store(false);
        assert(!expiredAXStage.RequestIsCurrent());

        TextLensSelectionMonitor monitor("", nullptr, nullptr);
        monitor.running.store(true);
        monitor.enqueueDismiss("mouseDown", CGPointZero, 123);
        const Task oldDismiss = monitor.dismissTasks.front();
        monitor.enqueueCapture(
            Trigger::Drag, CGPointZero, true, CGPointMake(10, 10), true, CGPointMake(10, 10));
        const Task newCapture = monitor.tasks.front();
        assert(oldDismiss.sequence > 0);
        assert(newCapture.sequence > oldDismiss.sequence);

        SelectionInfo info;
        info.text = "selected text";
        info.bundleId = "com.example.editor";
        info.sequence = newCapture.sequence;
        info.timestampMs = newCapture.timestampMs;
        FallbackTask fallback;
        fallback.info = info;
        monitor.enqueueFallbackTask(std::move(fallback));
        const SelectionInfo &fallbackInfo = monitor.fallbackTasks.front().info;

        // Simulate a newer selection completing before the older dismiss.
        NSDictionary *selection = Decode(SelectionJSON(fallbackInfo));
        NSDictionary *dismiss = Decode(DismissJSON(oldDismiss));
        assert([selection[@"sequence"] unsignedLongLongValue] == newCapture.sequence);
        assert([dismiss[@"sequence"] unsignedLongLongValue] == oldDismiss.sequence);
        assert([selection[@"sequence"] unsignedLongLongValue] >
            [dismiss[@"sequence"] unsignedLongLongValue]);
        assert([selection[@"timestampMs"] unsignedLongLongValue] == newCapture.timestampMs);
        assert([dismiss[@"timestampMs"] unsignedLongLongValue] == oldDismiss.timestampMs);
        // Re-serializing must not manufacture a newer input identity.
        assert([Decode(DismissJSON(oldDismiss))[@"sequence"] unsignedLongLongValue] ==
            oldDismiss.sequence);

        monitor.enqueueDismiss("mouseDown", CGPointZero, 456);
        assert(monitor.dismissTasks.size() == 1);
        assert(monitor.dismissTasks.front().sequence > newCapture.sequence);
        assert(monitor.dismissTasks.front().targetPid == 456);
    }
    std::cout << "selection bridge ordering tests passed\n";
    return 0;
}
