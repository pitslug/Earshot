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
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}, quitApp?: Ipc["quitApp"]): Promise<Setup> {
  const timer = createManualTimer(1_000_000);
  const base = createMockIpc({ timer, ...data });
  let shell: ((event: ShellEvent) => void) | null = null;
  let live: ((event: MeetingEvent) => void) | null = null;
  const ipc: Ipc = {
    ...base,
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

  it("leaves the finished meeting's result when that meeting is deleted from the list", async () => {
    const [gone, kept] = sampleMeetings();
    const s = await open({ meetings: [gone, kept] });
    s.meeting({ type: "Started", meeting: gone.id, mode: "Call", tracks: ["You", "Them"] });
    s.meeting({ type: "Ended", meeting: gone.id, reason: "Stopped" });
    s.meeting({
      type: "Summary",
      meeting: gone.id,
      result: { kind: "Skipped", reason: "Summaries are turned off." },
    });
    expect(await screen.findByRole("heading", { level: 1, name: gone.title })).toBeInTheDocument();
    expect(screen.getByText("No summary")).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: `Delete ${gone.title}` }));
    await s.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete meeting" }));
    expect(await screen.findByRole("heading", { level: 1, name: kept.title })).toBeInTheDocument();
    expect(screen.queryByText("No summary")).toBeNull();
    expect(screen.queryByRole("region", { name: "Meeting status" })).toBeNull();
  });

  it("keeps the finished meeting's page and result when another meeting is deleted from the list", async () => {
    const [shown, other] = sampleMeetings();
    const s = await open({ meetings: [shown, other] });
    s.meeting({ type: "Started", meeting: shown.id, mode: "Call", tracks: ["You", "Them"] });
    s.meeting({ type: "Ended", meeting: shown.id, reason: "Stopped" });
    s.meeting({
      type: "Summary",
      meeting: shown.id,
      result: { kind: "Skipped", reason: "Summaries are turned off." },
    });
    await screen.findByRole("heading", { level: 1, name: shown.title });
    expect(screen.getByText("No summary")).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: `Delete ${other.title}` }));
    await s.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete meeting" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(await screen.findByRole("heading", { level: 1, name: shown.title })).toBeInTheDocument();
    expect(screen.getByText("No summary")).toBeInTheDocument();
  });

  it("shows what the work after a meeting warned of on its page, and not the recording's own banners", async () => {
    const [warned] = sampleMeetings();
    const s = await open({ meetings: sampleMeetings() });
    const warning = (key: string, message: string): MeetingEvent => ({
      type: "Warning",
      key,
      kind: "Backend",
      message,
    });
    s.meeting({ type: "Started", meeting: warned.id, mode: "Call", tracks: ["You", "Them"] });
    s.meeting(warning("backend:you", "A note from during the recording."));
    expect(await screen.findByText("A note from during the recording.")).toBeInTheDocument();
    s.meeting({ type: "Ended", meeting: warned.id, reason: "Stopped" });
    expect(await screen.findByRole("heading", { level: 1, name: warned.title })).toBeInTheDocument();
    expect(screen.queryByText("A note from during the recording.")).toBeNull();
    s.meeting(warning("post-voices:them", "A note from the final pass."));
    const status = screen.getByRole("region", { name: "Meeting status" });
    expect(within(status).getByText("A note from the final pass.")).toBeInTheDocument();
    expect(within(status).getByText("Summary in progress")).toBeInTheDocument();
    await s.user.click(within(status).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("A note from the final pass.")).toBeNull();
    // A warning that arrives with the result stays beside it.
    s.meeting(warning("recording-gap", "An invented gap in the recording."));
    s.meeting({ type: "Summary", meeting: warned.id, result: { kind: "Cancelled" } });
    expect(await screen.findByText("Summary cancelled")).toBeInTheDocument();
    expect(screen.getByText("An invented gap in the recording.")).toBeInTheDocument();
  });

  it("says on the meeting's page that audio capture failed", async () => {
    const [failed] = sampleMeetings();
    const s = await open({ meetings: sampleMeetings() });
    s.meeting({ type: "Started", meeting: failed.id, mode: "Call", tracks: ["You", "Them"] });
    s.meeting({ type: "Ended", meeting: failed.id, reason: "Failed" });
    expect(await screen.findByRole("heading", { level: 1, name: failed.title })).toBeInTheDocument();
    const status = screen.getByRole("region", { name: "Meeting status" });
    expect(
      within(status).getByText("The recording stopped because audio capture failed. What was recorded is kept."),
    ).toBeInTheDocument();
    s.meeting({ type: "Summary", meeting: failed.id, result: { kind: "PostCancelled" } });
    expect(await screen.findByText("Transcript not finished")).toBeInTheDocument();
    expect(
      screen.getByText("The recording stopped because audio capture failed. What was recorded is kept."),
    ).toBeInTheDocument();
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
