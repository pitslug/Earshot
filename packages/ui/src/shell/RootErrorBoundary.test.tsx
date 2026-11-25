import { Button, Dialog, DialogBody, DialogSurface, DialogTitle } from "@fluentui/react-components";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  type FollowedSnapshot,
  type Ipc,
  IpcFailure,
  type MeetingEvent,
  type MeetingSnapshot,
  createMockIpc,
} from "../ipc";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { createManualTimer } from "../time";
import { CRASH_COPY, RootErrorBoundary, STOP_ENDED_WAIT_MS } from "./RootErrorBoundary";

/** Every shipped source of the UI and of the desktop page. */
const SOURCES = import.meta.glob(
  ["../**/*.{ts,tsx}", "!../**/*.test.{ts,tsx}", "!../test/**", "../../../../apps/desktop/src/**/*.{ts,tsx}"],
  { query: "?raw", import: "default", eager: true },
) as Record<string, string>;

/** A listener for promises nobody caught, however it is attached. */
const REJECTION_LISTENER = /unhandledrejection/i;
/** Reading what an error says, or writing to the console. */
const READS_THE_ERROR = /\.message\b|\.stack\b|\bconsole\s*\.|componentStack|errorText|String\(\s*e(rr(or)?)?\s*\)/;

/** A source without its comments (line and block), so the words of a comment do not count. */
const code = (source: string) => source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");

/** Words only the thrown error holds: no screen may show them. */
const ERROR_WORDS = "zebra-quartz at InventedComponent.tsx:41";

/** A screen that throws while rendering once "Break" is pressed. */
function Screen({ renders }: { renders?: { n: number } }) {
  const [broken, setBroken] = useState(false);
  if (renders) renders.n += 1;
  if (broken) throw new Error(ERROR_WORDS);
  return (
    <button type="button" onClick={() => setBroken(true)}>
      Break
    </button>
  );
}

/** A screen that throws from inside its own open dialog. */
function DialogScreen() {
  const [open, setOpen] = useState(false);
  const [broken, setBroken] = useState(false);
  if (broken) throw new Error(ERROR_WORDS);
  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>
        Ask
      </button>
      <Dialog open={open} modalType="alert">
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Invented question</DialogTitle>
            <Button onClick={() => setBroken(true)}>Break</Button>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </>
  );
}

/** The meeting the screen follows, as the app's snapshot gives it (T14.51). */
function followed(meeting: string, ended: FollowedSnapshot["ended"] = null): FollowedSnapshot {
  return {
    meeting,
    mode: "Call",
    tracks: ["You", "Them"],
    elapsed_ms: 60_000,
    ended,
    banners: [],
    backends: [],
    words: 0,
    lag_seconds: null,
    transcript: "Live",
    call_end_offer: false,
    call_end_stops: false,
    progress: null,
  };
}

/** A recording this window never heard start (the window loaded during it). */
const RECORDING: MeetingSnapshot = { followed: followed("m-rec"), behind: [] };
const NOTHING: MeetingSnapshot = { followed: null, behind: [] };

function setup(over: Partial<Ipc> = {}, snapshot: MeetingSnapshot = NOTHING) {
  const listeners = new Set<(e: MeetingEvent) => void>();
  const stopMeeting = vi.fn(async () => {});
  const setCompact = vi.fn(async () => {});
  const meetingSnapshot = vi.fn(async () => snapshot);
  const ipc: Ipc = {
    ...createMockIpc({ meetings: [] }),
    onMeetingEvent: async (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    stopMeeting,
    setCompact,
    meetingSnapshot,
    ...over,
  };
  const emit = (event: MeetingEvent) =>
    act(() => {
      for (const l of [...listeners]) l(event);
    });
  const onReload = vi.fn();
  const renders = { n: 0 };
  const timer = createManualTimer(1_000);
  render(
    <EarshotThemeProvider>
      <RootErrorBoundary ipc={ipc} onReload={onReload} timer={timer}>
        <Screen renders={renders} />
      </RootErrorBoundary>
    </EarshotThemeProvider>,
  );
  const user = userEvent.setup();
  const crash = () => user.click(screen.getByRole("button", { name: "Break" }));
  /** Moves the injected clock on, as time passing does. */
  const advance = (ms: number) => act(() => timer.advance(ms));
  return { ipc, emit, onReload, renders, user, crash, advance, stopMeeting, setCompact, meetingSnapshot, listeners };
}

const started = (meeting: string): MeetingEvent => ({ type: "Started", meeting, mode: "Call", tracks: ["You", "Them"] });
const ended = (meeting: string): MeetingEvent => ({ type: "Ended", meeting, reason: "Stopped" });

describe("root error boundary", () => {
  // React reports a render error on the console; the tests expect the errors they throw.
  beforeEach(() => {
    vi.spyOn(console, "error").mockImplementation(() => {});
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("error boundary: a screen that throws leaves fixed words and Reload, never the error's text", async () => {
    const s = setup();
    // Let the subscription settle, as it has in a window that has been open a moment.
    await act(async () => {});
    await s.crash();
    const heading = await screen.findByRole("heading", { level: 1, name: CRASH_COPY.title });
    expect(heading).toHaveFocus();
    expect(screen.getByText(CRASH_COPY.body)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Break" })).toBeNull();
    expect(document.body.textContent).not.toContain("zebra");
    expect(document.body.textContent).not.toContain("InventedComponent");
    expect(document.body.innerHTML).not.toContain("zebra");
    // Nothing records, so there is nothing to stop.
    await act(async () => {});
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    expect(screen.queryByText(CRASH_COPY.recording)).toBeNull();
    await s.user.click(screen.getByRole("button", { name: CRASH_COPY.reload }));
    expect(s.onReload).toHaveBeenCalledTimes(1);
  });

  it("error boundary: while a meeting records it stops it straight through the app", async () => {
    const s = setup();
    await act(async () => {});
    s.emit(started("m-live"));
    await s.crash();
    await screen.findByRole("heading", { level: 1, name: CRASH_COPY.title });
    expect(screen.getByText(CRASH_COPY.recording)).toBeInTheDocument();
    const stop = screen.getByRole("button", { name: CRASH_COPY.stop });
    // Focus is on the heading, never on Stop: a key meant for the lost screen stops nothing.
    expect(stop).not.toHaveFocus();
    expect(s.stopMeeting).not.toHaveBeenCalled();
    await s.user.click(stop);
    expect(s.stopMeeting).toHaveBeenCalledTimes(1);
    expect(screen.getByText(CRASH_COPY.stopping)).toBeInTheDocument();
    // A second press while it stops asks nothing more.
    await s.user.click(screen.getByRole("button", { name: CRASH_COPY.stop }));
    expect(s.stopMeeting).toHaveBeenCalledTimes(1);
    s.emit(ended("m-live"));
    expect(screen.getByText(CRASH_COPY.stopped)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    expect(screen.getByRole("button", { name: CRASH_COPY.reload })).toBeInTheDocument();
  });

  it("error boundary: a stop the app took but whose end never arrives does not say Stopping for good", async () => {
    const s = setup();
    await act(async () => {});
    s.emit(started("m-live"));
    await s.crash();
    await s.user.click(await screen.findByRole("button", { name: CRASH_COPY.stop }));
    await act(async () => {});
    expect(screen.getByText(CRASH_COPY.stopping)).toBeInTheDocument();
    // Just short of the wait it still says it is stopping, and takes no second press.
    s.advance(STOP_ENDED_WAIT_MS - 1);
    expect(screen.getByText(CRASH_COPY.stopping)).toBeInTheDocument();
    expect(screen.queryByText(CRASH_COPY.saving)).toBeNull();
    s.advance(1);
    expect(screen.getByText(CRASH_COPY.saving)).toBeInTheDocument();
    expect(screen.queryByText(CRASH_COPY.stopping)).toBeNull();
    expect(screen.getByRole("button", { name: CRASH_COPY.reload })).toBeEnabled();
    // Stop can be pressed again, and the wait starts over.
    await s.user.click(screen.getByRole("button", { name: CRASH_COPY.stop }));
    expect(s.stopMeeting).toHaveBeenCalledTimes(2);
    await act(async () => {});
    expect(screen.getByText(CRASH_COPY.stopping)).toBeInTheDocument();
    s.advance(STOP_ENDED_WAIT_MS);
    expect(screen.getByText(CRASH_COPY.saving)).toBeInTheDocument();
    // The end, when it does arrive, says so.
    s.emit(ended("m-live"));
    expect(screen.getByText(CRASH_COPY.stopped)).toBeInTheDocument();
    expect(screen.queryByText(CRASH_COPY.saving)).toBeNull();
  });

  it("error boundary: an end that arrives in time is never followed by the saving words", async () => {
    const s = setup();
    await act(async () => {});
    s.emit(started("m-live"));
    await s.crash();
    await s.user.click(await screen.findByRole("button", { name: CRASH_COPY.stop }));
    await act(async () => {});
    s.advance(2_000);
    s.emit(ended("m-live"));
    expect(screen.getByText(CRASH_COPY.stopped)).toBeInTheDocument();
    s.advance(STOP_ENDED_WAIT_MS * 2);
    expect(screen.getByText(CRASH_COPY.stopped)).toBeInTheDocument();
    expect(screen.queryByText(CRASH_COPY.saving)).toBeNull();
    // Nor does the wait of the meeting before reach a meeting that starts later.
    s.emit(started("m-next"));
    s.advance(STOP_ENDED_WAIT_MS * 2);
    expect(screen.queryByText(CRASH_COPY.saving)).toBeNull();
    expect(screen.getByRole("button", { name: CRASH_COPY.stop })).toBeEnabled();
  });

  it("error boundary: a stop the app refuses says where else to stop, in fixed words", async () => {
    const refusal = "internal words of the refusal 0x80070005";
    const stopMeeting = vi.fn(async () => {
      throw new IpcFailure({ kind: "window", message: refusal, retryable: false });
    });
    const s = setup({ stopMeeting });
    await act(async () => {});
    s.emit(started("m-live"));
    await s.crash();
    await s.user.click(await screen.findByRole("button", { name: CRASH_COPY.stop }));
    expect(await screen.findByText(CRASH_COPY.stopFailed)).toBeInTheDocument();
    expect(document.body.textContent).not.toContain("0x80070005");
    // A refusal starts no wait: the words stay what they are.
    s.advance(STOP_ENDED_WAIT_MS * 2);
    expect(screen.getByText(CRASH_COPY.stopFailed)).toBeInTheDocument();
    expect(screen.queryByText(CRASH_COPY.saving)).toBeNull();
    // It can be tried again.
    await s.user.click(screen.getByRole("button", { name: CRASH_COPY.stop }));
    expect(stopMeeting).toHaveBeenCalledTimes(2);
  });

  it("error boundary: a recording that ended before the screen broke offers no Stop", async () => {
    const s = setup();
    await act(async () => {});
    s.emit(started("m-a"));
    s.emit(ended("m-a"));
    await s.crash();
    await screen.findByRole("heading", { level: 1, name: CRASH_COPY.title });
    await act(async () => {});
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    expect(screen.queryByText(CRASH_COPY.stopped)).toBeNull();
  });

  it("error boundary: an older meeting's end after a newer start leaves the newer one's Stop", async () => {
    const s = setup();
    await act(async () => {});
    s.emit(started("m-a"));
    s.emit(started("m-b"));
    s.emit(ended("m-a"));
    await s.crash();
    expect(await screen.findByRole("button", { name: CRASH_COPY.stop })).toBeInTheDocument();
    // A meeting that starts after the screen broke (the tray, the shortcut) is offered too.
    s.emit(ended("m-b"));
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    s.emit(started("m-c"));
    expect(screen.getByRole("button", { name: CRASH_COPY.stop })).toBeInTheDocument();
    expect(screen.queryByText(CRASH_COPY.stopped)).toBeNull();
  });

  it("error boundary: finds a recording this window never heard start, and only once it broke", async () => {
    const s = setup({}, RECORDING);
    await act(async () => {});
    expect(s.meetingSnapshot).not.toHaveBeenCalled();
    await s.crash();
    expect(await screen.findByRole("button", { name: CRASH_COPY.stop })).toBeInTheDocument();
    expect(s.meetingSnapshot).toHaveBeenCalledTimes(1);
    expect(screen.getByText(CRASH_COPY.recording)).toBeInTheDocument();
  });

  it("error boundary: a meeting that is only finishing offers no Stop", async () => {
    const finishing: MeetingSnapshot = {
      followed: followed("m-done", "Stopped"),
      behind: [{ meeting: "m-older", progress: null }],
    };
    const s = setup({}, finishing);
    await act(async () => {});
    await s.crash();
    await waitFor(() => expect(s.meetingSnapshot).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(screen.getByRole("button", { name: CRASH_COPY.reload })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    expect(screen.queryByText(CRASH_COPY.recording)).toBeNull();
  });

  it("error boundary: a recording that fails before it is saved leaves nothing to stop", async () => {
    const s = setup();
    await act(async () => {});
    s.emit(started("m-live"));
    await s.crash();
    expect(await screen.findByRole("button", { name: CRASH_COPY.stop })).toBeInTheDocument();
    // Its result with no `Ended` (T9.23: the recording failed before it was saved).
    s.emit({ type: "Summary", meeting: "m-live", result: { kind: "RecordingFailed", message: "Invented." } });
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    expect(screen.getByRole("button", { name: CRASH_COPY.reload })).toBeInTheDocument();
  });

  it("error boundary: an end heard while the snapshot is being read is believed over the snapshot", async () => {
    let answer: (snapshot: MeetingSnapshot) => void = () => {};
    const meetingSnapshot = vi.fn(() => new Promise<MeetingSnapshot>((resolve) => (answer = resolve)));
    const s = setup({ meetingSnapshot });
    await act(async () => {});
    await s.crash();
    await waitFor(() => expect(meetingSnapshot).toHaveBeenCalledTimes(1));
    // The recording this window never heard start ends (the tray stopped it) before the
    // snapshot, read while it still recorded, arrives.
    s.emit(ended("m-rec"));
    await act(async () => answer(RECORDING));
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    expect(screen.getByRole("button", { name: CRASH_COPY.reload })).toBeInTheDocument();
  });

  it("error boundary: a snapshot that cannot be read still leaves Reload", async () => {
    const meetingSnapshot = vi.fn(async (): Promise<MeetingSnapshot> => {
      throw new IpcFailure({ kind: "internal", message: "internal driver words", retryable: false });
    });
    const s = setup({ meetingSnapshot });
    await act(async () => {});
    await s.crash();
    await waitFor(() => expect(meetingSnapshot).toHaveBeenCalledTimes(1));
    await act(async () => {});
    expect(screen.getByRole("button", { name: CRASH_COPY.reload })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: CRASH_COPY.stop })).toBeNull();
    expect(document.body.textContent).not.toContain("internal driver words");
  });
  it("error boundary: leaves the compact card, so the words fit the window", async () => {
    const s = setup();
    await act(async () => {});
    expect(s.setCompact).not.toHaveBeenCalled();
    await s.crash();
    await screen.findByRole("heading", { level: 1, name: CRASH_COPY.title });
    expect(s.setCompact).toHaveBeenCalledTimes(1);
    expect(s.setCompact).toHaveBeenCalledWith(false);
  });

  it("error boundary: a meeting's events render nothing of a working app", async () => {
    const s = setup();
    await act(async () => {});
    const before = s.renders.n;
    s.emit(started("m-live"));
    s.emit({ type: "Levels", you_dbfs: -20, them_dbfs: -30 });
    s.emit(ended("m-live"));
    expect(s.renders.n).toBe(before);
    expect(screen.getByRole("button", { name: "Break" })).toBeInTheDocument();
  });

  it("error boundary: a dialog open when the screen broke does not leave the words hidden from screen readers", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <EarshotThemeProvider>
        <RootErrorBoundary ipc={createMockIpc({ meetings: [] })} onReload={() => {}}>
          <DialogScreen />
        </RootErrorBoundary>
      </EarshotThemeProvider>,
    );
    await user.click(screen.getByRole("button", { name: "Ask" }));
    const dialog = await screen.findByRole("alertdialog");
    // The open dialog hides the rest of the page (tabster's deferred pass), as in the app.
    await waitFor(() => expect(container).toHaveAttribute("aria-hidden", "true"));
    await user.click(within(dialog).getByRole("button", { name: "Break" }));
    // By its role: found only once nothing hides it.
    expect(await screen.findByRole("heading", { level: 1, name: CRASH_COPY.title })).toBeInTheDocument();
    expect(container).not.toHaveAttribute("aria-hidden", "true");
    expect(screen.getByRole("button", { name: CRASH_COPY.reload })).toBeInTheDocument();
  });

  it("error boundary: no shipped file reports a failed promise, and the boundary never reads an error", () => {
    // Controls: the patterns find what they are for.
    expect(REJECTION_LISTENER.test('window.addEventListener("unhandledrejection", report)')).toBe(true);
    expect(REJECTION_LISTENER.test("window.onunhandledrejection = report")).toBe(true);
    for (const reading of ["show(error.message)", "log(e.stack)", "console.error(e)", "info.componentStack", "errorText(e)", "String(error)"]) {
      expect(READS_THE_ERROR.test(reading)).toBe(true);
    }
    expect(Object.keys(SOURCES).some((f) => f.endsWith("/apps/desktop/src/main.tsx"))).toBe(true);
    expect(Object.keys(SOURCES).length).toBeGreaterThan(50);
    // A failed promise is shown only by the error bar of what was asked for (T14.14).
    const listening = Object.entries(SOURCES)
      .filter(([, source]) => REJECTION_LISTENER.test(code(source)))
      .map(([file]) => file);
    expect(listening).toEqual([]);
    const boundary = SOURCES["./RootErrorBoundary.tsx"];
    expect(boundary).toBeDefined();
    expect(READS_THE_ERROR.test(code(boundary))).toBe(false);
    // Neither hook takes the error it is handed.
    expect(code(boundary)).toMatch(/static getDerivedStateFromError\(\)/);
    expect(code(boundary)).toMatch(/componentDidCatch\(\)/);
  });

  it("error boundary: stops listening when the window's tree goes", async () => {
    const s = setup();
    await act(async () => {});
    expect(s.listeners.size).toBe(1);
    // `cleanup` unmounts; here by hand, to look at the listener.
    const { unmount } = render(
      <EarshotThemeProvider>
        <RootErrorBoundary ipc={s.ipc} onReload={s.onReload}>
          <span>second</span>
        </RootErrorBoundary>
      </EarshotThemeProvider>,
    );
    await act(async () => {});
    expect(s.listeners.size).toBe(2);
    unmount();
    expect(s.listeners.size).toBe(1);
  });
});
