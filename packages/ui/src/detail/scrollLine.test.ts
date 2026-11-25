import { afterEach, describe, expect, it, vi } from "vitest";

import { SETTLE_FRAMES, scrollLineIntoView } from "./scrollLine";

/** Frames run by hand. */
function frames() {
  let next = 0;
  const queue = new Map<number, FrameRequestCallback>();
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    next += 1;
    queue.set(next, cb);
    return next;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => queue.delete(id));
  return {
    run() {
      const first = queue.entries().next();
      if (first.done) return;
      queue.delete(first.value[0]);
      first.value[1](0);
    },
    pending: () => queue.size,
  };
}

/** A line whose position after each scroll is the next of `tops` (the estimate, then real
 * heights), staying at the last one. */
function line(tops: number[]) {
  const el = document.createElement("div");
  document.body.append(el);
  let at = 0;
  let top = tops[0];
  el.scrollIntoView = vi.fn(() => {
    top = tops[Math.min(at, tops.length - 1)];
    at += 1;
  });
  el.getBoundingClientRect = () => ({ top }) as DOMRect;
  return {
    el,
    /** Layout moves the line without a scroll (lines drawn on the way took real heights). */
    shift: (to: number) => (top = to),
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  document.body.innerHTML = "";
});

describe("scrollLineIntoView", () => {
  it("scrolls again while layout moves the line, and stops once it holds still", () => {
    const f = frames();
    const l = line([350, 350, 350]);
    scrollLineIntoView(l.el);
    expect(l.el.scrollIntoView).toHaveBeenCalledTimes(1);
    // The lines drawn on the way push it down.
    l.shift(1084);
    f.run();
    expect(l.el.scrollIntoView).toHaveBeenCalledTimes(2);
    // Now it stays: no further scroll, no further frame.
    f.run();
    expect(l.el.scrollIntoView).toHaveBeenCalledTimes(2);
    expect(f.pending()).toBe(0);
  });

  it("gives up after a bounded number of frames", () => {
    const f = frames();
    const l = line([350]);
    scrollLineIntoView(l.el);
    for (let i = 0; i < SETTLE_FRAMES + 5; i += 1) {
      l.shift(1000 + i);
      f.run();
    }
    expect(l.el.scrollIntoView).toHaveBeenCalledTimes(SETTLE_FRAMES + 1);
  });

  it("stops when cancelled", () => {
    const f = frames();
    const l = line([350]);
    const stop = scrollLineIntoView(l.el);
    stop();
    l.shift(900);
    f.run();
    expect(l.el.scrollIntoView).toHaveBeenCalledTimes(1);
  });
});
