import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, type MockIpcData, createMockIpc } from "../ipc";
import cases from "../../../../fixtures/audio/device_available_cases.json";
import { SAMPLE_DEVICES, deviceAvailable } from "../ipc/mockAudio";
import { MODE_LABEL } from "../meeting/liveMeeting";
import { openSettings, renderApp } from "../test/app";
import { createManualTimer } from "../time";
import {
  DEFAULT_VALUE,
  HANDS_FREE_HINT,
  MICROPHONE_DESCRIPTION,
  MODE_DESCRIPTION,
  OUTPUT_DESCRIPTION,
  SIGNAL_DESCRIPTION,
  SIGNAL_LABEL,
} from "./AudioPage";

interface Setup {
  ipc: Ipc;
  user: ReturnType<typeof userEvent.setup>;
}

function mount(data: MockIpcData = {}): Setup {
  const timer = createManualTimer(1_000_000);
  const ipc = createMockIpc({ meetings: [], timer, ...data });
  renderApp(ipc, { timer });
  return { ipc, user: userEvent.setup() };
}

async function open(data: MockIpcData = {}): Promise<Setup> {
  const s = mount(data);
  await openSettings(s.user, "Audio");
  await screen.findByRole("heading", { level: 1, name: "Audio" });
  await screen.findByRole("combobox", { name: "Microphone" });
  return s;
}

const mic = () => screen.getByRole<HTMLSelectElement>("combobox", { name: "Microphone" });
const output = () => screen.getByRole<HTMLSelectElement>("combobox", { name: "Output" });
const options = (select: HTMLSelectElement) => [...select.options].map((o) => o.textContent);

const surface = SAMPLE_DEVICES.surface;
const [builtIn, headset] = surface.microphones;
const [, headphones] = surface.outputs;

function pageText(): string {
  return document.body.textContent ?? "";
}

describe("Settings, Audio", () => {
  it("lists the devices with the Windows default first on both sample machines", async () => {
    for (const machine of ["surface", "desktop"] as const) {
      await open({ machine });
      const devices = SAMPLE_DEVICES[machine];
      const defaultMic = devices.microphones.find((d) => d.is_default);
      const defaultOut = devices.outputs.find((d) => d.is_default);
      expect(options(mic())).toEqual([
        `Windows default (${defaultMic?.name})`,
        ...devices.microphones.map((d) => d.name),
      ]);
      expect(mic().value).toBe(DEFAULT_VALUE);
      expect(options(output())).toEqual([
        `Windows default (${defaultOut?.name})`,
        ...devices.outputs.map((d) => d.name),
      ]);
      expect(output().value).toBe(DEFAULT_VALUE);
      expect(screen.getByText(MICROPHONE_DESCRIPTION)).toBeInTheDocument();
      expect(screen.getByText(OUTPUT_DESCRIPTION)).toBeInTheDocument();
      expect(screen.getByRole("radio", { name: MODE_LABEL.Call })).toBeChecked();
      expect(screen.getByText(MODE_DESCRIPTION)).toBeInTheDocument();
      expect(screen.getByRole("heading", { name: "Audio check" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Run audio check" })).toBeInTheDocument();
      cleanup();
    }
  });

  it("stores a chosen microphone and output by id, and the default again", async () => {
    const s = await open();
    await s.user.selectOptions(mic(), builtIn.name);
    await waitFor(async () =>
      expect((await s.ipc.audioSettings()).value.microphone).toEqual({ id: builtIn.id, name: builtIn.name }),
    );
    expect(mic().value).toBe(builtIn.id);
    await s.user.selectOptions(output(), headphones.name);
    await waitFor(async () =>
      expect((await s.ipc.audioSettings()).value.output).toEqual({ id: headphones.id, name: headphones.name }),
    );
    // The microphone stayed chosen.
    expect((await s.ipc.audioSettings()).value.microphone?.id).toBe(builtIn.id);
    await s.user.selectOptions(mic(), `Windows default (${builtIn.name})`);
    await waitFor(async () => expect((await s.ipc.audioSettings()).value.microphone).toBeNull());
    expect(mic().value).toBe(DEFAULT_VALUE);
    expect(output().value).toBe(headphones.id);
  });

  it("warns about a Bluetooth headset in hands-free mode", async () => {
    const s = await open();
    expect(screen.queryByText(HANDS_FREE_HINT)).toBeNull();
    await s.user.selectOptions(mic(), headset.name);
    expect(await screen.findByText(HANDS_FREE_HINT)).toBeInTheDocument();
    // Also when the headset is the Windows default.
    cleanup();
    await open({
      audio: {
        microphones: [
          { ...builtIn, is_default: false },
          { ...headset, is_default: true },
        ],
      },
    });
    expect(screen.getByText(HANDS_FREE_HINT)).toBeInTheDocument();
  });

  it("keeps a chosen device that is not connected and says meetings use the default", async () => {
    const gone = { id: "{0.0.1.00000000}.{gone}", name: "Acme Evolve" };
    const s = await open({ audio: { settings: { microphone: gone } } });
    expect(mic().value).toBe(gone.id);
    expect(options(mic())).toContain("Acme Evolve (not connected)");
    expect(
      screen.getByText(
        "Acme Evolve is not connected, so meetings record the Windows default microphone. It is used again from the next meeting you start with it connected.",
      ),
    ).toBeInTheDocument();
    // Choosing another device replaces it; the not-connected entry goes.
    await s.user.selectOptions(mic(), builtIn.name);
    await waitFor(() => expect(options(mic())).not.toContain("Acme Evolve (not connected)"));
    expect((await s.ipc.audioSettings()).value.microphone?.id).toBe(builtIn.id);
  });

  it("says when Windows has no output and when the devices cannot be listed", async () => {
    await open({ audio: { outputs: [] } });
    expect(options(output())).toEqual(["Windows default (none now)"]);
    expect(
      screen.getByText("Windows has no output now. Connect one before you start a meeting."),
    ).toBeInTheDocument();
    cleanup();
    await open({ audio: { listError: "Windows could not list the audio devices: the audio service is not running." } });
    expect(screen.getByText("The devices could not be listed")).toBeInTheDocument();
    expect(pageText()).toContain("the audio service is not running");
    expect(options(mic())).toEqual(["Windows default (none now)"]);
    expect(screen.queryByText(/before you start a meeting/)).toBeNull();
  });

  it("stores the capture mode, and a new meeting starts with it without listing devices", async () => {
    const s = await open();
    await s.user.click(screen.getByRole("radio", { name: MODE_LABEL.InRoom }));
    await waitFor(async () => expect(await s.ipc.defaultCaptureMode()).toBe("InRoom"));
    expect(screen.getByRole("radio", { name: MODE_LABEL.InRoom })).toBeChecked();
    cleanup();
    // The new-meeting screen preselects it through the cheap read, never the device list.
    const calls: string[] = [];
    const counting: Ipc = {
      ...s.ipc,
      audioSettings: async () => {
        calls.push("audioSettings");
        return s.ipc.audioSettings();
      },
      defaultCaptureMode: async () => {
        calls.push("defaultCaptureMode");
        return s.ipc.defaultCaptureMode();
      },
    };
    renderApp(counting, { timer: createManualTimer(1_000_000) });
    const user = userEvent.setup();
    await user.click((await screen.findAllByRole("button", { name: "New meeting" }))[0]);
    await waitFor(() => expect(screen.getByRole("radio", { name: MODE_LABEL.InRoom })).toBeChecked());
    expect(calls).toEqual(["defaultCaptureMode"]);
  });

  it("records the microphone with Windows' processing unless raw is chosen, and stores raw", async () => {
    const s = await open();
    expect(screen.getByRole("radio", { name: SIGNAL_LABEL.Processed })).toBeChecked();
    expect(screen.getByRole("radiogroup", { name: "Microphone sound processing" })).toBeInTheDocument();
    expect(pageText()).toContain(SIGNAL_DESCRIPTION);
    await s.user.click(screen.getByRole("radio", { name: SIGNAL_LABEL.Raw }));
    await waitFor(async () => expect((await s.ipc.audioSettings()).value.mic_signal).toBe("Raw"));
    expect(screen.getByRole("radio", { name: SIGNAL_LABEL.Raw })).toBeChecked();
    // The other choices are kept.
    const stored = (await s.ipc.audioSettings()).value;
    expect(stored.capture_mode).toBe("Call");
    expect(stored.microphone).toBeNull();
    await s.user.click(screen.getByRole("radio", { name: SIGNAL_LABEL.Processed }));
    await waitFor(async () => expect((await s.ipc.audioSettings()).value.mic_signal).toBe("Processed"));
  });

  it("pins the mock's device rule to the app's shared case table", () => {
    const table = cases as unknown as {
      cases: { name: string; listed: [string, boolean][]; chosen: string | null; available: boolean }[];
    };
    expect(table.cases.length).toBeGreaterThanOrEqual(9);
    for (const c of table.cases) {
      expect(deviceAvailable(c.listed, c.chosen), c.name).toBe(c.available);
    }
  });

  it("shows a refused setting and keeps what was stored", async () => {
    const s = await open();
    const failing: Ipc = {
      ...s.ipc,
      setAudioSettings: async () => {
        throw new Error("This device cannot be chosen: its id is not usable.");
      },
    };
    cleanup();
    renderApp(failing, { timer: createManualTimer(1_000_000) });
    const user = userEvent.setup();
    await openSettings(user, "Audio");
    await user.selectOptions(await screen.findByRole("combobox", { name: "Microphone" }), builtIn.name);
    expect(await screen.findByText("This setting was not saved")).toBeInTheDocument();
    expect(pageText()).toContain("its id is not usable");
    expect(mic().value).toBe(DEFAULT_VALUE);
    await user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("This setting was not saved")).toBeNull();
  });

  it("lists the devices again on refresh", async () => {
    const s = await open();
    let calls = 0;
    const counting: Ipc = {
      ...s.ipc,
      audioSettings: async () => {
        calls += 1;
        return s.ipc.audioSettings();
      },
    };
    cleanup();
    renderApp(counting, { timer: createManualTimer(1_000_000) });
    const user = userEvent.setup();
    await openSettings(user, "Audio");
    await screen.findByRole("combobox", { name: "Microphone" });
    const before = calls;
    await user.click(screen.getByRole("button", { name: "Refresh devices" }));
    await waitFor(() => expect(calls).toBe(before + 1));
  });
});
