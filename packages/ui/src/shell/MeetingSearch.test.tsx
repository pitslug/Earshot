import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  type Ipc,
  IpcFailure,
  IpcProvider,
  type MeetingEvent,
  type MeetingListItem,
  type SearchResultsView,
  createMockIpc,
} from "../ipc";
import { sampleDetail } from "../ipc/mockDetail";
import { SEARCH_MAX_RESULTS, searchSample } from "../ipc/mockSearch";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "./AppShell";
import { SEARCH_DEBOUNCE_MS, resultLine } from "./MeetingSearch";

const at = (day: number, hour: number) => new Date(2026, 10, day, hour, 0).getTime();
const NOW = at(2, 16);

const row = (id: string, title: string, started: number): MeetingListItem => ({
  id,
  title,
  started_at_ms: started,
  duration_ms: 30 * 60_000,
  state: "Ready",
  folder_id: null,
});

const MEETINGS: MeetingListItem[] = [
  row("m3", "Orchard walk", at(2, 14)),
  row("m2", "Budget review", at(2, 9)),
  row("m1", "Orchard planning", at(1, 11)),
];

function setup(ipc: Ipc = createMockIpc({ meetings: MEETINGS })) {
  const timer = createManualTimer(NOW);
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell now={() => NOW} timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  const settle = () => act(async () => timer.advance(SEARCH_DEBOUNCE_MS));
  return { timer, settle, user: userEvent.setup() };
}

const box = () => screen.findByRole("searchbox", { name: "Search meetings" });
const results = () => screen.queryByRole("list", { name: "Search results" });
/** Each result's first line, its title. */
const resultTitles = () =>
  within(screen.getByRole("list", { name: "Search results" }))
    .getAllByRole("listitem")
    .map((item) => item.querySelector("button span span")?.textContent);

beforeEach(() => {
  installMatchMedia(false);
});

describe("meeting search", () => {
  it("search finds meetings after typing pauses, and only then", async () => {
    const base = createMockIpc({ meetings: MEETINGS });
    const searchMeetings = vi.fn(base.searchMeetings);
    const { settle, user, timer } = setup({ ...base, searchMeetings });
    await user.type(await box(), "orch");
    expect(searchMeetings).not.toHaveBeenCalled();

    act(() => timer.advance(SEARCH_DEBOUNCE_MS - 1));
    expect(searchMeetings).not.toHaveBeenCalled();
    await user.type(await box(), "ard");
    act(() => timer.advance(SEARCH_DEBOUNCE_MS - 1));
    expect(searchMeetings).not.toHaveBeenCalled();
    await settle();
    expect(searchMeetings).toHaveBeenCalledTimes(1);
    expect(searchMeetings).toHaveBeenCalledWith("orchard");

    expect(resultTitles()).toEqual(["Orchard walk", "Orchard planning"]);
    expect(screen.queryByRole("list", { name: "Meetings" })).toBeNull();
    // New meeting and Settings stay where they are.
    expect(screen.getByRole("button", { name: "New meeting" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Settings" })).toBeInTheDocument();
  });

  it("search opens the picked meeting and keeps the results", async () => {
    const { settle, user } = setup();
    await user.type(await box(), "planning");
    await settle();
    await user.click(screen.getByRole("button", { name: /^Orchard planning/ }));
    const main = screen.getByRole("main");
    await waitFor(() =>
      expect(within(main).getByRole("heading", { level: 1 }).textContent).toBe("Orchard planning"),
    );
    expect(screen.getByRole("button", { name: /^Orchard planning/ })).toHaveAttribute(
      "aria-current",
      "true",
    );
    expect(results()).not.toBeNull();
  });

  it("search shows where a transcript matched", async () => {
    const { settle, user } = setup();
    await user.type(await box(), "LEAD TIME");
    await settle();
    const list = screen.getByRole("list", { name: "Search results" });
    const first = within(list).getAllByRole("button")[0];
    // 724 000 ms into the sample meeting.
    expect(first.textContent).toContain("Transcript 12:04");
    expect(first.textContent).toContain("lead time");
  });

  it("search clearing brings the same list back, by the clear button and by Esc", async () => {
    const { settle, user } = setup();
    const list = await screen.findByRole("list", { name: "Meetings" });
    const before = list.textContent;
    const input = await box();

    await user.type(input, "orchard");
    await settle();
    expect(results()).not.toBeNull();
    await user.keyboard("{Escape}");
    expect(input).toHaveValue("");
    expect(results()).toBeNull();
    expect(screen.getByRole("list", { name: "Meetings" }).textContent).toBe(before);

    await user.type(input, "orchard");
    await settle();
    await user.click(screen.getByRole("button", { name: "Clear search" }));
    expect(input).toHaveValue("");
    expect(results()).toBeNull();
    expect(screen.getByRole("list", { name: "Meetings" }).textContent).toBe(before);
  });

  it("search of only white space keeps the list and asks nothing", async () => {
    const base = createMockIpc({ meetings: MEETINGS });
    const searchMeetings = vi.fn(base.searchMeetings);
    const { settle, user } = setup({ ...base, searchMeetings });
    await user.type(await box(), "   ");
    await settle();
    expect(searchMeetings).not.toHaveBeenCalled();
    expect(screen.getByRole("list", { name: "Meetings" })).toBeInTheDocument();
  });

  it("search with no match says so quietly", async () => {
    const { settle, user } = setup();
    await user.type(await box(), "quinceharvest");
    await settle();
    expect(within(screen.getByRole("status")).getByText("No meetings found")).toBeInTheDocument();
    expect(results()).toBeNull();
    expect(screen.queryByRole("list", { name: "Meetings" })).toBeNull();
    expect(document.body.textContent).not.toMatch(/—|please|!/i);
  });

  it("search treats wildcards as text", async () => {
    const { settle, user } = setup(
      createMockIpc({ meetings: [row("a", "Up 50% today", at(2, 9)), row("b", "Up 50 today", at(2, 8))] }),
    );
    await user.type(await box(), "%");
    await settle();
    expect(resultTitles()).toEqual(["Up 50% today"]);
  });

  it("search ignores a stale answer that arrives after a newer one", async () => {
    const base = createMockIpc({ meetings: MEETINGS });
    const answers = new Map<string, (r: SearchResultsView) => void>();
    const searchMeetings = (query: string) =>
      new Promise<SearchResultsView>((resolve) => answers.set(query, resolve));
    const { settle, user } = setup({ ...base, searchMeetings });
    const input = await box();
    await user.type(input, "first");
    await settle();
    await user.clear(input);
    await user.type(input, "second");
    await settle();
    const hit = (title: string): SearchResultsView => ({
      items: [
        {
          id: "m1",
          title,
          started_at_ms: at(1, 11),
          matched: "Title",
          snippet: title,
          line_ms: null,
        },
      ],
      capped: false,
    });
    await act(async () => answers.get("second")?.(hit("Second answer")));
    await act(async () => answers.get("first")?.(hit("First answer")));
    expect(resultTitles()).toEqual(["Second answer"]);
  });

  it("search does not let an old no match stand for a newer query", async () => {
    const { settle, user } = setup();
    const input = await box();
    await user.type(input, "quinceharvest");
    await settle();
    expect(screen.getByText("No meetings found")).toBeInTheDocument();
    await user.clear(input);
    await user.type(input, "orchard");
    expect(screen.queryByText("No meetings found")).toBeNull();
    await settle();
    expect(resultTitles()).toEqual(["Orchard walk", "Orchard planning"]);
  });

  it("search says when it stopped at the cap", async () => {
    const many = Array.from({ length: SEARCH_MAX_RESULTS + 3 }, (_, i) =>
      row(`n${i}`, `Orchard ${i}`, at(1, 1) + i * 60_000),
    );
    const { settle, user } = setup(createMockIpc({ meetings: many }));
    await user.type(await box(), "orchard");
    await settle();
    expect(resultTitles()).toHaveLength(SEARCH_MAX_RESULTS);
    expect(
      screen.getByText(`Showing the newest ${SEARCH_MAX_RESULTS} matches. Add a word to narrow them.`),
    ).toBeInTheDocument();
  });

  it("search reports a failure in fixed copy without the error's own text", async () => {
    const base = createMockIpc({ meetings: MEETINGS });
    const { settle, user } = setup({
      ...base,
      searchMeetings: () =>
        Promise.reject(new IpcFailure({ kind: "store", message: "store: disk full", retryable: false })),
    });
    await user.type(await box(), "orchard");
    await settle();
    expect(screen.getByRole("status").textContent).toBe("The search did not finish. Try again.");
    expect(screen.queryByText(/disk full/)).toBeNull();
  });

  it("search reads the list again when a result's meeting is gone from it, and drops the result", async () => {
    const base = createMockIpc({ meetings: MEETINGS });
    const ghost: SearchResultsView = {
      items: [
        { id: "gone", title: "Ghost", started_at_ms: at(1, 9), matched: "Title", snippet: "Ghost", line_ms: null },
      ],
      capped: false,
    };
    const answers = [ghost, { items: [], capped: false }];
    const listMeetings = vi.fn(base.listMeetings);
    const { settle, user } = setup({
      ...base,
      listMeetings,
      searchMeetings: async () => answers.shift() ?? { items: [], capped: false },
    });
    await user.type(await box(), "ghost");
    await settle();
    const lists = listMeetings.mock.calls.length;
    await user.click(screen.getByRole("button", { name: /^Ghost/ }));
    await waitFor(() => expect(listMeetings.mock.calls.length).toBe(lists + 1));
    await settle();
    expect(screen.getByText("No meetings found")).toBeInTheDocument();
    // What was open stays open.
    expect(within(screen.getByRole("main")).getByRole("heading", { level: 1 }).textContent).toBe(
      "Orchard walk",
    );
  });

  it("search does not crash on picking a meeting deleted since the list was read", async () => {
    const base = createMockIpc({ meetings: MEETINGS });
    const { settle, user } = setup(base);
    await user.type(await box(), "orchard");
    await settle();
    await base.deleteMeeting("m1");
    await user.click(screen.getByRole("button", { name: /^Orchard planning/ }));
    await waitFor(() => expect(screen.getByRole("main").textContent).not.toBe(""));
    expect(screen.getByRole("searchbox", { name: "Search meetings" })).toHaveValue("orchard");
    expect(results()).not.toBeNull();
  });

  it("search answers again when a meeting changes while results show", async () => {
    const base = createMockIpc({ meetings: MEETINGS });
    let shout: ((e: MeetingEvent) => void) | null = null;
    const ipc: Ipc = {
      ...base,
      onMeetingEvent: async (handler) => {
        shout = handler;
        return () => {};
      },
    };
    const { settle, user } = setup(ipc);
    await user.type(await box(), "orchard");
    await settle();
    await base.deleteMeeting("m3");
    expect(resultTitles()).toEqual(["Orchard walk", "Orchard planning"]);
    await act(async () => shout?.({ type: "Ended", meeting: "other", reason: "Stopped" }));
    await settle();
    expect(resultTitles()).toEqual(["Orchard planning"]);
  });

  it("search has no box on a computer with no meetings yet", async () => {
    setup(createMockIpc({ meetings: [] }));
    await screen.findByRole("heading", { name: "Record your first meeting" });
    expect(screen.queryByRole("searchbox", { name: "Search meetings" })).toBeNull();
  });

  it("search result lines say the start and where it matched", () => {
    const base = { id: "a", title: "A", started_at_ms: at(2, 14), snippet: "s", line_ms: null };
    expect(resultLine({ ...base, matched: "Title" })).toMatch(/14:00$/);
    expect(resultLine({ ...base, matched: "Summary" })).toMatch(/14:00 · Summary$/);
    expect(resultLine({ ...base, matched: "Transcript", line_ms: 65_000 })).toMatch(
      /14:00 · Transcript 1:05$/,
    );
  });
});

describe("mock search", () => {
  it("search of the sample finds title, then transcript, then summary, newest first", () => {
    const rows = [row("old", "Quiet", at(1, 9)), row("new", "Pump order", at(2, 9))];
    const found = searchSample(rows, sampleDetail, " PUMP ");
    expect(found.capped).toBe(false);
    expect(found.items.map((i) => i.id)).toEqual(["new", "old"]);
    expect(found.items[0].matched).toBe("Title");
    const line = searchSample(rows, sampleDetail, "air freight").items[0];
    expect(line.matched).toBe("Transcript");
    expect(line.line_ms).toBe(739_000);
    expect(line.snippet).toContain("air freight");
    expect(searchSample(rows, sampleDetail, "holds commissioning").items).toEqual([]);
    expect(searchSample(rows, sampleDetail, "to hold commissioning").items[0].matched).toBe("Summary");
  });

  it("search of the sample treats an empty query as nothing and never splits a character", () => {
    expect(searchSample(MEETINGS, sampleDetail, "  ")).toEqual({ items: [], capped: false });
    const long = `${"🦉".repeat(40)}needle${"🦉".repeat(80)}`;
    const found = searchSample([row("a", long, 1)], sampleDetail, "needle").items[0];
    expect(found.snippet).toBe(`...${"🦉".repeat(30)}needle${"🦉".repeat(70)}...`);
  });
});
