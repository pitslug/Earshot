// What a commit costs (T9.24): the clock, the 10 Hz levels and a landed line re-render only what
// they change, and transcript rows that did not change are not rendered again. Rows are counted
// through the time formatters each row calls (`clock` for a live line, `rowTime` for a line of
// the meeting's page), wrapped below.
import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Profiler } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import * as detailModule from "../detail/detail";
import { type Ipc, IpcProvider, type MeetingEvent, type MeetingListItem, createMockIpc } from "../ipc";
import { sampleDetail } from "../ipc/mockDetail";
import * as liveModule from "../meeting/liveMeeting";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "./AppShell";

vi.mock("../meeting/liveMeeting", async (original) => {
  const real = await original<typeof import("../meeting/liveMeeting")>();
  return { ...real, clock: vi.fn(real.clock) };
});
vi.mock("../detail/detail", async (original) => {
  const real = await original<typeof import("../detail/detail")>();
  return { ...real, rowTime: vi.fn(real.rowTime) };
});

const clockSpy = vi.mocked(liveModule.clock);
const rowTimeSpy = vi.mocked(detailModule.rowTime);

const at = (day: number, hour: number, minute = 0, second = 0) =>
  new Date(2026, 10, day, hour, minute, second).getTime();
const NOW = at(2, 16);

const MEETINGS: MeetingListItem[] = [
  { id: "m2", title: "Hedge trimming plan", started_at_ms: at(2, 14), duration_ms: 30 * 60_000, state: "Ready", folder_id: null },
  { id: "m1", title: "Gravel order", started_at_ms: at(1, 11), duration_ms: 20 * 60_000, state: "Ready", folder_id: null },
];

/** The mock IPC with its meeting events sent by the test, and the app's commits counted. */
function setup(start = NOW, meetings = MEETINGS) {
  const timer = createManualTimer(start);
  const base = createMockIpc({ timer, now: start, meetings });
  const listeners = new Set<(e: MeetingEvent) => void>();
  const ipc: Ipc = {
    ...base,
    onMeetingEvent: async (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  const emit = (event: MeetingEvent) =>
    act(() => {
      for (const l of [...listeners]) l(event);
    });
  const commits = { n: 0 };
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <Profiler id="app" onRender={() => (commits.n += 1)}>
          <AppShell timer={timer} />
        </Profiler>
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  return { timer, emit, commits, user: userEvent.setup() };
}

/** When invented line `i` starts. */
const startOf = (i: number) => 60_000 + i * 6_000;

const landed = (i: number): MeetingEvent => ({
  type: "Landed",
  track: i % 2 === 0 ? "Them" : "You",
  start_ms: startOf(i),
  end_ms: startOf(i) + 4_000,
  text: `Invented line number ${i} about the gravel.`,
});

/** How often `clock` formatted a live line starting at `ms`. */
const clockCallsAt = (ms: number) => clockSpy.mock.calls.filter(([x]) => x === ms).length;

beforeEach(() => {
  installMatchMedia(false);
  clockSpy.mockClear();
  rowTimeSpy.mockClear();
});

describe("render cost", () => {
  it("render cost: an idle window commits nothing while time passes", async () => {
    const s = setup();
    await screen.findByRole("heading", { level: 1, name: "Hedge trimming plan" });
    await act(async () => s.timer.advance(1_000));
    s.commits.n = 0;
    // A second at a time, as the real clock moves (one `act` would batch the ticks into one).
    for (let i = 0; i < 10; i += 1) await act(async () => s.timer.advance(1_000));
    expect(s.commits.n).toBe(0);
  });

  it("render cost: the list's days still move on at midnight", async () => {
    const s = setup(at(2, 23, 59, 30));
    const list = await screen.findByRole("list", { name: "Meetings" });
    expect(within(list).getAllByRole("heading").map((h) => h.textContent)).toEqual(["Today", "Yesterday"]);
    await act(async () => s.timer.advance(2 * 60_000));
    expect(within(list).getAllByRole("heading").map((h) => h.textContent)).toEqual(["Yesterday", "Earlier"]);
  });

  it("render cost: the clock ticks only the elapsed time, and levels only the meters", async () => {
    const s = setup();
    await screen.findByRole("heading", { level: 1, name: "Hedge trimming plan" });
    s.emit({ type: "Started", meeting: "live", mode: "Call", tracks: ["You", "Them"] });
    const view = await screen.findByRole("region", { name: "Meeting" });
    for (let i = 0; i < 3; i += 1) s.emit(landed(i));
    expect(clockCallsAt(startOf(0))).toBeGreaterThan(0);

    clockSpy.mockClear();
    await act(async () => s.timer.advance(1_000));
    expect(within(view).getByLabelText("Elapsed").textContent).toBe("0:01");
    // The elapsed time only: no line was formatted again.
    for (let i = 0; i < 3; i += 1) expect(clockCallsAt(startOf(i))).toBe(0);

    clockSpy.mockClear();
    s.emit({ type: "Levels", you_dbfs: -12, them_dbfs: -45 });
    const meter = within(view).getByRole("progressbar", { name: "You level" });
    expect(Number(meter.getAttribute("aria-valuenow"))).toBeCloseTo(0.8);
    expect(
      Number(within(view).getByRole("progressbar", { name: "Them level" }).getAttribute("aria-valuenow")),
    ).toBeCloseTo(0.25);
    // Nothing else rendered: neither a line nor the elapsed time.
    expect(clockSpy).not.toHaveBeenCalled();

    // The recording's end empties the meters.
    s.emit({ type: "Ended", meeting: "live", reason: "Stopped" });
    s.emit({ type: "Started", meeting: "next", mode: "Call", tracks: ["You", "Them"] });
    const next = await screen.findByRole("region", { name: "Meeting" });
    expect(
      Number(within(next).getByRole("progressbar", { name: "You level" }).getAttribute("aria-valuenow")),
    ).toBe(0);
  });

  it("render cost: a landed line renders itself, not the lines before it", async () => {
    const s = setup();
    await screen.findByRole("heading", { level: 1, name: "Hedge trimming plan" });
    s.emit({ type: "Started", meeting: "live", mode: "Call", tracks: ["You", "Them"] });
    await screen.findByRole("region", { name: "Meeting" });
    for (let i = 0; i < 5; i += 1) s.emit(landed(i));
    clockSpy.mockClear();
    s.emit(landed(5));
    expect(screen.getByText("Invented line number 5 about the gravel.")).toBeInTheDocument();
    expect(clockCallsAt(startOf(5))).toBe(1);
    for (let i = 0; i < 5; i += 1) expect(clockCallsAt(startOf(i))).toBe(0);
  });

  it("render cost: typing a search does not render the open transcript's lines again", async () => {
    const s = setup();
    await screen.findByRole("heading", { level: 1, name: "Hedge trimming plan" });
    await s.user.click(await screen.findByRole("tab", { name: "Transcript" }));
    const table = await screen.findByRole("table", { name: "Transcript" });
    const rows = within(table).getAllByRole("row").length;
    expect(rows).toBe(sampleDetail(MEETINGS[0]).transcript.length);
    expect(rowTimeSpy).toHaveBeenCalled();
    rowTimeSpy.mockClear();
    await s.user.type(screen.getByRole("searchbox", { name: "Search meetings" }), "gr");
    expect(within(table).getAllByRole("row")).toHaveLength(rows);
    expect(rowTimeSpy).not.toHaveBeenCalled();
  });
});
