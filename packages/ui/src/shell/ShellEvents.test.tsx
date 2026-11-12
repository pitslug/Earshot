// What the tray, the global shortcut and notifications ask of the window (T7.5).

import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  type Ipc,
  IpcFailure,
  IpcProvider,
  type MeetingEvent,
  type MockIpcData,
  type RecoveredRecording,
  type ShellEvent,
  createMockIpc,
  sampleMeetings,
} from "../ipc";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { AppShell } from "./AppShell";

interface Setup {
  send(event: ShellEvent): void;
  meeting(event: MeetingEvent): void;
  /** Recovery saved these after the window opened (the app hands them over at the next ask). */
  recover(list: RecoveredRecording[]): void;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}, quitApp?: Ipc["quitApp"]): Promise<Setup> {
  const timer = createManualTimer(1_000_000);
  const base = createMockIpc({ timer, ...data });
  let shell: ((event: ShellEvent) => void) | null = null;
  let live: ((event: MeetingEvent) => void) | null = null;
  const later: RecoveredRecording[] = [];
  const ipc: Ipc = {
    ...base,
    recoveredRecordings: async () => [...(await base.recoveredRecordings()), ...later.splice(0)],
    quitApp: quitApp ?? base.quitApp,
    onShellEvent: async (handler) => {
      shell = handler;
      return () => {};
    },
    onMeetingEvent: async (handler) => {
      live = handler;
      return () => {};
    },
  };
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  // The first-run wizard has no Settings button.
  if (!data.firstRun?.show) await screen.findByRole("button", { name: "Settings" });
  await waitFor(() => {
    expect(shell).not.toBeNull();
    expect(live).not.toBeNull();
  });
  return {
    send: (event) => act(() => shell?.(event)),
    meeting: (event) => act(() => live?.(event)),
    recover: (list) => later.push(...list),
    user: userEvent.setup(),
  };
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("the window hears the tray, the shortcut and notifications", () => {
  it("asks before quitting while recording, and stops and quits on yes", async () => {
    const onQuit = vi.fn();
    const s = await open({ onQuit });
    s.send({ type: "QuitAsked", recording: true });
    expect(await screen.findByText("Quit while recording?")).toBeInTheDocument();
    expect(onQuit).not.toHaveBeenCalled();
    await s.user.click(screen.getByRole("button", { name: "Stop and quit" }));
    expect(onQuit).toHaveBeenCalledTimes(1);
    expect(await screen.findByText("Saving and quitting")).toBeInTheDocument();
  });

  it("keeps recording when the answer is no", async () => {
    const onQuit = vi.fn();
    const s = await open({ onQuit });
    s.send({ type: "QuitAsked", recording: true });
    await s.user.click(await screen.findByRole("button", { name: "Keep recording" }));
    await waitFor(() => expect(screen.queryByText("Quit while recording?")).toBeNull());
    expect(onQuit).not.toHaveBeenCalled();
  });

  it("says what is lost when quitting during the last steps", async () => {
    const onQuit = vi.fn();
    const s = await open({ onQuit });
    s.send({ type: "QuitAsked", recording: false });
    expect(await screen.findByText("Quit before the summary is ready?")).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Quit" }));
    expect(onQuit).toHaveBeenCalledTimes(1);
  });

  it("shows a refused quit and lets the user try again", async () => {
    const s = await open({}, async () => {
      throw new IpcFailure({ kind: "internal", message: "The task stopped.", retryable: false });
    });
    s.send({ type: "QuitAsked", recording: true });
    await s.user.click(await screen.findByRole("button", { name: "Stop and quit" }));
    expect(await screen.findByText("The task stopped.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Stop and quit" })).toBeEnabled();
  });

  it("shows why a meeting started from the tray could not start", async () => {
    const s = await open();
    s.send({ type: "StartFailed", message: "The model is not installed." });
    expect(await screen.findByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText("The model is not installed.")).toBeInTheDocument();
  });

  it("keeps the first-run wizard when a tray or shortcut start is refused, and says why", async () => {
    const s = await open({ firstRun: { show: true } });
    const first = await screen.findByRole("heading", { level: 1 });
    s.send({ type: "StartFailed", message: "The model is not installed." });
    expect(await screen.findByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText("The model is not installed.")).toBeInTheDocument();
    // The same step of the same wizard: nothing was remounted.
    expect(screen.getByRole("heading", { level: 1 })).toBe(first);
    await s.user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("The meeting could not start")).toBeNull();
    expect(screen.getByRole("heading", { level: 1 })).toBe(first);
  });

  it("leaves the after-meeting screen when its finished meeting is deleted from the list", async () => {
    const [gone, kept] = sampleMeetings();
    const s = await open({ meetings: [gone, kept] });
    s.meeting({ type: "Started", meeting: gone.id, mode: "Call", tracks: ["You", "Them"] });
    s.meeting({ type: "Ended", meeting: gone.id, reason: "Stopped" });
    s.meeting({
      type: "Summary",
      meeting: gone.id,
      result: { kind: "Skipped", reason: "Summaries are turned off." },
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Meeting finished" })).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: `Delete ${gone.title}` }));
    await s.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete meeting" }));
    expect(await screen.findByRole("heading", { level: 1, name: kept.title })).toBeInTheDocument();
    expect(screen.queryByText("Meeting finished")).toBeNull();
  });

  it("keeps the after-meeting screen when another meeting is deleted from the list", async () => {
    const [shown, other] = sampleMeetings();
    const s = await open({ meetings: [shown, other] });
    s.meeting({ type: "Started", meeting: shown.id, mode: "Call", tracks: ["You", "Them"] });
    s.meeting({ type: "Ended", meeting: shown.id, reason: "Stopped" });
    s.meeting({
      type: "Summary",
      meeting: shown.id,
      result: { kind: "Skipped", reason: "Summaries are turned off." },
    });
    await screen.findByRole("heading", { level: 1, name: "Meeting finished" });
    await s.user.click(screen.getByRole("button", { name: `Delete ${other.title}` }));
    await s.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete meeting" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(await screen.findByRole("heading", { level: 1, name: "Meeting finished" })).toBeInTheDocument();
  });

  it("shows what the work after a meeting warned of, and not the recording's own banners", async () => {
    const s = await open();
    const warning = (key: string, message: string): MeetingEvent => ({
      type: "Warning",
      key,
      kind: "Backend",
      message,
    });
    s.meeting({ type: "Started", meeting: "m-warned", mode: "Call", tracks: ["You", "Them"] });
    s.meeting(warning("backend:you", "A note from during the recording."));
    expect(await screen.findByText("A note from during the recording.")).toBeInTheDocument();
    s.meeting({ type: "Ended", meeting: "m-warned", reason: "Stopped" });
    expect(screen.queryByText("A note from during the recording.")).toBeNull();
    s.meeting(warning("post-voices:them", "A note from the final pass."));
    expect(screen.getByText("A note from the final pass.")).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("A note from the final pass.")).toBeNull();
  });

  it("opens the meeting screen when the shortcut started a meeting, even from Settings", async () => {
    const s = await open();
    s.send({ type: "StartFailed", message: "The model is not installed." });
    await s.user.click(screen.getByRole("button", { name: "Settings" }));
    await screen.findByRole("navigation", { name: "Settings sections" });
    s.meeting({
      type: "Started",
      meeting: "m-from-the-shortcut",
      mode: "Call",
      tracks: ["You", "Them"],
    });
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeInTheDocument();
    expect(screen.queryByText("The meeting could not start")).toBeNull();
  });

  it("opens the meeting a notification names", async () => {
    const meetings = sampleMeetings(1_000_000);
    const target = meetings.filter((m) => m.state === "Ready" && m.title !== "").at(-1);
    if (target === undefined) throw new Error("the sample has no finished meeting");
    const s = await open({ meetings });
    s.send({ type: "OpenMeeting", meeting: target.id, name_voices: false });
    expect(await screen.findByRole("heading", { level: 1, name: target.title })).toBeInTheDocument();
  });
});

describe("a recording Earshot was closed during", () => {
  it("is told once at the start, opens its meeting, and is not told again", async () => {
    const [target] = sampleMeetings();
    const onCall = vi.fn();
    const s = await open({ recovered: [{ meeting: target.id, saved_ms: 12 * 60_000 + 5_000 }], onCall });
    expect(
      await screen.findByText("Earshot closed during this recording. 12 minutes were saved."),
    ).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Open meeting" }));
    expect(await screen.findByRole("heading", { level: 1, name: target.title })).toBeInTheDocument();
    expect(screen.queryByText(/Earshot closed during this recording/)).toBeNull();
    // A later ask finds nothing: the app handed it over once.
    s.send({ type: "RecordingsRecovered" });
    await waitFor(() =>
      expect(onCall.mock.calls.filter((c) => c[0][0] === "recoveredRecordings").length).toBe(2),
    );
    expect(screen.queryByText(/Earshot closed during this recording/)).toBeNull();
  });

  it("is asked for again when recovery finishes after the window opened", async () => {
    const [target] = sampleMeetings();
    const s = await open();
    expect(screen.queryByText("Recording recovered")).toBeNull();
    // The recovery thread finished later: the app says so and the window reads it.
    s.recover([{ meeting: target.id, saved_ms: 30_000 }]);
    s.send({ type: "RecordingsRecovered" });
    expect(
      await screen.findByText("Earshot closed during this recording. Less than a minute was saved."),
    ).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("Recording recovered")).toBeNull();
  });
});
