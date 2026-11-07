// The clock and timers the UI uses, injected so tests never wait on real time.

/** A clock with timers. */
export interface Timer {
  /** Milliseconds (Unix epoch for the real timer). */
  now(): number;
  /** Runs `fn` once after `ms`; returns a cancel function. */
  setTimeout(fn: () => void, ms: number): () => void;
  /** Runs `fn` every `ms`; returns a cancel function. */
  setInterval(fn: () => void, ms: number): () => void;
}

/** The browser's clock and timers. */
export const realTimer: Timer = {
  now: () => Date.now(),
  setTimeout(fn, ms) {
    const id = globalThis.setTimeout(fn, ms);
    return () => globalThis.clearTimeout(id);
  },
  setInterval(fn, ms) {
    const id = globalThis.setInterval(fn, ms);
    return () => globalThis.clearInterval(id);
  },
};

/** A timer that moves only when told to (tests). */
export interface ManualTimer extends Timer {
  /** Moves the clock forward by `ms`, running every timer due on the way, in time order. */
  advance(ms: number): void;
}

/** A manual timer starting at `start`. */
export function createManualTimer(start = 0): ManualTimer {
  let now = start;
  let seq = 0;
  const due = new Map<number, { at: number; every: number | null; fn: () => void }>();
  const add = (fn: () => void, ms: number, every: number | null) => {
    const id = seq++;
    due.set(id, { at: now + Math.max(0, ms), every, fn });
    return () => {
      due.delete(id);
    };
  };
  return {
    now: () => now,
    setTimeout: (fn, ms) => add(fn, ms, null),
    setInterval: (fn, ms) => add(fn, ms, Math.max(1, ms)),
    advance(ms: number) {
      const end = now + ms;
      for (;;) {
        let next: [number, { at: number; every: number | null; fn: () => void }] | null = null;
        for (const entry of due) {
          if (entry[1].at <= end && (next === null || entry[1].at < next[1].at)) next = entry;
        }
        if (next === null) break;
        const [id, t] = next;
        now = t.at;
        if (t.every === null) due.delete(id);
        else t.at += t.every;
        t.fn();
      }
      now = end;
    },
  };
}
