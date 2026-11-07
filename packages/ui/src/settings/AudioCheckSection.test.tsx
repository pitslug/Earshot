import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { SAMPLE_DEVICES } from "../ipc/mockAudio";
import {
  CHECK_HEADLINE,
  CHECK_STEP_MS,
  CHECK_TOTAL_MS,
  CLIPPING_LINE,
  DELAY_USED_LINE,
  MOCK_CHECK_FAILURE,
  TOO_QUIET_LINE,
  mockResult,
} from "../ipc/mockAudioCheck";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { CHECK_EXPLAINED, CHECK_WARNING, checkedLine } from "./AudioCheckSection";

const START = 1_793_000_000_000;

interface Setup {
  timer: ManualTimer;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}): Promise<Setup> {
  const timer = createManualTimer(START);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  const nav = await screen.findByRole("navigation", { name: "Settings sections" });
  await user.click(within(nav).getByRole("button", { name: "Audio" }));
  await screen.findByRole("heading", { level: 2, name: "Audio check" });
  return { timer, user };
}

function advance(timer: ManualTimer, ms: number) {
  act(() => timer.advance(ms));
}

const runButton = () => screen.getByRole("button", { name: /^Run audio check/ });
const output = () => screen.getByRole<HTMLSelectElement>("combobox", { name: "Output" });
const [speakers, headphones] = SAMPLE_DEVICES.surface.outputs;
const [builtIn] = SAMPLE_DEVICES.surface.microphones;

async function runToEnd(s: Setup) {
  await s.user.click(runButton());
  await screen.findByRole("group", { name: "Audio check running" });
  for (let t = 0; t < CHECK_TOTAL_MS; t += CHECK_STEP_MS) advance(s.timer, CHECK_STEP_MS);
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Audio check", () => {
  it("explains the check and warns that it plays sound before it runs", async () => {
    await open();
    expect(screen.getByText(CHECK_EXPLAINED)).toBeInTheDocument();
    expect(screen.getByText(CHECK_WARNING)).toBeInTheDocument();
    expect(await screen.findByText("These devices have not been checked yet.")).toBeInTheDocument();
    expect(runButton()).toHaveTextContent("Run audio check");
    expect(screen.queryByText("Coming later")).toBeNull();
  });

  it("plays, listens, then shows the delay, the echo level and the date in plain words", async () => {
    const s = await open();
    await s.user.click(runButton());
    const running = await screen.findByRole("group", { name: "Audio check running" });
    expect(within(running).getByRole("progressbar", { name: "Listened so far" })).toBeInTheDocument();
    // The devices stay put while the check uses them; the button is gone.
    expect(output()).toBeDisabled();
    expect(screen.queryByRole("button", { name: /^Run audio check/ })).toBeNull();
    advance(s.timer, 2 * CHECK_STEP_MS);
    expect(within(running).getByText("Playing and listening: 1 of 5 seconds")).toBeInTheDocument();
    for (let t = 2 * CHECK_STEP_MS; t < CHECK_TOTAL_MS; t += CHECK_STEP_MS) advance(s.timer, CHECK_STEP_MS);
    const result = await screen.findByRole("group", { name: "Last audio check" });
    expect(within(result).getByText(CHECK_HEADLINE.Echo)).toBeInTheDocument();
    expect(within(result).getByText("Delay from speaker to microphone: 52 ms.")).toBeInTheDocument();
    expect(within(result).getByText("Echo level: 18 dB quieter than the sound played.")).toBeInTheDocument();
    expect(within(result).getByText(DELAY_USED_LINE)).toBeInTheDocument();
    const stored = mockResult("Echo", builtIn.name, speakers.name, START + CHECK_TOTAL_MS);
    expect(within(result).getByText(checkedLine(stored))).toBeInTheDocument();
    expect(checkedLine(stored)).toMatch(/^Checked .+ with .+ and .+\.$/);
    expect(runButton()).toHaveTextContent("Run audio check again");
    expect(output()).not.toBeDisabled();
  });

  it("says headphones, too quiet and clipping", async () => {
    const s = await open({ audioCheck: { end: "Headphones" } });
    await runToEnd(s);
    expect(await screen.findByText(CHECK_HEADLINE.Headphones)).toBeInTheDocument();
    expect(screen.queryByText(/^Delay from speaker/)).toBeNull();
    cleanup();
    const quiet = await open({ audioCheck: { end: "TooQuiet" } });
    await runToEnd(quiet);
    expect(await screen.findByText(TOO_QUIET_LINE)).toBeInTheDocument();
    expect(screen.queryByText(DELAY_USED_LINE)).toBeNull();
    cleanup();
    const loud = await open({ audioCheck: { end: "Clipping" } });
    await runToEnd(loud);
    expect(await screen.findByText(CLIPPING_LINE)).toBeInTheDocument();
  });

  it("shows the result of the devices in use only", async () => {
    const s = await open();
    await runToEnd(s);
    await screen.findByRole("group", { name: "Last audio check" });
    await s.user.selectOptions(output(), headphones.id);
    expect(await screen.findByText("These devices have not been checked yet.")).toBeInTheDocument();
    await s.user.selectOptions(output(), "default");
    expect(await screen.findByRole("group", { name: "Last audio check" })).toBeInTheDocument();
  });

  it("shows a stored result when the page opens", async () => {
    const stored = mockResult("Echo", builtIn.name, speakers.name, START - 86_400_000);
    await open({ audioCheck: { stored } });
    const result = await screen.findByRole("group", { name: "Last audio check" });
    expect(within(result).getByText(checkedLine(stored))).toBeInTheDocument();
  });

  it("cancels, fails and refuses with the app's words, storing nothing", async () => {
    const s = await open();
    await s.user.click(runButton());
    const running = await screen.findByRole("group", { name: "Audio check running" });
    await s.user.click(within(running).getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("The audio check was cancelled. Nothing was saved.")).toBeInTheDocument();
    expect(screen.getByText("These devices have not been checked yet.")).toBeInTheDocument();
    cleanup();
    const failing = await open({ audioCheck: { end: "Failed" } });
    await runToEnd(failing);
    expect(await screen.findByText(MOCK_CHECK_FAILURE)).toBeInTheDocument();
    cleanup();
    const busy = "A meeting is running; run the audio check when it has finished.";
    const refused = await open({ audioCheck: { startError: { kind: "busy", message: busy, retryable: true } } });
    await refused.user.click(runButton());
    await waitFor(() => expect(screen.getByText(busy)).toBeInTheDocument());
    expect(screen.getByText("The audio check did not start")).toBeInTheDocument();
    expect(runButton()).toBeInTheDocument();
  });
});
