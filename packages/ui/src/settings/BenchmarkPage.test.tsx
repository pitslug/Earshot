import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import {
  BENCHMARK_MEETING_MS,
  BENCHMARK_STEP_MS,
  type Ipc,
  IpcProvider,
  MOCK_BENCHMARK_FAILURE,
  type MockIpcData,
  createMockIpc,
  mockBenchmarkResult,
} from "../ipc";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { BENCHMARK_EXPLAINED, BENCHMARK_PRIVATE, lengthNote, minutesText, progressText } from "./BenchmarkPage";

interface Setup {
  ipc: Ipc;
  timer: ManualTimer;
  user: ReturnType<typeof userEvent.setup>;
}

async function open(data: MockIpcData = {}): Promise<Setup> {
  const timer = createManualTimer(1_000_000);
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
  await user.click(within(nav).getByRole("button", { name: "Benchmark" }));
  await screen.findByRole("heading", { level: 1, name: "Benchmark" });
  return { ipc, timer, user };
}

/** Moves the mock's clock by `steps` benchmark steps. */
function steps(timer: ManualTimer, n: number) {
  for (let i = 0; i < n; i++) {
    act(() => timer.advance(BENCHMARK_STEP_MS));
  }
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Settings, Benchmark", () => {
  it("comes after Models in the section list", async () => {
    await open();
    const nav = screen.getByRole("navigation", { name: "Settings sections" });
    const names = within(nav)
      .getAllByRole("button")
      .map((b) => b.textContent);
    expect(names.indexOf("Benchmark")).toBe(names.indexOf("Models") + 1);
  });

  it("says what it does, how long it takes and where the meeting comes from", async () => {
    await open();
    expect(screen.getByText(BENCHMARK_EXPLAINED)).toBeInTheDocument();
    expect(screen.getByText(BENCHMARK_PRIVATE)).toBeInTheDocument();
    expect(screen.getByText(lengthNote(BENCHMARK_MEETING_MS))).toBeInTheDocument();
    expect(screen.getByText(/It takes about 3 minutes for the meeting/)).toBeInTheDocument();
    expect(screen.getByText(/AMI Meeting Corpus/)).toHaveTextContent("CC BY 4.0");
    expect(screen.getByText("The benchmark has not run on this computer yet.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run benchmark" })).toBeEnabled();
  });

  it("shows progress through the meeting and after it, then the result in plain words", async () => {
    const { timer, user } = await open();
    await user.click(screen.getByRole("button", { name: "Run benchmark" }));
    const running = await screen.findByRole("group", { name: "Benchmark running" });
    expect(within(running).getByText("Playing the meeting: 0:00 of 2:30")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Run benchmark" })).toBeNull();
    steps(timer, 3);
    expect(within(running).getByText("Playing the meeting: 0:30 of 2:30")).toBeInTheDocument();
    expect(within(running).getByRole("progressbar", { name: "Meeting played so far" })).toHaveAttribute(
      "aria-valuenow",
      "30000",
    );
    steps(timer, 12);
    expect(within(running).getByText("Playing the meeting: 2:30 of 2:30")).toBeInTheDocument();
    steps(timer, 1);
    expect(within(running).getByText("The meeting ended. Finishing the live transcript")).toBeInTheDocument();
    steps(timer, 1);
    expect(within(running).getByText("Making the final transcript")).toBeInTheDocument();
    steps(timer, 1);
    expect(within(running).getByText("Writing the summary")).toBeInTheDocument();
    steps(timer, 1);
    expect(screen.queryByRole("group", { name: "Benchmark running" })).toBeNull();
    const result = screen.getByRole("group", { name: "Last benchmark" });
    expect(
      within(result).getByText(
        "Live transcription kept up, and the summary was ready 48 seconds after the meeting ended.",
      ),
    ).toBeInTheDocument();
    const measured = within(result).getByLabelText("Measured");
    const terms = within(measured)
      .getAllByRole("term")
      .map((t) => t.textContent);
    expect(terms).toEqual([
      "Transcript lag during the meeting (both tracks)",
      "Live transcript",
      "Final transcript",
      "Summary",
      "Model check",
    ]);
    expect(within(measured).getByText("Found Parakeet 0.6B v2 fast enough on the NPU")).toBeInTheDocument();
    const settings = within(result).getByLabelText("Settings used");
    expect(within(settings).getByText("Qwen 3.5 4B on the processor")).toBeInTheDocument();
    expect(within(result).getByText(/^Ran .* with Earshot 0\.0\.0-preview\.$/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run benchmark again" })).toBeEnabled();
  });

  it("shows the last result when the page opens again", async () => {
    await open({ benchmark: { stored: mockBenchmarkResult("FellBehind", 900_000) } });
    const result = screen.getByRole("group", { name: "Last benchmark" });
    expect(within(result).getByText(/^Live transcription fell behind\./)).toBeInTheDocument();
    expect(within(result).getByText("Fell behind: complete 74 seconds after the meeting ended")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run benchmark again" })).toBeInTheDocument();
  });

  it("cancels, saving nothing", async () => {
    const { ipc, timer, user } = await open();
    await user.click(screen.getByRole("button", { name: "Run benchmark" }));
    steps(timer, 2);
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("The benchmark was cancelled. Nothing was saved.")).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "Benchmark running" })).toBeNull();
    expect((await ipc.benchmark()).last).toBeNull();
    expect(screen.getByRole("button", { name: "Run benchmark" })).toBeEnabled();
  });

  it("says why a run failed or did not start", async () => {
    const { timer, user } = await open({ benchmark: { end: "Failed" } });
    await user.click(screen.getByRole("button", { name: "Run benchmark" }));
    steps(timer, 20);
    expect(screen.getByText(MOCK_BENCHMARK_FAILURE)).toBeInTheDocument();
  });

  it("says when another job keeps it from starting", async () => {
    const { user } = await open({
      benchmark: {
        startError: {
          kind: "busy",
          message: "A meeting or a meeting job is running; run the benchmark when it has finished.",
          retryable: true,
        },
      },
    });
    await user.click(screen.getByRole("button", { name: "Run benchmark" }));
    expect(await screen.findByText("The benchmark did not start")).toBeInTheDocument();
    expect(screen.getByText(/run the benchmark when it has finished/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Run benchmark" })).toBeEnabled();
  });

  it("shows a run under way when the page opens during it, and can cancel it", async () => {
    const { ipc, user } = await open({
      benchmark: { running: { phase: "Transcribing", heard_ms: 150_000, total_ms: 150_000 } },
    });
    const running = await screen.findByRole("group", { name: "Benchmark running" });
    expect(within(running).getByText("Making the final transcript")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Run benchmark" })).toBeNull();
    await user.click(within(running).getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("The benchmark was cancelled. Nothing was saved.")).toBeInTheDocument();
    expect((await ipc.benchmark()).running).toBeNull();
  });

  it("says that cancelling waits for the live transcript in progress", async () => {
    await open();
    expect(screen.getByText(/Cancelling stops it once the live transcript in progress is finished/)).toBeInTheDocument();
    expect(screen.queryByText(/at any time/)).toBeNull();
    expect(progressText({ status: "cancelling" })).toBe("Stopping once the live transcript in progress is finished");
  });

  it("shows the notes about the run's settings", async () => {
    const stored = {
      ...mockBenchmarkResult("KeptUp", 900_000),
      notes: ["Parakeet 0.6B v3 has no NPU version, so it runs on the processor."],
    };
    await open({ benchmark: { stored } });
    const result = screen.getByRole("group", { name: "Last benchmark" });
    expect(within(result).getByText(stored.notes[0])).toBeInTheDocument();
  });

  it("warns about a result it cannot read", async () => {
    await open({ benchmark: { unreadable: true } });
    expect(screen.getByText(/cannot be read\. Run the benchmark again\./)).toBeInTheDocument();
  });

  it("words the time and the phases", () => {
    expect(minutesText(0)).toBe("0:00");
    expect(minutesText(150_000)).toBe("2:30");
    expect(minutesText(61_400)).toBe("1:01");
    expect(progressText({ status: "starting" })).toBe("Loading the models");
    expect(progressText({ status: "running", phase: "Summarising", heardMs: 0, totalMs: 0 })).toBe(
      "Writing the summary",
    );
    expect(lengthNote(150_000)).toMatch(/^It takes about 3 minutes/);
  });
});
