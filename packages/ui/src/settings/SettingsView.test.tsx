import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { DOWNLOADING_MESSAGE, MOCK_DISPLAY_NAMES, MODEL_STEP_MS } from "../ipc/mockModels";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { SETTINGS_SECTIONS, type SettingsSection } from "./SettingsView";
import { formatSize } from "./copy";

interface Setup {
  timer: ManualTimer;
  ipc: Ipc;
  user: ReturnType<typeof userEvent.setup>;
}

function setup(data: MockIpcData = {}, wrap: (ipc: Ipc) => Ipc = (i) => i): Setup {
  const timer = createManualTimer(1_000_000);
  const ipc = wrap(createMockIpc({ meetings: [], timer, ...data }));
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  return { timer, ipc, user: userEvent.setup() };
}

/** Opens Settings, which starts on General. */
async function openSettings(s: Setup) {
  await s.user.click(await screen.findByRole("button", { name: "Settings" }));
  await screen.findByRole("heading", { level: 1, name: "General" });
}

/** Goes to `section` and waits for its model parts. */
async function goTo(s: Setup, section: SettingsSection) {
  const nav = screen.getByRole("navigation", { name: "Settings sections" });
  await s.user.click(within(nav).getByRole("button", { name: section }));
  await screen.findByRole("heading", { level: 1, name: section });
  if (section === "System") await screen.findByRole("list", { name: "This is your hardware" });
  if (section === "Transcription") await screen.findByRole("radiogroup", { name: "Transcription model" });
  if (section === "Summaries") await screen.findByRole("radiogroup", { name: "Summary model" });
}

async function openPage(s: Setup, section: SettingsSection) {
  await openSettings(s);
  await goTo(s, section);
}

/** The section headings of the page shown. */
function sections(): string[] {
  return within(screen.getByRole("main"))
    .getAllByRole("heading", { level: 2 })
    .map((h) => h.textContent ?? "");
}

function advance(timer: ManualTimer, ms: number) {
  act(() => timer.advance(ms));
}

function hwRow(label: string): HTMLElement {
  const list = screen.getByRole("list", { name: "This is your hardware" });
  return within(list).getByRole("listitem", { name: label });
}

function card(model: string): HTMLElement {
  const el = document.querySelector<HTMLElement>(`[data-model="${model}"]`);
  if (!el) throw new Error(`no card for ${model}`);
  return el;
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings", () => {
  it("opens on General, lists the eight pages in order, switches and goes back to the meetings", async () => {
    const s = setup();
    await openSettings(s);
    expect(SETTINGS_SECTIONS).toEqual([
      "General",
      "Audio",
      "System",
      "Transcription",
      "Summaries",
      "Dictionary",
      "People",
      "Notes and storage",
    ]);
    const nav = screen.getByRole("navigation", { name: "Settings sections" });
    const items = within(nav)
      .getAllByRole("button")
      .map((b) => b.textContent);
    expect(items).toEqual(["Back to meetings", ...SETTINGS_SECTIONS]);
    expect(within(nav).getByRole("button", { name: "General" })).toHaveAttribute("aria-current", "page");

    await s.user.click(within(nav).getByRole("button", { name: "Audio" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Audio" })).toBeInTheDocument();
    expect(await screen.findByRole("combobox", { name: "Microphone" })).toBeInTheDocument();
    expect(within(nav).getByRole("button", { name: "Audio" })).toHaveAttribute("aria-current", "page");

    await s.user.click(within(nav).getByRole("button", { name: "Notes and storage" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Notes and storage" })).toBeInTheDocument();
    expect(await screen.findByRole("combobox", { name: "Keep recordings for" })).toBeInTheDocument();
    expect(within(nav).getByRole("button", { name: "Notes and storage" })).toHaveAttribute("aria-current", "page");
    expect(within(nav).getByRole("button", { name: "General" })).not.toHaveAttribute("aria-current");

    await goTo(s, "System");

    await s.user.click(within(nav).getByRole("button", { name: "Back to meetings" }));
    expect(await screen.findByRole("heading", { name: "Record your first meeting" })).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "Settings sections" })).toBeNull();
  });

  it("puts each part on the page the design gives it", async () => {
    const expected: Record<"surface" | "desktop", Partial<Record<SettingsSection, string[]>>> = {
      surface: {
        General: ["Start", "Keyboard shortcut", "Theme", "Updates", "Setup guide"],
        System: [
          "This is your hardware",
          "What every meeting needs",
          "Where it runs",
          "Model check",
          "How fast Earshot is on this computer",
        ],
        Transcription: [
          "Meeting languages",
          "Transcription model",
          "NPU versions",
          "During a meeting",
          "When speakers are found",
        ],
        Summaries: ["Who writes the summary", "During the meeting", "Summary style"],
        Dictionary: ["Your words"],
        People: ["Your name", "Your voice", "People"],
        "Notes and storage": ["Obsidian", "Recordings", "Crash logs"],
      },
      desktop: {
        System: [
          "This is your hardware",
          "What every meeting needs",
          "Where it runs",
          "Model check",
          "NVIDIA CUDA libraries",
          "How fast Earshot is on this computer",
        ],
        Transcription: ["Meeting languages", "Transcription model", "During a meeting", "When speakers are found"],
      },
    };
    for (const machine of ["surface", "desktop"] as const) {
      const s = setup({ machine });
      await openSettings(s);
      for (const [section, headings] of Object.entries(expected[machine]) as [SettingsSection, string[]][]) {
        await goTo(s, section);
        await waitFor(() => expect(sections()).toEqual(headings));
      }
      // The Recommendation leads the System page, after the hardware.
      await goTo(s, "System");
      expect(screen.getByRole("region", { name: "Recommendation" })).toBeInTheDocument();
      // The summary models sit under "On this computer".
      await goTo(s, "Summaries");
      const local = screen.getByRole("group", { name: "Which model writes the summary" });
      expect(within(local).getByRole("radiogroup", { name: "Summary model" })).toBeInTheDocument();
      cleanup();
    }
  });

  it("keeps choices out of every part a Save button saves: a choice applies at once, typed text needs Save", async () => {
    // The one exception: the endpoint's provider is saved with the address and model typed beside
    // it, because a provider alone is not an endpoint (docs/ui-design.md "Settings").
    const allowed = (el: Element) =>
      el instanceof HTMLSelectElement &&
      document.getElementById(el.getAttribute("aria-labelledby") ?? "")?.textContent === "Provider";
    const seen = new Set<string>();
    const checkPage = () => {
      const saves = within(screen.getByRole("main")).queryAllByRole("button", { name: /^(Save|Use this endpoint|Add$)/ });
      for (const button of saves) {
        const part = button.closest("form, [role='group'], section");
        expect(part).not.toBeNull();
        const choices = [
          ...(part as Element).querySelectorAll("input[type='radio'], input[type='checkbox'], select, [role='combobox']"),
        ].filter((el) => !allowed(el));
        expect(choices, `choices beside "${button.textContent}"`).toEqual([]);
        seen.add(button.textContent ?? "");
      }
    };
    const s = setup({ machine: "desktop" });
    await openSettings(s);
    for (const section of SETTINGS_SECTIONS) {
      await goTo(s, section);
      if (section === "People") await screen.findByRole("textbox", { name: "Your name" });
      if (section === "Dictionary") await screen.findByRole("textbox", { name: "Add a word" });
      if (section === "Notes and storage") await screen.findByRole("textbox", { name: "Vault folder" });
      checkPage();
    }
    // The writers' own parts on Summaries: the endpoint (with the Anthropic key) and the two CLIs.
    await goTo(s, "Summaries");
    await s.user.click(screen.getByRole("radio", { name: /^Your own server/ }));
    const form = screen.getByRole("group", { name: "Your own server" });
    await s.user.selectOptions(within(form).getByRole("combobox", { name: "Provider" }), "Anthropic");
    checkPage();
    await s.user.click(screen.getByRole("radio", { name: /^Claude \(through Claude Code\)/ }));
    const claude = await screen.findByRole("group", { name: "Claude through Claude Code" });
    checkPage();
    // The model and effort lists apply at once; only Other's typed name has a Save button.
    await s.user.selectOptions(within(claude).getByRole("combobox", { name: "Model" }), "Other");
    within(claude).getByRole("button", { name: "Save model" });
    checkPage();
    await s.user.click(screen.getByRole("radio", { name: /^OpenAI \(through Codex\)/ }));
    const codex = await screen.findByRole("group", { name: "OpenAI through Codex" });
    checkPage();
    await s.user.selectOptions(within(codex).getByRole("combobox", { name: "Model" }), "Other");
    within(codex).getByRole("button", { name: "Save model" });
    checkPage();
    expect([...seen].sort()).toEqual(["Add", "Save", "Save key", "Save model", "Use this endpoint"]);
  });

  it("holds one subscription to model job events across its pages, and the setup guide its own", async () => {
    let live = 0;
    let most = 0;
    const s = setup({ machine: "surface" }, (ipc) => ({
      ...ipc,
      onModelJobEvent: async (handler) => {
        const stop = await ipc.onModelJobEvent(handler);
        live += 1;
        most = Math.max(most, live);
        return () => {
          live -= 1;
          stop();
        };
      },
    }));
    await openSettings(s);
    await waitFor(() => expect(live).toBe(1));
    for (const section of ["System", "Transcription", "Summaries", "People", "General"] as const) {
      await goTo(s, section);
      expect(live).toBe(1);
    }
    // A download started on one page goes on showing on another.
    await goTo(s, "Transcription");
    await s.user.click(screen.getByRole("button", { name: "Download Whisper large-v3-turbo (1.6 GB)" }));
    advance(s.timer, 3 * MODEL_STEP_MS);
    await goTo(s, "System");
    await goTo(s, "Transcription");
    await waitFor(() => expect(card("large-v3-turbo")).toHaveTextContent("487 MB of 1.6 GB"));
    // The setup guide replaces Settings: its own subscription, never beside this one.
    await goTo(s, "General");
    await s.user.click(screen.getByRole("button", { name: "Open the setup guide" }));
    await screen.findByRole("heading", { level: 1, name: "Summaries" });
    await waitFor(() => expect(live).toBe(1));
    expect(screen.queryByRole("navigation", { name: "Settings sections" })).toBeNull();
    expect(most).toBe(1);
  });

  it("shows the Surface-like hardware and what runs on each part", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "System");
    const cpu = hwRow("Processor");
    expect(cpu).toHaveTextContent("Snapdragon(R) X2 Elite - X2E88100 - Qualcomm(R) Oryon(TM) CPU");
    expect(cpu).toHaveTextContent("12 cores");
    expect(cpu).toHaveTextContent("Speaker detection, Summaries");
    const gpu = hwRow("Graphics");
    expect(gpu).toHaveTextContent("Qualcomm(R) Adreno(TM) X2-90 GPU, 512 MB video memory");
    expect(gpu).toHaveTextContent("Not used");
    const npu = hwRow("NPU");
    expect(npu).toHaveTextContent("Qualcomm(R) Hexagon(TM) NPU");
    expect(npu).toHaveTextContent("Transcription");
    const mem = hwRow("Memory");
    expect(mem).toHaveTextContent("15.6 GB");
    expect(mem).toHaveTextContent("Summaries run on the processor and use it");
    const rec = screen.getByRole("region", { name: "Recommendation" });
    expect(rec).toHaveTextContent(
      "Transcription with Parakeet 0.6B v2 on the NPU, summaries with Qwen 3.5 4B on the processor.",
    );
    expect(rec).toHaveTextContent(
      "Transcription: the most accurate English model for the NPU. Summaries: runs on the processor, where Qwen 3.5 9B is too slow.",
    );
  });

  it("shows the desktop-like hardware and what runs on each part", async () => {
    const s = setup({ machine: "desktop" });
    await openPage(s, "System");
    const cpu = hwRow("Processor");
    expect(cpu).toHaveTextContent("AMD Ryzen 9 9950X3D 16-Core Processor");
    expect(cpu).toHaveTextContent("16 cores, 32 threads");
    expect(cpu).toHaveTextContent("Speaker detection");
    expect(cpu).not.toHaveTextContent("Transcription");
    const gpu = hwRow("Graphics");
    expect(gpu).toHaveTextContent("NVIDIA GeForce RTX 5080, 16 GB video memory");
    expect(gpu).toHaveTextContent("AMD Radeon(TM) Graphics, 512 MB video memory");
    expect(gpu).toHaveTextContent("Transcription, Summaries");
    const npu = hwRow("NPU");
    expect(npu.textContent).toBe("NPUNone found");
    const mem = hwRow("Memory");
    expect(mem).toHaveTextContent("61.6 GB");
    expect(mem).toHaveTextContent("Only matters for summaries on the processor");
    expect(screen.getByRole("region", { name: "Recommendation" })).toHaveTextContent(
      "Transcription with Parakeet 0.6B v2 on the graphics card, summaries with Qwen 3.5 9B on the graphics card.",
    );
  });

  it("moves the hardware rows with a model choice and a where-it-runs override", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ }));
    await waitFor(() => expect(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ })).toBeChecked());
    await goTo(s, "System");
    await waitFor(() => expect(hwRow("NPU")).toHaveTextContent("Not used"));
    expect(hwRow("Processor")).toHaveTextContent("Speaker detection, Transcription, Summaries");

    // Forcing the NPU for a model without an NPU version: it runs on the processor, with the note.
    await s.user.selectOptions(screen.getByRole("combobox", { name: "Transcription" }), "Npu");
    expect(
      await screen.findByText("Parakeet 0.6B v3 has no NPU version, so it runs on the processor."),
    ).toBeInTheDocument();
    expect(hwRow("NPU")).toHaveTextContent("Not used");

    // Back to a model with an NPU version: the NPU row follows.
    await goTo(s, "Transcription");
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v2/ }));
    await waitFor(() => expect(screen.getByRole("radio", { name: /^Parakeet 0\.6B v2/ })).toBeChecked());
    await goTo(s, "System");
    await waitFor(() => expect(hwRow("NPU")).toHaveTextContent("Transcription"));
    expect(screen.queryByText(/has no NPU version/)).toBeNull();
  });

  it("moves summaries to the processor when chosen on the desktop", async () => {
    const s = setup({ machine: "desktop" });
    await openPage(s, "System");
    const select = screen.getByRole("combobox", { name: "Summaries" });
    expect(within(select).getAllByRole("option").map((o) => o.textContent)).toEqual([
      "Automatic (graphics card or processor)",
      "Graphics card",
      "Processor",
    ]);
    await s.user.selectOptions(select, "Processor");
    await waitFor(() => expect(hwRow("Processor")).toHaveTextContent("Speaker detection, Summaries"));
    expect(hwRow("Graphics")).toHaveTextContent("Transcription");
    expect(hwRow("Graphics")).not.toHaveTextContent("Summaries");
    expect(hwRow("Memory")).toHaveTextContent("Summaries run on the processor and use it");
    expect(await s.ipc.modelSettings()).toMatchObject({ value: { summary_part: "Processor" } });
  });

  it("reads the models again when the writer changes on the Summaries page", async () => {
    const s = setup({ machine: "desktop" });
    await openPage(s, "System");
    expect(hwRow("Graphics")).toHaveTextContent("Transcription, Summaries");
    await goTo(s, "Summaries");
    await s.user.click(screen.getByRole("radio", { name: /^No summary/ }));
    await waitFor(async () => expect((await s.ipc.summarySettings()).value.writer).toBe("NoSummary"));
    // No summary: the model choices under "On this computer" go.
    expect(screen.queryByRole("group", { name: "Which model writes the summary" })).toBeNull();
    await goTo(s, "System");
    await waitFor(() => expect(hwRow("Graphics")).not.toHaveTextContent("Summaries"));
  });

  it("chooses the summary model under On this computer", async () => {
    const s = setup({ machine: "desktop" });
    await openPage(s, "Summaries");
    const local = within(screen.getByRole("group", { name: "Which model writes the summary" }));
    expect(local.getByRole("radio", { name: /^Qwen 3\.5 9B/ })).toBeChecked();
    await s.user.click(local.getByRole("radio", { name: /^Qwen 3\.5 4B/ }));
    await waitFor(async () => expect((await s.ipc.modelSettings()).value.summary).toBe("qwen3.5-4b-q4_k_m"));
    await waitFor(() => expect(local.getByRole("radio", { name: /^Qwen 3\.5 4B/ })).toBeChecked());
  });

  it("offers the speech helpers on a clean install, follows the download and never offers Remove", async () => {
    const s = setup({ cleanInstall: true });
    await openPage(s, "System");
    const section = within(await screen.findByRole("region", { name: "What every meeting needs" }));
    const size = (await s.ipc.models()).helpers.size;
    expect(section.getByText("Speech helpers")).toBeInTheDocument();
    await s.user.click(section.getByRole("button", { name: `Download Speech helpers (${formatSize(size)})` }));
    act(() => s.timer.advance(3 * MODEL_STEP_MS));
    expect(await section.findByRole("progressbar", { name: "Speech helpers download" })).toBeInTheDocument();
    await s.user.click(section.getByRole("button", { name: "Pause Speech helpers" }));
    expect(await section.findByRole("button", { name: "Resume Speech helpers" })).toBeInTheDocument();
    expect(section.queryByRole("button", { name: /^Remove/ })).toBeNull();
    await s.user.click(section.getByRole("button", { name: "Resume Speech helpers" }));
    act(() => s.timer.advance(20 * MODEL_STEP_MS));
    expect(await section.findByText("Installed")).toBeInTheDocument();
    expect(section.queryByRole("button", { name: /^Remove/ })).toBeNull();
  });

  it("offers the four transcription choices with their copy", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    const group = screen.getByRole("radiogroup", { name: "Transcription model" });
    const radios = within(group).getAllByRole("radio");
    expect(radios.map((r) => (r as HTMLInputElement).labels?.[0]?.textContent)).toEqual([
      "Parakeet 0.6B v2RecommendedKeeps up with live speech on this computer",
      "Parakeet 110MLighter",
      "Parakeet 0.6B v3Multilingual: European",
      "Whisper large-v3-turboMultilingual: every language",
    ]);
    expect(radios[0]).toBeChecked();
    expect(card("parakeet-tdt-0.6b-v2")).toHaveTextContent("Best accuracy for English meetings. Runs on the NPU.");
    expect(card("parakeet-tdt-110m")).toHaveTextContent(
      "For an older or busy computer, or when space is tight. A little less accurate. Runs on the NPU.",
    );
    expect(card("parakeet-tdt-0.6b-v3")).toHaveTextContent(
      "English and 24 other European languages. Runs on the processor here (no NPU version).",
    );
    expect(card("large-v3-turbo")).toHaveTextContent(
      "About 100 languages, including Asian languages. Less accurate and slower than Parakeet 0.6B v3: use it only for languages Parakeet 0.6B v3 does not cover. Runs on the processor here (no NPU version).",
    );
    expect(card("large-v3-turbo")).toHaveTextContent("Download 1.6 GB");
    expect(card("parakeet-tdt-0.6b-v2")).toHaveTextContent("Installed");
    // Summaries: the shipped models, recommended first, no verdict without a stored one, under
    // "On this computer" on the Summaries page.
    await goTo(s, "Summaries");
    const local = within(screen.getByRole("group", { name: "Which model writes the summary" }));
    const sums = within(local.getByRole("radiogroup", { name: "Summary model" })).getAllByRole("radio");
    expect(sums.map((r) => (r as HTMLInputElement).labels?.[0]?.textContent)).toEqual([
      "Qwen 3.5 4BRecommended",
      "Qwen 3.5 9BMore thorough",
    ]);
    expect(card("qwen3.5-4b-q4_k_m")).toHaveTextContent("Runs on the processor.");
    expect(card("qwen3.5-9b-q4_k_m")).toHaveTextContent("Slower. More thorough summaries.");
    expect(card("qwen3.5-9b-q4_k_m")).not.toHaveTextContent("Too slow for live speech on this computer");
  });

  it("uses the desktop copy for the lighter model and the faster summary model", async () => {
    const s = setup({ machine: "desktop" });
    await openPage(s, "Transcription");
    expect(card("parakeet-tdt-110m")).toHaveTextContent(
      "For when the graphics card is busy with other work, or when space is tight. A little less accurate. Runs on the graphics card.",
    );
    expect(card("large-v3-turbo")).toHaveTextContent("Runs on the graphics card.");
    await goTo(s, "Summaries");
    const sums = within(screen.getByRole("radiogroup", { name: "Summary model" })).getAllByRole("radio");
    expect(sums.map((r) => (r as HTMLInputElement).labels?.[0]?.textContent)).toEqual([
      "Qwen 3.5 9BRecommended",
      "Qwen 3.5 4BFaster",
    ]);
    expect(card("qwen3.5-4b-q4_k_m")).toHaveTextContent("A shorter wait, slightly less thorough.");
  });

  it("shows a stored verdict of too slow or failed and keeps the model selectable", async () => {
    const s = setup({ machine: "surface" }, (ipc) => ({
      ...ipc,
      models: async () => {
        const v = await ipc.models();
        const verdict = { "parakeet-tdt-0.6b-v3": "TooSlow", "large-v3-turbo": "Fails" } as const;
        const patch = (i: (typeof v.transcription)[number]) =>
          i.name in verdict ? { ...i, check: verdict[i.name as keyof typeof verdict] } : i;
        return { ...v, transcription: v.transcription.map(patch) };
      },
    }));
    await openPage(s, "Transcription");
    expect(card("parakeet-tdt-0.6b-v3")).toHaveTextContent("Too slow for live speech on this computer");
    expect(card("parakeet-tdt-0.6b-v3")).toHaveTextContent("You can still use it.");
    expect(card("large-v3-turbo")).toHaveTextContent("Did not work on this computer");
    expect(card("parakeet-tdt-110m")).not.toHaveTextContent(/Keeps up|Too slow|Did not work/);
    const slow = screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ });
    expect(slow).toBeEnabled();
    await s.user.click(slow);
    await waitFor(() => expect(slow).toBeChecked());
  });

  it("downloads a model with progress, pauses keeping what arrived, and resumes to installed", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("button", { name: "Download Whisper large-v3-turbo (1.6 GB)" }));
    advance(s.timer, 3 * MODEL_STEP_MS);
    const whisper = card("large-v3-turbo");
    await waitFor(() => expect(whisper).toHaveTextContent("487 MB of 1.6 GB"));
    expect(within(whisper).getByRole("progressbar")).toBeInTheDocument();

    await s.user.click(within(whisper).getByRole("button", { name: "Pause Whisper large-v3-turbo" }));
    const resume = await within(card("large-v3-turbo")).findByRole("button", {
      name: "Resume Whisper large-v3-turbo",
    });
    expect(card("large-v3-turbo")).toHaveTextContent("Partly downloaded: 487 MB of 1.6 GB");
    // Paused means nothing more arrives.
    advance(s.timer, 5 * MODEL_STEP_MS);
    expect(card("large-v3-turbo")).toHaveTextContent("Partly downloaded: 487 MB of 1.6 GB");

    await s.user.click(resume);
    advance(s.timer, 7 * MODEL_STEP_MS);
    await waitFor(() => expect(card("large-v3-turbo")).toHaveTextContent("Installed"));
    expect(
      within(card("large-v3-turbo")).getByRole("button", { name: "Remove Whisper large-v3-turbo (frees 1.6 GB)" }),
    ).toBeInTheDocument();
  });

  it("removes a model that is not kept, showing the space freed; kept ones have no Remove", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Summaries");
    // The chosen model, the fallback and the summary model are kept.
    expect(within(card("qwen3.5-4b-q4_k_m")).queryByRole("button", { name: /^Remove/ })).toBeNull();
    await goTo(s, "Transcription");
    for (const m of ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m"]) {
      expect(within(card(m)).queryByRole("button", { name: /^Remove/ })).toBeNull();
    }
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ }));
    const remove = await within(card("parakeet-tdt-0.6b-v2")).findByRole("button", {
      name: "Remove Parakeet 0.6B v2 (frees 2.5 GB)",
    });
    await s.user.click(remove);
    expect(await screen.findByText("Parakeet 0.6B v2 removed")).toBeInTheDocument();
    expect(screen.getByText("2.5 GB freed.")).toBeInTheDocument();
    await waitFor(() => expect(card("parakeet-tdt-0.6b-v2")).toHaveTextContent("Download 2.5 GB"));
  });

  it("lists the NPU versions on an ARM64 build and runs a model on the NPU only while its version is there", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    const section = within(await screen.findByRole("region", { name: "NPU versions" }));
    expect(section.getByText("Parakeet 0.6B v2 (NPU version)")).toBeInTheDocument();
    expect(section.getByText("Parakeet 110M (NPU version)")).toBeInTheDocument();
    // Kept with their models (the chosen model and the fallback): no Remove.
    expect(section.queryByRole("button", { name: /^Remove/ })).toBeNull();
    expect(card("parakeet-tdt-0.6b-v2")).toHaveTextContent("Runs on the NPU.");
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ }));
    const size = (await s.ipc.models()).npu.find((m) => m.name === "parakeet-tdt-0.6b-v2-npu")?.size ?? 0;
    const remove = await within(card("parakeet-tdt-0.6b-v2-npu")).findByRole("button", {
      name: `Remove Parakeet 0.6B v2 (NPU version) (frees ${formatSize(size)})`,
    });
    await s.user.click(remove);
    await waitFor(() =>
      expect(card("parakeet-tdt-0.6b-v2-npu")).toHaveTextContent(`Download ${formatSize(size)}`),
    );
    expect(card("parakeet-tdt-0.6b-v2")).not.toHaveTextContent("Runs on the NPU.");
    expect(card("parakeet-tdt-110m")).toHaveTextContent("Runs on the NPU.");
    // Downloaded again: back on the NPU.
    await s.user.click(
      within(card("parakeet-tdt-0.6b-v2-npu")).getByRole("button", {
        name: `Download Parakeet 0.6B v2 (NPU version) (${formatSize(size)})`,
      }),
    );
    advance(s.timer, 12 * MODEL_STEP_MS);
    await waitFor(() => expect(card("parakeet-tdt-0.6b-v2-npu")).toHaveTextContent("Installed"));
    await waitFor(() => expect(card("parakeet-tdt-0.6b-v2")).toHaveTextContent("Runs on the NPU."));
  });

  it("lists no NPU versions on an x64 build", async () => {
    const s = setup({ machine: "desktop" });
    await openPage(s, "Transcription");
    expect(screen.queryByRole("region", { name: "NPU versions" })).toBeNull();
    expect((await s.ipc.models()).npu).toEqual([]);
  });

  it("shows a refused removal", async () => {
    const s = setup({ machine: "surface" }, (ipc) => ({
      ...ipc,
      removeModel: () =>
        Promise.reject(
          new IpcFailure({
            kind: "model_in_use",
            message: "Parakeet 0.6B v2 is in use; choose another model first",
            retryable: false,
          }),
        ),
    }));
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ }));
    await s.user.click(
      await within(card("parakeet-tdt-0.6b-v2")).findByRole("button", { name: /^Remove Parakeet 0\.6B v2/ }),
    );
    expect(await screen.findByText("Parakeet 0.6B v2 was not removed")).toBeInTheDocument();
    expect(
      screen.getByText("Parakeet 0.6B v2 is in use; choose another model first"),
    ).toBeInTheDocument();
  });

  it("runs the check with its progress and shows the result", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "System");
    const line = screen.getByRole("status", { name: "Model check" });
    expect(line).toHaveTextContent("Keeps up with live speech on this computer");
    expect(line).toHaveTextContent(
      "Last check: Parakeet 0.6B v2 on the NPU. If a meeting falls behind, Earshot switches to Parakeet 110M.",
    );
    await s.user.click(within(line).getByRole("button", { name: "Check again" }));
    expect(screen.getByRole("status", { name: "Model check" })).toHaveTextContent("Starting the check");
    advance(s.timer, MODEL_STEP_MS);
    expect(screen.getByRole("status", { name: "Model check" })).toHaveTextContent(
      "Checking Parakeet 0.6B v2 on the NPU",
    );
    advance(s.timer, 2 * MODEL_STEP_MS);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Check again" })).toBeInTheDocument(),
    );
    expect(screen.getByRole("status", { name: "Model check" })).toHaveTextContent("Keeps up with live speech on this computer");
  });

  it("cancels a running check and says why it stopped", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "System");
    await s.user.click(screen.getByRole("button", { name: "Check again" }));
    advance(s.timer, MODEL_STEP_MS);
    await s.user.click(
      within(screen.getByRole("status", { name: "Model check" })).getByRole("button", { name: "Cancel" }),
    );
    expect(await screen.findByText("The check stopped")).toBeInTheDocument();
    expect(screen.getByText("The model check was cancelled.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check again" })).toBeInTheDocument();
    // Nothing more arrives from the cancelled check.
    advance(s.timer, 5 * MODEL_STEP_MS);
    expect(screen.getByRole("status", { name: "Model check" })).not.toHaveTextContent("Checking");
  });

  it("shows a check warning as a message bar", async () => {
    const warning =
      "The Cuda library of ONNX Runtime is unavailable, so the model check tests Parakeet models on the processor only.";
    const s = setup({ machine: "desktop", modelCheckWarning: warning });
    await openPage(s, "System");
    await s.user.click(screen.getByRole("button", { name: "Check again" }));
    advance(s.timer, 0);
    expect(await screen.findByText(warning)).toBeInTheDocument();
    // The notice is titled; the section heading of the same words is left out.
    expect(screen.getByText("Model check", { ignore: "h2, script, style" })).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(warning)).toBeNull();
  });

  it("shows a check that could not start", async () => {
    const s = setup({ machine: "surface" }, (ipc) => ({
      ...ipc,
      runCheck: () =>
        Promise.reject(
          new IpcFailure({ kind: "busy", message: "A meeting is running; check after it.", retryable: true }),
        ),
    }));
    await openPage(s, "System");
    await s.user.click(screen.getByRole("button", { name: "Check again" }));
    expect(await screen.findByText("The check could not start")).toBeInTheDocument();
    expect(screen.getByText("A meeting is running; check after it.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check again" })).toBeInTheDocument();
  });

  it("shows a refused setting and keeps what was stored", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 110M/ }));
    await waitFor(() => expect(screen.getByRole("radio", { name: /^Parakeet 110M/ })).toBeChecked());
    // German with an English-only model: the app refuses.
    await s.user.click(screen.getByRole("combobox", { name: "Meeting languages" }));
    await s.user.click(await screen.findByRole("menuitemcheckbox", { name: "German" }));
    expect(await screen.findByText("The setting was not saved")).toBeInTheDocument();
    expect(
      screen.getByText("Parakeet 110M does not transcribe de; choose a multilingual model"),
    ).toBeInTheDocument();
    expect((await s.ipc.modelSettings()).value.languages).toEqual(["en"]);
  });

  it("saves meeting languages and follows the new recommendation", async () => {
    const s = setup({ machine: "desktop" });
    await openPage(s, "Transcription");
    // The languages come first on the page: they decide the recommendation.
    expect(sections()[0]).toBe("Meeting languages");
    await s.user.click(screen.getByRole("combobox", { name: "Meeting languages" }));
    await s.user.click(await screen.findByRole("menuitemcheckbox", { name: "German" }));
    await waitFor(async () => expect((await s.ipc.modelSettings()).value.languages).toEqual(["en", "de"]));
    await goTo(s, "System");
    await waitFor(() =>
      expect(screen.getByRole("region", { name: "Recommendation" })).toHaveTextContent(
        "Transcription with Parakeet 0.6B v3 on the graphics card",
      ),
    );
  });

  it("shows a busy refusal of a setting", async () => {
    const s = setup({ machine: "surface" }, (ipc) => ({
      ...ipc,
      setModelSettings: () =>
        Promise.reject(new IpcFailure({ kind: "busy", message: "A meeting is running.", retryable: true })),
    }));
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 110M/ }));
    expect(await screen.findByText("The setting was not saved")).toBeInTheDocument();
    expect(screen.getByText("A meeting is running.")).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /^Parakeet 0\.6B v2/ })).toBeChecked();
  });

  it("starts the check after a transcription model's download and shows it running", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("button", { name: "Download Parakeet 0.6B v3 (2.5 GB)" }));
    advance(s.timer, 10 * MODEL_STEP_MS);
    await waitFor(() => expect(card("parakeet-tdt-0.6b-v3")).toHaveTextContent("Installed"));
    advance(s.timer, MODEL_STEP_MS);
    // The check is on the System page, which hears it from the state Settings holds.
    await goTo(s, "System");
    expect(screen.getByRole("status", { name: "Model check" })).toHaveTextContent(
      "Checking Parakeet 0.6B v2 on the NPU",
    );
    expect(
      within(screen.getByRole("status", { name: "Model check" })).getByRole("button", { name: "Cancel" }),
    ).toBeInTheDocument();
    advance(s.timer, 2 * MODEL_STEP_MS);
    await waitFor(() => expect(screen.getByRole("button", { name: "Check again" })).toBeInTheDocument());
  });

  it("refuses the check while a transcription model downloads", async () => {
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("button", { name: "Download Parakeet 0.6B v3 (2.5 GB)" }));
    advance(s.timer, 2 * MODEL_STEP_MS);
    await goTo(s, "System");
    await s.user.click(screen.getByRole("button", { name: "Check again" }));
    expect(await screen.findByText("The check could not start")).toBeInTheDocument();
    expect(screen.getByText(DOWNLOADING_MESSAGE)).toBeInTheDocument();
  });

  it("says when the check did not start after a download", async () => {
    // Two transcription downloads: the first to end cannot start the check while the
    // second still writes; the last one starts it.
    const s = setup({ machine: "surface" });
    await openPage(s, "Transcription");
    await s.user.click(screen.getByRole("button", { name: "Download Parakeet 0.6B v3 (2.5 GB)" }));
    advance(s.timer, 5 * MODEL_STEP_MS);
    await s.user.click(screen.getByRole("button", { name: "Download Whisper large-v3-turbo (1.6 GB)" }));
    advance(s.timer, 5 * MODEL_STEP_MS);
    expect(
      await screen.findByText(`The model check did not start after the download. ${DOWNLOADING_MESSAGE}`),
    ).toBeInTheDocument();
    expect(screen.getAllByText("Parakeet 0.6B v3").length).toBeGreaterThan(0);
    advance(s.timer, 6 * MODEL_STEP_MS);
    await goTo(s, "System");
    await waitFor(() =>
      expect(screen.getByRole("status", { name: "Model check" })).toHaveTextContent("Checking"),
    );
  });

  it("names models only by their display names in every text", async () => {
    const ids = Object.keys(MOCK_DISPLAY_NAMES);
    const names = Object.values(MOCK_DISPLAY_NAMES).sort((a, b) => b.length - a.length);
    const idIn = (text: string) => {
      let rest = text;
      for (const n of names) rest = rest.split(n).join("|");
      return ids.find((id) => rest.includes(id)) ?? null;
    };
    expect(idIn("Whisper base.en")).toBeNull();
    expect(idIn("base.en is too slow")).toBe("base.en");
    for (const machine of ["surface", "desktop"] as const) {
      const s = setup({ machine });
      await openPage(s, "Transcription");
      const texts = [document.body.textContent ?? ""];
      await goTo(s, "Summaries");
      texts.push(document.body.textContent ?? "");
      // Force the notes: a model without an NPU version on the NPU, summaries on the card.
      await goTo(s, "Transcription");
      await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ }));
      await waitFor(() => expect(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ })).toBeChecked());
      texts.push(document.body.textContent ?? "");
      await goTo(s, "System");
      texts.push(document.body.textContent ?? "");
      await s.user.selectOptions(screen.getByRole("combobox", { name: "Transcription" }), machine === "surface" ? "Npu" : "Processor");
      if (machine === "surface") await screen.findByText(/has no NPU version/);
      texts.push(document.body.textContent ?? "");
      await s.user.click(screen.getByRole("button", { name: "Check again" }));
      advance(s.timer, MODEL_STEP_MS);
      texts.push(document.body.textContent ?? "");
      advance(s.timer, 2 * MODEL_STEP_MS);
      await waitFor(() => expect(screen.getByRole("button", { name: "Check again" })).toBeInTheDocument());
      texts.push(document.body.textContent ?? "");
      await goTo(s, "Transcription");
      texts.push(document.body.textContent ?? "");
      for (const t of texts) expect(idIn(t)).toBeNull();
      cleanup();
    }
  });

  it("has no em dash, please or exclamation mark in any section", async () => {
    for (const machine of ["surface", "desktop"] as const) {
      const s = setup({ machine });
      await openSettings(s);
      const texts = [document.body.textContent ?? ""];
      for (const section of SETTINGS_SECTIONS) {
        await goTo(s, section);
        texts.push(document.body.textContent ?? "");
      }
      for (const t of texts) expect(t).not.toMatch(/—|please|!/i);
      cleanup();
    }
  });
});
