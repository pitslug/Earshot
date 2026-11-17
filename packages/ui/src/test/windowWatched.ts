// Whether the window is watched, as a test sets it (T12.76). jsdom's `document.hasFocus()` is
// false until something is focused, and its page is always visible: every test starts on a
// watched window (`setup.ts`), a test of an unwatched one sets that here, and `setup.ts` puts
// jsdom's own behaviour back after each test (`restoreWindowWatched`).

/** Makes the page visible or hidden and the window focused or not; with `announce`, sends the
 * events a browser would (`visibilitychange` on the document, `focus` or `blur` on the window). */
export function setWindowWatched(visible: boolean, focused: boolean, announce = false): void {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    get: () => (visible ? "visible" : "hidden"),
  });
  Object.defineProperty(document, "hidden", { configurable: true, get: () => !visible });
  document.hasFocus = () => focused;
  if (announce) {
    document.dispatchEvent(new Event("visibilitychange"));
    window.dispatchEvent(new Event(focused ? "focus" : "blur"));
  }
}

/** Removes what `setWindowWatched` put on the document, so jsdom's own `visibilityState`,
 * `hidden` and `hasFocus` apply again (they live on the prototype). */
export function restoreWindowWatched(): void {
  const own = document as unknown as Record<string, unknown>;
  for (const name of ["visibilityState", "hidden", "hasFocus"]) delete own[name];
}