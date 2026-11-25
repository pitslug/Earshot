// What the window shows when a screen throws while rendering (T14.14). Without it React takes
// the whole tree down and the window goes blank, Stop included, while the app keeps recording.
//
// The words are fixed. The boundary itself never reads the error it is handed: it shows none of
// its text or stack and keeps and sends nothing of it (it can hold meeting content). React
// writes a caught error to the webview's console by itself; that console is not kept anywhere and
// the app has no way to read it. The boundary sits outside the app's tree and reaches the app
// itself, never through that tree: it hears the meeting events from the moment the window opens,
// so it knows whether a meeting records without asking the screen that broke, and its Stop calls
// the app directly.

import { Body1, Button, Title2, makeStyles, tokens, useFocusFinders } from "@fluentui/react-components";
import { Component, type ReactNode, useEffect, useRef } from "react";

import type { Ipc, MeetingEvent } from "../ipc";
import { type Timer, realTimer } from "../time";

/** Every word the screen can show. */
export const CRASH_COPY = {
  title: "Something went wrong",
  body: "Earshot could not show this screen. Your saved meetings are not affected. Reload to carry on.",
  recording: "A meeting is still recording. It keeps recording until you stop it, also if you reload.",
  stop: "Stop recording",
  stopping: "Stopping the recording",
  saving: "Earshot has been asked to stop and is saving the recording. It is safe to reload.",
  stopped: "The recording has stopped.",
  stopFailed:
    "The recording could not be stopped from here. Right-click the Earshot icon near the clock and choose Stop meeting.",
  reload: "Reload",
} as const;

/** How long after the app took a stop its `Ended` event is waited for before the words change
 * from "Stopping" to "saving" (a judgement: a little over the 15 s the app itself gives a stop,
 * `MeetingDriver`'s `STOP_CEILING`). The event may never arrive, and "Stopping" must not stay. */
export const STOP_ENDED_WAIT_MS = 20_000;

export interface RootErrorBoundaryProps {
  /** The app, reached directly: the tree under the boundary may be gone. */
  ipc: Ipc;
  /** Loads the window's page again (default: the webview's own reload). */
  onReload?: () => void;
  /** The timer of the wait after a stop (injected so tests never use real time). */
  timer?: Timer;
  children: ReactNode;
}

/** `stopping`: asked, no answer yet, or answered and its `Ended` is waited for. `saving`: the
 * app took the stop and no `Ended` arrived in `STOP_ENDED_WAIT_MS`. `failed`: the app refused. */
type StopState = "idle" | "stopping" | "saving" | "failed";

interface State {
  /** A screen threw: the fixed words show in place of the app. */
  failed: boolean;
  /** The meeting that records, as the fixed words show it. */
  recordingId: string | null;
  stop: StopState;
  /** A recording ended while the fixed words were showing. */
  stopped: boolean;
}

const noop = () => {};

/** Catches a render error anywhere in the app and shows fixed words, Stop recording while a
 * meeting records, and Reload. */
export function RootErrorBoundary(props: RootErrorBoundaryProps) {
  // Holds a tabster reference for the life of the window, as `AppShell` does for its own. When
  // the app's tree goes with a dialog open and nothing else holds tabster, it is disposed without
  // undoing the aria-hidden that dialog put on the page, and the fixed words would be hidden from
  // screen readers. Held here, above the boundary: React unmounts everything under a boundary
  // that caught an error, so a hold under it would go with the tree.
  useFocusFinders();
  return <Boundary {...props} />;
}

class Boundary extends Component<RootErrorBoundaryProps, State> {
  state: State = { failed: false, recordingId: null, stop: "idle", stopped: false };

  // The meeting that records, followed outside React state: while the app works, a meeting's
  // events render nothing here (T9.24).
  private recordingId: string | null = null;
  /** Counts starts and ends, so an answer read before one of them is not believed after it. */
  private heard = 0;
  private subscription = 0;
  private unsubscribe: (() => void) | null = null;
  private mounted = false;
  /** Cancels the wait for `Ended` after a stop the app took. */
  private cancelStopWait: (() => void) | null = null;

  static getDerivedStateFromError(): Partial<State> {
    // Only that it failed: the error it is handed is not read or kept.
    return { failed: true };
  }

  private endStopWait() {
    this.cancelStopWait?.();
    this.cancelStopWait = null;
  }

  componentDidMount() {
    this.mounted = true;
    this.subscription += 1;
    const mine = this.subscription;
    this.props.ipc.onMeetingEvent(this.onEvent).then((stop) => {
      if (this.subscription === mine) this.unsubscribe = stop;
      else stop();
    }, noop);
  }

  componentWillUnmount() {
    this.mounted = false;
    this.subscription += 1;
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.endStopWait();
  }

  componentDidCatch() {
    // The error is not read here: the boundary shows, keeps and sends nothing of it.
    this.setState({ recordingId: this.recordingId });
    // The compact card is too small for the words; repeating the full window changes nothing.
    this.props.ipc.setCompact(false).catch(noop);
    if (this.recordingId === null) this.lookForRecording();
  }

  /** A recording this window never heard start (the page was loaded again during it): the
   * meeting list says so. Asked only once a screen has broken. */
  private lookForRecording() {
    const asked = this.heard;
    this.props.ipc.listMeetings().then((meetings) => {
      // A start or an end heard since is newer than the list.
      if (!this.mounted || this.heard !== asked || this.recordingId !== null) return;
      const recording = meetings.find((m) => m.state === "Recording");
      if (recording === undefined) return;
      this.recordingId = recording.id;
      this.setState({ recordingId: recording.id });
    }, noop);
  }

  private onEvent = (event: MeetingEvent) => {
    if (event.type === "Started") {
      this.heard += 1;
      this.endStopWait();
      this.recordingId = event.meeting;
      if (this.state.failed) this.setState({ recordingId: event.meeting, stop: "idle", stopped: false });
      return;
    }
    if (event.type !== "Ended") return;
    // Any end is newer than a list read before it, also the end of a recording this window
    // never heard start.
    this.heard += 1;
    // An older meeting's end can follow a newer one's start: only the recording one's counts.
    if (event.meeting === this.recordingId) {
      this.endStopWait();
      this.recordingId = null;
      if (this.state.failed) this.setState({ recordingId: null, stop: "idle", stopped: true });
    }
  };

  private stop = () => {
    if (this.state.stop === "stopping") return;
    this.endStopWait();
    this.setState({ stop: "stopping" });
    const timer = this.props.timer ?? realTimer;
    this.props.ipc.stopMeeting().then(
      () => {
        // The app took the stop, and its `Ended` event says when it has stopped. That event can
        // fail to arrive (the event was lost, or the stop hangs): "Stopping" must not stay for
        // good, so after a wait the words say what is known and Stop can be pressed again.
        if (!this.mounted || this.state.stop !== "stopping") return;
        this.endStopWait();
        this.cancelStopWait = timer.setTimeout(() => {
          this.cancelStopWait = null;
          if (this.mounted) this.setState((s) => (s.stop === "stopping" ? { ...s, stop: "saving" } : s));
        }, STOP_ENDED_WAIT_MS);
      },
      // A refusal's own words are not shown: fixed copy.
      () => {
        if (this.mounted) this.setState((s) => (s.stop === "stopping" ? { ...s, stop: "failed" } : s));
      },
    );
  };

  private reload = () => {
    if (this.props.onReload) this.props.onReload();
    else window.location.reload();
  };

  render() {
    const { failed, recordingId, stop, stopped } = this.state;
    if (!failed) return this.props.children;
    return (
      <CrashScreen
        recording={recordingId !== null}
        stop={stop}
        stopped={stopped}
        onStop={this.stop}
        onReload={this.reload}
      />
    );
  }
}

const useStyles = makeStyles({
  root: {
    // The padding inside the window's height: nothing to scroll.
    boxSizing: "border-box",
    height: "100%",
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    justifyContent: "center",
    gap: tokens.spacingVerticalM,
    padding: tokens.spacingHorizontalXXL,
    textAlign: "center",
  },
  heading: {
    margin: 0,
    ":focus-visible": { outline: `2px solid ${tokens.colorStrokeFocus2}`, outlineOffset: "4px" },
  },
  text: {
    maxWidth: "440px",
  },
  status: {
    maxWidth: "440px",
    minHeight: tokens.lineHeightBase300,
  },
  buttons: {
    display: "flex",
    flexWrap: "wrap",
    justifyContent: "center",
    gap: tokens.spacingHorizontalS,
  },
});

function CrashScreen({
  recording,
  stop,
  stopped,
  onStop,
  onReload,
}: {
  recording: boolean;
  stop: StopState;
  stopped: boolean;
  onStop(): void;
  onReload(): void;
}) {
  const styles = useStyles();
  // Focus goes to the heading, never to a button: a key pressed for the screen that broke
  // (Space or Enter while typing) must not stop a recording or reload the window.
  const heading = useRef<HTMLHeadingElement | null>(null);
  useEffect(() => {
    heading.current?.focus();
  }, []);
  return (
    <main className={styles.root}>
      <h1 ref={heading} tabIndex={-1} className={styles.heading}>
        <Title2>{CRASH_COPY.title}</Title2>
      </h1>
      <Body1 align="center" className={styles.text}>
        {CRASH_COPY.body}
      </Body1>
      {recording && (
        <Body1 align="center" className={styles.text}>
          {CRASH_COPY.recording}
        </Body1>
      )}
      <div className={styles.buttons}>
        {recording && (
          // Focusable while it stops: focus must not fall off the only screen there is.
          <Button appearance="primary" disabledFocusable={stop === "stopping"} onClick={onStop}>
            {CRASH_COPY.stop}
          </Button>
        )}
        <Button appearance={recording ? "secondary" : "primary"} onClick={onReload}>
          {CRASH_COPY.reload}
        </Button>
      </div>
      {/* Under the buttons, with its line kept while empty: words that arrive move no button. */}
      <Body1 align="center" className={styles.status} role="status">
        {recording && stop === "stopping" && CRASH_COPY.stopping}
        {recording && stop === "saving" && CRASH_COPY.saving}
        {recording && stop === "failed" && CRASH_COPY.stopFailed}
        {!recording && stopped && CRASH_COPY.stopped}
      </Body1>
    </main>
  );
}
