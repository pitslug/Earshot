// Banners on the meeting page that hide by themselves (T14.59). The rule: a banner hides after
// `BANNER_HIDE_MS` only when its intent is success or info, it offers nothing but Dismiss, and
// it is not progress of work still running. Errors, warnings, anything with an action, running
// work, the waiting-for-another-computer banner, the meeting status and the Summary tab's
// banners never hide by themselves.

import { type FocusEvent, useCallback, useEffect, useRef, useState } from "react";

import type { MessageBarIntent } from "../meeting/types";
import type { Timer } from "../time";

/** How long a banner that may hide by itself stays, while nothing holds it. */
export const BANNER_HIDE_MS = 8_000;

/** Whether a banner may hide by itself: success or info, nothing but Dismiss to offer, and not
 * progress of work still running. */
export function hidesByItself(banner: { intent: MessageBarIntent; busy?: boolean; action?: boolean }): boolean {
  return (banner.intent === "success" || banner.intent === "info") && banner.busy !== true && banner.action !== true;
}

/** Handlers that hold a banner's countdown while the pointer is over it or focus is inside it. */
export interface HoldHandlers {
  onPointerEnter(): void;
  onPointerLeave(): void;
  onFocus(): void;
  onBlur(e: FocusEvent<HTMLElement>): void;
  /** The banner's element, read when a new banner replaces the one held. */
  ref(el: HTMLElement | null): void;
}

/**
 * Calls `hide` after `BANNER_HIDE_MS` on `timer` while `active` holds and nothing holds the
 * banner. `shown` names the banner shown: a new one starts the countdown again. Leaving the
 * banner (pointer and focus both gone) starts it again from the full delay.
 */
export function useAutoHide(timer: Timer, active: boolean, shown: unknown, hide: () => void): HoldHandlers {
  const [pointer, setPointer] = useState(false);
  const [focus, setFocus] = useState(false);
  const hideRef = useRef(hide);
  hideRef.current = hide;
  const held = pointer || focus;

  const element = useRef<HTMLElement | null>(null);

  // A banner replaced or gone while held fires no leave event, so the hold is cleared whenever
  // the banner shown changes, and taken again only from what holds the new one: focus still
  // inside the element (the bar is re-used for a banner in the same place) or the pointer on it.
  useEffect(() => {
    const el = element.current;
    setPointer(el !== null && el.matches(":hover"));
    setFocus(el !== null && el.contains(document.activeElement));
  }, [shown]);

  useEffect(() => {
    if (!active || held) return;
    return timer.setTimeout(() => hideRef.current(), BANNER_HIDE_MS);
  }, [timer, active, held, shown]);

  // A stable ref, so React does not detach and attach it on every render.
  const ref = useCallback((el: HTMLElement | null) => {
    element.current = el;
  }, []);

  return {
    ref,
    onPointerEnter: () => setPointer(true),
    onPointerLeave: () => setPointer(false),
    onFocus: () => setFocus(true),
    onBlur: (e) => {
      // Focus moving between the banner's own buttons still holds it.
      if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setFocus(false);
    },
  };
}
