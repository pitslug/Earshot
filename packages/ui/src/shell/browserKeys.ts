// Keys the webview acts on by itself and the app has no use for (T14.14). F5 and Ctrl+R load the
// page again, which throws away what is on screen, a meeting's recording screen included, while
// the app keeps recording; Alt+Left goes back a page; the find keys open the webview's own find
// bar, which searches the page's markup and not a transcript; the rest open the webview's print
// dialog, its downloads, the page's source or its caret-browsing question. None of them is a key
// a text field uses: the editing keys (arrows without Alt, Ctrl+A, C, V, X, Z, Y and the like)
// are never touched, and a letter typed with AltGr (which arrives as Ctrl+Alt) stays a letter.
//
// Ctrl+F is apart. The meeting page has its own (T12.98): a `keydown` listener on the window, in
// the bubble phase, mounted only while a meeting's page is shown, that opens find in the
// transcript. Its first line is `if (e.defaultPrevented || e.altKey) return;`, so it ignores a key
// something else has already taken. That is why this guard cannot prevent Ctrl+F in the capture
// phase as it does every other key: `preventDefault` does not stop the event, but the page's
// handler would see `defaultPrevented` and open nothing. The guard does not know whether the page
// is mounted, and does not need to: it puts its own Ctrl+F listener last on the window at every
// press, so whatever listens there has run first. When the page's handler took the key, the
// default is already prevented and nothing is left to do; when no page is mounted, the key ends
// here and opens nothing. Either way exactly one of the two handles a press. Like the page's
// handler, it relies on the event reaching the window: a handler that stops a Ctrl+F on its way
// up leaves the webview's find to open, on the meeting page as anywhere else.

/** Which other keys must be down for a key to count.
 * - `any`: the key itself, whatever else is held.
 * - `ctrl`: with Ctrl (or the Windows key's `meta`), with or without Shift, and without Alt:
 *   AltGr arrives as Ctrl+Alt on some layouts and types a letter.
 * - `alt`: with Alt alone. */
type Held = "any" | "ctrl" | "alt";

/** A key the webview would act on. `key` is `KeyboardEvent.key` in lower case. */
export interface BrowserKey {
  /** What the webview does with it, for the tests' names. */
  does: string;
  key: string;
  held: Held;
}

/** Every key swallowed before anything else hears it. Ctrl+F is not here: see above. */
export const BROWSER_KEYS: readonly BrowserKey[] = [
  { does: "reload", key: "f5", held: "any" },
  { does: "reload", key: "r", held: "ctrl" },
  { does: "reload", key: "browserrefresh", held: "any" },
  { does: "find again", key: "f3", held: "any" },
  { does: "find again", key: "g", held: "ctrl" },
  { does: "print", key: "p", held: "ctrl" },
  { does: "caret browsing", key: "f7", held: "any" },
  { does: "view source", key: "u", held: "ctrl" },
  { does: "downloads", key: "j", held: "ctrl" },
  { does: "back", key: "arrowleft", held: "alt" },
  { does: "forward", key: "arrowright", held: "alt" },
  { does: "back", key: "browserback", held: "any" },
  { does: "forward", key: "browserforward", held: "any" },
];

function holds(e: KeyboardEvent, held: Held): boolean {
  switch (held) {
    case "any":
      return true;
    case "ctrl":
      return (e.ctrlKey || e.metaKey) && !e.altKey;
    case "alt":
      return e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey;
  }
}

function isBrowserKey(e: KeyboardEvent): boolean {
  if (typeof e.key !== "string") return false;
  const key = e.key.toLowerCase();
  return BROWSER_KEYS.some((k) => k.key === key && holds(e, k.held));
}

/** Ctrl+F, exactly as the meeting page reads it (`MeetingDetailView`'s key listener). */
function isFind(e: KeyboardEvent): boolean {
  if (typeof e.key !== "string") return false;
  return e.key.toLowerCase() === "f" && (e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey;
}

/** Makes the webview's own keys do nothing: reload, back and forward, find again, print, view
 * source, downloads and caret browsing everywhere, and Ctrl+F where no meeting page handles it.
 * Returns a function that removes the guard. */
export function suppressBrowserKeys(win: Window): () => void {
  // After every other listener on the window: the meeting page's handler, when mounted, has
  // already prevented the default.
  const afterThePage = (e: KeyboardEvent) => {
    if (isFind(e) && !e.defaultPrevented) e.preventDefault();
  };
  // Before every other listener: nothing in the app reads one of these keys.
  const first = (e: KeyboardEvent) => {
    if (isBrowserKey(e)) {
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
