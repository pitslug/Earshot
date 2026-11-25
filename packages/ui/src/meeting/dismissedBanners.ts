// The banners the user dismissed on the meeting the window follows, kept in the window's session
// storage so a reload does not show them again (T14.51). Never in the store: this is about one
// window. It holds banner keys and a meeting id only, never banner text, and is cleared when the
// meeting's last event arrives or another meeting starts. A banner sent again after the reload
// shows again, as it would have without the reload.

/** The session storage key. */
export const DISMISSED_KEY = "earshot-dismissed-banners";

interface Dismissed {
  meeting: string;
  keys: string[];
}

function storage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.sessionStorage;
  } catch {
    // A webview with storage turned off: nothing is remembered, and banners come back as before.
    return null;
  }
}

function read(): Dismissed | null {
  const raw = storage()?.getItem(DISMISSED_KEY) ?? null;
  if (raw === null) return null;
  try {
    const v: unknown = JSON.parse(raw);
    if (
      typeof v === "object" &&
      v !== null &&
      typeof (v as Dismissed).meeting === "string" &&
      Array.isArray((v as Dismissed).keys) &&
      (v as Dismissed).keys.every((k) => typeof k === "string")
    ) {
      return v as Dismissed;
    }
  } catch {
    // Unreadable: as if nothing was dismissed.
  }
  return null;
}

function write(value: Dismissed | null) {
  const s = storage();
  if (s === null) return;
  try {
    if (value === null || value.keys.length === 0) s.removeItem(DISMISSED_KEY);
    else s.setItem(DISMISSED_KEY, JSON.stringify(value));
  } catch {
    // Full or refused: the banner may come back after a reload, nothing worse.
  }
}

/** The banner keys dismissed on `meeting`. */
export function dismissedOn(meeting: string): Set<string> {
  const d = read();
  return new Set(d !== null && d.meeting === meeting ? d.keys : []);
}

/** The user dismissed banner `key` of `meeting`. */
export function rememberDismissed(meeting: string, key: string) {
  const d = read();
  const keys = d !== null && d.meeting === meeting ? d.keys : [];
  if (!keys.includes(key)) write({ meeting, keys: [...keys, key] });
}

/** Banner `key` of `meeting` was sent again: it shows, and a later reload shows it too. */
export function forgetDismissed(meeting: string, key: string) {
  const d = read();
  if (d === null || d.meeting !== meeting || !d.keys.includes(key)) return;
  write({ meeting, keys: d.keys.filter((k) => k !== key) });
}

/** The meeting ended, or another one started: nothing of it is remembered. */
export function clearDismissed(unlessMeeting?: string) {
  const d = read();
  if (d !== null && d.meeting !== unlessMeeting) write(null);
}
