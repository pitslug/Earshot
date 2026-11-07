import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// jsdom has no ResizeObserver; Fluent's MessageBar observes its own size for reflow.
if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}

// jsdom does no layout: `offsetParent` is always null and the body measures 0x0, so tabster
// (Fluent's focus manager, `Focusable.isVisible`) treats every element as hidden. A Dialog then
// focuses its own surface instead of its first button, tabster never activates the dialog's
// modalizer (it does not when the modalizer element itself is focused) and aria-hides the
// dialog 250 ms later (T7.3d). A browser lays the page out, so give jsdom the two facts tabster
// reads: an element has an offset parent unless it or an ancestor is display:none, and the
// body has a size.
Object.defineProperty(HTMLElement.prototype, "offsetParent", {
  configurable: true,
  get(this: HTMLElement): Element | null {
    for (let e: Element | null = this; e; e = e.parentElement) {
      if (getComputedStyle(e).display === "none") return null;
    }
    return this === document.body ? null : this.parentElement;
  },
});
const bodyRect = HTMLBodyElement.prototype.getBoundingClientRect;
HTMLBodyElement.prototype.getBoundingClientRect = function (this: HTMLBodyElement): DOMRect {
  const r = bodyRect.call(this);
  return r.width === 0 && r.height === 0 ? new DOMRect(0, 0, 1024, 768) : r;
};

afterEach(() => {
  cleanup();
  // The theme chosen in one test is not the next test's.
  window.localStorage.clear();
});
