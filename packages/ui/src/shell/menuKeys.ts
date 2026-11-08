// The keyboard way into a row's context menu, shared by the meeting and folder rows.

import type { KeyboardEvent } from "react";

/** The Menu key and Shift+F10 open the row's menu, under the row. Windows normally turns them into
 * a `contextmenu` event itself; doing it here does not depend on that, and opening an open menu
 * changes nothing. */
export function openMenuFromKeyboard(e: KeyboardEvent<HTMLElement>) {
  if (e.key !== "ContextMenu" && !(e.shiftKey && e.key === "F10")) return;
  e.preventDefault();
  const r = e.currentTarget.getBoundingClientRect();
  e.currentTarget.dispatchEvent(
    new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: r.left + 16, clientY: r.bottom }),
  );
}
