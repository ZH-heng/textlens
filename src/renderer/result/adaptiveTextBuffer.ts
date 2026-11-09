// Keep a small playout cushion instead of draining every network burst. The
// first content is still flushed synchronously by the channel handler.
const MIN_CUSHION_MS = 80;
const MAX_CUSHION_MS = 400;
const CUSHION_RELEASE_MS = 6000;
const RELAX_MS = 200;
const MAX_QUEUED_MS = 1000;

interface Chunk {
  text: string;
  offset: number;
  arrivedAt: number;
}

export class AdaptiveTextBuffer {
  private chunks: Chunk[] = [];
  private size = 0;
  private lastArrival: number | null = null;
  private lastFrame = 0;
  private cushion = MIN_CUSHION_MS;
  private rate = 0;
  private sampled = false;
  private measuredAt = 0;
  private unmeasured = 0;
  private credit = 0;

  get length(): number { return this.size; }
  get deadline(): number {
    return this.chunks.length ? this.chunks[0]!.arrivedAt + this.maxDelay : Infinity;
  }

  private get maxDelay(): number {
    return Math.min(MAX_QUEUED_MS, this.cushion * 2 + RELAX_MS);
  }

  push(text: string, now: number): void {
    if (!text) return;
    if (this.lastArrival !== null) {
      const gap = now - this.lastArrival;
      if (gap > 0) {
        // Fast attack and slow release keep recurring gaps covered, without
        // letting a long startup/thinking delay enter the body's estimator.
        this.cushion = Math.max(MIN_CUSHION_MS,
          this.cushion * Math.exp(-gap / CUSHION_RELEASE_MS),
          Math.min(MAX_CUSHION_MS, gap * 1.25));
      }
      // IPC can deliver several deltas at the same timestamp. Measure their
      // combined volume over a real interval instead of a near-zero divisor.
      this.unmeasured += text.length;
    } else {
      this.rate = text.length * 1000 / MIN_CUSHION_MS;
      this.measuredAt = now;
    }
    this.lastArrival = now;
    if (!this.size) {
      this.lastFrame = now;
      this.credit = 0;
    }
    this.chunks.push({ text, offset: 0, arrivedAt: now });
    this.size += text.length;
  }

  take(now: number, all = false): string {
    if (!this.size) return "";
    // Sample at playout so splitting a coalesced arrival into several IPC
    // events cannot change the estimated speed. Idle frames add no samples.
    const duration = this.lastArrival! - this.measuredAt;
    if (duration >= 16) {
      const sample = this.unmeasured * 1000 / duration;
      if (!this.sampled || duration >= MAX_QUEUED_MS * 2) {
        this.rate = sample;
        this.sampled = true;
      } else {
        const weight = 1 - Math.exp(-duration / 400);
        this.rate += weight * (sample - this.rate);
      }
      this.measuredAt = this.lastArrival!;
      this.unmeasured = 0;
    }
    const elapsed = Math.max(0, now - this.lastFrame);
    this.lastFrame = now;
    // React/layout can occasionally delay a frame. Replaying the whole missed
    // interval drains the cushion in a burst and creates another pause after
    // the main thread has already recovered. Deadlines still force catch-up.
    // Detect a developing gap while there is still text to play, rather than
    // waiting for the next arrival to tell us how long the gap was.
    if (this.sampled) {
      this.cushion = Math.max(this.cushion,
        Math.min(MAX_CUSHION_MS, (now - this.lastArrival!) * 1.25));
    }
    const target = this.rate * this.cushion / 1000;
    // A delayed frame may catch up when enough text exists above the cushion.
    // Near/below the cushion keep the stricter limit that prevents underruns.
    const spareMs = Math.max(0, this.size - target) * 1000 / Math.max(1, this.rate);
    const playbackElapsed = Math.min(elapsed, 32 + Math.min(32, spareMs));
    // Below target: slow down enough to retain text for the next network gap.
    // Above target: accelerate, with no fixed character-speed ceiling. Keep
    // fractional credit so low-rate streams aren't forced to one char/frame.
    const speed = Math.max(this.rate * 0.15, this.rate + (this.size - target) * 1000 / RELAX_MS);
    const budget = this.credit + speed * playbackElapsed / 1000;
    let count = all ? this.size : Math.max(0, Math.floor(budget));
    let due = 0;
    for (const chunk of this.chunks) {
      if (now - chunk.arrivedAt < this.maxDelay) break;
      due += chunk.text.length - chunk.offset;
    }
    const forced = all || due > count;
    count = Math.min(this.size, Math.max(count, due));
    const sizeBefore = this.size;

    const parts: string[] = [];
    while (count > 0 && this.chunks.length) {
      const chunk = this.chunks[0]!;
      let end = Math.min(chunk.text.length, chunk.offset + count);
      // Rate accounting uses UTF-16 units; visible slices preserve code points.
      const before = chunk.text.charCodeAt(end - 1);
      const after = chunk.text.charCodeAt(end);
      if (before >= 0xd800 && before <= 0xdbff && after >= 0xdc00 && after <= 0xdfff) end++;
      parts.push(chunk.text.slice(chunk.offset, end));
      const taken = end - chunk.offset;
      this.size -= taken;
      count -= taken;
      chunk.offset = end;
      if (end === chunk.text.length) this.chunks.shift();
    }
    // A preserved surrogate pair can spend one extra UTF-16 unit. Carry that
    // debt too, or emoji-heavy text plays faster and drains the cushion.
    this.credit = forced || !this.size ? 0 : budget - (sizeBefore - this.size);
    return parts.join("");
  }
}
