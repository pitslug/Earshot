// The handle between the sidebar and the page: drag, arrow keys, Home and End resize the sidebar,
// a double-click puts back the default width. The width is browser-side only (like the theme,
// `earshot-theme`): it is a matter of this window, never a setting in the store.

import { makeStyles, mergeClasses, tokens } from "@fluentui/react-components";
import { type KeyboardEvent, type PointerEvent, useCallback, useRef, useState } from "react";

/** `localStorage` key of the sidebar's width in CSS pixels. */
export const SIDEBAR_WIDTH_KEY = "earshot-sidebar-width";
export const SIDEBAR_DEFAULT = 240;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 480;
/** What the page keeps beside the sidebar; the sidebar gives way down to its minimum. */
export const PAGE_MIN_WIDTH = 600;
/** One arrow key press. */
export const SIDEBAR_STEP = 16;

/** `width` within the limits, rounded to a whole pixel; anything unreadable is the default. */
export function clampSidebar(width: number): number {
  if (!Number.isFinite(width)) return SIDEBAR_DEFAULT;
  return Math.round(Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, width)));
}

function storage(): Storage | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

/** The stored width, or the default when none is stored or it does not read. */
export function storedSidebarWidth(): number {
  const raw = storage()?.getItem(SIDEBAR_WIDTH_KEY);
  return raw == null || raw.trim() === "" ? SIDEBAR_DEFAULT : clampSidebar(Number(raw));
}

/** The sidebar's width and a setter that keeps it within the limits and remembers it. */
export function useSidebarWidth(): [number, (width: number) => void] {
  const [width, setWidth] = useState(storedSidebarWidth);
  const set = useCallback((next: number) => {
    const w = clampSidebar(next);
    setWidth(w);
    try {
      storage()?.setItem(SIDEBAR_WIDTH_KEY, String(w));
    } catch {
      // A full or refused storage only means the width is not remembered.
    }
  }, []);
  return [width, set];
}

const useStyles = makeStyles({
  handle: {
    flexShrink: 0,
    width: "5px",
    marginLeft: "-2px",
    marginRight: "-2px",
    position: "relative",
    zIndex: 1,
    cursor: "col-resize",
    touchAction: "none",
    outlineStyle: "none",
    // The visible line is the sidebar's border; the handle around it is wider to grab.
    "::after": {
      content: '""',
      position: "absolute",
      top: 0,
      bottom: 0,
      left: "2px",
      width: tokens.strokeWidthThin,
      backgroundColor: tokens.colorNeutralStroke2,
    },
    ":hover::after": { width: tokens.strokeWidthThickest, left: "1px", backgroundColor: tokens.colorBrandStroke1 },
    ":focus-visible::after": {
      width: tokens.strokeWidthThickest,
      left: "1px",
      backgroundColor: tokens.colorStrokeFocus2,
    },
  },
  dragging: {
    "::after": { width: tokens.strokeWidthThickest, left: "1px", backgroundColor: tokens.colorBrandStroke1 },
  },
});

/** The sidebar's width as drawn: the stored width unless the window caps it (AppShell's
 * `maxWidth`). The sidebar is the handle's previous sibling; without layout (jsdom) it reads 0. */
function drawnWidth(handle: HTMLElement, stored: number): number {
  const drawn = handle.previousElementSibling?.getBoundingClientRect().width ?? 0;
  return drawn > 0 ? drawn : stored;
}

/** The resize handle; `width` is the sidebar's stored width. */
export function SidebarSplitter({ width, onWidth }: { width: number; onWidth(width: number): void }) {
  const styles = useStyles();
  const drag = useRef<{ x: number; width: number } | null>(null);
  const [dragging, setDragging] = useState(false);

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture?.(e.pointerId);
    drag.current = { x: e.clientX, width: drawnWidth(e.currentTarget, width) };
    setDragging(true);
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (d !== null) onWidth(d.width + e.clientX - d.x);
  };
  const end = (e: PointerEvent<HTMLDivElement>) => {
    if (drag.current === null) return;
    drag.current = null;
    setDragging(false);
    e.currentTarget.releasePointerCapture?.(e.pointerId);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const now = drawnWidth(e.currentTarget, width);
    const next =
      e.key === "ArrowLeft"
        ? now - SIDEBAR_STEP
        : e.key === "ArrowRight"
          ? now + SIDEBAR_STEP
          : e.key === "Home"
            ? SIDEBAR_MIN
            : e.key === "End"
              ? SIDEBAR_MAX
              : null;
    if (next === null) return;
    e.preventDefault();
    onWidth(next);
  };

  return (
    // A focusable separator is the ARIA window splitter pattern: it takes a value and keys.
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize the sidebar"
      aria-valuenow={width}
      aria-valuemin={SIDEBAR_MIN}
      aria-valuemax={SIDEBAR_MAX}
      tabIndex={0}
      className={mergeClasses(styles.handle, dragging && styles.dragging)}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onLostPointerCapture={end}
      onDoubleClick={() => onWidth(SIDEBAR_DEFAULT)}
      onKeyDown={onKeyDown}
    />
  );
}
