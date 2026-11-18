import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import syncCopy from "../../../../fixtures/ui/sync_copy_cases.json";
import {
  type Ipc,
  MOCK_SYNC_CODE,
  MOCK_SYNC_SECRET,
  type MockIpcData,
  ORDER_COPY,
  createMockIpc,
  fillCopy,
  mockDeviceId,
} from "../ipc";
import { CHECK_TOTAL_MS } from "../ipc/mockAudioCheck";
import { MODEL_STEP_MS } from "../ipc/mockModels";
import { VOICE_SAMPLE_MS } from "../ipc/mockSettings";
import { CHECK_WARNING } from "../settings/AudioCheckSection";
import { SETUP_EXPLAINED } from "../settings/GeneralPage";
import { EXPORT_EXPLAINED } from "../settings/ObsidianSection";
import { CLAUDE_CODE_PRIVACY, CODEX_PRIVACY, remoteWarning } from "../settings/SummariesPage";
import { SYNC_INTRO } from "../settings/SyncPage";
import { PREVIEWS_HEADING, PREVIEWS_SUGGESTED } from "../settings/TranscriptionPage";
import { formatSize } from "../settings/copy";
import { openSettings, renderApp } from "../test/app";
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
  SYNC_JOIN,
  SYNC_JOINED,
  SYNC_LATER,
  SYNC_LATER_NOTE,
  SYNC_QUESTION,
  SYNC_SET_UP,
  SYNC_STILL_READING,
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
    renderApp(ipc, { timer });
  mount();
  return { ipc, timer, user: userEvent.setup(), mount };
}

/** A machine straight after installing: no helpers, no models. */
const CLEAN = { cleanInstall: true, noModels: true };

const stepHeading = (title: string) => screen.findByRole("heading", { level: 1, name: title });
const button = (name: string | RegExp) => screen.getByRole("button", { name });
const HOME = { name: "Record your first meeting" };

/** The Summaries step's list of writers: the Summaries page's own (T12.68). */
const PAGE = ORDER_COPY.page;
const LOCAL_LABEL = ORDER_COPY.steps.local;
const CLAUDE_LABEL = ORDER_COPY.steps.claude_code;
const CODEX_LABEL = ORDER_COPY.steps.codex;
const tick = (writer: string) => screen.getByRole("checkbox", { name: fillCopy(PAGE.use, { writer }) });
const up = (writer: string) => screen.queryByRole("button", { name: fillCopy(PAGE.move_up, { writer }) });
const stored = async (ipc: Ipc) => (await ipc.summaryOrder()).steps.map((s) => s.step);

/** The steps by name, for `toStep` and `STEPS[..]`. */
const SYNC = STEPS.findIndex((s) => s.title === "Sync");
const DOWNLOAD = STEPS.findIndex((s) => s.title === "Download");
const ABOUT_ME = STEPS.findIndex((s) => s.title === "About me");
const EXTRAS = STEPS.findIndex((s) => s.title === "Optional extras");

async function toStep(s: Setup, index: number) {
  await toStepFrom(s, 0, index);
}

/** Next from step `from` (shown) to step `to`. */
async function toStepFrom(s: Setup, from: number, to: number) {
  await stepHeading(STEPS[from].title);
  // The steps' own parts read on mount; Next before they finish would save nothing anyway.
  for (let i = from; i < to; i++) {
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

describe("First run wizard", () => {
  it("has its steps in the agreed order and opens on the first, ahead of the meetings", async () => {
    setup();
    expect(STEPS.map((s) => s.title)).toEqual(["Summaries", "Sync", "Download", "About me", "Optional extras"]);
    await stepHeading("Summaries");
    expect(screen.getByRole("progressbar", { name: `Step 1 of ${STEPS.length}` })).toBeInTheDocument();
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
      expect(screen.getByRole("progressbar", { name: `Step ${i + 1} of ${STEPS.length}` })).toBeInTheDocument();
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
    await openSettings(s.user, "General");
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
    await toStep(s, EXTRAS);
    await s.user.click(button("Open Audio check"));
    await screen.findByText(CHECK_WARNING);
    advance(s.timer, VOICE_SAMPLE_MS);
    for (const spy of spies) expect(spy).not.toHaveBeenCalled();
  });

  it("holds one subscription to model job events across the steps", async () => {
    const timer = createManualTimer(1_000_000);
    const ipc = createMockIpc({ meetings: [], timer, firstRun: { show: true } });
    const subscribe = vi.spyOn(ipc, "onModelJobEvent");
    renderApp(ipc, { timer });
    const user = userEvent.setup();
    for (const [i, step] of STEPS.entries()) {
      await stepHeading(step.title);
      if (i < STEPS.length - 1) await user.click(button("Next"));
    }
    expect(subscribe).toHaveBeenCalledTimes(1);
  });

  describe("Summaries", () => {
    it("is the Summaries page's list of writers, this computer ticked, each saying what is sent", async () => {
      setup();
      await screen.findByRole("list", { name: PAGE.list });
      await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
      expect(tick(LOCAL_LABEL)).toBeChecked();
      expect(tick(CLAUDE_LABEL)).not.toBeChecked();
      expect(tick(CODEX_LABEL)).not.toBeChecked();
      // The one-writer radios are gone: the only radios are Write summaries and No summary.
      expect(screen.getAllByRole("radio").map((r) => r.getAttribute("value"))).toEqual(["Write", "NoSummary"]);
      expect(screen.queryByRole("radio", { name: /^On this computer/ })).toBeNull();
      expect(screen.queryByRole("button", { name: "Other" })).toBeNull();
      expect(screen.getByText(CLAUDE_CODE_PRIVACY)).toBeInTheDocument();
      expect(screen.getByText(CODEX_PRIVACY)).toBeInTheDocument();
      expect(screen.getByText(PAGE.kept_local)).toBeInTheDocument();
    });

    it("ticks and orders writers, each saved at once, and Next goes on", async () => {
      const s = setup();
      await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
      await s.user.click(tick(CLAUDE_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
      await s.user.click(tick(CODEX_LABEL));
      await waitFor(async () =>
        expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }, { kind: "Codex" }]),
      );
      await waitFor(() => expect(up(CODEX_LABEL)).toBeEnabled());
      await s.user.click(up(CODEX_LABEL)!);
      await waitFor(async () =>
        expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Codex" }, { kind: "ClaudeCode" }]),
      );
      await s.user.click(tick(LOCAL_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }, { kind: "ClaudeCode" }]));
      // The old writer setting follows the first writer.
      expect((await s.ipc.summarySettings()).value.writer).toBe("Codex");
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
    });

    it("keeps an order of several writers, from another computer too", async () => {
      const desk = { kind: "Computer" as const, device: mockDeviceId("e4a17d30") };
      const s = setup({ sync: { setUp: true }, summaryOrder: [desk, { kind: "Local" }, { kind: "ClaudeCode" }] });
      await waitFor(() => expect(tick(LOCAL_LABEL)).toBeEnabled());
      expect(tick(LOCAL_LABEL)).toBeChecked();
      expect(tick(CLAUDE_LABEL)).toBeChecked();
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
      expect(await stored(s.ipc)).toEqual([desk, { kind: "Local" }, { kind: "ClaudeCode" }]);
    });

    it("No summary stores no order", async () => {
      const s = setup();
      await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
      await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
      await waitFor(async () => expect((await s.ipc.summaryOrder()).none).toBe(true));
      expect((await s.ipc.summarySettings()).value.writer).toBe("NoSummary");
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
    });

    it("lets this computer be ticked before its model is downloaded, which the Download step fetches", async () => {
      const s = setup(CLEAN);
      await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
      expect(screen.queryByText(PAGE.not_downloaded)).toBeNull();
      await s.user.click(tick(CLAUDE_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]));
      await s.user.click(tick(LOCAL_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "ClaudeCode" }]));
      await waitFor(() => expect(tick(LOCAL_LABEL)).toBeEnabled());
      await s.user.click(tick(LOCAL_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "ClaudeCode" }, { kind: "Local" }]));
      // The summary model is chosen on the Download step, not here.
      expect(screen.queryByRole("group", { name: "Which model writes the summary" })).toBeNull();
    });

    it("Next saves a typed endpoint and ticks it, with no Save button", async () => {
      const s = setup();
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      expect(form.queryByRole("button", { name: /endpoint/ })).toBeNull();
      await s.user.type(form.getByRole("textbox", { name: /^Address/ }), "http://127.0.0.1:8080/v1");
      await s.user.type(form.getByRole("textbox", { name: /^Model/ }), "local");
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Endpoint" }]);
      expect((await s.ipc.summarySettings()).value).toMatchObject({
        url: "http://127.0.0.1:8080/v1",
        model: "local",
      });
    });

    it("a refused endpoint stays on the step and says why, and emptying the form gets past it", async () => {
      const s = setup();
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      const address = form.getByRole("textbox", { name: /^Address/ });
      await s.user.type(address, "http://127.0.0.1:8080/v1");
      await s.user.click(button("Next"));
      expect(await screen.findByText("Enter the model name the endpoint expects.")).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[0].title);
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
      await s.user.clear(address);
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
    });

    it("choosing Anthropic alone fills in its address but holds nothing, and switching back undoes it", async () => {
      const s = setup();
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      const address = form.getByRole("textbox", { name: /^Address/ });
      const anthropicUrl = (await s.ipc.summarySettings()).anthropic_url;
      await s.user.selectOptions(form.getByRole("combobox", { name: "Provider" }), "Anthropic");
      expect(address).toHaveValue(anthropicUrl);
      await s.user.selectOptions(form.getByRole("combobox", { name: "Provider" }), "OpenAiCompatible");
      expect(address).toHaveValue("");
      await s.user.selectOptions(form.getByRole("combobox", { name: "Provider" }), "Anthropic");
      expect(address).toHaveValue(anthropicUrl);
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
      expect(screen.queryByText(SAVE_REFUSED_NOTE)).toBeNull();
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
      expect((await s.ipc.summarySettings()).value).toMatchObject({ url: "", model: "" });
    });

    it("a typed API key with only Anthropic's own address asks for the model, and saves no key", async () => {
      const s = setup();
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      await s.user.selectOptions(form.getByRole("combobox", { name: "Provider" }), "Anthropic");
      await s.user.type(await screen.findByLabelText(/^API key/), "test-key");
      await s.user.click(button("Next"));
      expect(await screen.findByText("Enter the model name the endpoint expects.")).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[0].title);
      expect(await s.ipc.hasApiKey()).toBe(false);
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }]);
    });

    it("names where the transcript would go as soon as an address that is not local is typed", async () => {
      const s = setup();
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      const address = form.getByRole("textbox", { name: /^Address/ });
      await s.user.type(address, "http://192.168.1.20:8080/v1");
      expect(screen.queryByText(remoteWarning("192.168.1.20"))).toBeNull();
      await s.user.clear(address);
      await s.user.type(address, "https://llm.example.com/v1");
      expect(await screen.findByText(remoteWarning("llm.example.com"))).toBeInTheDocument();
      // Nothing is stored until Next.
      expect((await s.ipc.summarySettings()).value.url).toBe("");
      await s.user.clear(address);
      await waitFor(() => expect(screen.queryByText(remoteWarning("llm.example.com"))).toBeNull());
    });

    it("Next saves a typed API key with the endpoint", async () => {
      const s = setup();
      const form = within(await screen.findByRole("group", { name: "Your own server" }));
      await s.user.selectOptions(form.getByRole("combobox", { name: "Provider" }), "Anthropic");
      await s.user.type(form.getByRole("textbox", { name: /^Model/ }), "a-model");
      await s.user.type(await screen.findByLabelText(/^API key/), "test-key");
      expect(form.queryByRole("button", { name: "Save key" })).toBeNull();
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
      expect(await s.ipc.hasApiKey()).toBe(true);
    });

    it("Next saves a typed Claude Code model, with no Save button", async () => {
      const s = setup();
      await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
      await s.user.click(tick(CLAUDE_LABEL));
      const card = within(await screen.findByRole("group", { name: "Claude through Claude Code" }));
      expect(card.queryByRole("button", { name: "Save model" })).toBeNull();
      await s.user.selectOptions(card.getByRole("combobox", { name: "Model" }), "Other");
      expect(card.queryByRole("button", { name: "Save model" })).toBeNull();
      await s.user.type(card.getByRole("textbox", { name: /^Model name/ }), "opus-custom");
      await s.user.click(button("Next"));
      await stepHeading(STEPS[SYNC].title);
      expect((await s.ipc.summarySettings()).value).toMatchObject({ claude_model: "opus-custom" });
      expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "ClaudeCode" }]);
    });

    it("applies a listed Claude Code model and effort at once, as the Summaries page does", async () => {
      const s = setup();
      await waitFor(() => expect(tick(CLAUDE_LABEL)).toBeEnabled());
      await s.user.click(tick(CLAUDE_LABEL));
      const card = within(await screen.findByRole("group", { name: "Claude through Claude Code" }));
      await s.user.selectOptions(card.getByRole("combobox", { name: "Model" }), "Haiku");
      await s.user.selectOptions(card.getByRole("combobox", { name: "Effort" }), "Medium");
      await waitFor(async () =>
        expect((await s.ipc.summarySettings()).value).toMatchObject({
          claude_model: "haiku",
          claude_effort: "Medium",
        }),
      );
      expect(card.queryByRole("button", { name: /^Save/ })).toBeNull();
    });
  });

  describe("Sync", () => {
    const SYNC_CALLS = [
      "syncView",
      "syncDevices",
      "setUpSync",
      "joinSync",
      "offerSyncCode",
      "setSyncOn",
    ] as const;
    const syncSpies = (ipc: Ipc) => SYNC_CALLS.map((name) => vi.spyOn(ipc, name));
    const choice = (name: string) => screen.getByRole("radio", { name });

    it("asks about another computer with Not now chosen, and Not now asks the app nothing", async () => {
      const s = setup();
      const spies = syncSpies(s.ipc);
      await toStep(s, SYNC);
      const group = screen.getByRole("radiogroup", { name: SYNC_QUESTION });
      expect(within(group).getAllByRole("radio").map((r) => r.getAttribute("value"))).toEqual([
        "join",
        "setup",
        "later",
      ]);
      expect(choice(SYNC_LATER)).toBeChecked();
      expect(screen.getByText(SYNC_INTRO)).toBeInTheDocument();
      // It cannot know whether this computer already syncs, so it says only what Not now does.
      expect(screen.getByText(SYNC_LATER_NOTE)).toBeInTheDocument();
      expect(SYNC_LATER_NOTE).not.toMatch(/sent/i);
      expect(screen.queryByRole("form", { name: "Join with a pairing code" })).toBeNull();
      expect(screen.queryByRole("form", { name: "Set up the first computer" })).toBeNull();
      await s.user.click(button("Next"));
      await stepHeading(STEPS[DOWNLOAD].title);
      for (const spy of spies) expect(spy).not.toHaveBeenCalled();
      expect((await s.ipc.syncView()).set_up).toBe(false);
    });

    it("a choice taken back to Not now sends nothing", async () => {
      const s = setup();
      await toStep(s, SYNC);
      await s.user.click(choice(SYNC_JOIN));
      const form = within(await screen.findByRole("form", { name: "Join with a pairing code" }));
      await s.user.type(form.getByRole("textbox", { name: "Server address" }), "https://sync.example.com");
      const join = vi.spyOn(s.ipc, "joinSync");
      await s.user.click(choice(SYNC_LATER));
      await s.user.click(button("Next"));
      await stepHeading(STEPS[DOWNLOAD].title);
      expect(join).not.toHaveBeenCalled();
    });

    it("joins with a pairing code on its own button and moves on, and says so on the way back", async () => {
      const s = setup();
      await toStep(s, SYNC);
      await s.user.click(choice(SYNC_JOIN));
      const form = within(await screen.findByRole("form", { name: "Join with a pairing code" }));
      const join = vi.spyOn(s.ipc, "joinSync");
      await s.user.type(form.getByRole("textbox", { name: "Server address" }), "https://sync.example.com");
      await s.user.type(form.getByRole("textbox", { name: "Pairing code" }), MOCK_SYNC_CODE);
      await s.user.click(form.getByRole("button", { name: "Join" }));
      await stepHeading(STEPS[DOWNLOAD].title);
      expect(join).toHaveBeenCalledTimes(1);
      expect(join).toHaveBeenCalledWith("https://sync.example.com", MOCK_SYNC_CODE);
      expect((await s.ipc.syncView()).set_up).toBe(true);
      await s.user.click(button("Back"));
      await stepHeading(STEPS[SYNC].title);
      expect(await screen.findByText(SYNC_JOINED)).toBeInTheDocument();
      expect(screen.queryByRole("form", { name: "Join with a pairing code" })).toBeNull();
      // Not now after a join still says this computer syncs.
      await s.user.click(choice(SYNC_LATER));
      expect(screen.getByText(SYNC_JOINED)).toBeInTheDocument();
      expect(screen.queryByText(SYNC_LATER_NOTE)).toBeNull();
      await s.user.click(button("Next"));
      await stepHeading(STEPS[DOWNLOAD].title);
      expect(join).toHaveBeenCalledTimes(1);
    });

    it("holds the choices while a join is on its way", async () => {
      const s = setup();
      await toStep(s, SYNC);
      await s.user.click(choice(SYNC_JOIN));
      const form = within(await screen.findByRole("form", { name: "Join with a pairing code" }));
      const real = s.ipc.joinSync.bind(s.ipc);
      let answer = () => {};
      const answered = new Promise<void>((resolve) => {
        answer = resolve;
      });
      vi.spyOn(s.ipc, "joinSync").mockImplementation(async (address, code) => {
        await answered;
        return real(address, code);
      });
      await s.user.type(form.getByRole("textbox", { name: "Server address" }), "https://sync.example.com");
      await s.user.type(form.getByRole("textbox", { name: "Pairing code" }), MOCK_SYNC_CODE);
      await s.user.click(button("Next"));
      await waitFor(() => expect(choice(SYNC_LATER)).toBeDisabled());
      expect(choice(SYNC_JOIN)).toBeDisabled();
      expect(choice(SYNC_SET_UP)).toBeDisabled();
      await act(async () => answer());
      await stepHeading(STEPS[DOWNLOAD].title);
      expect((await s.ipc.syncView()).set_up).toBe(true);
    });

    it("says why Next waits while the sync settings are still being read", async () => {
      const s = setup();
      await toStep(s, SYNC);
      const read = vi.spyOn(s.ipc, "syncView").mockImplementation(() => new Promise(() => {}));
      const join = vi.spyOn(s.ipc, "joinSync");
      await s.user.click(choice(SYNC_JOIN));
      await waitFor(() => expect(read).toHaveBeenCalled());
      await s.user.click(button("Next"));
      expect(await screen.findByText(SYNC_STILL_READING)).toBeInTheDocument();
      await stepHeading(STEPS[SYNC].title);
      expect(join).not.toHaveBeenCalled();
      // Not now still gets past it.
      await s.user.click(choice(SYNC_LATER));
      await s.user.click(button("Next"));
      await stepHeading(STEPS[DOWNLOAD].title);
    });

    it("a failed join stays on the step with the Sync page's copy, and Next tries again", async () => {
      const s = setup();
      await toStep(s, SYNC);
      await s.user.click(choice(SYNC_JOIN));
      const form = within(await screen.findByRole("form", { name: "Join with a pairing code" }));
      await s.user.type(form.getByRole("textbox", { name: "Server address" }), "https://sync.example.com");
      await s.user.type(form.getByRole("textbox", { name: "Pairing code" }), "not a code");
      await s.user.click(button("Next"));
      expect(await screen.findByText(syncCopy.command_problems.bad_code)).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[SYNC].title);
      // The reason stays once the choices are free again.
      await waitFor(() => expect(choice(SYNC_LATER)).toBeEnabled());
      expect(screen.getByText(syncCopy.command_problems.bad_code)).toBeInTheDocument();
      expect(choice(SYNC_JOIN)).toBeChecked();
      // The code is cleared after every try; the address stays.
      expect(form.getByRole("textbox", { name: "Pairing code" })).toHaveValue("");
      await s.user.type(form.getByRole("textbox", { name: "Pairing code" }), MOCK_SYNC_CODE);
      await s.user.click(button("Next"));
      await stepHeading(STEPS[DOWNLOAD].title);
      expect((await s.ipc.syncView()).set_up).toBe(true);
    });

    it("sets up the first computer with the setup secret on Next", async () => {
      const s = setup();
      await toStep(s, SYNC);
      await s.user.click(choice(SYNC_SET_UP));
      const form = within(await screen.findByRole("form", { name: "Set up the first computer" }));
      const setUp = vi.spyOn(s.ipc, "setUpSync");
      await s.user.type(form.getByRole("textbox", { name: "Server address" }), "https://sync.example.com");
      await s.user.type(form.getByLabelText("Setup secret"), "wrong");
      await s.user.click(button("Next"));
      expect(await screen.findByText(syncCopy.command_problems.bad_secret)).toBeInTheDocument();
      await stepHeading(STEPS[SYNC].title);
      await s.user.type(form.getByLabelText("Setup secret"), MOCK_SYNC_SECRET);
      await s.user.click(button("Next"));
      await stepHeading(STEPS[DOWNLOAD].title);
      expect(setUp).toHaveBeenCalledTimes(2);
      expect((await s.ipc.syncView()).set_up).toBe(true);
    });

    it("Skip setup sends nothing typed", async () => {
      const s = setup();
      await toStep(s, SYNC);
      await s.user.click(choice(SYNC_JOIN));
      const form = within(await screen.findByRole("form", { name: "Join with a pairing code" }));
      await s.user.type(form.getByRole("textbox", { name: "Server address" }), "https://sync.example.com");
      await s.user.type(form.getByRole("textbox", { name: "Pairing code" }), MOCK_SYNC_CODE);
      const join = vi.spyOn(s.ipc, "joinSync");
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", HOME);
      expect(join).not.toHaveBeenCalled();
    });
  });

  describe("Download", () => {
    const THIS_COMPUTER = ["speech-helpers", "parakeet-tdt-0.6b-v2", "qwen3.5-4b-q4_k_m"];

    it("asks for the languages first, then states the total for this computer on one button", async () => {
      const s = setup(CLEAN);
      await toStep(s, DOWNLOAD);
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

    it("goes by who writes first when the order cannot be read after the Summaries step", async () => {
      const s = setup(CLEAN);
      await waitFor(() => expect(tick(CODEX_LABEL)).toBeEnabled());
      await s.user.click(tick(CODEX_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Codex" }]));
      await waitFor(() => expect(up(CODEX_LABEL)).toBeEnabled());
      await s.user.click(up(CODEX_LABEL)!);
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }, { kind: "Local" }]));
      // This computer is second, so the order would count its model; unread, only the first counts.
      vi.spyOn(s.ipc, "summaryOrder").mockRejectedValueOnce(new Error("unreadable"));
      await toStepFrom(s, 0, DOWNLOAD);
      const total = await totalOf(s.ipc, ["speech-helpers", "parakeet-tdt-0.6b-v2"]);
      await screen.findByText(`Earshot needs ${total} for this computer: Speech helpers and Parakeet 0.6B v2.`);
    });

    it("leaves the summary model out unless the Summaries step chose this computer", async () => {
      const s = setup(CLEAN);
      await waitFor(() => expect(tick(CODEX_LABEL)).toBeEnabled());
      await s.user.click(tick(CODEX_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Local" }, { kind: "Codex" }]));
      await s.user.click(tick(LOCAL_LABEL));
      await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("Codex"));
      await toStepFrom(s, 0, DOWNLOAD);
      const total = await totalOf(s.ipc, ["speech-helpers", "parakeet-tdt-0.6b-v2"]);
      await screen.findByText(`Earshot needs ${total} for this computer: Speech helpers and Parakeet 0.6B v2.`);
      await s.user.click(button("Back"));
      await stepHeading(STEPS[SYNC].title);
      await s.user.click(button("Back"));
      await waitFor(() => expect(tick(LOCAL_LABEL)).toBeEnabled());
      await s.user.click(tick(LOCAL_LABEL));
      await waitFor(async () => expect(await stored(s.ipc)).toEqual([{ kind: "Codex" }, { kind: "Local" }]));
      await toStepFrom(s, 0, DOWNLOAD);
      await screen.findByText(new RegExp(`^Earshot needs ${await totalOf(s.ipc, THIS_COMPUTER)} .* Qwen 3\\.5 4B\\.$`));
    });

    it("follows the languages to another model", async () => {
      const s = setup({ ...CLEAN, machine: "desktop" });
      await toStep(s, DOWNLOAD);
      await s.user.click(await screen.findByRole("combobox", { name: "Meeting languages" }));
      await s.user.click(await screen.findByRole("menuitemcheckbox", { name: "German" }));
      await screen.findByText(/for this computer: Speech helpers, Parakeet 0\.6B v3 and Qwen 3\.5 9B\.$/);
    });

    it("says so when no recommended model covers the languages, and leaves the rest to get", async () => {
      const s = setup(CLEAN);
      const models = s.ipc.models.bind(s.ipc);
      vi.spyOn(s.ipc, "models").mockImplementation(async () => ({ ...(await models()), recommended: null }));
      await toStep(s, DOWNLOAD);
      await screen.findByText(NO_MODEL_FOR_LANGUAGES);
      await screen.findByText(/for this computer: Speech helpers and Qwen 3\.5 4B\.$/);
    });

    it("offers the NPU versions in the full list on an ARM64 computer, and fetches one", async () => {
      const s = setup(CLEAN);
      const start = vi.spyOn(s.ipc, "startDownload");
      await toStep(s, DOWNLOAD);
      await s.user.click(await screen.findByRole("button", { name: "Choose different models" }));
      const npu = await screen.findByRole("region", { name: "NPU versions" });
      await s.user.click(within(npu).getByRole("button", { name: /^Download Parakeet 0\.6B v2 \(NPU version\)/ }));
      await waitFor(() => expect(start.mock.calls.map(([m]) => m)).toEqual(["parakeet-tdt-0.6b-v2-npu"]));
      // The next steps say it is still downloading, as they do for the other models.
      await s.user.click(button("Next"));
      await stepHeading(STEPS[ABOUT_ME].title);
      expect(await screen.findByText(stillDownloading(["Parakeet 0.6B v2 (NPU version)"]))).toBeInTheDocument();
    });

    it("has no NPU versions on a computer without them", async () => {
      const s = setup({ ...CLEAN, machine: "desktop" });
      await toStep(s, DOWNLOAD);
      await s.user.click(await screen.findByRole("button", { name: "Choose different models" }));
      await screen.findByRole("region", { name: "What every meeting needs" });
      expect(screen.queryByRole("region", { name: "NPU versions" })).toBeNull();
    });

    it("follows a model chosen from the full list", async () => {
      const s = setup(CLEAN);
      await toStep(s, DOWNLOAD);
      await s.user.click(await screen.findByRole("button", { name: "Choose different models" }));
      await s.user.click(await screen.findByRole("radio", { name: /^Parakeet 110M/ }));
      await screen.findByText(/for this computer: Speech helpers, Parakeet 110M and Qwen 3\.5 4B\.$/);
    });

    it("downloads everything on the one button, goes on through the other steps and after finishing", async () => {
      const s = setup(CLEAN);
      const start = vi.spyOn(s.ipc, "startDownload");
      const cancel = vi.spyOn(s.ipc, "cancelDownload");
      await toStep(s, DOWNLOAD);
      await screen.findByText(/^Earshot needs/);
      expect(start).not.toHaveBeenCalled();
      await s.user.click(button("Download"));
      await waitFor(() => expect(start.mock.calls.map(([m]) => m)).toEqual(THIS_COMPUTER));
      expect(await screen.findByRole("progressbar", { name: "Downloads for this computer" })).toBeInTheDocument();
      expect(screen.getByText(new RegExp(DOWNLOADS_CARRY_ON))).toBeInTheDocument();
      await waitFor(() => expect(screen.queryByRole("button", { name: "Download" })).toBeNull());
      await s.user.click(button("Next"));
      await stepHeading(STEPS[ABOUT_ME].title);
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
      await toStep(s, DOWNLOAD);
      await screen.findByText(NOTHING_TO_DOWNLOAD);
      expect(screen.queryByRole("button", { name: "Download" })).toBeNull();
      expect(button("Choose different models")).toBeEnabled();
    });

    it("counts only what is missing", async () => {
      const s = setup({ cleanInstall: true });
      await toStep(s, DOWNLOAD);
      await screen.findByText(
        `Earshot needs ${await totalOf(s.ipc, ["speech-helpers"])} for this computer: Speech helpers.`,
      );
    });

    it("shows why a download was refused", async () => {
      const s = setup(CLEAN);
      vi.spyOn(s.ipc, "startDownload").mockRejectedValueOnce(new Error("The disk is full."));
      await toStep(s, DOWNLOAD);
      await screen.findByText(/^Earshot needs/);
      await s.user.click(button("Download"));
      expect(await screen.findByText("The disk is full.")).toBeInTheDocument();
    });
  });

  describe("About me", () => {
    it("Next saves your name, with no Save button", async () => {
      const s = setup();
      await toStep(s, ABOUT_ME);
      await s.user.type(await screen.findByRole("textbox", { name: /^Your name/ }), "Casey");
      expect(screen.queryByRole("button", { name: /^Save/ })).toBeNull();
      await s.user.click(button("Next"));
      await stepHeading(STEPS[EXTRAS].title);
      expect((await s.ipc.speakers()).voice.name).toBe("Casey");
    });

    it("a refused name stays on the step and says why", async () => {
      const s = setup();
      await toStep(s, ABOUT_ME);
      const name = await screen.findByRole("textbox", { name: /^Your name/ });
      await s.user.click(name);
      await s.user.paste("x".repeat(101));
      await s.user.click(button("Next"));
      expect(await screen.findByText("That name is too long.")).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[ABOUT_ME].title);
      await s.user.clear(name);
      await s.user.click(button("Next"));
      await stepHeading(STEPS[EXTRAS].title);
    });

    it("the voice sample has its own Skip, which saves the name and goes on", async () => {
      const s = setup();
      const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
      await toStep(s, ABOUT_ME);
      await s.user.type(await screen.findByRole("textbox", { name: /^Your name/ }), "Casey");
      await s.user.click(button("Skip the voice sample"));
      await stepHeading(STEPS[EXTRAS].title);
      expect((await s.ipc.speakers()).voice.name).toBe("Casey");
      expect(startVoice).not.toHaveBeenCalled();
    });

    it("asks before recording your voice, and shows why the app refused", async () => {
      const s = setup({
        settings: { voiceStartError: { kind: "busy", message: "A meeting is running.", retryable: true } },
      });
      const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
      await toStep(s, ABOUT_ME);
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
      await toStep(s, ABOUT_ME);
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
      await toStep(s, ABOUT_ME);
      expect(await screen.findByRole("button", { name: "Record my voice" })).toBeDisabled();
      expect(screen.getByText(HELPERS_MISSING)).toBeInTheDocument();
      expect(button("Skip the voice sample")).toBeEnabled();
      expect(start).not.toHaveBeenCalled();
      expect(startVoice).not.toHaveBeenCalled();
    });

    it("the record button turns on by itself when the helpers arrive, and records nothing", async () => {
      const s = setup(CLEAN);
      const startVoice = vi.spyOn(s.ipc, "startVoiceSample");
      await toStep(s, DOWNLOAD);
      await s.user.click(await screen.findByRole("button", { name: "Download" }));
      await waitFor(() => expect(screen.queryByRole("button", { name: "Download" })).toBeNull());
      await s.user.click(button("Next"));
      await stepHeading(STEPS[ABOUT_ME].title);
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
    const previews = () => screen.findByRole("radiogroup", { name: PREVIEWS_HEADING });
    const labelsOf = async () =>
      within(await previews())
        .getAllByRole("radio")
        .map((r) => (r as HTMLInputElement).labels?.[0].textContent);
    const suggested = (label: string) => `${label} (${PREVIEWS_SUGGESTED})`;

    for (const machine of ["desktop", "surface"] as const) {
      it(`offers whose words show while someone talks, the app's suggestion marked (${machine})`, async () => {
        const s = setup({ machine });
        await toStep(s, EXTRAS);
        const want = (await s.ipc.previewsSettings()).suggested;
        const labels = ["Off", "Other people only", "Everyone"];
        const marked = labels[["off", "other_people", "everyone"].indexOf(want)];
        await waitFor(async () =>
          expect(await labelsOf()).toEqual(labels.map((l) => (l === marked ? suggested(l) : l))),
        );
        // What the app shows now stays selected: the suggestion is marked, not chosen.
        const current = within(await previews()).getByRole("radio", { name: "Other people only" });
        expect((current as HTMLInputElement).checked).toBe(true);
      });
    }

    it("marks the option the app suggests, whichever it is", async () => {
      const s = setup({
        settings: {
          previews: {
            choice: "other_people",
            can_change: true,
            note: null,
            unknown: null,
            suggested: "off",
            other_people_refused: null,
            everyone_refused: null,
          },
        },
      });
      await toStep(s, EXTRAS);
      await waitFor(async () => expect(await labelsOf()).toEqual([suggested("Off"), "Other people only", "Everyone"]));
    });

    it("disables an option the app refuses here, with its reason beside it", async () => {
      const s = setup({
        settings: {
          previews: {
            choice: "other_people",
            can_change: true,
            note: null,
            unknown: null,
            suggested: "off",
            other_people_refused: "A reason for other people.",
            everyone_refused: "A reason for everyone.",
          },
        },
      });
      await toStep(s, EXTRAS);
      const group = await previews();
      const radio = (name: string) => within(group).getByRole("radio", { name }) as HTMLInputElement;
      await waitFor(() => expect(radio("Everyone").disabled).toBe(true));
      expect(radio("Other people only").disabled).toBe(true);
      expect(radio(suggested("Off")).disabled).toBe(false);
      expect(radio("Everyone")).toHaveAccessibleDescription("A reason for everyone.");
      expect(radio("Other people only")).toHaveAccessibleDescription("A reason for other people.");
      expect(screen.getByText("A reason for everyone.")).toBeInTheDocument();
    });

    it("disables only the refused option", async () => {
      const s = setup({
        settings: {
          previews: {
            choice: "off",
            can_change: true,
            note: null,
            unknown: null,
            suggested: "off",
            other_people_refused: null,
            everyone_refused: "Not enough memory here.",
          },
        },
      });
      await toStep(s, EXTRAS);
      const group = await previews();
      const radio = (name: string) => within(group).getByRole("radio", { name }) as HTMLInputElement;
      await waitFor(() => expect(radio("Everyone").disabled).toBe(true));
      expect(radio("Other people only").disabled).toBe(false);
      expect(screen.getByText("Not enough memory here.")).toBeInTheDocument();
    });

    it("shows the app's note only while the choice it is about is picked", async () => {
      const note = "A note about this computer.";
      const s = setup({
        settings: {
          previews: {
            choice: "other_people",
            can_change: true,
            note,
            unknown: null,
            suggested: "everyone",
            other_people_refused: null,
            everyone_refused: null,
          },
        },
      });
      await toStep(s, EXTRAS);
      expect(await screen.findByText(note)).toBeInTheDocument();
      await s.user.click(within(await previews()).getByRole("radio", { name: "Off" }));
      expect(screen.queryByText(note)).toBeNull();
      await s.user.click(within(await previews()).getByRole("radio", { name: "Other people only" }));
      expect(screen.getByText(note)).toBeInTheDocument();
    });

    it("Finish saves a changed choice, and nothing before it", async () => {
      const s = setup({ machine: "desktop" });
      const save = vi.spyOn(s.ipc, "setPreviews");
      await toStep(s, EXTRAS);
      await s.user.click(within(await previews()).getByRole("radio", { name: suggested("Everyone") }));
      expect(save).not.toHaveBeenCalled();
      expect((await s.ipc.previewsSettings()).choice).toBe("other_people");
      await s.user.click(button("Finish"));
      await screen.findByRole("heading", HOME);
      expect(save).toHaveBeenCalledTimes(1);
      expect(save).toHaveBeenCalledWith("everyone");
      expect((await s.ipc.previewsSettings()).choice).toBe("everyone");
    });

    it("stores nothing when left as it is, or when the wizard is skipped or left by Back", async () => {
      const s = setup({ machine: "desktop" });
      const save = vi.spyOn(s.ipc, "setPreviews");
      await toStep(s, EXTRAS);
      await previews();
      await s.user.click(within(await previews()).getByRole("radio", { name: "Off" }));
      await s.user.click(button("Back"));
      await stepHeading(STEPS[ABOUT_ME].title);
      await s.user.click(button("Next"));
      await stepHeading(STEPS[EXTRAS].title);
      await s.user.click(within(await previews()).getByRole("radio", { name: "Off" }));
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", HOME);
      expect(save).not.toHaveBeenCalled();
      expect((await s.ipc.previewsSettings()).choice).toBe("other_people");
    });

    it("Finish with the choice untouched stores nothing", async () => {
      const s = setup({ machine: "desktop" });
      const save = vi.spyOn(s.ipc, "setPreviews");
      await toStep(s, EXTRAS);
      await previews();
      await s.user.click(button("Finish"));
      await screen.findByRole("heading", HOME);
      expect(save).not.toHaveBeenCalled();
    });

    it("a refused choice stays on the step and says why", async () => {
      const s = setup({ machine: "desktop" });
      vi.spyOn(s.ipc, "setPreviews").mockRejectedValueOnce(new Error("The setting could not be stored."));
      await toStep(s, EXTRAS);
      await s.user.click(within(await previews()).getByRole("radio", { name: "Off" }));
      await s.user.click(button("Finish"));
      expect(await screen.findByText("The setting could not be stored.")).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[EXTRAS].title);
      expect((await s.ipc.firstRunState()).show).toBe(true);
    });

    it("is two rows that open when pressed, and then Finish", async () => {
      const s = setup();
      await toStep(s, EXTRAS);
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
      await toStep(s, EXTRAS);
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
      await toStep(s, EXTRAS);
      await s.user.click(button("Open Obsidian"));
      await s.user.type(await screen.findByRole("textbox", { name: /^Vault folder/ }), "not a path");
      await s.user.click(button("Finish"));
      expect(await screen.findByText(/The vault must be a full folder path/)).toBeInTheDocument();
      expect(screen.getByText(SAVE_REFUSED_NOTE)).toBeInTheDocument();
      await stepHeading(STEPS[EXTRAS].title);
      expect((await s.ipc.firstRunState()).show).toBe(true);
    });

    it("Skip setup and Back leave typed text unsaved", async () => {
      const s = setup({ settings: { pickedVault: "C:\\Notes" } });
      await toStep(s, EXTRAS);
      await s.user.click(button("Open Obsidian"));
      await s.user.click(await screen.findByRole("button", { name: "Choose folder..." }));
      await screen.findByDisplayValue("C:\\Notes");
      await s.user.click(button("Back"));
      await stepHeading(STEPS[ABOUT_ME].title);
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
      await toStep(s, EXTRAS);
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
      await toStep(s, EXTRAS);
      await s.user.click(button("Open Audio check"));
      await s.user.click(await screen.findByRole("button", { name: "Run audio check" }));
      await screen.findByText("The model check is running.");
      expect(button("Finish")).toBeEnabled();
    });
  });

  describe("with no transcription model", () => {
    it("Skip setup says meetings need one and stops nothing, until dismissed", async () => {
      const s = setup(CLEAN);
      await toStep(s, DOWNLOAD);
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
      await toStep(s, EXTRAS);
      await s.user.click(button("Finish"));
      await screen.findByRole("heading", HOME);
      expect(screen.getByText(NO_MODEL_NOTE)).toBeInTheDocument();
    });

    it("the words reach Settings when the wizard was opened from there", async () => {
      const s = setup({ ...CLEAN, firstRun: { show: false } });
      await openSettings(s.user, "General");
      await s.user.click(await screen.findByRole("button", { name: "Open the setup guide" }));
      await toStep(s, DOWNLOAD);
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", { level: 1, name: "General" });
      expect(screen.getByText(NO_MODEL_NOTE)).toBeInTheDocument();
    });

    it("says nothing while one is downloading, or when one is here", async () => {
      const s = setup(CLEAN);
      await toStep(s, DOWNLOAD);
      await s.user.click(await screen.findByRole("button", { name: "Download" }));
      await waitFor(() => expect(screen.queryByRole("button", { name: "Download" })).toBeNull());
      await s.user.click(button("Skip setup"));
      await screen.findByRole("heading", HOME);
      expect(screen.queryByText(NO_MODEL_NOTE)).toBeNull();
      cleanup();
      const t = setup();
      await toStep(t, EXTRAS);
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
      SYNC_JOIN,
      SYNC_JOINED,
      SYNC_LATER,
      SYNC_LATER_NOTE,
      SYNC_QUESTION,
      SYNC_SET_UP,
      SYNC_STILL_READING,
      ...STEPS.flatMap((s) => [s.title, s.intro]),
    ];
    for (const w of words) expect(w).not.toContain("\u2014");
  });
});
