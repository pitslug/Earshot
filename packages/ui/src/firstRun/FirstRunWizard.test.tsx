import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { type Ipc, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { CHECK_TOTAL_MS } from "../ipc/mockAudioCheck";
import { MODEL_STEP_MS } from "../ipc/mockModels";
import { VOICE_SAMPLE_MS } from "../ipc/mockSettings";
import { CHECK_WARNING } from "../settings/AudioCheckSection";
import { SETUP_EXPLAINED } from "../settings/GeneralPage";
import { EXPORT_EXPLAINED } from "../settings/ObsidianPage";
import { HELPERS_WHY, formatSize } from "../settings/copy";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { OFFER_EXPLAINED, OTHER_WRITERS_NOTE, RUNNING_NOTE, STEPS, UNSAVED_NOTE, stillDownloading } from "./FirstRunWizard";

interface Setup {
  ipc: Ipc;
  timer: ManualTimer;
  user: ReturnType<typeof userEvent.setup>;
  mount(): void;
}

/** The app on a store where the wizard is due, unless `data.firstRun` says otherwise. */
function setup(data: MockIpcData = {}): Setup {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, firstRun: { show: true }, ...data });
  const mount = () =>
    render(
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <AppShell timer={timer} />
        </IpcProvider>
      </EarshotThemeProvider>,
    );
  mount();
  return { ipc, timer, user: userEvent.setup(), mount };
}

const stepHeading = (title: string) => screen.findByRole("heading", { level: 1, name: title });
const button = (name: string | RegExp) => screen.getByRole("button", { name });

async function toStep(s: Setup, index: number) {
  await stepHeading(STEPS[0].title);
  for (let i = 0; i < index; i++) {
    await s.user.click(button("Skip this step"));
    await stepHeading(STEPS[i + 1].title);
  }
}

function advance(timer: ManualTimer, ms: number) {
  act(() => timer.advance(ms));
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("First run wizard", () => {
  it("offers the speech helpers with their size in the models step, with no later or never", async () => {
    const s = setup({ cleanInstall: true });
    await toStep(s, 1);
    const section = within(await screen.findByRole("region", { name: "What every meeting needs" }));
    const size = (await s.ipc.models()).helpers.size;
    expect(section.getByText(HELPERS_WHY)).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "When to download Speech helpers" })).toBeNull();
    await s.user.click(section.getByRole("button", { name: `Download Speech helpers (${formatSize(size)})` }));
    advance(s.timer, MODEL_STEP_MS);
    expect(await section.findByRole("button", { name: "Pause Speech helpers" })).toBeInTheDocument();
    // The download goes on through the other steps and says so.
    await s.user.click(button("Skip this step"));
    await stepHeading(STEPS[2].title);
    expect(await screen.findByText(stillDownloading(["Speech helpers"]))).toBeInTheDocument();
  });

  it("opens on the first step with the progress bar, ahead of the meetings", async () => {
    setup();
    await stepHeading("Obsidian vault");
    expect(screen.getByRole("progressbar", { name: "Step 1 of 4" })).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "Meetings and settings" })).toBeNull();
    expect(await screen.findByText(EXPORT_EXPLAINED)).toBeInTheDocument();
    const steps = within(screen.getByRole("list"));
    for (const step of STEPS) expect(steps.getByText(step.title)).toBeInTheDocument();
    expect(button("Back")).toBeDisabled();
    expect(button("Next")).toBeEnabled();
  });

  it("does not show when the app says it is done, or when its state cannot be read", async () => {
    setup({ firstRun: { show: false } });
    await screen.findByRole("navigation", { name: "Meetings and settings" });
    expect(screen.queryByRole("progressbar")).toBeNull();
    cleanup();
    setup({ firstRun: { show: true, stateFails: true } });
    await screen.findByRole("navigation", { name: "Meetings and settings" });
    expect(screen.queryByRole("progressbar")).toBeNull();
  });

  it("skipping every step counts as done and it is shown once", async () => {
    const onCall = vi.fn();
    const s = setup({ firstRun: { show: true, onCall } });
    for (const [i, step] of STEPS.entries()) {
      await stepHeading(step.title);
      expect(screen.getByRole("progressbar", { name: `Step ${i + 1} of 4` })).toBeInTheDocument();
      await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("heading", { level: 1 })));
      await s.user.click(button("Skip this step"));
    }
    await screen.findByRole("heading", { name: "Record your first meeting" });
    expect(onCall.mock.calls).toEqual([[["finishFirstRun"]]]);
    expect((await s.ipc.firstRunState()).show).toBe(false);
    cleanup();
    s.mount();
    await screen.findByRole("navigation", { name: "Meetings and settings" });
    expect(screen.queryByRole("progressbar")).toBeNull();
  });

  it("Skip setup finishes from any step, and Next and Back walk the steps", async () => {
    const s = setup();
    await stepHeading(STEPS[0].title);
    await s.user.click(button("Next"));
    await stepHeading(STEPS[1].title);
    await s.user.click(button("Back"));
    await stepHeading(STEPS[0].title);
    await s.user.click(button("Next"));
    await s.user.click(button("Skip setup"));
    await screen.findByRole("heading", { name: "Record your first meeting" });
    expect((await s.ipc.firstRunState()).show).toBe(false);
  });

  it("says when it could not store that it finished, retries, and never traps you", async () => {
    const s = setup({ firstRun: { show: true, finishFailures: 2 } });
    await stepHeading(STEPS[0].title);
    await s.user.click(button("Skip setup"));
    await screen.findByText(/the database could not be written/);
    await s.user.click(button("Skip setup"));
    await screen.findByText(/the database could not be written/);
    await s.user.click(button("Go on anyway"));
    await screen.findByRole("heading", { name: "Record your first meeting" });
    expect((await s.ipc.firstRunState()).show).toBe(true);
  });

  it("reopens from Settings, General and goes back there, with the record still done", async () => {
    const s = setup({ firstRun: { show: false } });
    await s.user.click(await screen.findByRole("button", { name: "Settings" }));
    const nav = await screen.findByRole("navigation", { name: "Settings sections" });
    await s.user.click(within(nav).getByRole("button", { name: "General" }));
    await screen.findByText(SETUP_EXPLAINED);
    await s.user.click(button("Open the setup guide"));
    await stepHeading(STEPS[0].title);
    await s.user.click(button("Skip setup"));
    await screen.findByRole("heading", { level: 1, name: "General" });
    expect((await s.ipc.firstRunState()).show).toBe(false);
  });

  it("leaves when the tray or the shortcut starts a meeting", async () => {
    const s = setup();
    await stepHeading(STEPS[0].title);
    await act(async () => {
      await s.ipc.startMeeting("Call", null);
    });
    advance(s.timer, 1);
    await screen.findByRole("button", { name: /^Stop/ });
    expect(screen.queryByRole("progressbar", { name: /^Step/ })).toBeNull();
  });

  it("step 1 stores the vault through the Obsidian page, and Next waits for typed text to be saved", async () => {
    const s = setup({ settings: { pickedVault: "C:\\Notes" } });
    await stepHeading(STEPS[0].title);
    await s.user.click(await screen.findByRole("button", { name: "Choose folder..." }));
    expect(await screen.findByDisplayValue("C:\\Notes")).toBeInTheDocument();
    await s.user.click(button("Next"));
    expect(await screen.findByText(UNSAVED_NOTE)).toBeInTheDocument();
    await stepHeading(STEPS[0].title);
    expect((await s.ipc.obsidianSettings()).value.vault).toBe("");
    await s.user.click(button("Save"));
    await waitFor(async () => expect((await s.ipc.obsidianSettings()).value.vault).toBe("C:\\Notes"));
    await waitFor(() => expect(screen.queryByText(UNSAVED_NOTE)).toBeNull());
    await s.user.click(button("Next"));
    await stepHeading(STEPS[1].title);
  });

  it("skipping step 1 leaves typed text unsaved", async () => {
    const s = setup({ settings: { pickedVault: "C:\\Notes" } });
    await stepHeading(STEPS[0].title);
    await s.user.click(await screen.findByRole("button", { name: "Choose folder..." }));
    await screen.findByDisplayValue("C:\\Notes");
    await s.user.click(button("Skip this step"));
    await stepHeading(STEPS[1].title);
    expect((await s.ipc.obsidianSettings()).value.vault).toBe("");
  });

  it("step 2 shows the hardware and the pick, sizes from the catalogue, and later or never only on optional models", async () => {
    const s = setup({ machine: "desktop" });
    await toStep(s, 1);
    await screen.findByRole("list", { name: "This is your hardware" });
    expect(screen.getByRole("region", { name: "Recommendation" })).toBeInTheDocument();
    expect(screen.getByText(OFFER_EXPLAINED)).toBeInTheDocument();
    const { transcription } = await s.ipc.models();
    const lighter = transcription.find((m) => m.name === "parakeet-tdt-110m");
    expect(lighter).toBeDefined();
    const size = formatSize(lighter?.size ?? 0);
    expect(button(`Download Parakeet 110M (${size})`)).toBeInTheDocument();
    const offer = screen.getByRole("group", { name: "When to download Parakeet 110M" });
    expect(within(offer).getByRole("button", { name: "Later", pressed: true })).toBeInTheDocument();
    expect(within(offer).getByRole("button", { name: "Never", pressed: false })).toBeInTheDocument();
    // The model picked for this computer is not optional, and neither is one already here.
    expect(screen.queryByRole("group", { name: "When to download Parakeet 0.6B v2" })).toBeNull();
    expect(screen.queryByRole("group", { name: "When to download Qwen 3.5 9B" })).toBeNull();
  });

  it("step 2 stores never, shows it again when reopened, and a download takes it back", async () => {
    const onCall = vi.fn();
    const s = setup({ machine: "desktop", firstRun: { show: true, onCall } });
    await toStep(s, 1);
    const offer = await screen.findByRole("group", { name: "When to download Parakeet 110M" });
    await s.user.click(within(offer).getByRole("button", { name: "Never" }));
    await waitFor(async () =>
      expect((await s.ipc.firstRunState()).declined_models).toEqual(["parakeet-tdt-110m"]),
    );
    await within(offer).findByRole("button", { name: "Never", pressed: true });
    cleanup();
    s.mount();
    await toStep(s, 1);
    const again = await screen.findByRole("group", { name: "When to download Parakeet 110M" });
    await within(again).findByRole("button", { name: "Never", pressed: true });
    await s.user.click(button(/^Download Parakeet 110M/));
    await waitFor(async () => expect((await s.ipc.firstRunState()).declined_models).toEqual([]));
    expect(onCall).toHaveBeenCalledWith(["setModelDeclined", "parakeet-tdt-110m", false]);
  });

  it("going past a step while a model downloads does not cancel it, and finishing does not either", async () => {
    const s = setup({ machine: "desktop" });
    const cancel = vi.spyOn(s.ipc, "cancelDownload");
    await toStep(s, 1);
    await s.user.click(await screen.findByRole("button", { name: /^Download Parakeet 110M/ }));
    await waitFor(async () =>
      expect((await s.ipc.models()).transcription.find((m) => m.name === "parakeet-tdt-110m")?.downloading).toBe(true),
    );
    await s.user.click(button("Next"));
    await stepHeading(STEPS[2].title);
    expect(await screen.findByText(stillDownloading(["Parakeet 110M"]))).toBeInTheDocument();
    advance(s.timer, MODEL_STEP_MS * 3);
    await s.user.click(button("Skip setup"));
    await screen.findByRole("heading", { name: "Record your first meeting" });
    advance(s.timer, MODEL_STEP_MS * 7);
    await waitFor(async () => {
      const m = (await s.ipc.models()).transcription.find((x) => x.name === "parakeet-tdt-110m");
      expect(m?.installed).toBe("Yes");
    });
    expect(cancel).not.toHaveBeenCalled();
  });

  it("holds one subscription to model job events across the steps", async () => {
    const timer = createManualTimer(1_000_000);
    const ipc = createMockIpc({ meetings: [], timer, firstRun: { show: true } });
    const subscribe = vi.spyOn(ipc, "onModelJobEvent");
    render(
      <EarshotThemeProvider>
        <IpcProvider ipc={ipc}>
          <AppShell timer={timer} />
        </IpcProvider>
      </EarshotThemeProvider>,
    );
    const user = userEvent.setup();
    for (const [i, step] of STEPS.entries()) {
      await stepHeading(step.title);
      if (i < STEPS.length - 1) await user.click(button("Next"));
    }
    expect(subscribe).toHaveBeenCalledTimes(1);
  });

  it("step 3 offers this computer, your own endpoint or none, and saves the choice", async () => {
    const s = setup();
    await toStep(s, 2);
    await screen.findByRole("radiogroup", { name: "Who writes the summary" });
    expect(screen.getAllByRole("radio")).toHaveLength(3);
    expect(screen.getByRole("radio", { name: /^On this computer/ })).toBeChecked();
    expect(screen.getByText(OTHER_WRITERS_NOTE)).toBeInTheDocument();
    await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("NoSummary"));
  });

  it("step 3 shows a stored choice it does not offer, and Next waits for an endpoint to be saved", async () => {
    const s = setup({ summaries: { settings: { writer: "Codex" } } });
    await toStep(s, 2);
    expect(await screen.findByRole("radio", { name: /^OpenAI \(through Codex\)/ })).toBeChecked();
    expect(screen.getAllByRole("radio")).toHaveLength(4);
    await s.user.click(screen.getByRole("radio", { name: /^Your own endpoint/ }));
    await screen.findByRole("group", { name: "Your own endpoint" });
    await s.user.click(button("Next"));
    expect(await screen.findByText(UNSAVED_NOTE)).toBeInTheDocument();
    await stepHeading(STEPS[2].title);
    await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
    await waitFor(() => expect(screen.queryByText(UNSAVED_NOTE)).toBeNull());
    await s.user.click(button("Next"));
    await stepHeading(STEPS[3].title);
  });

  it("step 4 plays and records nothing until its button, and holds the buttons while the check runs", async () => {
    const s = setup();
    const startCheck = vi.spyOn(s.ipc, "startAudioCheck");
    const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
    await toStep(s, 3);
    await screen.findByRole("button", { name: "Record my voice" });
    expect(screen.getByText(CHECK_WARNING)).toBeInTheDocument();
    advance(s.timer, VOICE_SAMPLE_MS);
    expect(startCheck).not.toHaveBeenCalled();
    expect(startVoice).not.toHaveBeenCalled();
    await s.user.click(button("Run audio check"));
    expect(startCheck).toHaveBeenCalledTimes(1);
    await screen.findByText(RUNNING_NOTE);
    for (const name of ["Finish", "Back", "Skip this step", "Skip setup"]) expect(button(name)).toBeDisabled();
    advance(s.timer, CHECK_TOTAL_MS);
    await screen.findByRole("group", { name: "Last audio check" });
    await waitFor(() => expect(button("Finish")).toBeEnabled());
    expect(startVoice).not.toHaveBeenCalled();
  });

  it("step 4 asks before recording your voice, and shows why the app refused", async () => {
    const s = setup({
      settings: { voiceStartError: { kind: "busy", message: "A meeting is running.", retryable: true } },
    });
    const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
    await toStep(s, 3);
    await s.user.click(await screen.findByRole("button", { name: "Record my voice" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Save a voiceprint of your voice?" });
    expect(startVoice).not.toHaveBeenCalled();
    await s.user.click(within(dialog).getByRole("button", { name: "Record and save" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(startVoice).toHaveBeenCalledTimes(1);
    await screen.findByText("A meeting is running.");
    expect(await screen.findByRole("button", { name: "Finish" })).toBeEnabled();
  });

  it("step 4 shows why the app refused the audio check", async () => {
    const s = setup({
      audioCheck: { startError: { kind: "busy", message: "The model check is running.", retryable: true } },
    });
    await toStep(s, 3);
    await s.user.click(await screen.findByRole("button", { name: "Run audio check" }));
    await screen.findByText("The model check is running.");
    expect(button("Finish")).toBeEnabled();
  });

  it("uses no em dash in its copy", () => {
    const words = [
      OFFER_EXPLAINED,
      OTHER_WRITERS_NOTE,
      RUNNING_NOTE,
      UNSAVED_NOTE,
      SETUP_EXPLAINED,
      ...STEPS.flatMap((s) => [s.title, s.intro]),
    ];
    for (const w of words) expect(w).not.toContain("\u2014");
  });
});
