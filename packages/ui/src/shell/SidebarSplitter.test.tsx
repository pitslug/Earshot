import { fireEvent, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { createMockIpc } from "../ipc";
import { renderApp } from "../test/app";
import {
  SIDEBAR_DEFAULT,
  SIDEBAR_MAX,
  SIDEBAR_MIN,
  SIDEBAR_STEP,
  SIDEBAR_WIDTH_KEY,
  clampSidebar,
  storedSidebarWidth,
} from "./SidebarSplitter";

const sidebar = () => screen.getByRole("navigation", { name: "Meetings and settings" });
const handle = () => screen.getByRole("separator", { name: "Resize the sidebar" });
const shownWidth = () => sidebar().style.width;

async function start() {
  const user = userEvent.setup();
  renderApp(createMockIpc());
  await screen.findByRole("separator", { name: "Resize the sidebar" });
  return user;
}

describe("the sidebar's width", () => {
  it("keeps a width within the limits and reads anything else as the default", () => {
    expect(clampSidebar(SIDEBAR_MIN - 50)).toBe(SIDEBAR_MIN);
    expect(clampSidebar(SIDEBAR_MAX + 50)).toBe(SIDEBAR_MAX);
    expect(clampSidebar(300.4)).toBe(300);
    expect(clampSidebar(Number.NaN)).toBe(SIDEBAR_DEFAULT);
    expect(storedSidebarWidth()).toBe(SIDEBAR_DEFAULT);
    for (const [stored, read] of [
      ["", SIDEBAR_DEFAULT],
      ["wide", SIDEBAR_DEFAULT],
      ["9999", SIDEBAR_MAX],
      ["320", 320],
    ] as const) {
      window.localStorage.setItem(SIDEBAR_WIDTH_KEY, stored);
      expect(storedSidebarWidth()).toBe(read);
    }
  });

  it("starts at the default, and at the stored width when there is one", async () => {
    await start();
    expect(shownWidth()).toBe(`${SIDEBAR_DEFAULT}px`);
    expect(handle()).toHaveAttribute("aria-valuenow", String(SIDEBAR_DEFAULT));
  });

  it("starts at a stored width", async () => {
    window.localStorage.setItem(SIDEBAR_WIDTH_KEY, "320");
    await start();
    expect(shownWidth()).toBe("320px");
  });

  it("follows the arrow keys, Home and End, and remembers the width", async () => {
    const user = await start();
    handle().focus();
    await user.keyboard("{ArrowRight}{ArrowRight}");
    expect(shownWidth()).toBe(`${SIDEBAR_DEFAULT + 2 * SIDEBAR_STEP}px`);
    expect(window.localStorage.getItem(SIDEBAR_WIDTH_KEY)).toBe(String(SIDEBAR_DEFAULT + 2 * SIDEBAR_STEP));
    await user.keyboard("{ArrowLeft}");
    expect(shownWidth()).toBe(`${SIDEBAR_DEFAULT + SIDEBAR_STEP}px`);
    await user.keyboard("{End}");
    expect(shownWidth()).toBe(`${SIDEBAR_MAX}px`);
    await user.keyboard("{ArrowRight}");
    expect(shownWidth()).toBe(`${SIDEBAR_MAX}px`);
    await user.keyboard("{Home}");
    expect(shownWidth()).toBe(`${SIDEBAR_MIN}px`);
    expect(handle()).toHaveAttribute("aria-valuenow", String(SIDEBAR_MIN));
  });

  it("follows a drag and goes back to the default on a double-click", async () => {
    await start();
    fireEvent.pointerDown(handle(), { button: 0, clientX: 240, pointerId: 1 });
    fireEvent.pointerMove(handle(), { clientX: 300, pointerId: 1 });
    expect(shownWidth()).toBe(`${SIDEBAR_DEFAULT + 60}px`);
    fireEvent.pointerMove(handle(), { clientX: 2000, pointerId: 1 });
    expect(shownWidth()).toBe(`${SIDEBAR_MAX}px`);
    fireEvent.pointerUp(handle(), { clientX: 2000, pointerId: 1 });
    // After the button is up, a move is not a drag.
    fireEvent.pointerMove(handle(), { clientX: 250, pointerId: 1 });
    expect(shownWidth()).toBe(`${SIDEBAR_MAX}px`);
    fireEvent.doubleClick(handle());
    expect(shownWidth()).toBe(`${SIDEBAR_DEFAULT}px`);
    expect(window.localStorage.getItem(SIDEBAR_WIDTH_KEY)).toBe(String(SIDEBAR_DEFAULT));
  });
});
