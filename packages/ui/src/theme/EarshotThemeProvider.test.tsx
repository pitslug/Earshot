import { act, cleanup, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import bootScript from "../../../../apps/desktop/public/theme-boot.js?raw";
import { installMatchMedia } from "../test/matchMedia";
import {
  EarshotThemeProvider,
  THEME_KEY,
  type ThemeChoice,
  readThemeChoice,
  useThemeChoice,
} from "./EarshotThemeProvider";

function themeOf(): string | null {
  return screen.getByText("content").closest("[data-theme]")?.getAttribute("data-theme") ?? null;
}

let choose: (choice: ThemeChoice) => void = () => {};
function Chooser() {
  choose = useThemeChoice().setChoice;
  return <span>content</span>;
}

function renderApp() {
  return render(
    <EarshotThemeProvider>
      <Chooser />
    </EarshotThemeProvider>,
  );
}

describe("EarshotThemeProvider", () => {
  it("starts light when the system is light and follows a switch to dark live", () => {
    const media = installMatchMedia(false);
    render(
      <EarshotThemeProvider>
        <span>content</span>
      </EarshotThemeProvider>,
    );
    expect(themeOf()).toBe("light");
    act(() => media.setDark(true));
    expect(themeOf()).toBe("dark");
    act(() => media.setDark(false));
    expect(themeOf()).toBe("light");
  });

  it("starts dark when the system is dark", () => {
    installMatchMedia(true);
    render(
      <EarshotThemeProvider>
        <span>content</span>
      </EarshotThemeProvider>,
    );
    expect(themeOf()).toBe("dark");
  });

  it("uses a chosen theme whatever the system says, even when the system changes", () => {
    for (const system of [false, true]) {
      const media = installMatchMedia(system);
      renderApp();
      act(() => choose("dark"));
      expect(themeOf()).toBe("dark");
      act(() => media.setDark(!system));
      expect(themeOf()).toBe("dark");
      act(() => choose("light"));
      expect(themeOf()).toBe("light");
      act(() => media.setDark(system));
      expect(themeOf()).toBe("light");
      cleanup();
    }
  });

  it("follows a live system change again after going back to System", () => {
    const media = installMatchMedia(false);
    renderApp();
    act(() => choose("dark"));
    act(() => choose("system"));
    expect(themeOf()).toBe("light");
    act(() => media.setDark(true));
    expect(themeOf()).toBe("dark");
  });

  it("keeps the choice across a reload, from the first render", () => {
    installMatchMedia(false);
    const first = renderApp();
    act(() => choose("dark"));
    first.unmount();
    const seen: (string | null)[] = [];
    // The very first render already has the choice: nothing is corrected in an effect.
    function FirstRender() {
      seen.push(useThemeChoice().choice);
      return <span>content</span>;
    }
    render(
      <EarshotThemeProvider>
        <FirstRender />
      </EarshotThemeProvider>,
    );
    expect(seen[0]).toBe("dark");
    expect(themeOf()).toBe("dark");
  });

  it("reads anything unknown as System", () => {
    installMatchMedia(true);
    window.localStorage.setItem(THEME_KEY, "sepia");
    expect(readThemeChoice()).toBe("system");
    renderApp();
    expect(themeOf()).toBe("dark");
  });

  it("puts the page's own colour scheme on the theme, for native scrollbars", () => {
    const media = installMatchMedia(false);
    renderApp();
    expect(document.documentElement.style.colorScheme).toBe("light");
    act(() => media.setDark(true));
    expect(document.documentElement.style.colorScheme).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    act(() => choose("light"));
    expect(document.documentElement.style.colorScheme).toBe("light");
  });

  it("switches with no colour animation left to run, and leaves nothing behind", () => {
    const media = installMatchMedia(false);
    renderApp();
    // A style pass inside the switch sees transitions off; afterwards they are back.
    const seen: string[] = [];
    const append = Element.prototype.append;
    Element.prototype.append = function (this: Element, ...nodes: (Node | string)[]) {
      for (const n of nodes) if (n instanceof HTMLStyleElement) seen.push(n.textContent ?? "");
      return append.apply(this, nodes);
    };
    try {
      act(() => media.setDark(true));
    } finally {
      Element.prototype.append = append;
    }
    expect(seen.some((css) => css.includes("transition:none"))).toBe(true);
    expect([...document.querySelectorAll("style")].some((s) => s.textContent?.includes("transition:none"))).toBe(false);
  });

  it("agrees with the script that runs before the first paint", () => {
    const run = new Function(bootScript);
    const stored: (string | null)[] = [null, "system", "light", "dark", "sepia"];
    for (const system of [false, true]) {
      for (const value of stored) {
        installMatchMedia(system);
        if (value === null) window.localStorage.removeItem(THEME_KEY);
        else window.localStorage.setItem(THEME_KEY, value);
        delete document.documentElement.dataset.theme;
        run();
        const before = document.documentElement.dataset.theme;
        const scheme = document.documentElement.style.colorScheme;
        renderApp();
        expect([value, system, before]).toEqual([value, system, themeOf()]);
        expect(scheme).toBe(before);
        cleanup();
      }
    }
  });
});
