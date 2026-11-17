// The window follows what sync brings, and a summary written on another computer has one banner
// (T12.81): a sync event reads the list and the open meeting again; the waiting banner names the
// computer, says what happens if it does not answer, and goes when the summary arrives.

import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import {
  type Ipc,
  type MeetingDetail,
  type MockIpcData,
  MOCK_OFFLOAD_ANSWER_MS,
  OFFLOAD_COPY,
  POST_STEPS,
  POST_STEP_MS,
  type SyncEvent,
  createMockIpc,
} from "../ipc";
import { renderApp } from "../test/app";
import { ITEM, OTHER } from "../test/meetingDetail";
import { createManualTimer } from "../time";
import { WAITING_TITLE } from "./MeetingDetailView";

const WAITING = `Waiting for Studio to write the summary. ${OFFLOAD_COPY.if_no_answer.writes_here}`;

/** The detail with `ITEM` waiting for another computer, and a hand on the sync events: until
 * `arrive()` the meeting reads as waiting; after it, as the summary arriving by sync does. */
async function waitingSetup(data: MockIpcData = {}) {
  const timer = createManualTimer(10_000);
  const base = createMockIpc({ timer, meetings: [ITEM, OTHER], waitingFor: { [ITEM.id]: WAITING }, ...data });
  let arrived = false;
  const reads: string[] = [];
  let lists = 0;
  let handler: ((e: SyncEvent) => void) | null = null;
  const ipc: Ipc = {
    ...base,
    onSyncEvent: async (h) => {
      handler = h;
      return () => {
        handler = null;
      };
    },
    listMeetings: async () => {
      lists += 1;
      return base.listMeetings();
    },
    getMeeting: async (id) => {
      reads.push(id);
      const d: MeetingDetail = await base.getMeeting(id);
      return arrived && id === ITEM.id ? { ...d, waiting_for: null } : d;
    },
  };
  const user = userEvent.setup();
  renderApp(ipc, { timer });
  await screen.findByRole("heading", { level: 1, name: ITEM.title });
  await screen.findByText(WAITING);
  return {
    user,
    reads: () => reads.filter((r) => r === ITEM.id).length,
    lists: () => lists,
    arrive: () => {
      arrived = true;
    },
    sync: async (e: SyncEvent) => {
      await act(async () => {
        handler?.(e);
      });
    },
  };
}

const waitingBanners = () => screen.queryAllByText(WAITING_TITLE);

describe("the window follows sync", () => {
  it("reads the open meeting again when a sync tick names it, and the waiting banner goes", async () => {
    const s = await waitingSetup();
    expect(waitingBanners()).toHaveLength(1);
    const readsBefore = s.reads();
    const listsBefore = s.lists();
    // Another meeting changed: the list is read again, the open page is not.
    await s.sync({ meetings: [OTHER.id] });
    expect(s.lists()).toBeGreaterThan(listsBefore);
    expect(s.reads()).toBe(readsBefore);
    expect(screen.getByText(WAITING)).toBeInTheDocument();
    // The summary arrived by sync: the page reads it again and the banner goes.
    s.arrive();
    await s.sync({ meetings: [ITEM.id] });
    expect(s.reads()).toBeGreaterThan(readsBefore);
    expect(screen.queryByText(WAITING)).toBeNull();
    expect(waitingBanners()).toHaveLength(0);
  });

  it("reads the list again for a change outside any meeting", async () => {
    const s = await waitingSetup();
    const before = s.lists();
    await s.sync({ meetings: [] });
    expect(s.lists()).toBeGreaterThan(before);
  });
});

describe("one banner for a summary written on another computer", () => {
  it("shows the waiting banner on either tab, and no kept-on-this-computer note beside it", async () => {
    const s = await waitingSetup({
      keptLocal: [ITEM.id],
      summaries: { settings: { writer: "ClaudeCode" } },
    });
    expect(waitingBanners()).toHaveLength(1);
    expect(screen.queryByText("Kept on this computer")).toBeNull();
    await s.user.click(screen.getByRole("tab", { name: "Transcript" }));
    expect(waitingBanners()).toHaveLength(1);
    expect(screen.getByText(WAITING)).toBeInTheDocument();
    await s.user.click(screen.getByRole("tab", { name: "Summary" }));
    // Settled: the note comes back, with what the user can do about it.
    s.arrive();
    await s.sync({ meetings: [ITEM.id] });
    expect(waitingBanners()).toHaveLength(0);
    expect(await screen.findByText("Kept on this computer")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Let Claude Code summarise it" })).toBeInTheDocument();
  });

  it("asking a computer by hand shows the page's banner alone, naming it", async () => {
    const timer = createManualTimer(10_000);
    const ipc = createMockIpc({
      timer,
      meetings: [ITEM, OTHER],
      sync: { setUp: true, lastSuccessUnixMs: 9_000 },
    });
    const user = userEvent.setup();
    renderApp(ipc, { timer });
    await screen.findByRole("heading", { level: 1, name: ITEM.title });
    await user.click(screen.getByRole("button", { name: /: choose who writes it$/ }));
    const studio = await screen.findByRole("menuitem", { name: /^On Studio/ });
    await user.click(studio);
    const banner = (await screen.findByText(WAITING_TITLE)).closest("[role=status]") as HTMLElement;
    expect(banner).toHaveTextContent(
      `Waiting for Studio to write the summary. ${OFFLOAD_COPY.if_no_answer.nothing}`,
    );
    expect(waitingBanners()).toHaveLength(1);
    // No job or notice says it a second time.
    expect(screen.queryAllByText(/Waiting for Studio/)).toHaveLength(1);
  });

  it("a meeting handed to another computer shows one banner until its summary arrives by sync", async () => {
    const timer = createManualTimer(new Date(2026, 10, 2, 10, 0).getTime());
    const ipc = createMockIpc({
      meetings: [],
      timer,
      summary: { kind: "Offloaded", message: "Waiting for Studio to write the summary." },
    });
    const user = userEvent.setup();
    renderApp(ipc, { timer });
    const nav = await screen.findByRole("navigation");
    await user.click(within(nav).getByRole("button", { name: "New meeting" }));
    await user.type(screen.getByRole("textbox", { name: /Title/ }), "Pump order");
    await user.click(screen.getByRole("button", { name: "Start" }));
    await act(async () => {
      timer.advance(0);
    });
    const view = await screen.findByRole("region", { name: "Meeting" });
    await user.click(within(view).getByRole("button", { name: "Stop meeting" }));
    act(() => timer.advance((POST_STEPS.length + 1) * POST_STEP_MS));
    const text = `Waiting for Studio to write the summary. ${OFFLOAD_COPY.if_no_answer.writes_here}`;
    expect(await screen.findByText(text)).toBeInTheDocument();
    expect(waitingBanners()).toHaveLength(1);
    // The meeting's status adds nothing of its own about it.
    expect(screen.queryByRole("region", { name: "Meeting status" })).toBeNull();
    // The other computer's summary arrives by sync: the banner goes and the summary shows.
    await act(async () => {
      timer.advance(MOCK_OFFLOAD_ANSWER_MS);
    });
    await screen.findByRole("heading", { level: 1, name: "Pump order" });
    expect(screen.queryByText(text)).toBeNull();
    expect(waitingBanners()).toHaveLength(0);
    await user.click(screen.getByRole("tab", { name: "Summary" }));
    expect(await screen.findByText(/TL;DR/)).toBeInTheDocument();
  });
});
