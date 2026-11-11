import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { type Ipc, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { CHECK_TOTAL_MS } from "../ipc/mockAudioCheck";
import { MODEL_STEP_MS } from "../ipc/mockModels";
import { VOICE_SAMPLE_MS } from "../ipc/mockSettings";
import { CHECK_WARNING } from "../settings/AudioCheckSection";
import { SETUP_EXPLAINED } from "../settings/GeneralPage";
import { EXPORT_EXPLAINED } from "../settings/ObsidianSection";
import { CLAUDE_CODE_PRIVACY, CODEX_PRIVACY } from "../settings/SummariesPage";
import { formatSize } from "../settings/copy";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import {
  AUDIO_CHECK_ROW,
  DEVICES_NOTE,
  DOWNLOADS_CARRY_ON,
  HELPERS_ARRIVING,
  HELPERS_MISSING,
  NOTHING_TO_DOWNLOAD,
  NO_MODEL_FOR_LANGUAGES,
  NO_MODEL_NOTE,
  OBSIDIAN_ROW,
  RUNNING_NOTE,
  SAVE_REFUSED_NOTE,
  STEPS,
  stillDownloading,
} from "./FirstRunWizard";

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

/** A machine straight after installing: no helpers, no models. */
const CLEAN = { cleanInstall: true, noModels: true };

const stepHeading = (title: string) => screen.findByRole("heading", { level: 1, name: title });
const button = (name: string | RegExp) => screen.getByRole("button", { name });
const HOME = { name: "Record your first meeting" };

async function toStep(s: Setup, index: number) {
  await stepHeading(STEPS[0].title);
  // The steps' own parts read on mount; Next before they finish would save nothing anyway.
  for (let i = 0; i < index; i++) {
    await s.user.click(button("Next"));
    await stepHeading(STEPS[i + 1].title);
  }
}

function advance(timer: ManualTimer, ms: number) {
  act(() => timer.advance(ms));
}

/** The total a computer needs of `names`. */
async function totalOf(ipc: Ipc, names: string[]) {
  const m = await ipc.models();
  const all = [m.helpers, ...m.transcription, ...m.summary];
  return formatSize(all.filter((i) => names.includes(i.name)).reduce((sum, i) => sum + i.size, 0));
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("First run wizard", () => {
  it("has its steps in the agreed order and opens on the first, ahead of the meetings", async () => {
    setup();
    expect(STEPS.map((s) => s.title)).toEqual(["Summaries", "Download", "About me", "Optional extras"]);
    await stepHeading("Summaries");
    expect(screen.getByRole("progressbar", { name: "Step 1 of 4" })).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "Meetings and settings" })).toBeNull();
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

  it("going straight through counts as done and it is shown once, with no Skip this step", async () => {
    const onCall = vi.fn();
    const s = setup({ firstRun: { show: true, onCall } });
    for (const [i, step] of STEPS.entries()) {
      await stepHeading(step.title);
      expect(screen.getByRole("progressbar", { name: `Step ${i + 1} of 4` })).toBeInTheDocument();
      await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("heading", { level: 1 })));
      expect(screen.queryByRole("button", { name: "Skip this step" })).toBeNull();
      await s.user.click(button(i === STEPS.length - 1 ? "Finish" : "Next"));
    }
    await screen.findByRole("heading", HOME);
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
    await stepHeading(STEPS[1].title);
    await s.user.click(button("Skip setup"));
    await screen.findByRole("heading", HOME);
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
    await screen.findByRole("heading", HOME);
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
      await s.ipc.startMeeting("Call", null, false);
    });
    advance(s.timer, 1);
    await screen.findByRole("button", { name: /^Stop/ });
    expect(screen.queryByRole("progressbar", { name: /^Step/ })).toBeNull();
  });

  it("starts no download, check, sample or sound on any step until a button is pressed", async () => {
    const s = setup(CLEAN);
    const spies = [
      vi.spyOn(s.ipc, "startDownload"),
      vi.spyOn(s.ipc, "runCheck"),
      vi.spyOn(s.ipc, "startAudioCheck"),
      vi.spyOn(s.ipc, "startVoiceSample"),
    ];
    await toStep(s, 3);
    await s.user.click(button("Open Audio check"));
    await screen.findByText(CHECK_WARNING);
    advance(s.timer, VOICE_SAMPLE_MS);
    for (const spy of spies) expect(spy).not.toHaveBeenCalled();
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

  describe("Summaries", () => {
    it("preselects this computer and offers Claude Code, Codex and none, each saying what is sent", async () => {
      const s = setup();
      await screen.findByRole("radiogroup", { name: "Who writes the summary" });
      expect(screen.getByRole("radio", { name: /^On this computer/ })).toBeChecked();
      expect(screen.getAllByRole("radio").map((r) => r.getAttribute("value"))).toEqual([
        "OnThisComputer",
        "ClaudeCode",
        "Codex",
        "NoSummary",
      ]);
      expect(screen.getByText("Nothing leaves this computer.")).toBeInTheDocument();
      expect(screen.getByText(CLAUDE_CODE_PRIVACY)).toBeInTheDocument();
      expect(screen.getByText(CODEX_PRIVACY)).toBeInTheDocument();
      await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
      await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("NoSummary"));
    });

    it("keeps your own endpoint under Other, with its words", async () => {
      const s = setup();
      const group = await screen.findByRole("radiogroup", { name: "Who writes the summary" });
      expect(screen.queryByRole("radio", { name: /^Your own server/ })).toBeNull();
      // Other is a button, so it sits just after the group, not among its radios.
      expect(within(group).queryByRole("button", { name: "Other" })).toBeNull();
      await s.user.click(screen.getByRole("button", { name: "Other", expanded: false }));
      expect(await screen.findByRole("radio", { name: /^Your own server/ })).not.toBeChecked();
      expect(screen.getByText(/are sent to the address you enter, and only there/)).toBeInTheDocument();
    });

    it("shows a stored endpoint with nothing pressed", async () => {
      setup({
        summaries: { settings: { writer: "OwnEndpoint", url: "http://127.0.0.1:8080/v1", model: "local" } },
      });
      expect(await screen.findByRole("radio", { name: /^Your own server/ })).toBeChecked();
    });

    it("Next saves a typed endpoint, with no Save button", async () => {
      const s = setup();
      await s.user.click(await screen.findByRole("button", { name: "Other" }));
      await s.user.click(await screen.findByRole("radio", { name: /^Your own server/ }));
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      expect(form.queryByRole("button", { name: /endpoint/ })).toBeNull();
      await s.user.type(form.getByRole("textbox", { name: /^Address/ }), "http://127.0.0.1:8080/v1");
      await s.user.type(form.getByRole("textbox", { name: /^Model/ }), "local");
      await s.user.click(button("Next"));
      await stepHeading(STEPS[1].title);
      expect((await s.ipc.summarySettings()).value).toMatchObject({
        writer: "OwnEndpoint",
        url: "http://127.0.0.1:8080/v1",
        model: "local",
      });
    });

    it("a refused endpoint stays on the step and says why, and another choice gets past it", async () => {
      const s = setup();
      await s.user.click(await screen.findByRole("button", { name: "Other" }));
      await s.user.click(await screen.findByRole("radio", { name: /^Your own server/ }));
      await screen.findByRole("group", { name: "Your own server" });
      await s.user.click(button("Next"));
      expect(await screen.findByText("Enter the endpoint's address.")).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[0].title);
      await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
      await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("NoSummary"));
      await s.user.click(button("Next"));
      await stepHeading(STEPS[1].title);
    });

    it("Next saves a typed API key with the endpoint", async () => {
      const s = setup();
      await s.user.click(await screen.findByRole("button", { name: "Other" }));
      await s.user.click(await screen.findByRole("radio", { name: /^Your own server/ }));
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      await s.user.selectOptions(form.getByRole("combobox", { name: "Provider" }), "Anthropic");
      await s.user.type(form.getByRole("textbox", { name: /^Model/ }), "a-model");
      await s.user.type(await screen.findByLabelText(/^API key/), "test-key");
      expect(form.queryByRole("button", { name: "Save key" })).toBeNull();
      await s.user.click(button("Next"));
      await stepHeading(STEPS[1].title);
      expect(await s.ipc.hasApiKey()).toBe(true);
    });

    it("Next saves a typed Claude Code model, with no Save button", async () => {
      const s = setup();
      await s.user.click(await screen.findByRole("radio", { name: /^Claude \(through Claude Code\)/ }));
      const card = within(await screen.findByRole("group", { name: "Claude through Claude Code" }));
      expect(card.queryByRole("button", { name: "Save model" })).toBeNull();
      await s.user.selectOptions(card.getByRole("combobox", { name: "Model" }), "Other");
      expect(card.queryByRole("button", { name: "Save model" })).toBeNull();
      await s.user.type(card.getByRole("textbox", { name: /^Model name/ }), "opus-custom");
      await s.user.click(button("Next"));
      await stepHeading(STEPS[1].title);
      expect((await s.ipc.summarySettings()).value).toMatchObject({ writer: "ClaudeCode", claude_model: "opus-custom" });
    });

    it("applies a listed Claude Code model and effort at once, as the Summaries page does", async () => {
      const s = setup();
      await s.user.click(await screen.findByRole("radio", { name: /^Claude \(through Claude Code\)/ }));
      const card = within(await screen.findByRole("group", { name: "Claude through Claude Code" }));
      await s.user.selectOptions(card.getByRole("combobox", { name: "Model" }), "Haiku");
      await s.user.selectOptions(card.getByRole("combobox", { name: "Effort" }), "Medium");
      await waitFor(async () =>
        expect((await s.ipc.summarySettings()).value).toMatchObject({
          writer: "ClaudeCode",
          claude_model: "haiku",
          claude_effort: "Medium",
        }),
      );
      expect(card.queryByRole("button", { name: /^Save/ })).toBeNull();
    });
  });

  describe("Download", () => {
    const THIS_COMPUTER = ["speech-helpers", "parakeet-tdt-0.6b-v2", "qwen3.5-4b-q4_k_m"];

    it("asks for the languages first, then states the total for this computer on one button", async () => {
      const s = setup(CLEAN);
      await toStep(s, 1);
      const languages = await screen.findByRole("combobox", { name: "Meeting languages" });
      const line = await screen.findByText(
        `Earshot needs ${await totalOf(s.ipc, THIS_COMPUTER)} for this computer: Speech helpers, Parakeet 0.6B v2 and Qwen 3.5 4B.`,
      );
      expect(languages.compareDocumentPosition(line) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
      expect(button("Download")).toBeEnabled();
      // The full list waits for its button, and Later and Never are gone with it.
      expect(screen.queryByRole("region", { name: "What every meeting needs" })).toBeNull();
      await s.user.click(button("Choose different models"));
      expect(await screen.findByRole("region", { name: "What every meeting needs" })).toBeInTheDocument();
      expect(screen.getByRole("region", { name: "Recommendation" })).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Later" })).toBeNull();
      expect(screen.queryByRole("button", { name: "Never" })).toBeNull();
    });

    it("leaves the summary model out unless the Summaries step chose this computer", async () => {
      const s = setup(CLEAN);
      await s.user.click(await screen.findByRole("radio", { name: /^OpenAI \(through Codex\)/ }));
      await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("Codex"));
      await s.user.click(button("Next"));
      await stepHeading(STEPS[1].title);
      const total = await totalOf(s.ipc, ["speech-helpers", "parakeet-tdt-0.6b-v2"]);
      await screen.findByText(`Earshot needs ${total} for this computer: Speech helpers and Parakeet 0.6B v2.`);
      await s.user.click(button("Back"));
      await s.user.click(await screen.findByRole("radio", { name: /^On this computer/ }));
      await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("OnThisComputer"));
      await s.user.click(button("Next"));
      await screen.findByText(new RegExp(`^Earshot needs ${await totalOf(s.ipc, THIS_COMPUTER)} .* Qwen 3\\.5 4B\\.$`));
    });

    it("follows the languages to another model", async () => {
      const s = setup({ ...CLEAN, machine: "desktop" });
      await toStep(s, 1);
      await s.user.click(await screen.findByRole("combobox", { name: "Meeting languages" }));
      await s.user.click(await screen.findByRole("menuitemcheckbox", { name: "German" }));
      await screen.findByText(/for this computer: Speech helpers, Parakeet 0\.6B v3 and Qwen 3\.5 9B\.$/);
    });

    it("says so when no recommended model covers the languages, and leaves the rest to get", async () => {
      const s = setup(CLEAN);
      const models = s.ipc.models.bind(s.ipc);
      vi.spyOn(s.ipc, "models").mockImplementation(async () => ({ ...(await models()), recommended: null }));
      await toStep(s, 1);
      await screen.findByText(NO_MODEL_FOR_LANGUAGES);
      await screen.findByText(/for this computer: Speech helpers and Qwen 3\.5 4B\.$/);
    });

    it("offers the NPU versions in the full list on an ARM64 computer, and fetches one", async () => {
      const s = setup(CLEAN);
      const start = vi.spyOn(s.ipc, "startDownload");
      await toStep(s, 1);
      await s.user.click(await screen.findByRole("button", { name: "Choose different models" }));
      const npu = await screen.findByRole("region", { name: "NPU versions" });
      await s.user.click(within(npu).getByRole("button", { name: /^Download Parakeet 0\.6B v2 \(NPU version\)/ }));
      await waitFor(() => expect(start.mock.calls.map(([m]) => m)).toEqual(["parakeet-tdt-0.6b-v2-npu"]));
    });

    it("has no NPU versions on a computer without them", async () => {
      const s = setup({ ...CLEAN, machine: "desktop" });
      await toStep(s, 1);
      await s.user.click(await screen.findByRole("button", { name: "Choose different models" }));
      await screen.findByRole("region", { name: "What every meeting needs" });
      expect(screen.queryByRole("region", { name: "NPU versions" })).toBeNull();
    });

    it("follows a model chosen from the full list", async () => {
      const s = setup(CLEAN);
      await toStep(s, 1);
      await s.user.click(await screen.findByRole("button", { name: "Choose different models" }));
      await s.user.click(await screen.findByRole("radio", { name: /^Parakeet 110M/ }));
      await screen.findByText(/for this computer: Speech helpers, Parakeet 110M and Qwen 3\.5 4B\.$/);
    });

    it("downloads everything on the one button, goes on through the other steps and after finishing", async () => {
      const s = setup(CLEAN);
      const start = vi.spyOn(s.ipc, "startDownload");
      const cancel = vi.spyOn(s.ipc, "cancelDownload");
      await toStep(s, 1);
      await screen.findByText(/^Earshot needs/);
      expect(start).not.toHaveBeenCalled();
      await s.user.click(button("Download"));
      await waitFor(() => expect(start.mock.calls.map(([m]) => m)).toEqual(THIS_COMPUTER));
      expect(await screen.findByRole("progressbar", { name: "Downloads for this computer" })).toBeInTheDocument();
      expect(screen.getByText(new RegExp(DOWNLOADS_CARRY_ON))).toBeInTheDocument();
      await waitFor(() => expect(screen.queryByRole("button", { name: "Download" })).toBeNull());
      await s.user.click(button("Next"));
      await stepHeading(STEPS[2].title);
      expect(
        await screen.findByText(stillDownloading(["Speech helpers", "Parakeet 0.6B v2", "Qwen 3.5 4B"])),
      ).toBeInTheDocument();
      advance(s.timer, MODEL_STEP_MS * 3);
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", HOME);
      advance(s.timer, MODEL_STEP_MS * 7);
      await waitFor(async () => {
        const m = (await s.ipc.models()).transcription.find((x) => x.name === "parakeet-tdt-0.6b-v2");
        expect(m?.installed).toBe("Yes");
      });
      expect(cancel).not.toHaveBeenCalled();
    });

    it("has no button when this computer has everything", async () => {
      const s = setup({ machine: "desktop" });
      await toStep(s, 1);
      await screen.findByText(NOTHING_TO_DOWNLOAD);
      expect(screen.queryByRole("button", { name: "Download" })).toBeNull();
      expect(button("Choose different models")).toBeEnabled();
    });

    it("counts only what is missing", async () => {
      const s = setup({ cleanInstall: true });
      await toStep(s, 1);
      await screen.findByText(
        `Earshot needs ${await totalOf(s.ipc, ["speech-helpers"])} for this computer: Speech helpers.`,
      );
    });

    it("shows why a download was refused", async () => {
      const s = setup(CLEAN);
      vi.spyOn(s.ipc, "startDownload").mockRejectedValueOnce(new Error("The disk is full."));
      await toStep(s, 1);
      await screen.findByText(/^Earshot needs/);
      await s.user.click(button("Download"));
      expect(await screen.findByText("The disk is full.")).toBeInTheDocument();
    });
  });

  describe("About me", () => {
    it("Next saves your name, with no Save button", async () => {
      const s = setup();
      await toStep(s, 2);
      await s.user.type(await screen.findByRole("textbox", { name: /^Your name/ }), "Casey");
      expect(screen.queryByRole("button", { name: /^Save/ })).toBeNull();
      await s.user.click(button("Next"));
      await stepHeading(STEPS[3].title);
      expect((await s.ipc.speakers()).voice.name).toBe("Casey");
    });

    it("a refused name stays on the step and says why", async () => {
      const s = setup();
      await toStep(s, 2);
      const name = await screen.findByRole("textbox", { name: /^Your name/ });
      await s.user.click(name);
      await s.user.paste("x".repeat(101));
      await s.user.click(button("Next"));
      expect(await screen.findByText("That name is too long.")).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[2].title);
      await s.user.clear(name);
      await s.user.click(button("Next"));
      await stepHeading(STEPS[3].title);
    });

    it("the voice sample has its own Skip, which saves the name and goes on", async () => {
      const s = setup();
      const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
      await toStep(s, 2);
      await s.user.type(await screen.findByRole("textbox", { name: /^Your name/ }), "Casey");
      await s.user.click(button("Skip the voice sample"));
      await stepHeading(STEPS[3].title);
      expect((await s.ipc.speakers()).voice.name).toBe("Casey");
      expect(startVoice).not.toHaveBeenCalled();
    });

    it("asks before recording your voice, and shows why the app refused", async () => {
      const s = setup({
        settings: { voiceStartError: { kind: "busy", message: "A meeting is running.", retryable: true } },
      });
      const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
      await toStep(s, 2);
      await s.user.click(await screen.findByRole("button", { name: "Record my voice" }));
      const dialog = await screen.findByRole("alertdialog", { name: "Save a voiceprint of your voice?" });
      expect(startVoice).not.toHaveBeenCalled();
      await s.user.click(within(dialog).getByRole("button", { name: "Record and save" }));
      await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
      expect(startVoice).toHaveBeenCalledTimes(1);
      await screen.findByText("A meeting is running.");
      expect(await screen.findByRole("button", { name: "Next" })).toBeEnabled();
    });

    it("holds the buttons while the sample runs", async () => {
      const s = setup();
      await toStep(s, 2);
      await s.user.click(await screen.findByRole("button", { name: "Record my voice" }));
      const dialog = await screen.findByRole("alertdialog");
      await s.user.click(within(dialog).getByRole("button", { name: "Record and save" }));
      await screen.findByText(RUNNING_NOTE);
      // Right after a Dialog closes tabster hides the page briefly (AGENTS.md): wait for it.
      for (const name of ["Next", "Back", "Skip setup"]) {
        expect(await screen.findByRole("button", { name })).toBeDisabled();
      }
      expect(screen.queryByRole("button", { name: "Skip the voice sample" })).toBeNull();
    });

    it("while the helpers are missing the record button is off and says why, starting nothing", async () => {
      const s = setup(CLEAN);
      const start = vi.spyOn(s.ipc, "startDownload");
      const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
      await toStep(s, 2);
      expect(await screen.findByRole("button", { name: "Record my voice" })).toBeDisabled();
      expect(screen.getByText(HELPERS_MISSING)).toBeInTheDocument();
      expect(button("Skip the voice sample")).toBeEnabled();
      expect(start).not.toHaveBeenCalled();
      expect(startVoice).not.toHaveBeenCalled();
    });

    it("the record button turns on by itself when the helpers arrive, and records nothing", async () => {
      const s = setup(CLEAN);
      const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
      await toStep(s, 1);
      await s.user.click(await screen.findByRole("button", { name: "Download" }));
      await waitFor(() => expect(screen.queryByRole("button", { name: "Download" })).toBeNull());
      await s.user.click(button("Next"));
      await stepHeading(STEPS[2].title);
      expect(await screen.findByRole("button", { name: "Record my voice" })).toBeDisabled();
      expect(screen.getByText(HELPERS_ARRIVING)).toBeInTheDocument();
      advance(s.timer, MODEL_STEP_MS * 10);
      await waitFor(() => expect(button("Record my voice")).toBeEnabled());
      expect(screen.queryByText(HELPERS_ARRIVING)).toBeNull();
      expect(screen.queryByRole("alertdialog")).toBeNull();
      expect(startVoice).not.toHaveBeenCalled();
    });
  });

  describe("Optional extras", () => {
    it("is two rows that open when pressed, and then Finish", async () => {
      const s = setup();
      await toStep(s, 3);
      expect(screen.getByText(OBSIDIAN_ROW)).toBeInTheDocument();
      expect(screen.getByText(AUDIO_CHECK_ROW)).toBeInTheDocument();
      expect(screen.queryByText(EXPORT_EXPLAINED)).toBeNull();
      expect(screen.queryByText(CHECK_WARNING)).toBeNull();
      await s.user.click(button("Open Obsidian"));
      expect(await screen.findByText(EXPORT_EXPLAINED)).toBeInTheDocument();
      expect(screen.queryByText(CHECK_WARNING)).toBeNull();
      await s.user.click(button("Open Audio check"));
      expect(await screen.findByText(CHECK_WARNING)).toBeInTheDocument();
      expect(screen.getByText(DEVICES_NOTE)).toBeInTheDocument();
      expect(button("Finish")).toBeEnabled();
    });

    it("Finish saves the Obsidian vault and folder, with no Save button", async () => {
      const s = setup({ settings: { pickedVault: "C:\\Notes" } });
      await toStep(s, 3);
      await s.user.click(button("Open Obsidian"));
      await s.user.click(await screen.findByRole("button", { name: "Choose folder..." }));
      expect(await screen.findByDisplayValue("C:\\Notes")).toBeInTheDocument();
      const folder = screen.getByRole("textbox", { name: /^Subfolder/ });
      await s.user.clear(folder);
      await s.user.type(folder, "Work");
      expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
      await s.user.click(button("Finish"));
      await screen.findByRole("heading", HOME);
      expect((await s.ipc.obsidianSettings()).value).toMatchObject({ vault: "C:\\Notes", folder: "Work" });
    });

    it("a refused vault stays on the step and says why", async () => {
      const s = setup();
      await toStep(s, 3);
      await s.user.click(button("Open Obsidian"));
      await s.user.type(await screen.findByRole("textbox", { name: /^Vault folder/ }), "not a path");
      await s.user.click(button("Finish"));
      expect(await screen.findByText(/The vault must be a full folder path/)).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[3].title);
      expect((await s.ipc.firstRunState()).show).toBe(true);
    });

    it("Skip setup and Back leave typed text unsaved", async () => {
      const s = setup({ settings: { pickedVault: "C:\\Notes" } });
      await toStep(s, 3);
      await s.user.click(button("Open Obsidian"));
      await s.user.click(await screen.findByRole("button", { name: "Choose folder..." }));
      await screen.findByDisplayValue("C:\\Notes");
      await s.user.click(button("Back"));
      await stepHeading(STEPS[2].title);
      expect((await s.ipc.obsidianSettings()).value.vault).toBe("");
      await s.user.click(button("Next"));
      await s.user.click(await screen.findByRole("button", { name: "Open Obsidian" }));
      await s.user.click(await screen.findByRole("button", { name: "Choose folder..." }));
      await screen.findByDisplayValue("C:\\Notes");
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", HOME);
      expect((await s.ipc.obsidianSettings()).value.vault).toBe("");
    });

    it("plays nothing until its button, and holds the buttons while the check runs", async () => {
      const s = setup();
      const startCheck = vi.spyOn(s.ipc, "startAudioCheck");
      await toStep(s, 3);
      await s.user.click(button("Open Audio check"));
      await screen.findByText(CHECK_WARNING);
      expect(startCheck).not.toHaveBeenCalled();
      await s.user.click(button("Run audio check"));
      expect(startCheck).toHaveBeenCalledTimes(1);
      await screen.findByText(RUNNING_NOTE);
      for (const name of ["Finish", "Back", "Skip setup"]) expect(button(name)).toBeDisabled();
      advance(s.timer, CHECK_TOTAL_MS);
      await screen.findByRole("group", { name: "Last audio check" });
      await waitFor(() => expect(button("Finish")).toBeEnabled());
    });

    it("shows why the app refused the audio check", async () => {
      const s = setup({
        audioCheck: { startError: { kind: "busy", message: "The model check is running.", retryable: true } },
      });
      await toStep(s, 3);
      await s.user.click(button("Open Audio check"));
      await s.user.click(await screen.findByRole("button", { name: "Run audio check" }));
      await screen.findByText("The model check is running.");
      expect(button("Finish")).toBeEnabled();
    });
  });

  describe("with no transcription model", () => {
    it("Skip setup says meetings need one and stops nothing, until dismissed", async () => {
      const s = setup(CLEAN);
      await toStep(s, 1);
      await screen.findByText(/^Earshot needs/);
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", HOME);
      expect(screen.getByText(NO_MODEL_NOTE)).toBeInTheDocument();
      expect((await s.ipc.firstRunState()).show).toBe(false);
      for (const b of screen.getAllByRole("button", { name: "New meeting" })) expect(b).toBeEnabled();
      await s.user.click(button("Dismiss"));
      await waitFor(() => expect(screen.queryByText(NO_MODEL_NOTE)).toBeNull());
    });

    it("Finish says the same", async () => {
      const s = setup(CLEAN);
      await toStep(s, 3);
      await s.user.click(button("Finish"));
      await screen.findByRole("heading", HOME);
      expect(screen.getByText(NO_MODEL_NOTE)).toBeInTheDocument();
    });

    it("the words reach Settings when the wizard was opened from there", async () => {
      const s = setup({ ...CLEAN, firstRun: { show: false } });
      await s.user.click(await screen.findByRole("button", { name: "Settings" }));
      const nav = await screen.findByRole("navigation", { name: "Settings sections" });
      await s.user.click(within(nav).getByRole("button", { name: "General" }));
      await s.user.click(await screen.findByRole("button", { name: "Open the setup guide" }));
      await toStep(s, 1);
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", { level: 1, name: "General" });
      expect(screen.getByText(NO_MODEL_NOTE)).toBeInTheDocument();
    });

    it("says nothing while one is downloading, or when one is here", async () => {
      const s = setup(CLEAN);
      await toStep(s, 1);
      await s.user.click(await screen.findByRole("button", { name: "Download" }));
      await waitFor(() => expect(screen.queryByRole("button", { name: "Download" })).toBeNull());
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", HOME);
      expect(screen.queryByText(NO_MODEL_NOTE)).toBeNull();
      cleanup();
      const t = setup();
      await toStep(t, 3);
      await t.user.click(button("Finish"));
      await screen.findByRole("heading", HOME);
      expect(screen.queryByText(NO_MODEL_NOTE)).toBeNull();
    });
  });

  it("uses no em dash in its copy", () => {
    const words = [
      AUDIO_CHECK_ROW,
      DEVICES_NOTE,
      DOWNLOADS_CARRY_ON,
      HELPERS_ARRIVING,
      HELPERS_MISSING,
      NOTHING_TO_DOWNLOAD,
      NO_MODEL_FOR_LANGUAGES,
      NO_MODEL_NOTE,
      OBSIDIAN_ROW,
      RUNNING_NOTE,
      SAVE_REFUSED_NOTE,
      SETUP_EXPLAINED,
      ...STEPS.flatMap((s) => [s.title, s.intro]),
    ];
    for (const w of words) expect(w).not.toContain("\u2014");
  });
});
