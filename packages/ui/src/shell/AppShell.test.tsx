import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { DATA_FOLDER_COPY, type Ipc, IpcFailure, type MeetingListItem, POST_STEP_MS, createMockIpc } from "../ipc";
import { renderApp } from "../test/app";
import { ITEM, listRow, setup } from "../test/meetingDetail";
import { setWindowWatched } from "../test/windowWatched";
import { createManualTimer } from "../time";
import { COLLAPSED_GROUPS_KEY } from "./collapsedGroups";

const at = (day: number, hour: number, minute = 0) =>
  new Date(2026, 10, day, hour, minute).getTime();
const NOW = at(2, 16, 0);

const MEETINGS: MeetingListItem[] = [
  { id: "m4", title: "Design review", started_at_ms: at(2, 14, 0), duration_ms: 34 * 60_000, state: "Ready", folder_id: null, new_summary: false },
  { id: "m3", title: "", started_at_ms: at(2, 9, 30), duration_ms: 47 * 60_000, state: "Ready", folder_id: null, new_summary: false },
  { id: "m2", title: "Supplier call", started_at_ms: at(1, 11, 5), duration_ms: 22 * 60_000, state: "Ready", folder_id: null, new_summary: false },
  { id: "m1", title: "Quarterly planning", started_at_ms: new Date(2026, 9, 27, 10, 0).getTime(), duration_ms: 95 * 60_000, state: "Ready", folder_id: null, new_summary: false },
];

function renderShell(ipc: Ipc) {
  return renderApp(ipc, { now: () => NOW });
}

describe("AppShell", () => {
  it("lists meetings grouped by day with their date, time and length", async () => {
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const list = await screen.findByRole("list", { name: "Meetings" });
    const headings = within(list).getAllByRole("heading").map((h) => h.textContent);
    // 2 Nov 2026 is a Monday: the meeting of Tuesday 27 October was last week (T12.103).
    expect(headings).toEqual(["Today", "Yesterday", "Last week"]);
    const today = within(list).getByRole("group", { name: "Today" });
    // Every row button has its trash can beside it.
    const rowsOnly = { name: /^(?!Delete )/ };
    const rows = within(today).getAllByRole("button", rowsOnly);
    expect(rows.map((r) => r.textContent)).toEqual([
      "Design review2 Nov 2026 14:00 · 34 min",
      "Untitled meeting2 Nov 2026 09:30 · 47 min",
    ]);
    const yesterday = within(list).getByRole("group", { name: "Yesterday" });
    expect(within(yesterday).getByRole("button", rowsOnly).textContent).toBe("Supplier call1 Nov 2026 11:05 · 22 min");
    const earlier = within(list).getByRole("group", { name: "Last week" });
    expect(within(earlier).getByRole("button", rowsOnly).textContent).toContain("Quarterly planning");
    expect(screen.getByRole("button", { name: "New meeting" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Settings" })).toBeInTheDocument();
  });

  it("closes and opens a group from its heading, greying its count while closed (T12.103)", async () => {
    const user = userEvent.setup();
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const list = await screen.findByRole("list", { name: "Meetings" });
    const toggle = within(list).getByRole("button", { name: "Yesterday" });
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(list).getByRole("button", { name: /^Supplier call/ })).toBeInTheDocument();
    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(within(list).queryByRole("button", { name: /^Supplier call/ })).toBeNull();
    // The count shows while closed, outside the group's name.
    expect(within(toggle).getByText("(1)")).toBeInTheDocument();
    expect(within(list).getByRole("group", { name: "Yesterday" })).toBeInTheDocument();
    // The other groups stay as they were.
    expect(within(list).getByRole("button", { name: /^Design review/ })).toBeInTheDocument();
    // Remembered on this computer by the group's key, never a title.
    expect(JSON.parse(window.localStorage.getItem(COLLAPSED_GROUPS_KEY) ?? "[]")).toEqual(["yesterday"]);
    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(within(toggle).queryByText("(1)")).toBeNull();
    expect(within(list).getByRole("button", { name: /^Supplier call/ })).toBeInTheDocument();
    expect(JSON.parse(window.localStorage.getItem(COLLAPSED_GROUPS_KEY) ?? "[]")).toEqual([]);
  });

  it("keeps a closed group closed when the window opens again (T12.103)", async () => {
    window.localStorage.setItem(COLLAPSED_GROUPS_KEY, JSON.stringify(["today", "last-week"]));
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const list = await screen.findByRole("list", { name: "Meetings" });
    // The newest meeting is shown on opening, and its closed group still stays closed.
    expect(within(list).getByRole("button", { name: "Today" })).toHaveAttribute("aria-expanded", "false");
    expect(within(list).getByRole("button", { name: "Last week" })).toHaveAttribute("aria-expanded", "false");
    expect(within(list).getByRole("button", { name: "Yesterday" })).toHaveAttribute("aria-expanded", "true");
    expect(within(within(list).getByRole("button", { name: "Today" })).getByText("(2)")).toBeInTheDocument();
  });

  it("reads damaged stored groups as all open (T12.103)", async () => {
    window.localStorage.setItem(COLLAPSED_GROUPS_KEY, "not json");
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const list = await screen.findByRole("list", { name: "Meetings" });
    for (const name of ["Today", "Yesterday", "Last week"]) {
      expect(within(list).getByRole("button", { name })).toHaveAttribute("aria-expanded", "true");
    }
  });

  it("opens a closed group when a new meeting lands in it (T12.103)", async () => {
    const user = userEvent.setup();
    const timer = createManualTimer(NOW);
    renderApp(createMockIpc({ meetings: MEETINGS, timer }), { timer });
    const list = await screen.findByRole("list", { name: "Meetings" });
    await user.click(within(list).getByRole("button", { name: "Today" }));
    expect(within(list).getByRole("button", { name: "Today" })).toHaveAttribute("aria-expanded", "false");
    const nav = screen.getByRole("navigation");
    await user.click(within(nav).getByRole("button", { name: "New meeting" }));
    await user.click(screen.getByRole("button", { name: "Start" }));
    await act(async () => timer.advance(0));
    await waitFor(() =>
      expect(within(screen.getByRole("list", { name: "Meetings" })).getByRole("button", { name: "Today" })).toHaveAttribute(
        "aria-expanded",
        "true",
      ),
    );
    expect(JSON.parse(window.localStorage.getItem(COLLAPSED_GROUPS_KEY) ?? "[]")).toEqual([]);
  });

  it("opens a closed group when the meeting shown moves into it (T12.103)", async () => {
    const user = userEvent.setup();
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const list = await screen.findByRole("list", { name: "Meetings" });
    await user.click(within(list).getByRole("button", { name: "Yesterday" }));
    expect(within(list).queryByRole("button", { name: /^Supplier call/ })).toBeNull();
    // A search result in the closed group is shown, and its group opens.
    await user.type(screen.getByRole("searchbox", { name: "Search meetings" }), "Supplier");
    await user.click(await screen.findByRole("button", { name: /Supplier call/ }));
    await user.clear(screen.getByRole("searchbox", { name: "Search meetings" }));
    const shown = await screen.findByRole("list", { name: "Meetings" });
    await waitFor(() => expect(within(shown).getByRole("button", { name: "Yesterday" })).toHaveAttribute("aria-expanded", "true"));
    expect(within(shown).getByRole("button", { name: /^Supplier call/ })).toHaveAttribute("aria-current", "true");
  });

  it("shows the newest meeting first and switches on selection", async () => {
    const user = userEvent.setup();
    renderShell(createMockIpc({ meetings: MEETINGS }));
    const main = await screen.findByRole("main");
    await waitFor(() =>
      expect(within(main).getByRole("heading", { level: 1 }).textContent).toBe("Design review"),
    );
    const first = screen.getByRole("button", { name: /^Design review/ });
    expect(first).toHaveAttribute("aria-current", "true");

    await user.click(screen.getByRole("button", { name: /^Supplier call/ }));
    expect(within(main).getByRole("heading", { level: 1 }).textContent).toBe("Supplier call");
    expect(screen.getByRole("button", { name: /^Supplier call/ })).toHaveAttribute("aria-current", "true");
    expect(first).not.toHaveAttribute("aria-current");

    await user.click(screen.getByRole("button", { name: /^Untitled meeting/ }));
    expect(within(main).getByRole("heading", { level: 1 }).textContent).toBe("Untitled meeting");
  });

  it("shows the empty state when there are no meetings", async () => {
    renderShell(createMockIpc({ meetings: [] }));
    const main = await screen.findByRole("main");
    expect(
      await within(main).findByRole("heading", { name: "Record your first meeting" }),
    ).toBeInTheDocument();
    expect(within(main).getByRole("button", { name: "New meeting" })).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Meetings" })).toBeNull();
    // Copy rules (docs/ui-design.md): no em dashes, no "please", no exclamation marks.
    const text = document.body.textContent ?? "";
    expect(text).not.toMatch(/\u2014|please|!/i);
  });

  it("reports a failed load", async () => {
    const failing: Ipc = {
      ...createMockIpc(),
      listMeetings: () =>
        Promise.reject(new IpcFailure({ kind: "store", message: "store: disk full", retryable: false })),
    };
    renderShell(failing);
    expect(await screen.findByText("Meetings could not be loaded")).toBeInTheDocument();
    expect(screen.getByText("store: disk full")).toBeInTheDocument();
  });

  // T9.23: a data folder that could not be opened has its own screen, whose Try again restarts.
  it("shows the data folder screen with Try again, which restarts Earshot", async () => {
    let restarts = 0;
    const ipc: Ipc = {
      ...createMockIpc({ dataFolderFails: true }),
      restartApp: async () => {
        restarts += 1;
      },
    };
    renderShell(ipc);
    expect(
      await screen.findByRole("heading", { level: 1, name: "Earshot could not open its data" }),
    ).toBeInTheDocument();
    expect(screen.getByText(DATA_FOLDER_COPY)).toBeInTheDocument();
    expect(screen.queryByText("Meetings could not be loaded")).toBeNull();
    await userEvent.setup().click(screen.getByRole("button", { name: "Try again" }));
    expect(restarts).toBe(1);
    expect(document.body.textContent ?? "").not.toMatch(/\u2014|please|!/i);
  });

  it("says why when Try again cannot restart", async () => {
    const ipc: Ipc = {
      ...createMockIpc({ dataFolderFails: true }),
      restartApp: () =>
        Promise.reject(new IpcFailure({ kind: "busy", message: "A download is running.", retryable: true })),
    };
    renderShell(ipc);
    await userEvent.setup().click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByText("A download is running.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Try again" })).toBeEnabled();
  });

  it("shows a notice when given one", async () => {
    renderApp(createMockIpc({ meetings: MEETINGS }), { now: () => NOW, notice: "Preview: sample data" });
    expect(await screen.findByText("Preview: sample data")).toBeInTheDocument();
  });
});

describe("AppShell status dots (T12.76)", () => {
  // Inside the meeting list only, as `listRow`: a whole-page role query inside `waitFor` starves
  // the render it waits for on a slow machine.
  const row = (title: RegExp) => {
    const list = document.querySelector<HTMLElement>('ul[aria-label="Meetings"]');
    if (list === null) throw new Error("the meeting list is not on the page");
    return within(list).getByRole("button", { name: title });
  };
  const dot = (el: HTMLElement) => el.querySelector("[data-dot]")?.getAttribute("data-dot") ?? null;
  const STATES: MeetingListItem[] = [
    { ...MEETINGS[0], id: "s5", title: "Read meeting" },
    { ...MEETINGS[0], id: "s4", title: "Finishing meeting", started_at_ms: at(2, 13, 0), state: "Processing" },
    { ...MEETINGS[0], id: "s3", title: "Queued meeting", started_at_ms: at(2, 12, 0), state: "Waiting" },
    { ...MEETINGS[0], id: "s2", title: "Unread meeting", started_at_ms: at(2, 11, 0), new_summary: true },
    {
      ...MEETINGS[0],
      id: "s1",
      title: "Recording meeting",
      started_at_ms: at(2, 10, 0),
      duration_ms: null,
      state: "Recording",
    },
  ];

  it("gives each state the recording dot's style in its own colour, with its word", async () => {
    renderShell(createMockIpc({ meetings: STATES }));
    await screen.findByRole("list", { name: "Meetings" });
    const cases: [RegExp, string | null, string][] = [
      [/^Read meeting/, null, "2 Nov 2026 14:00 · 34 min"],
      [/^Finishing meeting/, "working", "2 Nov 2026 13:00 · 34 min · Finishing..."],
      [/^Queued meeting/, "working", "2 Nov 2026 12:00 · 34 min · Waiting..."],
      [/^Unread meeting/, "newSummary", "2 Nov 2026 11:00 · 34 min · New summary"],
      [/^Recording meeting/, "recording", "2 Nov 2026 10:00 · Recording..."],
    ];
    for (const [title, want, line] of cases) {
      await waitFor(() => expect(dot(row(title))).toBe(want));
      // Never colour alone: the word is in the row's line, and the dot is hidden from readers.
      expect(row(title)).toHaveTextContent(line);
      expect(row(title).querySelector("[data-dot]")?.getAttribute("aria-hidden") ?? "true").toBe("true");
    }
    // No spinner any more: the dots are the one style.
    expect(within(screen.getByRole("list", { name: "Meetings" })).queryByRole("progressbar")).toBeNull();
  });

  it("clears the new summary mark once the meeting's summary is shown", async () => {
    const user = userEvent.setup();
    const ipc = createMockIpc({ meetings: STATES });
    const seen: [string, string][] = [];
    const spied: Ipc = {
      ...ipc,
      markSummarySeen: (meeting, summary) => {
        seen.push([meeting, summary]);
        return ipc.markSummarySeen(meeting, summary);
      },
    };
    renderShell(spied);
    await screen.findByRole("list", { name: "Meetings" });
    await waitFor(() => expect(dot(row(/^Unread meeting/))).toBe("newSummary"));
    await user.click(row(/^Unread meeting/));
    const shown = await spied.getMeeting("s2");
    await waitFor(() => expect(dot(row(/^Unread meeting/))).toBeNull());
    expect(row(/^Unread meeting/)).not.toHaveTextContent("New summary");
    expect(seen).toContainEqual(["s2", shown.summary?.id ?? ""]);
  });

  it("marks nothing read while the window is not watched, then marks it once it is", async () => {
    setWindowWatched(false, false);
    const user = userEvent.setup();
    const ipc = createMockIpc({ meetings: STATES });
    const seen: string[] = [];
    renderShell({
      ...ipc,
      markSummarySeen: (meeting, summary) => {
        seen.push(meeting);
        return ipc.markSummarySeen(meeting, summary);
      },
    });
    await screen.findByRole("list", { name: "Meetings" });
    await user.click(row(/^Unread meeting/));
    // The summary is on screen, but nobody is looking at the window.
    expect(await screen.findByText(/TL;DR/)).toBeInTheDocument();
    // The effect that marks a summary runs in the commit that shows it and calls
    // `markSummarySeen` at once, so after one flush a call would already be in `seen` (the last
    // step below shows that one flush is enough). No real time passes.
    await act(async () => {});
    expect(seen).toEqual([]);
    expect(dot(row(/^Unread meeting/))).toBe("newSummary");
    // Focused but hidden (minimised or in the tray) is still not watched.
    act(() => setWindowWatched(false, true, true));
    await act(async () => {});
    expect(seen).toEqual([]);
    // Brought up on that meeting: now it counts as read, after the same single flush.
    act(() => setWindowWatched(true, true, true));
    await act(async () => {});
    expect(seen).toEqual(["s2"]);
    await waitFor(() => expect(dot(row(/^Unread meeting/))).toBeNull());
  });

  it("shows a running re-summary of a read meeting as green Summarising..., and no dot at its end", async () => {
    const { user, timer } = await setup();
    const item = () => listRow(ITEM);
    // The shown meeting's summary is read here at once.
    await waitFor(() => expect(dot(item())).toBeNull());
    expect(ITEM.state).toBe("Ready");
    await user.click(screen.getAllByRole("button", { name: "Re-summarise" })[0]);
    await waitFor(() => expect(dot(item())).toBe("working"));
    expect(item()).toHaveTextContent(/· Summarising\.\.\.$/);
    // The job's terminal event: the new summary is shown on this page, so it is read too.
    act(() => timer.advance(POST_STEP_MS));
    expect(await screen.findByText(/New summary ready/)).toBeInTheDocument();
    await waitFor(() => expect(dot(item())).toBeNull());
    expect(item()).not.toHaveTextContent(/Summarising|New summary/);
  });
});
