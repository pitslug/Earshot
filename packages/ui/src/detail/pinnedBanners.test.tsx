// Every banner on the meeting page stays pinned at the top with the header (T14.58).

import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { createMockIpc, sampleDetail } from "../ipc";
import { IpcProvider } from "../ipc/context";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { ITEM, OTHER } from "../test/meetingDetail";
import { type JobView, MeetingDetailView, PANEL_MIN_HEIGHT, panelHeight } from "./MeetingDetailView";

/** The pinned area: the element that holds the header's buttons and the tabs. */
function pinned(): HTMLElement {
  const tabs = screen.getByRole("tablist");
  let el: HTMLElement | null = tabs;
  while (el !== null && !el.contains(screen.getByRole("heading", { level: 1 }))) el = el.parentElement;
  if (el === null) throw new Error("no pinned header");
  return el;
}

function show(job: JobView | undefined, details = {}) {
  const ipc = createMockIpc({ timer: createManualTimer(10_000), meetings: [ITEM, OTHER], details });
  return render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <MeetingDetailView
          item={ITEM}
          onChanged={() => {}}
          onJobStarted={() => {}}
          onJobDismissed={() => {}}
          onJobCancel={() => {}}
          finishing={false}
          job={job}
        />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
}

describe("panelHeight", () => {
  it("gives the Speakers panel the space below the pinned header", () => {
    expect(panelHeight(800, 200)).toBe(600);
  });

  it("never gives it less than the floor, however much is pinned", () => {
    expect(panelHeight(450, 300)).toBe(PANEL_MIN_HEIGHT);
    expect(panelHeight(450, 450)).toBe(PANEL_MIN_HEIGHT);
    expect(panelHeight(450, 900)).toBe(PANEL_MIN_HEIGHT);
    expect(panelHeight(450, 450 - PANEL_MIN_HEIGHT)).toBe(PANEL_MIN_HEIGHT);
  });
});

describe("the meeting page's banners", () => {
  it("keeps the job bar inside the pinned header", async () => {
    show({ job: "Retranscribe", last: null, seq: 1 });
    await screen.findByText(/TL;DR/);
    const cancel = screen.getByRole("button", { name: "Cancel the re-transcription" });
    expect(pinned()).toContainElement(cancel);
    // After the tabs: the banners follow the buttons, the tabs and the find bar.
    const order = screen.getByRole("tablist").compareDocumentPosition(cancel);
    expect(order & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("pins the Summary tab's banners only while that tab shows", async () => {
    show(undefined, { [ITEM.id]: { ...sampleDetail(ITEM), summary_stale: true } });
    await screen.findByText(/TL;DR/);
    const stale = screen.getByText(/refers to lines of an earlier transcript/);
    expect(pinned()).toContainElement(stale);
    // The summary itself still scrolls below the pinned area.
    expect(pinned()).not.toContainElement(screen.getByText(/TL;DR/));
    await userEvent.setup().click(screen.getByRole("tab", { name: "Transcript" }));
    expect(screen.queryByText(/refers to lines of an earlier transcript/)).toBeNull();
    expect(within(pinned()).queryAllByRole("button", { name: "Re-summarise" })).toHaveLength(1);
  });
});
