// Banners on the meeting page that hide by themselves, and the ones that never do (T14.59).

import { act, fireEvent, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { type JobEvent, type MockIpcData, createMockIpc, sampleDetail } from "../ipc";
import { IpcProvider } from "../ipc/context";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { ITEM, OTHER } from "../test/meetingDetail";
import { BANNER_HIDE_MS, hidesByItself } from "./autoHide";
import { type JobView, MeetingDetailView } from "./MeetingDetailView";

const EXPORTED = /^Exported to /;

function show(data: MockIpcData, job?: JobView, opts: { status?: ReactNode } = {}) {
  const timer = createManualTimer(10_000);
  const ipc = createMockIpc({ timer, meetings: [ITEM, OTHER], ...data });
  const dismissed = vi.fn();
  const view = (j: JobView | undefined) => (
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <MeetingDetailView
          item={ITEM}
          timer={timer}
          onChanged={() => {}}
          onJobStarted={() => {}}
          onJobDismissed={dismissed}
          onJobCancel={() => {}}
          finishing={false}
          status={opts.status}
          job={j}
        />
      </IpcProvider>
    </EarshotThemeProvider>
  );
  const r = render(view(job));
  return { timer, dismissed, rerender: (j: JobView | undefined) => r.rerender(view(j)) };
}

const advance = (timer: ManualTimer, ms: number) => act(() => timer.advance(ms));

async function exported(timer: ManualTimer) {
  const user = userEvent.setup({ advanceTimers: (ms) => timer.advance(ms) });
  await screen.findByText(/TL;DR/);
  await user.click(screen.getByRole("button", { name: "Export to Obsidian" }));
  await screen.findByText(EXPORTED);
  return user;
}

const retranscribed = (extra: Partial<Extract<JobEvent, { type: "Retranscribed" }>> = {}): JobEvent => ({
  type: "Retranscribed",
  meeting: ITEM.id,
  missing_tracks: 0,
  no_audio: false,
  summary_stale: false,
  warnings: [],
  ...extra,
});

describe("banners that hide by themselves", () => {
  it("hides a success notice with nothing but Dismiss after the delay", async () => {
    const { timer } = show({ vault: "C:\\Vault" });
    await exported(timer);
    advance(timer, BANNER_HIDE_MS - 1);
    expect(screen.getByText(EXPORTED)).toBeInTheDocument();
    advance(timer, 1);
    expect(screen.queryByText(EXPORTED)).toBeNull();
  });

  it("dismisses a finished job's success bar after the delay", async () => {
    const { timer, dismissed } = show({}, { job: "Retranscribe", last: retranscribed(), seq: 3 });
    await screen.findByText("The meeting was transcribed again.");
    advance(timer, BANNER_HIDE_MS - 1);
    expect(dismissed).not.toHaveBeenCalled();
    advance(timer, 1);
    expect(dismissed).toHaveBeenCalledTimes(1);
  });

  it("starts again for a new banner of the same key", async () => {
    const { timer, dismissed, rerender } = show({}, { job: "Retranscribe", last: retranscribed(), seq: 3 });
    await screen.findByText("The meeting was transcribed again.");
    advance(timer, BANNER_HIDE_MS - 100);
    rerender({ job: "Retranscribe", last: retranscribed(), seq: 4 });
    advance(timer, BANNER_HIDE_MS - 100);
    expect(dismissed).not.toHaveBeenCalled();
    advance(timer, 100);
    expect(dismissed).toHaveBeenCalledTimes(1);
  });

  it("holds while the pointer is over it and starts again from the full delay when it leaves", async () => {
    const { timer } = show({ vault: "C:\\Vault" });
    await exported(timer);
    const bar = screen.getByText(EXPORTED).closest(".fui-MessageBar") as HTMLElement;
    fireEvent.pointerEnter(bar);
    advance(timer, BANNER_HIDE_MS * 3);
    expect(screen.getByText(EXPORTED)).toBeInTheDocument();
    fireEvent.pointerLeave(bar);
    advance(timer, BANNER_HIDE_MS - 1);
    expect(screen.getByText(EXPORTED)).toBeInTheDocument();
    advance(timer, 1);
    expect(screen.queryByText(EXPORTED)).toBeNull();
  });

  it("holds while focus is inside it", async () => {
    const { timer } = show({ vault: "C:\\Vault" });
    await exported(timer);
    const bar = screen.getByText(EXPORTED).closest(".fui-MessageBar") as HTMLElement;
    const dismiss = bar.querySelector("button") as HTMLButtonElement;
    act(() => dismiss.focus());
    advance(timer, BANNER_HIDE_MS * 3);
    expect(screen.getByText(EXPORTED)).toBeInTheDocument();
    act(() => dismiss.blur());
    advance(timer, BANNER_HIDE_MS);
    expect(screen.queryByText(EXPORTED)).toBeNull();
  });
});

describe("banners that never hide by themselves", () => {
  const LONG = BANNER_HIDE_MS * 4;

  it("keeps a warning notice", async () => {
    // No vault: the export is refused with a warning.
    const { timer } = show({});
    const user = userEvent.setup({ advanceTimers: (ms) => timer.advance(ms) });
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Export to Obsidian" }));
    await screen.findByText(/Set your Obsidian vault/);
    advance(timer, LONG);
    expect(screen.getByText(/Set your Obsidian vault/)).toBeInTheDocument();
  });

  it("keeps an error job bar", async () => {
    const { timer, dismissed } = show(
      {},
      {
        job: "Retranscribe",
        last: { type: "Failed", meeting: ITEM.id, job: "Retranscribe", message: "Invented.", retryable: true },
        seq: 2,
      },
    );
    await screen.findByText(/Invented\./);
    advance(timer, LONG);
    expect(dismissed).not.toHaveBeenCalled();
  });

  it("keeps a finished job's warning bar", async () => {
    const { timer, dismissed } = show({}, { job: "Retranscribe", last: retranscribed({ warnings: ["Invented note."] }), seq: 2 });
    await screen.findByText(/Invented note\./);
    advance(timer, LONG);
    expect(dismissed).not.toHaveBeenCalled();
  });

  it("keeps a running job's progress", async () => {
    const { timer, dismissed } = show({}, { job: "Retranscribe", last: null, seq: 1 });
    await screen.findByText("Re-transcribing from the recording");
    advance(timer, LONG);
    expect(screen.getByText("Re-transcribing from the recording")).toBeInTheDocument();
    expect(dismissed).not.toHaveBeenCalled();
  });

  it("keeps the waiting-for-another-computer bar", async () => {
    const { timer } = show({
      details: { [ITEM.id]: { ...sampleDetail(ITEM), waiting_for: "Invented waiting text." } },
    });
    await screen.findByText("Invented waiting text.");
    advance(timer, LONG);
    expect(screen.getByText("Invented waiting text.")).toBeInTheDocument();
  });

  it("keeps the Summary tab's stale and run-note bars", async () => {
    const { timer } = show({
      details: { [ITEM.id]: { ...sampleDetail(ITEM), summary_stale: true, summary_run: "Invented run note." } },
    });
    await screen.findByText("Invented run note.");
    advance(timer, LONG);
    expect(screen.getByText("Invented run note.")).toBeInTheDocument();
    expect(screen.getByText(/refers to lines of an earlier transcript/)).toBeInTheDocument();
  });

  it("keeps the meeting status", async () => {
    // AppShell owns it and passes it in: the page never hides it.
    const { timer } = show({}, undefined, { status: <div role="region" aria-label="Meeting status">Invented status.</div> });
    await screen.findByText(/TL;DR/);
    advance(timer, LONG);
    expect(screen.getByRole("region", { name: "Meeting status" })).toHaveTextContent("Invented status.");
  });
});

describe("hidesByItself", () => {
  it("hides only success and info with nothing to offer and nothing running", () => {
    expect(hidesByItself({ intent: "success" })).toBe(true);
    expect(hidesByItself({ intent: "info" })).toBe(true);
    expect(hidesByItself({ intent: "warning" })).toBe(false);
    expect(hidesByItself({ intent: "error" })).toBe(false);
    expect(hidesByItself({ intent: "info", busy: true })).toBe(false);
    expect(hidesByItself({ intent: "success", action: true })).toBe(false);
  });
});
