// The webview's own right-click menu (Back, Refresh, Save as, Print) does nothing useful in the
// app, and Refresh throws away what is on screen. It stays only where it helps: in text fields
// (cut, copy, paste) and over selected text (copy). Earshot's own menus (meeting and folder rows)
// call preventDefault themselves before this listener sees the event, so they are unaffected.

const TEXT_INPUT_TYPES = new Set(["", "text", "search", "email", "url", "tel", "password", "number"]);

function isTextField(el: Element | null): boolean {
  if (!el) return false;
  if (el instanceof HTMLTextAreaElement) return true;
  if (el instanceof HTMLInputElement) return TEXT_INPUT_TYPES.has(el.type);
  // Not `isContentEditable`, which jsdom does not implement.
  const editable = el.closest("[contenteditable]");
  return editable !== null && editable.getAttribute("contenteditable") !== "false";
}

function isOverSelection(doc: Document, target: Node): boolean {
  const selection = doc.getSelection();
  if (!selection || selection.isCollapsed || selection.toString().trim() === "") return false;
  for (let i = 0; i < selection.rangeCount; i++) {
    if (selection.getRangeAt(i).intersectsNode(target)) return true;
  }
  return false;
}

/** Hides the webview's own context menu except in text fields and over selected text. Returns a
 * function that removes the listener. */
export function suppressBrowserMenu(doc: Document): () => void {
  const onContextMenu = (e: MouseEvent) => {
    if (e.defaultPrevented) return;
    const target = e.target instanceof Node ? e.target : null;
    if (!target) return;
    const el = target instanceof Element ? target : target.parentElement;
    if (isTextField(el) || isOverSelection(doc, target)) return;
    e.preventDefault();
  };
  doc.addEventListener("contextmenu", onContextMenu);
  return () => doc.removeEventListener("contextmenu", onContextMenu);
}
