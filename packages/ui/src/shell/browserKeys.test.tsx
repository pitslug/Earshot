import { act, fireEvent, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { createMockIpc } from "../ipc";
import { openSettings, renderApp } from "../test/app";
import { ITEM, setup } from "../test/meetingDetail";
import { BROWSER_KEYS, suppressBrowserKeys } from "./browserKeys";

let remove: (() => void) | null = null;
afterEach(() => {
  remove?.();
  remove = null;
});

/** A key press on `target` as the webview sends it; resolves to whether its default was prevented. */
function press(target: EventTarget, init: KeyboardEventInit): boolean {
  const event = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  act(() => {
    target.dispatchEvent(event);
  });
  return event.defaultPrevented;
}

/** Every key the webview would act on and the app swallows, written out (never read from the
 * guard's own table): what the webview does with it, and the press. */
const SWALLOWED: [string, string, KeyboardEventInit][] = [
  ["F5", "reload", { key: "F5" }],
  ["Ctrl+F5", "reload", { key: "F5", ctrlKey: true }],
  ["Shift+F5", "reload", { key: "F5", shiftKey: true }],
  ["Ctrl+R", "reload", { key: "r", ctrlKey: true }],
  ["Ctrl+Shift+R", "reload", { key: "R", ctrlKey: true, shiftKey: true }],
  ["the keyboard's refresh key", "reload", { key: "BrowserRefresh" }],
  ["F3", "find again", { key: "F3" }],
  ["Shift+F3", "find again", { key: "F3", shiftKey: true }],
  ["Ctrl+G", "find again", { key: "g", ctrlKey: true }],
  ["Ctrl+Shift+G", "find again", { key: "G", ctrlKey: true, shiftKey: true }],
  ["Ctrl+P", "print", { key: "p", ctrlKey: true }],
  ["Ctrl+Shift+P", "print", { key: "P", ctrlKey: true, shiftKey: true }],
  ["F7", "caret browsing", { key: "F7" }],
  ["Ctrl+U", "view source", { key: "u", ctrlKey: true }],
  ["Ctrl+J", "downloads", { key: "j", ctrlKey: true }],
  ["Alt+Left", "back", { key: "ArrowLeft", altKey: true }],
  ["Alt+Right", "forward", { key: "ArrowRight", altKey: true }],
  ["the keyboard's back key", "back", { key: "BrowserBack" }],
  ["the keyboard's forward key", "forward", { key: "BrowserForward" }],
];

/** Keys a text field or the app's own controls use: none may be touched, anywhere. */
const LEFT_ALONE: [string, KeyboardEventInit][] = [
  ["r", { key: "r" }],
  ["Shift+R", { key: "R", shiftKey: true }],
  ["f", { key: "f" }],
  ["g", { key: "g" }],
  ["p", { key: "p" }],
  ["u", { key: "u" }],
  ["j", { key: "j" }],
  // AltGr arrives as Ctrl+Alt on some layouts: a letter, not a shortcut.
  ["AltGr+R", { key: "r", ctrlKey: true, altKey: true }],
  ["AltGr+F", { key: "f", ctrlKey: true, altKey: true }],
  ["AltGr+G", { key: "g", ctrlKey: true, altKey: true }],
  ["AltGr+P", { key: "p", ctrlKey: true, altKey: true }],
  ["AltGr+U", { key: "u", ctrlKey: true, altKey: true }],
  ["AltGr+J", { key: "j", ctrlKey: true, altKey: true }],
  ["Ctrl+Shift+F", { key: "f", ctrlKey: true, shiftKey: true }],
  ["Left", { key: "ArrowLeft" }],
  ["Right", { key: "ArrowRight" }],
  ["Shift+Left", { key: "ArrowLeft", shiftKey: true }],
  ["Ctrl+Left", { key: "ArrowLeft", ctrlKey: true }],
  ["Ctrl+Right", { key: "ArrowRight", ctrlKey: true }],
  ["Ctrl+Shift+Right", { key: "ArrowRight", ctrlKey: true, shiftKey: true }],
  ["Alt+Shift+Left", { key: "ArrowLeft", altKey: true, shiftKey: true }],
  ["Alt+Down", { key: "ArrowDown", altKey: true }],
  ["Alt+Up", { key: "ArrowUp", altKey: true }],
  ["Home", { key: "Home" }],
  ["End", { key: "End" }],
  ["Backspace", { key: "Backspace" }],
  ["Ctrl+Backspace", { key: "Backspace", ctrlKey: true }],
  ["Delete", { key: "Delete" }],
  ["Ctrl+A", { key: "a", ctrlKey: true }],
  ["Ctrl+C", { key: "c", ctrlKey: true }],
  ["Ctrl+V", { key: "v", ctrlKey: true }],
  ["Ctrl+X", { key: "x", ctrlKey: true }],
  ["Ctrl+Z", { key: "z", ctrlKey: true }],
  ["Ctrl+Y", { key: "y", ctrlKey: true }],
  ["Tab", { key: "Tab" }],
  ["Enter", { key: "Enter" }],
  ["Escape", { key: "Escape" }],
  ["F2", { key: "F2" }],
  ["F4", { key: "F4" }],
  ["F6", { key: "F6" }],
  ["F10", { key: "F10" }],
  ["Shift+F10", { key: "F10", shiftKey: true }],
  ["the Menu key", { key: "ContextMenu" }],
];

describe("browser keys", () => {
  it.each(SWALLOWED)("browser keys: %s does nothing (the webview's %s)", (_name, _does, init) => {
    remove = suppressBrowserKeys(window);
    expect(press(document.body, init)).toBe(true);
  });

  it.each(SWALLOWED)("browser keys: %s is swallowed in a text field too, before any other listener", (_name, _does, init) => {
    remove = suppressBrowserKeys(window);
    const input = document.createElement("input");
    document.body.append(input);
    try {
      let seen: boolean | null = null;
      input.addEventListener("keydown", (e) => {
        seen = e.defaultPrevented;
      });
      expect(press(input, init)).toBe(true);
      expect(seen).toBe(true);
    } finally {
      input.remove();
    }
  });

  it.each(LEFT_ALONE)("browser keys: %s is left alone, in a text field and outside one", (_name, init) => {
    remove = suppressBrowserKeys(window);
    const input = document.createElement("input");
    document.body.append(input);
    try {
      expect(press(input, init)).toBe(false);
      expect(press(document.body, init)).toBe(false);
    } finally {
      input.remove();
    }
  });

  it("browser keys: every key in the guard's table has a press written out here, with what it does", () => {
    const pressed = new Set(SWALLOWED.map(([, does, init]) => `${does}:${String(init.key).toLowerCase()}`));
    for (const k of BROWSER_KEYS) expect(pressed).toContain(`${k.does}:${k.key}`);
    // And nothing is written out here that the table does not hold.
    const held = new Set(BROWSER_KEYS.map((k) => `${k.does}:${k.key}`));
    for (const p of pressed) expect(held).toContain(p);
    // Ctrl+F is not a table key: the meeting page must hear it first.
    expect(BROWSER_KEYS.some((k) => k.key === "f")).toBe(false);
  });

  it("browser keys: Ctrl+F with nothing else listening opens nothing", () => {
    remove = suppressBrowserKeys(window);
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    expect(press(document.body, { key: "F", ctrlKey: true })).toBe(true);
  });

  it("browser keys: a page's own Ctrl+F handler, mounted later, still hears the key first and once", () => {
    remove = suppressBrowserKeys(window);
    // As the meeting page listens (T12.98): on the window, ignoring a key something else took.
    const heard: boolean[] = [];
    const page = (e: KeyboardEvent) => {
      heard.push(e.defaultPrevented);
      if (e.defaultPrevented) return;
      e.preventDefault();
    };
    window.addEventListener("keydown", page);
    try {
      expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
      expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
      // Each press reached the page's handler untouched.
      expect(heard).toEqual([false, false]);
    } finally {
      window.removeEventListener("keydown", page);
    }
    // The page has gone: the key still opens nothing.
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    expect(heard).toHaveLength(2);
  });

  it("browser keys: removing the guard leaves every key alone", () => {
    const off = suppressBrowserKeys(window);
    off();
    expect(press(document.body, { key: "F5" })).toBe(false);
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(false);
  });

  it("browser keys: on the meeting page Ctrl+F still opens find, as without the guard (T12.98)", async () => {
    remove = suppressBrowserKeys(window);
    await setup();
    await screen.findByRole("tab", { name: "Transcript" });
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    const box = await screen.findByRole("textbox", { name: "Text to find" });
    expect(box).toHaveFocus();
    // Pressed again in the find box, it is still the page's own key.
    expect(press(box, { key: "f", ctrlKey: true })).toBe(true);
    expect(screen.getAllByRole("textbox", { name: "Text to find" })).toHaveLength(1);
  });

  it("browser keys: on the meeting page Ctrl+F in another text field still does nothing at all (T12.98)", async () => {
    remove = suppressBrowserKeys(window);
    const s = await setup();
    await screen.findByRole("tab", { name: "Transcript" });
    const search = screen.getByRole("searchbox", { name: "Search meetings" });
    await s.user.click(search);
    expect(press(search, { key: "f", ctrlKey: true })).toBe(true);
    expect(search).toHaveFocus();
    expect(screen.queryByRole("textbox", { name: "Text to find" })).toBeNull();
  });

  it("browser keys: on the new-meeting screen Ctrl+F does nothing, and the meeting page has it back", async () => {
    remove = suppressBrowserKeys(window);
    const s = await setup();
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await screen.findByRole("button", { name: "Start" });
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    expect(screen.queryByRole("textbox", { name: "Text to find" })).toBeNull();
    await s.user.click(screen.getByRole("button", { name: "Cancel" }));
    await screen.findByRole("heading", { level: 1, name: ITEM.title });
    await screen.findByRole("tab", { name: "Transcript" });
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    expect(await screen.findByRole("textbox", { name: "Text to find" })).toHaveFocus();
  });

  it("browser keys: in Settings Ctrl+F and the reload keys do nothing", async () => {
    remove = suppressBrowserKeys(window);
    const s = await setup();
    await openSettings(s.user, "General");
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    expect(screen.queryByRole("textbox", { name: "Text to find" })).toBeNull();
    expect(press(document.body, { key: "F5" })).toBe(true);
  });

  it("browser keys: with no meetings at all Ctrl+F does nothing", async () => {
    remove = suppressBrowserKeys(window);
    renderApp(createMockIpc({ meetings: [] }));
    await screen.findByRole("heading", { level: 1, name: "Record your first meeting" });
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    expect(fireEvent.keyDown(document.body, { key: "r", ctrlKey: true })).toBe(false);
  });
});
