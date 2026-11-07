import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { CUDA_STEP_MS, type CudaMockData } from "../ipc/mockCuda";
import { type Ipc, IpcFailure, IpcProvider, createMockIpc } from "../ipc";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { CUDA_APPLIES, CUDA_CHECKING_LINE, CUDA_RUNNING, CUDA_TRANSCRIPTION_WHY } from "./CudaSection";

const HEADING = "NVIDIA CUDA libraries";

interface Setup {
  ipc: Ipc;
  timer: ManualTimer;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(
  machine: "desktop" | "surface",
  cuda: CudaMockData = {},
  override?: (ipc: Ipc) => Ipc,
): Promise<Setup> {
  const timer = createManualTimer(1_000_000);
  const base = createMockIpc({ meetings: [], timer, machine, cuda });
  const ipc = override ? override(base) : base;
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  await screen.findByRole("list", { name: "This is your hardware" });
  return { ipc, timer, user };
}

const section = async () =>
  within(await screen.findByRole("region", { name: HEADING }));
const part = async (name: "Summaries" | "Transcription") => {
  const s = await screen.findByRole("region", { name: HEADING });
  const found = s.querySelector<HTMLElement>(`[data-part="${name}"]`);
  if (found === null) throw new Error(`no ${name} part`);
  return within(found);
};

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Models: the CUDA section", () => {
  it("is not there on a computer without a suitable card", async () => {
    const read = vi.fn();
    await open("surface", {}, (ipc) => ({
      ...ipc,
      cudaSettings: async () => {
        const view = await ipc.cudaSettings();
        read(view.offered);
        return view;
      },
    }));
    // The section's own read answered, and answered no.
    await waitFor(() => expect(read).toHaveBeenCalledWith(false));
    await act(async () => {});
    expect(screen.queryByRole("region", { name: HEADING })).toBeNull();
    expect(screen.queryByText(CUDA_APPLIES)).toBeNull();
  });

  it("keeps each switch's knob from inheriting the column's right alignment", async () => {
    // Fluent slides the knob from where text alignment puts it: inherited right alignment
    // starts it on the right, and "on" slides it further right.
    await open("desktop");
    for (const name of ["Summaries", "Transcription"] as const) {
      const input = (await part(name)).getByRole("switch");
      const indicator = input.parentElement?.querySelector(".fui-Switch__indicator");
      if (!indicator) throw new Error("no indicator");
      expect(getComputedStyle(indicator).textAlign).toBe("left");
    }
  });

  it("offers two parts, off, with their own sizes and what they do", async () => {
    await open("desktop");
    const s = await section();
    expect(s.getByText(/NVIDIA GeForce RTX 5080 can also run/)).toBeTruthy();
    const summaries = await part("Summaries");
    expect(summaries.getByRole("switch", { name: "Summaries on CUDA" })).not.toBeChecked();
    expect(summaries.getByText("484 MB to download, 623 MB on disk.")).toBeTruthy();
    const transcription = await part("Transcription");
    expect(transcription.getByRole("switch", { name: "Transcription on CUDA" })).not.toBeChecked();
    expect(transcription.getByText("1.6 GB to download, 800 MB on disk.")).toBeTruthy();
    // Plain about what it gains, and no number until one was measured.
    const why = transcription.getByText((t) => t.startsWith(CUDA_TRANSCRIPTION_WHY));
    expect(why.textContent).not.toMatch(/\d/);
    expect(s.getByText(CUDA_APPLIES)).toBeTruthy();
  });

  it("fills the slot only with what was measured", async () => {
    await open("desktop", { cpuSaved: "Measured on one meeting replay." });
    const transcription = await part("Transcription");
    expect(transcription.getByText(/Measured on one meeting replay\./)).toBeTruthy();
  });

  it("says so when the download does not exist yet and offers no button", async () => {
    const why = "This download is not available yet, so there is nothing to install.";
    await open("desktop", { notAvailable: why });
    for (const name of ["Summaries", "Transcription"] as const) {
      const p = await part(name);
      expect(p.getByRole("status")).toHaveTextContent(why);
      expect(p.queryByRole("button", { name: /^Download/ })).toBeNull();
      expect(p.queryByRole("checkbox")).toBeNull();
      expect(p.getByRole("switch")).toBeDisabled();
      expect(p.getByText(/^About .* to download, about .* on disk\.$/)).toBeTruthy();
    }
  });

  it("shows NVIDIA's terms and takes no download before they are accepted", async () => {
    const opened: string[] = [];
    const asked = vi.fn();
    const s = await open("desktop", { onOpenTerms: (u) => opened.push(u) }, (ipc) => ({
      ...ipc,
      startCudaDownload: (part, accepted) => {
        asked(part, accepted);
        return ipc.startCudaDownload(part, accepted);
      },
    }));
    const summaries = await part("Summaries");
    const download = summaries.getByRole("button", { name: "Download Summaries on CUDA (484 MB)" });
    // Not ticked: what was ticked goes to the app, and its refusal is what shows.
    await s.user.click(download);
    expect(asked).toHaveBeenLastCalledWith("Summaries", false);
    await (await section()).findByText("Accept NVIDIA's terms before this download.");
    expect(summaries.queryByRole("button", { name: "Pause Summaries on CUDA" })).toBeNull();
    expect(summaries.getByText("https://docs.nvidia.com/cuda/eula/index.html")).toBeTruthy();
    await s.user.click(summaries.getByRole("button", { name: "NVIDIA CUDA EULA" }));
    expect(opened).toEqual(["https://docs.nvidia.com/cuda/eula/index.html"]);
    // Transcription has cuDNN's too.
    const transcription = await part("Transcription");
    expect(transcription.getByRole("button", { name: "NVIDIA cuDNN licence" })).toBeTruthy();

    await s.user.click(summaries.getByRole("checkbox"));
    await s.user.click(download);
    expect(asked).toHaveBeenLastCalledWith("Summaries", true);
    await summaries.findByRole("button", { name: "Pause Summaries on CUDA" });
    for (let i = 0; i < 10; i += 1) {
      act(() => s.timer.advance(CUDA_STEP_MS));
    }
    await summaries.findByText("Installed");
    expect(summaries.queryByRole("checkbox")).toBeNull();
    expect(summaries.getByRole("switch")).toBeEnabled();
  });

  it("rejects the transcription part while summaries is missing, and shows why", async () => {
    const s = await open("desktop");
    const transcription = await part("Transcription");
    await s.user.click(transcription.getByRole("checkbox"));
    await s.user.click(transcription.getByRole("button", { name: /^Download Transcription/ }));
    const sec = await section();
    await sec.findByText(/this part needs its files/);
  });

  it("keeps transcription off until summaries is on, and off goes with summaries", async () => {
    const spy = vi.fn();
    const s = await open("desktop", { installed: ["Summaries", "Transcription"] }, (ipc) => ({
      ...ipc,
      setCudaSettings: (v) => {
        spy(v);
        return ipc.setCudaSettings(v);
      },
    }));
    const summaries = await part("Summaries");
    const transcription = await part("Transcription");
    const tSwitch = () => transcription.getByRole("switch", { name: "Transcription on CUDA" });
    expect(tSwitch()).toBeDisabled();
    await s.user.click(summaries.getByRole("switch", { name: "Summaries on CUDA" }));
    await waitFor(() => expect(tSwitch()).toBeEnabled());
    await s.user.click(tSwitch());
    await waitFor(() => expect(tSwitch()).toBeChecked());
    await s.user.click(summaries.getByRole("switch", { name: "Summaries on CUDA" }));
    await waitFor(() => expect(tSwitch()).not.toBeChecked());
    expect(spy).toHaveBeenLastCalledWith({ summaries: false, transcription: false });
  });

  it("says a switch applies at the next start and offers a restart", async () => {
    const onRestart = vi.fn();
    const s = await open("desktop", { installed: ["Summaries"], onRestart });
    const sec = await section();
    expect(sec.queryByRole("button", { name: "Restart Earshot" })).toBeNull();
    const summaries = await part("Summaries");
    await s.user.click(summaries.getByRole("switch", { name: "Summaries on CUDA" }));
    const restart = await sec.findByRole("button", { name: "Restart Earshot" });
    await s.user.click(restart);
    expect(onRestart).toHaveBeenCalledOnce();
    // Back to what this run started with: nothing to restart for.
    await s.user.click(summaries.getByRole("switch", { name: "Summaries on CUDA" }));
    await waitFor(() => expect(sec.queryByRole("button", { name: "Restart Earshot" })).toBeNull());
  });

  it("says what summaries stay on, from the app", async () => {
    await open("desktop", { staysOn: "the processor" });
    const summaries = await part("Summaries");
    expect(summaries.getByText(/instead of the processor\./)).toBeTruthy();
    expect(summaries.queryByText(/Vulkan/)).toBeNull();
  });

  it("offers no restart while a download runs", async () => {
    const s = await open("desktop", {
      installed: ["Summaries"],
      switch: { summaries: true, transcription: false },
      startedWith: { summaries: false, transcription: false },
    });
    const sec = await section();
    expect(sec.getByRole("button", { name: "Restart Earshot" })).toBeEnabled();
    const transcription = await part("Transcription");
    await s.user.click(transcription.getByRole("checkbox"));
    await s.user.click(transcription.getByRole("button", { name: /^Download Transcription/ }));
    await waitFor(() => expect(sec.getByRole("button", { name: "Restart Earshot" })).toBeDisabled());
    expect(sec.getByText(/A download is running/)).toBeTruthy();
  });

  it("offers no restart while a meeting is on", async () => {
    await open("desktop", {
      installed: ["Summaries"],
      switch: { summaries: true, transcription: false },
      startedWith: { summaries: false, transcription: false },
      meetingOn: true,
    });
    const sec = await section();
    expect(sec.getByRole("button", { name: "Restart Earshot" })).toBeDisabled();
    expect(sec.getByText(/A meeting is on/)).toBeTruthy();
  });

  it("shows a refused restart as the app's own words", async () => {
    const s = await open(
      "desktop",
      {
        installed: ["Summaries"],
        switch: { summaries: true, transcription: false },
        startedWith: { summaries: false, transcription: false },
      },
      (ipc) => ({
        ...ipc,
        restartApp: async () => {
          throw new IpcFailure({ kind: "busy", message: "Something is running.", retryable: true });
        },
      }),
    );
    const sec = await section();
    await s.user.click(sec.getByRole("button", { name: "Restart Earshot" }));
    await sec.findByText("Something is running.");
  });

  it("removes each part on its own, and says why when it will not", async () => {
    const s = await open("desktop", { installed: ["Summaries", "Transcription"] });
    const summaries = await part("Summaries");
    const transcription = await part("Transcription");
    const removeSummaries = () => summaries.getByRole("button", { name: /^Remove Summaries/ });
    expect(removeSummaries()).toBeDisabled();
    expect(summaries.getByText(/needs these files/)).toBeTruthy();
    await s.user.click(transcription.getByRole("button", { name: /^Remove Transcription/ }));
    await (await section()).findByText(/Transcription on CUDA removed/);
    await waitFor(() => expect(removeSummaries()).toBeEnabled());
    expect(transcription.queryByRole("button", { name: /^Remove/ })).toBeNull();
    await s.user.click(removeSummaries());
    await waitFor(() => expect(summaries.queryByRole("button", { name: /^Remove/ })).toBeNull());
  });

  it("says the start is still trying the files, and refreshes the page when it has finished", async () => {
    const reads = vi.fn();
    const s = await open(
      "desktop",
      {
        installed: ["Summaries"],
        switch: { summaries: true, transcription: false },
        summariesOnCuda: true,
        checkingMs: 5_000,
      },
      (ipc) => ({
        ...ipc,
        models: () => {
          reads();
          return ipc.models();
        },
      }),
    );
    // The page did not wait: everything else is there, and the section says why it is not final.
    const sec = await section();
    expect(screen.getByRole("list", { name: "This is your hardware" })).toBeTruthy();
    expect(await sec.findByText(CUDA_CHECKING_LINE)).toBeTruthy();
    expect(sec.queryByText(CUDA_RUNNING)).toBeNull();
    const summaries = await part("Summaries");
    expect(summaries.getByRole("button", { name: /^Remove Summaries/ })).toBeDisabled();
    expect(summaries.getByText(/still trying these files/)).toBeTruthy();
    const before = reads.mock.calls.length;

    act(() => s.timer.advance(5_000));
    await sec.findByText(CUDA_RUNNING);
    expect(sec.queryByText(CUDA_CHECKING_LINE)).toBeNull();
    await waitFor(() => expect(reads.mock.calls.length).toBeGreaterThan(before));
  });

  it("says when summaries are on CUDA, from what the start found", async () => {
    await open("desktop", {
      installed: ["Summaries"],
      switch: { summaries: true, transcription: false },
      summariesOnCuda: true,
    });
    const sec = await section();
    expect(sec.getByRole("status")).toHaveTextContent(CUDA_RUNNING);
    const summaries = await part("Summaries");
    expect(summaries.getByRole("button", { name: /^Remove Summaries/ })).toBeDisabled();
  });
});
