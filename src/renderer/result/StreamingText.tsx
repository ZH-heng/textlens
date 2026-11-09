import { memo, useLayoutEffect, useRef } from "react";

const FADE_MS = 250;
// Give the browser time to finish painting before merging old animation nodes.
const SETTLE_MS = FADE_MS + 100;
const COMPACT_AFTER = 32;

interface Part { id: number; text: string; at: number }
interface Snapshot { text: string; settled: string; parts: Part[]; nextId: number }

function hasSelection(node: HTMLElement | null): boolean {
  const selection = window.getSelection();
  if (!node || !selection || !selection.rangeCount) return false;
  return selection.containsNode(node, true) ||
    node.contains(selection.anchorNode) || node.contains(selection.focusNode);
}

// Keys preserve the still-fading spans when old spans are merged. Do not merge
// while a selection touches this run: replacing selected text nodes loses it.
export const StreamingText = memo(function StreamingText({ text, animateInitial = false }: { text: string; animateInitial?: boolean }) {
  const element = useRef<HTMLSpanElement>(null);
  const committed = useRef<Snapshot>({ text, settled: animateInitial ? "" : text,
    parts: animateInitial ? [{ id: 0, text, at: performance.now() }] : [], nextId: animateInitial ? 1 : 0 });
  let snapshot = committed.current;
  if (text !== snapshot.text) {
    if (!text.startsWith(snapshot.text)) {
      snapshot = { ...snapshot, text, settled: text, parts: [] };
    } else {
      snapshot = { ...snapshot, text, nextId: snapshot.nextId + 1, parts: [...snapshot.parts,
        { id: snapshot.nextId, text: text.slice(snapshot.text.length), at: performance.now() }] };
    }
  }
  if (snapshot.parts.length > COMPACT_AFTER && !hasSelection(element.current)) {
    const cutoff = performance.now() - SETTLE_MS;
    let count = 0;
    while (count < snapshot.parts.length && snapshot.parts[count]!.at <= cutoff) count++;
    if (count) {
      snapshot = { ...snapshot,
        settled: snapshot.settled + snapshot.parts.slice(0, count).map((part) => part.text).join(""),
        parts: snapshot.parts.slice(count) };
    }
  }
  // Aborted/suspended renders must not advance the animation history.
  useLayoutEffect(() => { committed.current = snapshot; }, [snapshot]);
  return (
    <span ref={element} className="stream-text-run">
      {snapshot.settled}
      {snapshot.parts.map((part) => <span key={part.id} className="stream-text-enter">{part.text}</span>)}
    </span>
  );
});
