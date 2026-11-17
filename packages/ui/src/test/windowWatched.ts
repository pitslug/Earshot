// Whether the window is watched, as a test sets it (T12.76). jsdom's `document.hasFocus()` is
// false until something is focused, and its page is always visible: every test starts on a
// watched window (`setup.ts`), and a test of an unwatched one sets that here.

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