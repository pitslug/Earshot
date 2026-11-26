// The meeting header's buttons at a wide and a narrow width (T14.55). jsdom does no layout, so
// the width comes from `headerFit.measure`.

import { act, render, screen } from "@testing-library/react";
import { useRef } from "react";
import { afterEach, describe, expect, it } from "vitest";

import { sampleDetail } from "../ipc";
import { ITEM, setup } from "../test/meetingDetail";
import { type RowSpace, headerFit, labelsFit, measureRow, useLabelsFit } from "./headerFit";

const realResizeObserver = globalThis.ResizeObserver;

afterEach(() => {
  headerFit.measure = measureRow;
  globalThis.ResizeObserver = realResizeObserver;
});

/** A header `available` pixels wide whose labelled buttons need 900 and icon buttons 400. */
function headerOf(width: { available: number }) {
  headerFit.measure = (row: HTMLElement): RowSpace => ({
    need: (row.textContent ?? "").includes("Export to Obsidian") ? 900 : 400,
    available: width.available,
  });
}

/** Keeps every ResizeObserver callback, so a test can say the header changed size. */
function watchResizes(): () => void {
  const callbacks: ResizeObserverCallback[] = [];
  globalThis.ResizeObserver = class {
    constructor(cb: ResizeObserverCallback) {
      callbacks.push(cb);
    }
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
  return () => act(() => callbacks.forEach((cb) => cb([], {} as ResizeObserver)));
}

const speakers = () => screen.getByRole("button", { name: /^Speakers/ });
const exportButton = () => screen.getByRole("button", { name: "Export to Obsidian" });
const copyButton = () => screen.getByRole("button", { name: "Copy summary" });

describe("the meeting header's buttons", () => {
  it("keeps every label when the buttons fit on one row", async () => {
    headerOf({ available: 1000 });
    await setup();
    await screen.findByText(/TL;DR/);
    expect(speakers()).toHaveAccessibleName("Speakers, 2 need names");
    expect(speakers()).toHaveTextContent("Speakers2 need names");
    expect(exportButton()).toHaveTextContent("Export to Obsidian");
    expect(copyButton()).toHaveTextContent("Copy summary");
    expect(screen.getByRole("button", { name: "Re-transcribe" })).toHaveTextContent("Re-transcribe");
  });

  it("shows icons for Speakers, Export and Copy when the labels do not fit, with their names kept", async () => {
    headerOf({ available: 600 });
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    expect(speakers()).toHaveAccessibleName("Speakers, 2 need names");
    // The badge is the count alone; the name says what it counts.
    expect(speakers()).toHaveTextContent(/^2$/);
    expect(exportButton().textContent).toBe("");
    // Copy keeps its live text for a screen reader, hidden from view.
    expect(copyButton()).toHaveTextContent("Copy summary");
    expect(copyButton().querySelector("[aria-live]")).toHaveAttribute("data-sr-only");
    // Re-transcribe and Re-summarise keep their labels at every width.
    expect(screen.getByRole("button", { name: "Re-transcribe" })).toHaveTextContent("Re-transcribe");
    const [resummarise] = screen.getAllByRole("button", { name: "Re-summarise" });
    expect(resummarise).toHaveTextContent("Re-summarise");
    // The full label shows on hover.
    await user.hover(exportButton());
    expect(await screen.findByText("Export to Obsidian")).toBeInTheDocument();
  });

  it("does not go back to labels because the icons are narrower", async () => {
    // Icons need 400 of 600: a rule judged on the icon row would draw the labels again.
    headerOf({ available: 600 });
    await setup();
    await screen.findByText(/TL;DR/);
    expect(exportButton().textContent).toBe("");
  });

  it("follows the header's width both ways as it changes", async () => {
    const width = { available: 1000 };
    headerOf(width);
    const resized = watchResizes();
    await setup();
    await screen.findByText(/TL;DR/);
    expect(exportButton()).toHaveTextContent("Export to Obsidian");
    width.available = 600;
    resized();
    expect(exportButton().textContent).toBe("");
    width.available = 950;
    resized();
    expect(exportButton()).toHaveTextContent("Export to Obsidian");
  });

  it("keeps focus on the same Speakers, Export and Copy buttons across a change of form", async () => {
    const width = { available: 1000 };
    headerOf(width);
    const resized = watchResizes();
    await setup();
    await screen.findByText(/TL;DR/);
    for (const find of [speakers, exportButton, copyButton]) {
      const before = find();
      act(() => before.focus());
      width.available = 600;
      resized();
      expect(find()).toBe(before);
      expect(before).toHaveFocus();
      width.available = 1000;
      resized();
      expect(find()).toBe(before);
      expect(before).toHaveFocus();
    }
  });

  it("says why Re-transcribe is unavailable once the recording is gone", async () => {
    await setup({ details: { [ITEM.id]: { ...sampleDetail(ITEM), audio_kept: false } } });
    await screen.findByText(/TL;DR/);
    const button = screen.getByRole("button", { name: "Re-transcribe" });
    expect(button).toHaveAttribute("aria-disabled", "true");
    expect(button).toHaveTextContent(/^Re-transcribe$/);
    expect(button).toHaveAccessibleDescription("Recording not available");
  });
});

describe("useLabelsFit", () => {
  function Row({ buttons }: { buttons: string }) {
    const row = useRef<HTMLDivElement | null>(null);
    const fit = useLabelsFit(row, buttons);
    return (
      <div>
        <div ref={row} data-testid="row">
          {fit ? "labelled" : "icons"}
        </div>
      </div>
    );
  }

  it("measures the labelled row again when the buttons change, even while icons show", () => {
    const needs: Record<string, number> = { "with copy": 900, "no copy": 550 };
    let key = "with copy";
    headerFit.measure = (row) => ({
      need: row.textContent === "labelled" ? needs[key] : 10,
      available: 600,
    });
    const { rerender } = render(<Row buttons={key} />);
    expect(screen.getByTestId("row")).toHaveTextContent("icons");
    key = "no copy";
    rerender(<Row buttons={key} />);
    expect(screen.getByTestId("row")).toHaveTextContent("labelled");
    key = "with copy";
    rerender(<Row buttons={key} />);
    expect(screen.getByTestId("row")).toHaveTextContent("icons");
  });

  it("measures the labelled row once more when the fonts have loaded", async () => {
    let loaded: () => void = () => {};
    const ready = new Promise<void>((resolve) => {
      loaded = resolve;
    });
    Object.defineProperty(document, "fonts", { value: { ready }, configurable: true });
    try {
      // The fallback font is narrow enough; the loaded one is not.
      let need = 500;
      headerFit.measure = (row) => ({ need: row.textContent === "labelled" ? need : 10, available: 600 });
      render(<Row buttons="same" />);
      expect(screen.getByTestId("row")).toHaveTextContent("labelled");
      need = 700;
      await act(async () => {
        loaded();
        await ready;
      });
      expect(screen.getByTestId("row")).toHaveTextContent("icons");
    } finally {
      delete (document as { fonts?: unknown }).fonts;
    }
  });
});

describe("labelsFit", () => {
  it("fits up to and including the space available", () => {
    expect(labelsFit({ need: 500, available: 500 })).toBe(true);
    expect(labelsFit({ need: 501, available: 500 })).toBe(false);
  });
});
