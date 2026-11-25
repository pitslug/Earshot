// Which groups of the meeting list the user closed (T12.103): browser-side only, like the theme,
// kept by group key ("last-week", "2026-10"), so a closed month stays closed across restarts.
// Nothing about a meeting is stored here.

import { useCallback, useState } from "react";

/** The `localStorage` key. */
export const COLLAPSED_GROUPS_KEY = "earshot-collapsed-groups";

/** The closed group keys as stored; anything unreadable reads as every group open. */
export function readCollapsed(storage: Pick<Storage, "getItem"> | undefined = globalThis.localStorage): Set<string> {
  try {
    const raw = storage?.getItem(COLLAPSED_GROUPS_KEY);
    if (raw == null) return new Set();
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? new Set(parsed.filter((k): k is string => typeof k === "string")) : new Set();
  } catch {
    return new Set();
  }
}

function writeCollapsed(keys: Set<string>) {
  try {
    globalThis.localStorage?.setItem(COLLAPSED_GROUPS_KEY, JSON.stringify([...keys].sort()));
  } catch {
    // Not stored: the groups are still opened and closed for this session.
  }
}

/** The closed groups, a toggle, and `open` for a group that must show (it holds the meeting
 * shown or a new one). */
export function useCollapsedGroups(): {
  collapsed: Set<string>;
  toggle(key: string): void;
  open(key: string): void;
} {
  const [collapsed, setCollapsed] = useState(() => readCollapsed());
  const change = useCallback((key: string, close: boolean | null) => {
    setCollapsed((now) => {
      const closed = close ?? !now.has(key);
      if (closed === now.has(key)) return now;
      const next = new Set(now);
      if (closed) next.add(key);
      else next.delete(key);
      writeCollapsed(next);
      return next;
    });
  }, []);
  const toggle = useCallback((key: string) => change(key, null), [change]);
  const open = useCallback((key: string) => change(key, false), [change]);
  return { collapsed, toggle, open };
}
