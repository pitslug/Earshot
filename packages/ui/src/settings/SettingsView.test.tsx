import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { DOWNLOADING_MESSAGE, MOCK_DISPLAY_NAMES, MODEL_STEP_MS } from "../ipc/mockModels";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { SETTINGS_SECTIONS } from "./SettingsView";
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

async function openModels(s: Setup) {
  await s.user.click(await screen.findByRole("button", { name: "Settings" }));
  await screen.findByRole("heading", { level: 1, name: "Models" });
  await screen.findByRole("list", { name: "This is your hardware" });
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
  it("opens from the shell, switches sections and goes back to the meetings", async () => {
    const s = setup();
    await openModels(s);
    const nav = screen.getByRole("navigation", { name: "Settings sections" });
    const items = within(nav)
      .getAllByRole("button")
      .map((b) => b.textContent);
    expect(items).toEqual(["Back to meetings", ...SETTINGS_SECTIONS]);
    expect(within(nav).getByRole("button", { name: "Models" })).toHaveAttribute("aria-current", "page");

    await s.user.click(within(nav).getByRole("button", { name: "Audio" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Audio" })).toBeInTheDocument();
    expect(await screen.findByRole("combobox", { name: "Microphone" })).toBeInTheDocument();
    expect(within(nav).getByRole("button", { name: "Audio" })).toHaveAttribute("aria-current", "page");

    await s.user.click(within(nav).getByRole("button", { name: "Storage" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Storage" })).toBeInTheDocument();
    expect(await screen.findByRole("combobox", { name: "Keep recordings for" })).toBeInTheDocument();
    expect(within(nav).getByRole("button", { name: "Storage" })).toHaveAttribute("aria-current", "page");
    expect(within(nav).getByRole("button", { name: "Models" })).not.toHaveAttribute("aria-current");

    await s.user.click(within(nav).getByRole("button", { name: "Models" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Models" })).toBeInTheDocument();

    await s.user.click(within(nav).getByRole("button", { name: "Back to meetings" }));
    expect(await screen.findByRole("heading", { name: "Record your first meeting" })).toBeInTheDocument();
    expect(screen.queryByRole("navigation", { name: "Settings sections" })).toBeNull();
  });

  it("shows the Surface-like hardware and what runs on each part", async () => {
    const s = setup({ machine: "surface" });
    await openModels(s);
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
    await openModels(s);
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
    await openModels(s);
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ }));
    await waitFor(() => expect(hwRow("NPU")).toHaveTextContent("Not used"));
    expect(hwRow("Processor")).toHaveTextContent("Speaker detection, Transcription, Summaries");
    expect(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ })).toBeChecked();

    // Forcing the NPU for a model without an NPU version: it runs on the processor, with the note.
    await s.user.selectOptions(screen.getByRole("combobox", { name: "Transcription" }), "Npu");
    expect(
      await screen.findByText("Parakeet 0.6B v3 has no NPU version, so it runs on the processor."),
    ).toBeInTheDocument();
    expect(hwRow("NPU")).toHaveTextContent("Not used");

    // Back to a model with an NPU version: the NPU row follows.
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v2/ }));
    await waitFor(() => expect(hwRow("NPU")).toHaveTextContent("Transcription"));
    expect(screen.queryByText(/has no NPU version/)).toBeNull();
  });

  it("moves summaries to the processor when chosen on the desktop", async () => {
    const s = setup({ machine: "desktop" });
    await openModels(s);
    const select = screen.getByRole("combobox", { name: "Summaries" });
    expect(within(select).getAllByRole("option").map((o) => o.textContent)).toEqual([
      "Automatic (graphics card, then processor)",
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

  it("offers the speech helpers on a clean install, follows the download and never offers Remove", async () => {
    const s = setup({ cleanInstall: true });
    await openModels(s);
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
    await openModels(s);
    const group = screen.getByRole("radiogroup", { name: "Transcription model" });
    const radios = within(group).getAllByRole("radio");
    expect(radios.map((r) => (r as HTMLInputElement).labels?.[0]?.textContent)).toEqual([
      "Parakeet 0.6B v2RecommendedKeeps up",
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
      "About 100 languages, including Asian languages. Less accurate and slower than Parakeet: use it only for languages Parakeet v3 does not cover. Runs on the processor here (no NPU version).",
    );
    expect(card("large-v3-turbo")).toHaveTextContent("Download 1.6 GB");
    expect(card("parakeet-tdt-0.6b-v2")).toHaveTextContent("Installed");
    // Summaries: the shipped models, recommended first, no verdict without a stored one.
    const sums = within(screen.getByRole("radiogroup", { name: "Summary model" })).getAllByRole("radio");
    expect(sums.map((r) => (r as HTMLInputElement).labels?.[0]?.textContent)).toEqual([
      "Qwen 3.5 4BRecommended",
      "Qwen 3.5 9BMore thorough",
    ]);
    expect(card("qwen3.5-4b-q4_k_m")).toHaveTextContent("Runs on the processor.");
    expect(card("qwen3.5-9b-q4_k_m")).toHaveTextContent("Slower. More thorough summaries.");
    expect(card("qwen3.5-9b-q4_k_m")).not.toHaveTextContent("Too slow here");
    expect(screen.getByText("Your own endpoint or no summary: Settings, Summaries.")).toBeInTheDocument();
  });

  it("uses the desktop copy for the lighter model and the faster summary model", async () => {
    const s = setup({ machine: "desktop" });
    await openModels(s);
    expect(card("parakeet-tdt-110m")).toHaveTextContent(
      "For when the graphics card is busy with other work, or when space is tight. A little less accurate. Runs on the graphics card.",
    );
    expect(card("large-v3-turbo")).toHaveTextContent("Runs on the graphics card.");
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
    await openModels(s);
    expect(card("parakeet-tdt-0.6b-v3")).toHaveTextContent("Too slow here");
    expect(card("parakeet-tdt-0.6b-v3")).toHaveTextContent("You can still use it.");
    expect(card("large-v3-turbo")).toHaveTextContent("Did not work here");
    expect(card("parakeet-tdt-110m")).not.toHaveTextContent(/Keeps up|Too slow|Did not work/);
    const slow = screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ });
    expect(slow).toBeEnabled();
    await s.user.click(slow);
    await waitFor(() => expect(slow).toBeChecked());
  });

  it("downloads a model with progress, pauses keeping what arrived, and resumes to installed", async () => {
    const s = setup({ machine: "surface" });
    await openModels(s);
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
    await openModels(s);
    // The chosen model, the fallback and the summary model are kept.
    for (const m of ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m", "qwen3.5-4b-q4_k_m"]) {
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
    await openModels(s);
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
    await openModels(s);
    const line = screen.getByRole("status", { name: "Model check" });
    expect(line).toHaveTextContent("Keeps up");
    expect(line).toHaveTextContent(
      "Last check: Parakeet 0.6B v2 on the NPU. When it falls behind: Parakeet 110M.",
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
    expect(screen.getByRole("status", { name: "Model check" })).toHaveTextContent("Keeps up");
  });

  it("cancels a running check and says why it stopped", async () => {
    const s = setup({ machine: "surface" });
    await openModels(s);
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
    await openModels(s);
    await s.user.click(screen.getByRole("button", { name: "Check again" }));
    advance(s.timer, 0);
    expect(await screen.findByText(warning)).toBeInTheDocument();
    expect(screen.getByText("Model check")).toBeInTheDocument();
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
    await openModels(s);
    await s.user.click(screen.getByRole("button", { name: "Check again" }));
    expect(await screen.findByText("The check could not start")).toBeInTheDocument();
    expect(screen.getByText("A meeting is running; check after it.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check again" })).toBeInTheDocument();
  });

  it("shows a refused setting and keeps what was stored", async () => {
    const s = setup({ machine: "surface" });
    await openModels(s);
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
    await openModels(s);
    await s.user.click(screen.getByRole("combobox", { name: "Meeting languages" }));
    await s.user.click(await screen.findByRole("menuitemcheckbox", { name: "German" }));
    await waitFor(() =>
      expect(screen.getByRole("region", { name: "Recommendation" })).toHaveTextContent(
        "Transcription with Parakeet 0.6B v3 on the graphics card",
      ),
    );
    expect((await s.ipc.modelSettings()).value.languages).toEqual(["en", "de"]);
  });

  it("shows a busy refusal of a setting", async () => {
    const s = setup({ machine: "surface" }, (ipc) => ({
      ...ipc,
      setModelSettings: () =>
        Promise.reject(new IpcFailure({ kind: "busy", message: "A meeting is running.", retryable: true })),
    }));
    await openModels(s);
    await s.user.click(screen.getByRole("radio", { name: /^Parakeet 110M/ }));
    expect(await screen.findByText("The setting was not saved")).toBeInTheDocument();
    expect(screen.getByText("A meeting is running.")).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /^Parakeet 0\.6B v2/ })).toBeChecked();
  });

  it("starts the check after a transcription model's download and shows it running", async () => {
    const s = setup({ machine: "surface" });
    await openModels(s);
    await s.user.click(screen.getByRole("button", { name: "Download Parakeet 0.6B v3 (2.5 GB)" }));
    advance(s.timer, 10 * MODEL_STEP_MS);
    await waitFor(() => expect(card("parakeet-tdt-0.6b-v3")).toHaveTextContent("Installed"));
    advance(s.timer, MODEL_STEP_MS);
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
    await openModels(s);
    await s.user.click(screen.getByRole("button", { name: "Download Parakeet 0.6B v3 (2.5 GB)" }));
    advance(s.timer, 2 * MODEL_STEP_MS);
    await s.user.click(screen.getByRole("button", { name: "Check again" }));
    expect(await screen.findByText("The check could not start")).toBeInTheDocument();
    expect(screen.getByText(DOWNLOADING_MESSAGE)).toBeInTheDocument();
  });

  it("says when the check did not start after a download", async () => {
    // Two transcription downloads: the first to end cannot start the check while the
    // second still writes; the last one starts it.
    const s = setup({ machine: "surface" });
    await openModels(s);
    await s.user.click(screen.getByRole("button", { name: "Download Parakeet 0.6B v3 (2.5 GB)" }));
    advance(s.timer, 5 * MODEL_STEP_MS);
    await s.user.click(screen.getByRole("button", { name: "Download Whisper large-v3-turbo (1.6 GB)" }));
    advance(s.timer, 5 * MODEL_STEP_MS);
    expect(
      await screen.findByText(`The model check did not start after the download. ${DOWNLOADING_MESSAGE}`),
    ).toBeInTheDocument();
    expect(screen.getAllByText("Parakeet 0.6B v3").length).toBeGreaterThan(0);
    advance(s.timer, 6 * MODEL_STEP_MS);
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
      await openModels(s);
      const texts = [document.body.textContent ?? ""];
      // Force the notes: a model without an NPU version on the NPU, summaries on the card.
      await s.user.click(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ }));
      await s.user.selectOptions(screen.getByRole("combobox", { name: "Transcription" }), machine === "surface" ? "Npu" : "Processor");
      await waitFor(() => expect(screen.getByRole("radio", { name: /^Parakeet 0\.6B v3/ })).toBeChecked());
      if (machine === "surface") await screen.findByText(/has no NPU version/);
      texts.push(document.body.textContent ?? "");
      await s.user.click(screen.getByRole("button", { name: "Check again" }));
      advance(s.timer, MODEL_STEP_MS);
      texts.push(document.body.textContent ?? "");
      advance(s.timer, 2 * MODEL_STEP_MS);
      await waitFor(() => expect(screen.getByRole("button", { name: "Check again" })).toBeInTheDocument());
      texts.push(document.body.textContent ?? "");
      for (const t of texts) expect(idIn(t)).toBeNull();
      cleanup();
    }
  });

  it("has no em dash, please or exclamation mark in any section", async () => {
    for (const machine of ["surface", "desktop"] as const) {
      const s = setup({ machine });
      await openModels(s);
      const texts = [document.body.textContent ?? ""];
      const nav = screen.getByRole("navigation", { name: "Settings sections" });
      for (const section of SETTINGS_SECTIONS) {
        await s.user.click(within(nav).getByRole("button", { name: section }));
        if (section === "Models") await screen.findByRole("list", { name: "This is your hardware" });
        texts.push(document.body.textContent ?? "");
      }
      for (const t of texts) expect(t).not.toMatch(/—|please|!/i);
      cleanup();
    }
  });
});
