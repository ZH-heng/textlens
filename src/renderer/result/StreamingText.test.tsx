// @vitest-environment jsdom
import { act, Suspense } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { StreamingText } from "./StreamingText";

it("does not consume appended fragments in a render that suspends before commit", async () => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  const container = document.createElement("div");
  const root = createRoot(container);
  const pending = new Promise<void>(() => {});
  function Gate({ suspend }: { suspend: boolean }) { if (suspend) throw pending; return null; }
  const render = (text: string, suspend = false) => act(async () => root.render(
    <Suspense fallback={<span>等待</span>}><StreamingText text={text} /><Gate suspend={suspend} /></Suspense>,
  ));
  try {
    await render("首");
    await render("首未提交", true);
    await render("首未提交后续");
    const spans = Array.from(container.querySelectorAll(".stream-text-enter"));
    expect(spans.map((span) => span.textContent).join("")).toBe("未提交后续");
    expect(container.querySelector(".stream-text-run")?.textContent).toBe("首未提交后续");
  } finally { await act(async () => root.unmount()); }
});

it("merges finished fragments while keeping recent fragments and selected text intact", async () => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  let now = 0;
  const clock = vi.spyOn(performance, "now").mockImplementation(() => now);
  const container = document.createElement("div");
  document.body.append(container);
  const root = createRoot(container);
  let text = "首";
  const render = async () => act(async () => root.render(<StreamingText text={text} />));
  try {
    await render();
    for (let i = 0; i < 40; i++) { text += "字"; await render(); }
    const selected = container.querySelector(".stream-text-enter")!;
    const range = document.createRange();
    range.selectNodeContents(selected);
    window.getSelection()?.removeAllRanges();
    window.getSelection()?.addRange(range);
    now = 400;
    text += "新";
    await render();
    const recent = Array.from(container.querySelectorAll(".stream-text-enter")).at(-1);
    expect(window.getSelection()?.toString()).toBe("字");
    expect(container.querySelectorAll(".stream-text-enter")).toHaveLength(41);
    window.getSelection()?.removeAllRanges();
    text += "增";
    await render();
    expect(container.querySelectorAll(".stream-text-enter")).toHaveLength(2);
    expect(container.querySelector(".stream-text-enter")).toBe(recent);
    expect(container.textContent).toBe(text);
  } finally {
    window.getSelection()?.removeAllRanges();
    await act(async () => root.unmount());
    container.remove();
    clock.mockRestore();
  }
});
