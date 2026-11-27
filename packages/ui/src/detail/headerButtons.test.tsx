// The meeting header (the owner, 2026-11-28, T14.66): the title and caption take the full width,
// the buttons sit on their own row below them and above the tabs, and every button shows only
// its icon, with its accessible name and tooltip. jsdom does no layout, so the order and the
// containment are what is checked; the look is checked in the preview.

import { render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ORDER_COPY, createMockIpc, fillCopy, sampleDetail } from "../ipc";
import { IpcProvider } from "../ipc/context";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { MeetingDetailView } from "./MeetingDetailView";
import { renderApp } from "../test/app";
import { ITEM, OTHER, setup } from "../test/meetingDetail";
import { createManualTimer } from "../time";

const LONG_TITLE =
  "Quarterly planning review for the northern warehouse fit-out and supplier onboarding";

const row = () => screen.getByRole("group", { name: "Meeting actions" });
const speakers = () => screen.getByRole("button", { name: /^Speakers/ });
const exportButton = () => screen.getByRole("button", { name: "Export to Obsidian" });
const copyButton = () => screen.getByRole("button", { name: "Copy summary" });
const retranscribe = () => screen.getByRole("button", { name: "Re-transcribe" });

/** Whether `a` comes before `b` in the page. */
const before = (a: Element, b: Element) =>
  (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;

describe("the meeting header", () => {
  it("is the title, then the caption, then the button row, then the tabs", async () => {
    await setup();
    await screen.findByText(/TL;DR/);
    const title = screen.getByRole("heading", { level: 1 });
    const caption = screen.getByText(/Final transcript/);
    const tabs = screen.getByRole("tablist");
    expect(before(title, caption)).toBe(true);
    expect(before(caption, row())).toBe(true);
    expect(before(row(), tabs)).toBe(true);
  });

  it("shows every button as an icon with its accessible name", async () => {
    const { user } = await setup();
    await screen.findByText(/TL;DR/);
    const buttons = within(row());
    // Speakers: its count as a badge, its name saying what it counts.
    expect(speakers()).toHaveAccessibleName("Speakers, 2 need names");
    expect(speakers()).toHaveTextContent(/^2$/);
    expect(exportButton().textContent).toBe("");
    // Copy keeps its live text for a screen reader, hidden from view.
    expect(copyButton().querySelector("[aria-live]")).toHaveAttribute("data-sr-only");
    expect(retranscribe().textContent).toBe("");
    const [resummarise] = buttons.getAllByRole("button", { name: "Re-summarise" });
    expect(resummarise.textContent).toBe("");
    // The split menu stays.
    expect(buttons.getByRole("button", { name: "Re-summarise: choose who writes it" })).toBeInTheDocument();
    // Every button in the row has an icon.
    for (const b of buttons.getAllByRole("button")) {
      expect(b.querySelector("svg")).not.toBeNull();
    }
    // Every button's label shows on hover: its own tooltip leads with its name.
    for (const b of buttons.getAllByRole("button")) {
      const name = b.getAttribute("aria-label") ?? b.textContent ?? "";
      await user.hover(b);
      const tip = await screen.findByRole("tooltip");
      expect(name.startsWith(tip.textContent ?? "-") || (tip.textContent ?? "").startsWith(name)).toBe(true);
      await user.unhover(b);
      await waitFor(() => expect(screen.queryByRole("tooltip")).toBeNull());
    }
  });

  it("keeps the title out of the button row, so it wraps across the full width", async () => {
    const long = { ...ITEM, title: LONG_TITLE };
    renderApp(createMockIpc({ timer: createManualTimer(10_000), meetings: [long, OTHER] }), {
      timer: createManualTimer(10_000),
    });
    const title = await screen.findByRole("heading", { level: 1, name: LONG_TITLE });
    await screen.findByRole("group", { name: "Meeting actions" });
    expect(title).toHaveTextContent(LONG_TITLE);
    // Neither holds the other: the buttons never share the title's line.
    expect(row().contains(title)).toBe(false);
    expect(title.parentElement?.contains(row())).toBe(false);
  });

  it("says why Re-transcribe is unavailable once the recording is gone", async () => {
    await setup({ details: { [ITEM.id]: { ...sampleDetail(ITEM), audio_kept: false } } });
    await screen.findByText(/TL;DR/);
    expect(retranscribe()).toHaveAttribute("aria-disabled", "true");
    expect(retranscribe()).toHaveAccessibleDescription("Recording not available");
  });
});

describe("the header while the meeting finishes", () => {
  it("says on each greyed button what it is and when it comes back", async () => {
    const timer = createManualTimer(10_000);
    const ipc = createMockIpc({ timer, meetings: [ITEM, OTHER] });
    render(
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <MeetingDetailView
            item={ITEM}
            timer={timer}
            onChanged={() => {}}
            onJobStarted={() => {}}
            onJobDismissed={() => {}}
            onJobCancel={() => {}}
            finishing
          />
        </IpcProvider>
      </EarshotThemeProvider>,
    );
    await screen.findByText(/TL;DR/);
    for (const label of ["Re-transcribe", "Re-summarise"]) {
      const b = within(row()).getByRole("button", { name: label });
      expect(b).toHaveAttribute("aria-disabled", "true");
      expect(b).toHaveAccessibleDescription(fillCopy(ORDER_COPY.finishing_tip, { label }));
    }
  });
});
