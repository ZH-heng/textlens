import { describe, expect, it } from "vitest";
import { AdaptiveTextBuffer } from "./adaptiveTextBuffer";

interface Arrival { at: number; text: string }

// Replay model arrivals and display frames independently. Latency is measured
// from receipt of each UTF-16 unit, including the final tail without a done event.
function replay(arrivals: Arrival[], frameMs: number) {
  const buffer = new AdaptiveTextBuffer();
  const received: number[] = [];
  const lags: number[] = [];
  const steps: number[] = [];
  const frames: { at: number; count: number }[] = [];
  let shown = "";
  let index = 0;
  let nextFrame = frameMs;
  const end = arrivals.at(-1)!.at + 1200;
  for (let now = 0; now <= end; now++) {
    while (index < arrivals.length && arrivals[index]!.at <= now) {
      const item = arrivals[index++]!;
      received.push(...Array<number>(item.text.length).fill(now));
      buffer.push(item.text, now);
      if (index === 1) shown += buffer.take(now, true);
    }
    if (now < nextFrame) continue;
    nextFrame += frameMs;
    const part = buffer.take(now);
    frames.push({ at: now, count: part.length });
    if (part) {
      for (let i = shown.length; i < shown.length + part.length; i++) lags.push(now - received[i]!);
      shown += part;
      steps.push(part.length);
    }
    // No received character can sit in the queue past the latency budget plus
    // one display frame, even when a model suddenly changes speed.
    const cutoff = now - 1000;
    expect(received[shown.length]! === undefined || received[shown.length]! > cutoff).toBe(true);
  }
  expect(shown).toBe(arrivals.map((item) => item.text).join(""));
  const sorted = [...lags].sort((a, b) => a - b);
  return { maxLag: Math.max(0, ...lags), p95Lag: sorted[Math.floor(sorted.length * 0.95)] ?? 0, steps, frames };
}

function steady(rate: number, interval: number, duration = 2000, start = 0): Arrival[] {
  const result: Arrival[] = [];
  for (let t = 0; t < duration; t += interval) {
    const count = Math.floor((t + interval) * rate / 1000) - Math.floor(t * rate / 1000);
    if (count) result.push({ at: start + t, text: "文".repeat(count) });
  }
  return result;
}

describe("adaptive display latency across model speeds", () => {
  it.each([20, 80, 240, 1200, 6000])("keeps up with %i chars/s at different refresh rates", (rate) => {
    for (const frameMs of [1000 / 120, 1000 / 60, 1000 / 30]) {
      const result = replay(steady(rate, 16), frameMs);
      expect(result.maxLag).toBeLessThanOrEqual(1000 + Math.ceil(frameMs));
      // Stable arrivals should converge to a short cushion, not the hard cap.
      expect(result.p95Lag).toBeLessThanOrEqual(240);
      if (process.env.MYNA_STREAM_BENCHMARK) {
        console.info(JSON.stringify({ rate, frameMs: Math.round(frameMs), maxLagMs: result.maxLag, p95LagMs: result.p95Lag }));
      }
    }
  });

  it("smooths a slow model's batched delivery instead of waiting for more data", () => {
    const arrivals = steady(120, 100);
    const result = replay(arrivals, 16);
    expect(result.steps.length).toBeGreaterThan(arrivals.length * 2);
    expect(result.p95Lag).toBeLessThanOrEqual(240);
  });

  it("keeps frame increments even during sustained fast delivery", () => {
    const result = replay(steady(800, 16, 5000), 16);
    const steps = result.frames.filter((f) => f.at >= 2000 && f.at < 4800).map((f) => f.count);
    expect(Math.max(...steps) - Math.min(...steps)).toBeLessThanOrEqual(1);
  });

  it.each([100, 128, 200])("keeps output moving between %i ms bursts, including empty frames", (interval) => {
    for (const frameMs of [1000 / 120, 1000 / 60, 1000 / 30]) {
      const result = replay(steady(800, interval, 8000), frameMs);
      const active = result.frames.filter((f) => f.at >= 2000 && f.at < 7800);
      expect(active.every((f) => f.count > 0)).toBe(true);
      expect(result.p95Lag).toBeLessThanOrEqual(400);
    }
  });

  it("covers recurring jitter after fast arrivals without waiting for a new chunk", () => {
    const arrivals: Arrival[] = [];
    let at = 0;
    let previousGap = 16;
    const gaps = [16, 16, 16, 16, 192, 16, 16, 48];
    for (let i = 0; at < 8000; i++) {
      arrivals.push({ at, text: "文".repeat(Math.round(800 * previousGap / 1000)) });
      previousGap = gaps[i % gaps.length]!;
      at += previousGap;
    }
    const result = replay(arrivals, 16);
    const active = result.frames.filter((f) => f.at >= 2000 && f.at < 7800);
    expect(active.every((f) => f.count > 0)).toBe(true);
    expect(result.p95Lag).toBeLessThanOrEqual(400);
  });

  it("releases fractional low-rate output without forcing a character every frame", () => {
    const result = replay(steady(20, 200, 6000), 16);
    const active = result.frames.filter((f) => f.at >= 2000 && f.at < 5800);
    expect(active.filter((f) => f.count > 0).length).toBeLessThan(active.length / 2);
    expect(result.p95Lag).toBeLessThanOrEqual(600);
  });

  it("plays coalesced IPC events at the same speed as one combined arrival", () => {
    const combined = new AdaptiveTextBuffer();
    const split = new AdaptiveTextBuffer();
    for (const buffer of [combined, split]) {
      buffer.push("首", 0);
      expect(buffer.take(0, true)).toBe("首");
    }
    for (let now = 16; now <= 2000; now += 16) {
      if (now % 32 === 0) {
        combined.push("文".repeat(24), now);
        for (let i = 0; i < 6; i++) split.push("文".repeat(4), now);
      }
      expect(split.take(now)).toBe(combined.take(now));
    }
    expect(split.take(3000)).toBe(combined.take(3000));
    expect(split.length).toBe(0);
  });

  it("adapts immediately to acceleration, deceleration, pauses and network bursts", () => {
    replay([
      ...steady(40, 50, 500),
      ...steady(6000, 16, 500, 500),
      ...steady(20, 50, 500, 1000),
      { at: 2000, text: "突发😀".repeat(1000) },
      ...steady(240, 80, 500, 2100),
    ], 16);
  });

  it("keeps up with accelerating input even when display frames run at 48 ms", () => {
    const result = replay([
      ...steady(125, 16, 2000),
      ...steady(5000, 16, 5000, 2000),
    ], 48);
    expect(result.p95Lag).toBeLessThanOrEqual(128);
  });

  it("does not trade continuous output for faster release across cadence and rate changes", () => {
    const arrivals: Arrival[] = [];
    let seed = 1;
    let previousGap = 16;
    for (let at = 0; at < 11000;) {
      const rate = at < 3000 ? 800 : at < 5000 ? 3000 : at < 7000 ? 240 : 1200;
      arrivals.push({ at, text: "文".repeat(Math.max(1, Math.round(previousGap * rate / 1000))) });
      seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
      previousGap = [16, 16, 32, 48, 96, 128, 192, 320][seed % 8]!;
      at += previousGap;
    }
    for (const frameMs of [16, 48, 64]) {
      const result = replay(arrivals, frameMs);
      const active = result.frames.filter((frame) => frame.at >= 2000 && frame.at < 10500);
      expect(active.every((frame) => frame.count > 0)).toBe(true);
    }
  });

  it("bounds recovery after a stalled frame while still enforcing the hard deadline", () => {
    const buffer = new AdaptiveTextBuffer();
    buffer.push("文".repeat(10000), 0);
    const first = buffer.take(120);
    expect(first.length).toBeGreaterThan(0);
    expect(first.length).toBeLessThan(10000);
    expect(first + buffer.take(1100)).toBe("文".repeat(10000));
    expect(buffer.length).toBe(0);
  });

  it("preserves a cushion after a delayed frame followed by an upstream gap", () => {
    const buffer = new AdaptiveTextBuffer();
    const steps: number[] = [];
    for (let now = 0; now <= 2304; now += 16) {
      if (now <= 2000 || now >= 2176) buffer.push("文".repeat(now === 2176 ? 140 : 13), now);
      if (now === 0) buffer.take(now, true);
      else if (now <= 2000 || now >= 2064) {
        const count = buffer.take(now).length;
        if (now >= 2064) steps.push(count);
      }
    }
    expect(steps[0]).toBeLessThan(32);
    expect(steps.filter((count) => count === 0).length).toBeLessThanOrEqual(1);
  });

  it("preserves Unicode and order across many partial releases", () => {
    const buffer = new AdaptiveTextBuffer();
    const text = "中文😀𝄞e\u0301\n".repeat(100);
    buffer.push(text, 0);
    let shown = "";
    for (let now = 8; buffer.length; now += 8) {
      const part = buffer.take(now);
      expect(Array.from(part).every((char) => char.length === 2 || !/[\uD800-\uDFFF]/.test(char))).toBe(true);
      shown += part;
    }
    expect(shown).toBe(text);
  });

  it("keeps the same playout pace for emoji and BMP text measured in UTF-16 units", () => {
    const plain = new AdaptiveTextBuffer();
    const emoji = new AdaptiveTextBuffer();
    let plainCount = 0;
    let emojiCount = 0;
    for (let now = 0; now <= 5000; now += 16) {
      if (now % 128 === 0) {
        plain.push("文".repeat(40), now);
        emoji.push("😀".repeat(20), now);
      }
      plainCount += plain.take(now, now === 0).length;
      emojiCount += emoji.take(now, now === 0).length;
      expect(Math.abs(plainCount - emojiCount)).toBeLessThanOrEqual(2);
    }
  });
});
