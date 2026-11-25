// Keys the webview acts on by itself and the app has no use for (T14.14). F5, Ctrl+R and
// Ctrl+Shift+R load the page again, which throws away what is on screen, a meeting's recording
// screen included, while the app keeps recording; Ctrl+F opens the webview's own find bar, which
// searches the page's markup and not a transcript.
//
// The meeting page has its own Ctrl+F (T12.98): a `keydown` listener on the window that opens find
// in the transcript, mounted only while a meeting's page is shown, and which ignores a key
// something else has already taken (`defaultPrevented`). So this guard must never take Ctrl+F
// before that listener has heard it. It does not know whether the page is mounted, and does not
// need to: it puts its own Ctrl+F listener last on the window at every press, so whatever
// listens there has run first. When the page's handler took the key, the default is already
// prevented and nothing is left to do; when no page is mounted, the key ends here and opens
// nothing. Either way exactly one of the two handles a press.

/** F5 with any modifier, Ctrl+R with or without Shift, and a keyboard's own refresh key. */
function isReload(e: KeyboardEvent): boolean {
  if (typeof e.key !== "string") return false;
  if (e.key === "F5" || e.key === "BrowserRefresh") return true;
  // AltGr arrives as Ctrl+Alt on some layouts and types a letter.
  return e.key.toLowerCase() === "r" && (e.ctrlKey || e.metaKey) && !e.altKey;
}

/** Ctrl+F, exactly as the meeting page reads it (`MeetingDetailView`'s key listener). */
function isFind(e: KeyboardEvent): boolean {
  if (typeof e.key !== "string") return false;
  return e.key.toLowerCase() === "f" && (e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey;
}

/** Makes the webview's reload keys and, where no meeting page handles it, its find key do
 * nothing. Returns a function that removes the guard. */
export function suppressBrowserKeys(win: Window): () => void {
  // After every other listener on the window: the meeting page's handler, when mounted, has
  // already prevented the default.
  const afterThePage = (e: KeyboardEvent) => {
    if (isFind(e) && !e.defaultPrevented) e.preventDefault();
  };
  // Before every other listener: nothing in the app reads a reload key.
  const first = (e: KeyboardEvent) => {
    if (isReload(e)) {
      e.preventDefault();
      return;
    }
    if (!isFind(e)) return;
    // Added again at each press, so it is the last listener even after a page mounted since.
    win.removeEventListener("keydown", afterThePage);
    win.addEventListener("keydown", afterThePage);
  };
  win.addEventListener("keydown", first, true);
  return () => {
    win.removeEventListener("keydown", first, true);
    win.removeEventListener("keydown", afterThePage);
  };
}
