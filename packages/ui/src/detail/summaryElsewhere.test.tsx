// T14.66 (docs/summary-order.md 11.2, 11.5 to 11.7): a summary written elsewhere can be stopped,
// the out-of-date notes wait while a new summary is written, the header says one is being
// written, and a re-summary's step changes reach the page without replacing the job's bar.

import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import {
  type Ipc,
  type JobEvent,
  type MeetingDetail,
  type MockIpcData,
  MOCK_WAITING_DEVICE,
  ORDER_COPY,
  createMockIpc,
  fillCopy,
  sampleDetail,
} from "../ipc";
import { IpcProvider } from "../ipc/context";
import { renderApp } from "../test/app";
import { ITEM, OTHER, setup } from "../test/meetingDetail";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { headerLine } from "./detail";
import { type JobView, MeetingDetailView } from "./MeetingDetailView";

const WAITING = "Waiting for Invented desk to write the summary.";
const STOP_NAME = fillCopy(ORDER_COPY.stop.label, { device: MOCK_WAITING_DEVICE });
const STALE_NAMES = "This summary uses a speaker's earlier name. Re-summarise to use the current names.";

/** The detail alone, with `job` and `data`. */
async function view(data: MockIpcData, job?: JobView) {
  const timer = createManualTimer(10_000);
  const ipc = createMockIpc({ timer, meetings: [ITEM, OTHER], ...data });
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
          finishing={false}
          job={job}
        />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  await screen.findByText(/TL;DR/);
}

const stale = (extra: Partial<MeetingDetail> = {}): MockIpcData => ({
  details: { [ITEM.id]: { ...sampleDetail(ITEM), summary_stale: true, summary_stale_names: true, ...extra } },
});

describe("Stop on the waiting banner", () => {
  it("stops the summary and the run note says so", async () => {
    const { user } = await setup({ waitingFor: { [ITEM.id]: WAITING } });
    expect(await screen.findByText(WAITING)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: STOP_NAME }));
    expect(await screen.findByText(fillCopy(ORDER_COPY.stopped_note, { device: MOCK_WAITING_DEVICE }))).toBeInTheDocument();
    expect(screen.queryByText(WAITING)).toBeNull();
    expect(screen.queryByRole("button", { name: STOP_NAME })).toBeNull();
  });

  it("says why when nothing was waiting any more", async () => {
    // The page still shows the banner; the computer already answered.
    const { user } = await setup({
      details: { [ITEM.id]: { ...sampleDetail(ITEM), waiting_for: WAITING, waiting_device: MOCK_WAITING_DEVICE } },
    });
    await user.click(await screen.findByRole("button", { name: STOP_NAME }));
    expect(await screen.findByText(ORDER_COPY.stop.not_waiting)).toBeInTheDocument();
  });

  it("says the stop failed when it could not be stored", async () => {
    const timer = createManualTimer(10_000);
    const base = createMockIpc({ timer, meetings: [ITEM, OTHER], waitingFor: { [ITEM.id]: WAITING } });
    const ipc: Ipc = {
      ...base,
      stopWaitingSummary: async () => {
        throw new Error("an invented failure");
      },
    };
    const user = userEvent.setup();
    renderApp(ipc, { timer });
    await user.click(await screen.findByRole("button", { name: STOP_NAME }));
    expect(await screen.findByText(ORDER_COPY.stop.failed)).toBeInTheDocument();
  });

  it("is not offered while nothing waits (control)", async () => {
    await setup();
    await screen.findByText(/TL;DR/);
    expect(screen.queryByRole("button", { name: STOP_NAME })).toBeNull();
  });
});

describe("the out-of-date notes while a new summary is written", () => {
  it("waits while the summary waits on another computer", async () => {
    await view(stale({ waiting_for: WAITING, waiting_device: MOCK_WAITING_DEVICE }));
    expect(screen.queryByText(STALE_NAMES)).toBeNull();
  });

  it("waits while a re-summary runs, and comes back when it ends", async () => {
    const running: JobView = { job: "Resummarise", last: null, seq: 1 };
    await view(stale(), running);
    expect(screen.queryByText(STALE_NAMES)).toBeNull();
  });

  it("shows with no summary being written (control)", async () => {
    await view(stale());
    expect(screen.getByText(STALE_NAMES)).toBeInTheDocument();
  });
});

describe("the header line while a new summary is written", () => {
  const d = sampleDetail(ITEM);
  it("adds being written after the summary's writer, here or on a computer", () => {
    expect(headerLine(d, { on: null })).toContain(`Summary by Qwen 3.5 9B · ${ORDER_COPY.in_progress.here} · `);
    expect(headerLine(d, { on: "Invented desk" })).toContain(
      `Summary by Qwen 3.5 9B · ${fillCopy(ORDER_COPY.in_progress.on, { device: "Invented desk" })} · `,
    );
  });

  it("is today's line with nothing being written (control)", () => {
    expect(headerLine(d, null)).toBe(headerLine(d));
    expect(headerLine(d)).not.toContain(ORDER_COPY.in_progress.here);
  });
});

describe("a re-summary's step change", () => {
  it("re-reads the page and leaves the job's bar as it was", async () => {
    const timer = createManualTimer(10_000);
    const base = createMockIpc({ timer, meetings: [ITEM, OTHER] });
    let emit: (e: JobEvent) => void = () => {};
    let reads = 0;
    const ipc: Ipc = {
      ...base,
      onJobEvent: async (l) => {
        emit = l;
        return () => {};
      },
      getMeeting: async (id) => {
        reads += 1;
        return base.getMeeting(id);
      },
    };
    renderApp(ipc, { timer });
    await screen.findByText(/TL;DR/);
    act(() => emit({ type: "Warning", meeting: ITEM.id, message: "An invented job warning." }));
    expect(await screen.findByText("An invented job warning.")).toBeInTheDocument();
    const before = reads;
    act(() => emit({ type: "SummaryRunChanged", meeting: ITEM.id }));
    // The page reads the meeting again ...
    await screen.findByText(/TL;DR/);
    await act(async () => {});
    expect(reads).toBeGreaterThan(before);
    // ... and the warning on the bar survives the move.
    expect(screen.getByText("An invented job warning.")).toBeInTheDocument();
    // Control: a new warning replaces it.
    act(() => emit({ type: "Warning", meeting: ITEM.id, message: "Another invented warning." }));
    expect(await screen.findByText("Another invented warning.")).toBeInTheDocument();
    expect(screen.queryByText("An invented job warning.")).toBeNull();
  });
});
