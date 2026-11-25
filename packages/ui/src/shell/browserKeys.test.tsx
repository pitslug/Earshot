import { act, fireEvent, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { createMockIpc } from "../ipc";
import { openSettings, renderApp } from "../test/app";
import { ITEM, setup } from "../test/meetingDetail";
import { suppressBrowserKeys } from "./browserKeys";

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

const RELOADS: [string, KeyboardEventInit][] = [
  ["F5", { key: "F5" }],
  ["Ctrl+F5", { key: "F5", ctrlKey: true }],
  ["Shift+F5", { key: "F5", shiftKey: true }],
  ["Ctrl+R", { key: "r", ctrlKey: true }],
  ["Ctrl+Shift+R", { key: "R", ctrlKey: true, shiftKey: true }],
  ["the keyboard's refresh key", { key: "BrowserRefresh" }],
];

describe("browser keys", () => {
  it.each(RELOADS)("browser keys: %s reloads nothing", (_name, init) => {
    remove = suppressBrowserKeys(window);
    expect(press(document.body, init)).toBe(true);
  });

  it("browser keys: reload keys are swallowed in a text field too, and before any other listener", () => {
    remove = suppressBrowserKeys(window);
    const input = document.createElement("input");
    document.body.append(input);
    let seen: boolean | null = null;
    const onKey = (e: KeyboardEvent) => {
      seen = e.defaultPrevented;
    };
    input.addEventListener("keydown", onKey);
    expect(press(input, { key: "F5" })).toBe(true);
    expect(seen).toBe(true);
    input.remove();
  });

  it("browser keys: ordinary typing is left alone", () => {
    remove = suppressBrowserKeys(window);
    for (const init of [
      { key: "r" },
      { key: "R", shiftKey: true },
      { key: "f" },
      { key: "F4" },
      // AltGr arrives as Ctrl+Alt on some layouts: a letter, not a shortcut.
      { key: "r", ctrlKey: true, altKey: true },
      { key: "f", ctrlKey: true, altKey: true },
      { key: "f", ctrlKey: true, shiftKey: true },
    ]) {
      expect(press(document.body, init)).toBe(false);
    }
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
    const s = await setup();
    await screen.findByRole("tab", { name: "Transcript" });
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    const box = await screen.findByRole("textbox", { name: "Text to find" });
    expect(box).toHaveFocus();
    // In another text field it does nothing at all: no second bar, focus stays.
    const search = screen.getByRole("searchbox", { name: "Search meetings" });
    await s.user.click(search);
    expect(press(search, { key: "f", ctrlKey: true })).toBe(true);
    expect(search).toHaveFocus();
    expect(screen.getAllByRole("textbox", { name: "Text to find" })).toHaveLength(1);
  });

  it("browser keys: away from a meeting page Ctrl+F does nothing", async () => {
    remove = suppressBrowserKeys(window);
    const s = await setup();
    // The new-meeting screen.
    await s.user.click(screen.getByRole("button", { name: "New meeting" }));
    await screen.findByRole("button", { name: "Start" });
    expect(press(document.body, { key: "f", ctrlKey: true })).toBe(true);
    expect(screen.queryByRole("textbox", { name: "Text to find" })).toBeNull();
    await s.user.click(screen.getByRole("button", { name: "Cancel" }));
    await screen.findByRole("heading", { level: 1, name: ITEM.title });
    // Settings.
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
