// The audio check on the Audio page (PLAN T7.4j): plays a few short sounds through the output
// and listens with the microphone, then says what it found in plain words, with the date. The
// app measures, judges and words the result; this shows it.

import {
  Body1,
  Body1Strong,
  Button,
  Caption1,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  ProgressBar,
  Spinner,
  makeStyles,
  tokens,
} from "@fluentui/react-components";

import type { AudioCheckResultView } from "../ipc";
import { clockTime } from "../shell/meetings";
import { type AudioCheckState, checkRunning } from "./useAudioCheck";

const useStyles = makeStyles({
  text: {
    maxWidth: "70ch",
  },
  muted: {
    color: tokens.colorNeutralForeground2,
    maxWidth: "62ch",
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  result: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
    maxWidth: "70ch",
  },
  lines: {
    margin: 0,
    paddingLeft: tokens.spacingHorizontalL,
  },
  listening: {
    display: "grid",
    gap: tokens.spacingVerticalXS,
    maxWidth: "520px",
  },
});

/** What the check does. */
export const CHECK_EXPLAINED =
  "Plays a few short sounds through your output and listens with your microphone. Earshot measures how long the sound takes to reach the microphone and how loud the echo is, and uses the delay to cancel echo sooner in your meetings. It also tells you when you use headphones, or when the microphone is too quiet or too loud.";

/** The warning to expect sound. */
export const CHECK_WARNING =
  "You will hear four short chirps at your current volume. Keep the room quiet for a few seconds while it runs.";

/** "Checked Thu 5 Nov 2026 at 09:30 with Microphone Array and Speakers." */
export function checkedLine(r: AudioCheckResultView): string {
  const date = new Date(r.checked_at_ms).toLocaleDateString(undefined, {
    weekday: "short",
    day: "numeric",
    month: "short",
    year: "numeric",
  });
  return `Checked ${date} at ${clockTime(r.checked_at_ms)} with ${r.microphone} and ${r.output}.`;
}

function seconds(ms: number): number {
  return Math.round(ms / 1000);
}

function Result({ result }: { result: AudioCheckResultView }) {
  const styles = useStyles();
  const warn = result.outcome !== "Echo" && result.outcome !== "Headphones";
  return (
    <div className={styles.result} role="group" aria-label="Last audio check">
      <Body1Strong>{result.headline}</Body1Strong>
      {result.details.length > 0 && (
        <ul className={styles.lines}>
          {result.details.map((d) => (
            <li key={d}>
              <Body1>{d}</Body1>
            </li>
          ))}
        </ul>
      )}
      <Caption1 className={styles.muted}>{checkedLine(result)}</Caption1>
      {warn && <Caption1 className={styles.muted}>Nothing from this check is used in meetings.</Caption1>}
    </div>
  );
}

/** The audio check section's content. */
export function AudioCheckSection({ state, disabled }: { state: AudioCheckState; disabled: boolean }) {
  const styles = useStyles();
  const { run, view } = state;
  const running = checkRunning(run);
  return (
    <>
      <Body1 className={styles.text}>{CHECK_EXPLAINED}</Body1>
      {view?.unreadable === true && (
        <MessageBar intent="warning">
          <MessageBarBody>
            An earlier audio check was saved by another version of Earshot and cannot be read. Run the check again.
          </MessageBarBody>
        </MessageBar>
      )}
      {view?.result != null ? (
        <Result result={view.result} />
      ) : (
        view !== null && <Caption1 className={styles.muted}>These devices have not been checked yet.</Caption1>
      )}

      {run.status === "starting" && <Spinner size="small" label="Opening the output and the microphone" labelPosition="after" />}
      {run.status === "listening" && (
        <div className={styles.listening} role="group" aria-label="Audio check running">
          <ProgressBar value={run.heardMs} max={run.totalMs} thickness="large" aria-label="Listened so far" />
          <div className={styles.row}>
            <Caption1>
              Playing and listening: {seconds(run.heardMs)} of {seconds(run.totalMs)} seconds
            </Caption1>
            <Button onClick={() => void state.cancel()}>Cancel</Button>
          </div>
        </div>
      )}
      {run.status === "done" && run.note !== null && (
        <MessageBar intent="warning">
          <MessageBarBody>{run.note}</MessageBarBody>
        </MessageBar>
      )}
      {run.status === "cancelled" && (
        <MessageBar intent="info">
          <MessageBarBody>The audio check was cancelled. Nothing was saved.</MessageBarBody>
        </MessageBar>
      )}
      {run.status === "failed" && (
        <MessageBar intent="error">
          <MessageBarBody>{run.message}</MessageBarBody>
        </MessageBar>
      )}
      {state.notice !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>The audio check did not start</MessageBarTitle>
            {state.notice}
          </MessageBarBody>
        </MessageBar>
      )}

      {!running && (
        <>
          <Caption1 className={styles.muted}>{CHECK_WARNING}</Caption1>
          <div className={styles.row}>
            <Button appearance={view?.result != null ? "secondary" : "primary"} disabled={disabled} onClick={() => void state.start()}>
              {view?.result != null ? "Run audio check again" : "Run audio check"}
            </Button>
          </div>
        </>
      )}
    </>
  );
}
