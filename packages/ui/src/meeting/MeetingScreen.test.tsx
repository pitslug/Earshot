import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import {
  CATCH_UP_MS,
  type Ipc,
  IpcProvider,
  type MeetingEvent,
  type MockIpcData,
  POST_STEPS,
  POST_STEP_MS,
  SCRIPT_LINES,
  SCRIPT_WARNING_AT,
  createMockIpc,
} from "../ipc";
import { AppShell } from "../shell/AppShell";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { type ManualTimer, createManualTimer } from "../time";
import { MAX_TITLE, RecordingView } from "./MeetingViews";
import { type LiveMeeting, reduce, startedMeeting } from "./liveMeeting";

const START = new Date(2026, 10, 2, 10, 0).getTime();

interface Setup {
  timer: ManualTimer;
  ipc: Ipc;
  compactCalls: boolean[];
  user: ReturnType<typeof userEvent.setup>;
}

function setup(data: MockIpcData = {}): Setup {
  const timer = createManualTimer(START);
  const compactCalls: boolean[] = [];
  const ipc = createMockIpc({
    meetings: [],
    timer,
    onSetCompact: (c) => compactCalls.push(c),
    ...data,
  });
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell timer={timer} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  return { timer, ipc, compactCalls, user: userEvent.setup() };
}

const OFFER_AFTER_MS = 5_000;

describe("the offer to stop when a call ends", () => {
  function countKeeps(ipc: Ipc) {
    const kept = { count: 0 };
    const real = ipc.keepRecording.bind(ipc);
    ipc.keepRecording = async () => {
      kept.count += 1;
      await real();
    };
    return kept;
  }

  it("says it will stop, and the mock leaves the recording to the app", async () => {
    const s = setup({ callEndOfferAfterMs: OFFER_AFTER_MS });
    await startMeeting(s);
    expect(screen.queryByText("The call seems to have ended.")).toBeNull();
    advance(s.timer, OFFER_AFTER_MS);
    await screen.findByText("The call seems to have ended.");
    expect(
      screen.getByText("Earshot will stop recording within 3 minutes unless you keep it going."),
    ).toBeTruthy();
    // The stop itself is the app's: a driver test covers it. The question stays open until then.
    advance(s.timer, 60_000);
    expect(screen.getByText("The call seems to have ended.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Stop meeting" })).toBeTruthy();
    expect((await s.ipc.listMeetings())[0].state).toBe("Recording");
  });

  it("Keep recording closes the question and tells the app", async () => {
    const s = setup({ callEndOfferAfterMs: OFFER_AFTER_MS });
    const kept = countKeeps(s.ipc);
    await startMeeting(s);
    advance(s.timer, OFFER_AFTER_MS);
    await s.user.click(await screen.findByRole("button", { name: "Keep recording" }));
    expect(screen.queryByText("The call seems to have ended.")).toBeNull();
    expect(kept.count).toBe(1);
    expect((await s.ipc.listMeetings())[0].state).toBe("Recording");
  });

  it("Stop recording stops the meeting", async () => {
    const s = setup({ callEndOfferAfterMs: OFFER_AFTER_MS });
    await startMeeting(s);
    advance(s.timer, OFFER_AFTER_MS);
    await s.user.click(await screen.findByRole("button", { name: "Stop recording" }));
    await waitFor(async () => {
      expect((await s.ipc.listMeetings())[0].state).not.toBe("Recording");
    });
    expect(screen.queryByText("The call seems to have ended.")).toBeNull();
  });

  it("is not asked in the room", async () => {
    const s = setup({ callEndOfferAfterMs: OFFER_AFTER_MS });
    await startMeeting(s, "InRoom");
    advance(s.timer, OFFER_AFTER_MS * 2);
    expect(screen.queryByText("The call seems to have ended.")).toBeNull();
  });

  it("goes away when the app takes it back or the recording ends", () => {
    const started = startedMeeting("m", "Call", ["You", "Them"], 0);
    const open = reduce(started, { type: "CallEndOffer", open: true }, 1);
    expect(open?.callEndOffer).toBe(true);
    expect(reduce(open, { type: "CallEndOffer", open: false }, 2)?.callEndOffer).toBe(false);
    expect(reduce(open, { type: "CallEndStopped" }, 2)?.callEndOffer).toBe(false);
    expect(
      reduce(open, { type: "Ended", meeting: "m", reason: "Stopped" }, 2)?.callEndOffer,
    ).toBe(false);
  });
});

function advance(timer: ManualTimer, ms: number) {
  act(() => timer.advance(ms));
}

async function startMeeting(s: Setup, mode: "Call" | "InRoom" = "Call", title = "") {
  const nav = await screen.findByRole("navigation");
  await s.user.click(within(nav).getByRole("button", { name: "New meeting" }));
  if (mode === "InRoom") await s.user.click(screen.getByRole("radio", { name: "In the room: mic only" }));
  if (title !== "") await s.user.type(screen.getByRole("textbox", { name: /Title/ }), title);
  await s.user.click(screen.getByRole("button", { name: "Start" }));
  await act(async () => {
    s.timer.advance(0);
  });
  return screen.findByRole("region", { name: "Meeting" });
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("cancelling while a meeting finishes", () => {
  function countCancels(ipc: Ipc) {
    const pressed: string[] = [];
    const real = ipc.cancelFinishing.bind(ipc);
    ipc.cancelFinishing = async (meeting) => {
      pressed.push(meeting);
      await real(meeting);
    };
    return pressed;
  }

  async function finishing(s: Setup) {
    const view = await startMeeting(s, "Call", "Invented planning call");
    advance(s.timer, 10_000);
    await s.user.click(within(view).getByRole("button", { name: "Stop meeting" }));
    expect(await screen.findByRole("heading", { name: "Finishing the meeting" })).toBeInTheDocument();
  }

  it("says it is stopping, takes one press, and leaves the meeting not processed", async () => {
    const s = setup();
    const pressed = countCancels(s.ipc);
    await finishing(s);
    const cancel = screen.getByRole("button", { name: "Cancel" });
    expect(cancel).toBeEnabled();
    await s.user.click(cancel);
    expect(await screen.findByText(/^Stopping\./)).toBeInTheDocument();
    expect(cancel).toBeDisabled();
    await s.user.click(cancel);
    expect(pressed).toHaveLength(1);

    advance(s.timer, POST_STEP_MS);
    expect(await screen.findByRole("heading", { name: "Meeting saved" })).toBeInTheDocument();
    expect(screen.getByText("Transcript not finished")).toBeInTheDocument();
    expect(
      screen.getByText(/The recording and any draft transcript are saved\. Re-transcribe the meeting/),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
    const listed = await s.ipc.listMeetings();
    expect(listed[0].state).toBe("Unprocessed");
    const list = screen.getByRole("list", { name: "Meetings" });
    await waitFor(() => expect(within(list).getByText(/Draft transcript/)).toBeInTheDocument());

    // Nothing was left running: the steps that were due never come.
    advance(s.timer, (POST_STEPS.length + 2) * POST_STEP_MS);
    expect(screen.queryByText("Summary ready")).toBeNull();
    expect(screen.getByRole("button", { name: "Open" })).toBeInTheDocument();
  });

  it("past the final pass cancels only the summary", async () => {
    const s = setup();
    await finishing(s);
    advance(s.timer, POST_STEPS.length * POST_STEP_MS);
    expect(screen.getByText("Writing the summary")).toBeInTheDocument();
    await s.user.click(screen.getByRole("button", { name: "Cancel" }));
    advance(s.timer, POST_STEP_MS);
    expect(await screen.findByText("Summary cancelled")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Meeting finished" })).toBeInTheDocument();
    expect(screen.getByText(/The recording and transcript are saved\./)).toBeInTheDocument();
    expect((await s.ipc.listMeetings())[0].state).toBe("Ready");
  });

  it("gets the button back with the reason when the press does not reach the app", async () => {
    const s = setup();
    await finishing(s);
    s.ipc.cancelFinishing = async () => {
      throw new Error("Invented failure of the press.");
    };
    const cancel = screen.getByRole("button", { name: "Cancel" });
    await s.user.click(cancel);
    expect(await screen.findByText("Invented failure of the press.")).toBeInTheDocument();
    expect(cancel).toBeEnabled();
    expect(screen.queryByText(/^Stopping\./)).toBeNull();
  });

  it("is not offered while a newer meeting records, which keeps recording", async () => {
    const s = setup();
    await finishing(s);
    await startMeeting(s);
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
    expect(screen.getByRole("button", { name: "Stop meeting" })).toBeEnabled();
  }, 30_000);

  it("reads a cancel of an older meeting as nothing to do with this one", () => {
    const state = startedMeeting("this", "Call", ["You", "Them"], START);
    expect(
      reduce(state, { type: "Summary", meeting: "old", result: { kind: "PostCancelled" } }, START + 5),
    ).toEqual({ ...state });
  });

  it("the mock's press after the end does nothing", async () => {
    const timer = createManualTimer(START);
    const ipc = createMockIpc({ meetings: [], timer });
    const events: MeetingEvent[] = [];
    await ipc.onMeetingEvent((e) => events.push(e));
    const id = await ipc.startMeeting("Call", "Invented", false);
    await ipc.stopMeeting();
    timer.advance((POST_STEPS.length + 1) * POST_STEP_MS);
    await ipc.cancelFinishing(id);
    timer.advance(10 * POST_STEP_MS);
    expect(events.filter((e) => e.type === "Summary")).toHaveLength(1);
  });
});

describe("a meeting started while the previous one finishes", () => {
  const HELD = "Processing your previous meeting. Transcription will begin soon.";

  it("starts at once, then waits, catches up and follows", async () => {
    const s = setup();
    const first = await startMeeting(s);
    await s.user.click(within(first).getByRole("button", { name: "Stop meeting" }));
    expect(await screen.findByRole("heading", { name: "Finishing the meeting" })).toBeInTheDocument();
    // The older meeting is being processed: a new one starts anyway.
    await startMeeting(s);
    expect(await screen.findByText(HELD)).toBeInTheDocument();
    expect(screen.getByText("Audio is recording.")).toBeInTheDocument();
    expect(within(screen.getByRole("group", { name: "Running on" })).getByText("Waiting")).toBeInTheDocument();
    // Its steps and its summary do not reach this meeting's screen.
    advance(s.timer, POST_STEP_MS);
    expect(screen.queryByRole("heading", { name: "Finishing the meeting" })).toBeNull();
    expect(screen.getByRole("button", { name: "Stop meeting" })).toBeEnabled();
    advance(s.timer, POST_STEP_MS * POST_STEPS.length);
    await waitFor(() => expect(screen.getByText("Catching up on the transcript")).toBeInTheDocument());
    expect(screen.queryByText(HELD)).toBeNull();
    expect(screen.queryByRole("heading", { name: "Meeting finished" })).toBeNull();
    advance(s.timer, CATCH_UP_MS);
    await waitFor(() => expect(screen.queryByText("Catching up on the transcript")).toBeNull());
    expect(screen.queryByText("Audio is recording.")).toBeNull();
    expect(screen.getByRole("button", { name: "Stop meeting" })).toBeEnabled();
  }, 30_000);

  it("shows the older meeting as finishing in the list, and refuses a second recording", async () => {
    const s = setup();
    const first = await startMeeting(s);
    await s.user.click(within(first).getByRole("button", { name: "Stop meeting" }));
    await startMeeting(s);
    const listed = await s.ipc.listMeetings();
    expect(listed.map((m) => m.state).sort()).toEqual(["Processing", "Recording"]);
    await expect(s.ipc.startMeeting("Call", null, false)).rejects.toMatchObject({ error: { kind: "busy" } });
  }, 30_000);

  it("has a banner for each state of the transcript, and none while it keeps up", () => {
    const view = (state: "Live" | "WaitingForPreviousMeeting" | "WaitingForStart" | "CatchingUp" | "Stopped") => {
      const meeting = reduce(
        startedMeeting("m", "Call", ["You", "Them"], START),
        { type: "Transcript", state },
        START,
      );
      if (meeting === null) throw new Error("no meeting");
      return render(
        <EarshotThemeProvider>
          <RecordingView
            meeting={meeting}
            now={START}
            onStop={() => {}}
            onCompact={() => {}}
            onDismiss={() => {}}
            onKeepRecording={() => {}}
          />
        </EarshotThemeProvider>,
      );
    };
    const held = view("WaitingForPreviousMeeting");
    expect(within(held.container).getByText(HELD)).toBeInTheDocument();
    expect(within(held.container).getByText("Audio is recording.")).toBeInTheDocument();
    held.unmount();
    const catching = view("CatchingUp");
    expect(within(catching.container).getByText("Catching up on the transcript")).toBeInTheDocument();
    catching.unmount();
    const stopped = view("Stopped");
    expect(within(stopped.container).getByText("Audio is recording.")).toBeInTheDocument();
    expect(
      within(stopped.container).getByText(
        "The live transcript has stopped for this meeting. The full transcript will be ready after the meeting ends.",
      ),
    ).toBeInTheDocument();
    stopped.unmount();
    const starting = view("WaitingForStart");
    expect(within(starting.container).getByText("Transcription will begin soon.")).toBeInTheDocument();
    expect(within(starting.container).queryByText(HELD)).toBeNull();
    starting.unmount();
    const following = view("Live");
    expect(within(following.container).queryByText("Audio is recording.")).toBeNull();
    expect(within(following.container).queryByText(/Catching up/)).toBeNull();
    following.unmount();
  });

  it("lets an older meeting's end pass without touching this meeting", () => {
    const state = startedMeeting("new", "Call", ["You"], START);
    expect(reduce(state, { type: "Ended", meeting: "old", reason: "Stopped" }, START + 5)).toBe(state);
    expect(
      reduce(
        state,
        { type: "PostProgress", meeting: "old", step: { kind: "Loading" }, done: 0, total: null },
        START + 5,
      ),
    ).toBe(state);
    expect(
      reduce(state, { type: "Summary", meeting: "old", result: { kind: "Cancelled" } }, START + 5),
    ).toBe(state);
    expect(reduce(state, { type: "Ended", meeting: "new", reason: "Stopped" }, START + 5)?.phase).toBe(
      "processing",
    );
  });
});

describe("provisional words", () => {
  const meetingWith = (...events: MeetingEvent[]) =>
    events.reduce<LiveMeeting>(
      (state, event) => reduce(state, event, START) as LiveMeeting,
      startedMeeting("m", "Call", ["You", "Them"], START),
    );
  const show = (meeting: LiveMeeting) => {
    render(
      <EarshotThemeProvider>
        <RecordingView
          meeting={meeting}
          now={START}
          onStop={() => {}}
          onKeepRecording={() => {}}
          onCompact={() => {}}
          onDismiss={() => {}}
        />
      </EarshotThemeProvider>,
    );
  };
  const provisional =(...words: [number, string][]): MeetingEvent => ({
    type: "Provisional",
    track: "Them",
    words: words.map(([start_ms, text]) => ({ start_ms, text })),
  });
  const landed = (start_ms: number, end_ms: number, text: string): MeetingEvent => ({
    type: "Landed",
    track: "Them",
    start_ms,
    end_ms,
    text,
  });

  it("appear after the settled lines, marked as provisional", () => {
    show(meetingWith(landed(0, 900, "Settled words."), provisional([1_000, "the"], [1_200, "pump"])));
    const grid = screen.getByRole("table", { name: "Transcript as it lands" });
    const rows = within(grid).getAllByRole("row");
    expect(rows).toHaveLength(2);
    expect(rows[1]).toHaveAttribute("aria-label", "Them, provisional");
    expect(within(rows[1]).getByText("the pump")).toBeInTheDocument();
    expect(within(grid).queryByText("Next stretch transcribing")).toBeNull();
  });

  it("are replaced by the settled text that covers them, and the rest stay", () => {
    const meeting = meetingWith(
      provisional([1_000, "the"], [1_200, "pump"], [5_000, "order"]),
      landed(1_000, 1_500, "The pump."),
    );
    expect(meeting.provisional.Them.map((w) => w.text)).toEqual(["order"]);
    expect(meeting.lines.map((l) => l.text)).toEqual(["The pump."]);
    show(meeting);
    expect(screen.getByRole("row", { name: "Them, provisional" })).toHaveTextContent("order");
  });

  it("are removed when they held words the settled text lacks", () => {
    const meeting = meetingWith(
      provisional([1_000, "the"], [1_200, "pomp"], [1_400, "extra"]),
      landed(1_000, 1_800, "The pump."),
    );
    expect(meeting.provisional.Them).toEqual([]);
    show(meeting);
    expect(screen.queryByRole("row", { name: "Them, provisional" })).toBeNull();
    expect(screen.queryByText(/extra/)).toBeNull();
    expect(screen.getByText("Next stretch transcribing")).toBeInTheDocument();
  });

  it("are removed when the runtime says none is left, and when the recording ends", () => {
    const shown = provisional([1_000, "the"]);
    expect(meetingWith(shown, provisional()).provisional.Them).toEqual([]);
    const ended = meetingWith(shown, { type: "Ended", meeting: "m", reason: "Stopped" });
    expect(ended.provisional.Them).toEqual([]);
  });

  it("do not touch another track's words or count as words", () => {
    const meeting = meetingWith(provisional([1_000, "the"]), {
      type: "Landed",
      track: "You",
      start_ms: 0,
      end_ms: 5_000,
      text: "Mine.",
    });
    expect(meeting.provisional.Them.map((w) => w.text)).toEqual(["the"]);
    expect(meeting.words).toBe(0);
  });
});

describe("meeting screen", () => {
  it("validates the start view: call by default, title length checked", async () => {
    const s = setup();
    const main = await screen.findByRole("main");
    await s.user.click(within(main).getByRole("button", { name: "New meeting" }));
    expect(screen.getByRole("heading", { name: "New meeting" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "Call: mic + computer audio" })).toBeChecked();
    expect(screen.getByRole("radio", { name: "In the room: mic only" })).not.toBeChecked();
    const title = screen.getByRole("textbox", { name: /Title/ });
    const start = screen.getByRole("button", { name: "Start" });
    expect(start).toBeEnabled();
    await s.user.click(title);
    await s.user.paste("x".repeat(MAX_TITLE + 1));
    expect(screen.getByText(`Keep the title to ${MAX_TITLE} characters or fewer.`)).toBeInTheDocument();
    expect(start).toBeDisabled();
    await s.user.type(title, "{Backspace}");
    expect(start).toBeEnabled();
    expect(screen.queryByText(/Keep the title/)).toBeNull();
    // Cancel goes back without starting anything.
    await s.user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await s.ipc.listMeetings()).toEqual([]);
  });

  it("shows why a meeting could not start", async () => {
    const s = setup({
      startError: {
        kind: "model",
        message: "Earshot needs a transcription model that is not on this computer yet. Open Settings, Transcription to download one.",
        retryable: false,
      },
    });
    const main = await screen.findByRole("main");
    await s.user.click(within(main).getByRole("button", { name: "New meeting" }));
    await s.user.click(screen.getByRole("button", { name: "Start" }));
    expect(await screen.findByText("The meeting could not start")).toBeInTheDocument();
    expect(screen.getByText(/Open Settings, Transcription/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Start" })).toBeEnabled();
  });

  it("renders the status, meters, tiles and the transcript as it lands", async () => {
    const s = setup();
    const view = await startMeeting(s, "Call", "Pump order");
    advance(s.timer, 4_000);
    const levels = within(view).getByRole("group", { name: "Levels" });
    // Them talks first in the script; You is quiet.
    expect(within(levels).getByRole("img", { name: "Them speaking" })).toBeInTheDocument();
    expect(within(levels).getByRole("img", { name: "You quiet" })).toBeInTheDocument();
    advance(s.timer, 1_000);
    expect(within(view).getByText("Listening")).toBeInTheDocument();
    expect(within(view).getByText("Call: mic + computer audio")).toBeInTheDocument();
    expect(within(view).getByLabelText("Elapsed").textContent).toBe("0:05");
    expect(within(levels).getAllByRole("progressbar").map((p) => p.getAttribute("aria-label"))).toEqual([
      "You level",
      "Them level",
    ]);
    const words = SCRIPT_LINES.filter((l) => l.at <= 5_000)
      .map((l) => l.text.split(/\s+/).length)
      .reduce((a, b) => a + b, 0);
    expect(within(within(view).getByRole("group", { name: "Words so far" })).getByText(String(words))).toBeInTheDocument();
    expect(within(view).getByRole("group", { name: "Running on" }).textContent).toContain("GPU (CUDA)");
    expect(within(view).getByRole("group", { name: "Transcript behind" }).textContent).toMatch(/\d+ s/);
    // The transcript is open by default and collapsible.
    const header = within(view).getByRole("button", { name: "Transcript as it lands" });
    expect(header).toHaveAttribute("aria-expanded", "true");
    const grid = within(view).getByRole("table", { name: "Transcript as it lands" });
    const rows = within(grid).getAllByRole("row");
    const landed = SCRIPT_LINES.filter((l) => l.at <= 5_000);
    expect(rows).toHaveLength(landed.length + 1);
    landed.forEach((line, i) => {
      const cells = within(rows[i]).getAllByRole("cell");
      // Time, speaker and text are separate cells: the text never wraps under the label.
      expect(cells).toHaveLength(3);
      expect(cells[1].textContent).toBe(line.track);
      expect(cells[2].textContent).toBe(line.text);
    });
    expect(within(rows[rows.length - 1]).getByText("Next stretch transcribing")).toBeInTheDocument();
    await s.user.click(header);
    expect(header).toHaveAttribute("aria-expanded", "false");
    expect(within(view).getByRole("button", { name: "Stop meeting" })).toBeEnabled();
    expect(within(view).getByRole("button", { name: "Compact" })).toBeEnabled();
    expect(document.body.textContent ?? "").not.toMatch(/—|please|!/i);
  });

  it("keeps Stop in the controls above everything else", async () => {
    const s = setup();
    const view = await startMeeting(s);
    advance(s.timer, 5_000);
    const controls = within(view).getByRole("group", { name: "Meeting controls" });
    const stop = within(controls).getByRole("button", { name: "Stop meeting" });
    expect(within(controls).getByRole("button", { name: "Compact" })).toBeInTheDocument();
    expect(within(controls).getByLabelText("Elapsed")).toBeInTheDocument();
    // Before the transcript in the page, however long the transcript gets.
    const transcript = within(view).getByRole("table", { name: "Transcript as it lands" });
    expect(stop.compareDocumentPosition(transcript) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(within(view).getAllByRole("button", { name: "Stop meeting" })).toHaveLength(1);
  });

  it("follows the newest line until the user scrolls up, then offers to jump back", async () => {
    const s = setup();
    const view = await startMeeting(s);
    const area = within(view).getByRole("group", { name: "Transcript scroll area" });
    // jsdom does no layout: a viewport of 200 over content that grows with each line.
    const table = within(view).getByRole("table", { name: "Transcript as it lands" });
    const height = () => within(table).getAllByRole("row").length * 100;
    Object.defineProperty(area, "clientHeight", { configurable: true, value: 200 });
    Object.defineProperty(area, "scrollHeight", { configurable: true, get: height });
    const jump = () => within(view).queryByRole("button", { name: "Jump to latest" });

    // Following: each new line brings the view to the end.
    advance(s.timer, SCRIPT_LINES[1].at + 1);
    const first = height();
    expect(first).toBeGreaterThan(200);
    expect(area.scrollTop).toBe(first);
    expect(jump()).toBeNull();
    advance(s.timer, SCRIPT_LINES[2].at - SCRIPT_LINES[1].at);
    expect(height()).toBeGreaterThan(first);
    expect(area.scrollTop).toBe(height());

    // Scrolled up to read: new lines leave the view alone, and the way back appears.
    area.scrollTop = 10;
    fireEvent.scroll(area);
    expect(jump()).not.toBeNull();
    const before = height();
    advance(s.timer, SCRIPT_LINES[3].at - SCRIPT_LINES[2].at);
    expect(height()).toBeGreaterThan(before);
    expect(area.scrollTop).toBe(10);

    // Jump to latest: back at the end and following again.
    await s.user.click(jump()!);
    expect(area.scrollTop).toBe(height());
    expect(jump()).toBeNull();
    const after = height();
    advance(s.timer, SCRIPT_LINES[4].at - SCRIPT_LINES[3].at);
    expect(height()).toBeGreaterThan(after);
    expect(area.scrollTop).toBe(height());

    // Scrolling back down by hand also resumes following.
    area.scrollTop = 10;
    fireEvent.scroll(area);
    expect(jump()).not.toBeNull();
    area.scrollTop = height() - 200;
    fireEvent.scroll(area);
    expect(jump()).toBeNull();
  });

  it("lists the recording meeting at the top while it records", async () => {
    const s = setup({
      meetings: [
        {
          id: "old",
          title: "Earlier meeting",
          started_at_ms: START - 3_600_000,
          duration_ms: 1_800_000,
          state: "Ready",
          folder_id: null,
        },
      ],
    });
    await startMeeting(s, "Call", "Pump order");
    const list = await screen.findByRole("list", { name: "Meetings" });
    await waitFor(() => expect(within(list).getAllByRole("button")[0].textContent).toContain("Pump order"));
    expect(within(list).getAllByRole("button")[0].textContent).toContain("Recording...");
  });

  it("shows a warning banner that can be dismissed", async () => {
    const s = setup();
    const view = await startMeeting(s);
    expect(within(view).queryByText(/hands-free mode/)).toBeNull();
    advance(s.timer, SCRIPT_WARNING_AT);
    expect(within(view).getByText(/hands-free mode/)).toBeInTheDocument();
    await s.user.click(within(view).getByRole("button", { name: "Dismiss" }));
    expect(within(view).queryByText(/hands-free mode/)).toBeNull();
  });

  it("shows one meter in room mode", async () => {
    const s = setup();
    const view = await startMeeting(s, "InRoom");
    advance(s.timer, 1_000);
    expect(within(view).getByText("In the room: mic only")).toBeInTheDocument();
    const levels = within(view).getByRole("group", { name: "Levels" });
    expect(within(levels).getAllByRole("progressbar")).toHaveLength(1);
    expect(within(levels).getByRole("progressbar", { name: "You level" })).toBeInTheDocument();
  });

  it("switches to the compact card and back through setCompact", async () => {
    const s = setup();
    const view = await startMeeting(s);
    advance(s.timer, 4_000);
    await s.user.click(within(view).getByRole("button", { name: "Compact" }));
    expect(s.compactCalls).toEqual([true]);
    const card = await screen.findByRole("region", { name: "Meeting" });
    expect(within(card).getByText("Listening")).toBeInTheDocument();
    expect(within(card).getAllByRole("progressbar")).toHaveLength(2);
    expect(within(card).getByText(/words so far/)).toBeInTheDocument();
    expect(screen.queryByRole("navigation")).toBeNull();
    await s.user.click(within(card).getByRole("button", { name: "Expand" }));
    expect(s.compactCalls).toEqual([true, false]);
    expect(await screen.findByRole("button", { name: "Stop meeting" })).toBeInTheDocument();
  });

  it("expands the compact card when the meeting is stopped from it", async () => {
    const s = setup();
    const view = await startMeeting(s);
    await s.user.click(within(view).getByRole("button", { name: "Compact" }));
    await s.user.click(screen.getByRole("button", { name: "Stop" }));
    await waitFor(() => expect(s.compactCalls).toEqual([true, false]));
    expect(await screen.findByRole("heading", { name: "Finishing the meeting" })).toBeInTheDocument();
  });

  it("after stop shows the post-meeting progress, then the summary ready notice", async () => {
    const s = setup();
    const view = await startMeeting(s, "Call", "Pump order");
    advance(s.timer, 10_000);
    await s.user.click(within(view).getByRole("button", { name: "Stop meeting" }));
    expect(await screen.findByRole("heading", { name: "Finishing the meeting" })).toBeInTheDocument();
    const list = screen.getByRole("list", { name: "Meetings" });
    await waitFor(() => expect(within(list).getByLabelText("Processing")).toBeInTheDocument());
    advance(s.timer, 3 * POST_STEP_MS);
    expect(screen.getByText("Finished the transcript for you")).toBeInTheDocument();
    expect(screen.getByText("1 of 6 steps")).toBeInTheDocument();
    advance(s.timer, (POST_STEPS.length - 3) * POST_STEP_MS);
    expect(screen.getByText("Writing the summary")).toBeInTheDocument();
    advance(s.timer, POST_STEP_MS);
    expect(await screen.findByText("Summary ready")).toBeInTheDocument();
    expect(screen.getByText(/Vendor review: pumps · 4 actions, 2 decisions · 2 voices to name/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Name voices" })).toBeInTheDocument();
    await waitFor(() => expect(within(list).queryByLabelText("Processing")).toBeNull());
    await s.user.click(screen.getByRole("button", { name: "Open" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Pump order" })).toBeInTheDocument();
  });

  it("opens the meeting detail at the Speakers panel from Name voices", async () => {
    const s = setup();
    const view = await startMeeting(s, "Call", "Pump order");
    advance(s.timer, 10_000);
    await s.user.click(within(view).getByRole("button", { name: "Stop meeting" }));
    advance(s.timer, (POST_STEPS.length + 1) * POST_STEP_MS);
    await s.user.click(await screen.findByRole("button", { name: "Name voices" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Pump order" })).toBeInTheDocument();
    const panel = await screen.findByRole("complementary", { name: "Speakers" });
    await waitFor(() =>
      expect(within(panel).getByRole("textbox", { name: "Name of Speaker 2" })).toHaveFocus(),
    );
  });

  it("shows a skipped summary with its reason", async () => {
    const s = setup({ summary: { kind: "Skipped", reason: "Summaries are turned off." } });
    const view = await startMeeting(s);
    await s.user.click(within(view).getByRole("button", { name: "Stop meeting" }));
    advance(s.timer, (POST_STEPS.length + 1) * POST_STEP_MS);
    expect(await screen.findByText("No summary")).toBeInTheDocument();
    expect(screen.getByText(/Summaries are turned off. The transcript is saved./)).toBeInTheDocument();
  });

  it("shows a failed summary with a retry hint", async () => {
    const s = setup({
      summary: { kind: "Failed", message: "The summary failed: endpoint unreachable.", retryable: true },
    });
    const view = await startMeeting(s);
    await s.user.click(within(view).getByRole("button", { name: "Stop meeting" }));
    advance(s.timer, (POST_STEPS.length + 1) * POST_STEP_MS);
    expect(await screen.findByText("Summary failed")).toBeInTheDocument();
    expect(screen.getByText(/Trying again may work/)).toBeInTheDocument();
  });
});
