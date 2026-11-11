// Settings, System, the benchmark (PLAN T7.4c, T12.5b): like a game's benchmark, the current settings run against
// a short meeting shipped with Earshot, and the page shows how far the transcript fell behind
// during it and how long the final transcript and the summary took after it, next to what the
// model check found. The app runs, measures and words the result; this shows it.

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
  Subtitle2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useId } from "react";

import type { BenchmarkResultView, BenchmarkRow } from "../ipc";
import { clockTime } from "../shell/meetings";
import { type BenchmarkRun, benchmarkRunning, useBenchmark } from "./useBenchmark";

/** What the benchmark does. */
export const BENCHMARK_EXPLAINED =
  "Plays a short recorded meeting to Earshot as if it were a call, with your current settings, and measures how far the transcript falls behind during the meeting and how long the final transcript and the summary take after it ends.";

/** What it does not do. */
export const BENCHMARK_PRIVATE =
  "Nothing plays through your speakers, nothing from it is saved as a meeting, and nothing leaves this computer.";

const useStyles = makeStyles({
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  heading: {
    margin: 0,
  },
  text: {
    maxWidth: "70ch",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: "70ch",
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  running: {
    display: "grid",
    gap: tokens.spacingVerticalXS,
    maxWidth: "520px",
  },
  result: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    padding: `${tokens.spacingVerticalM} ${tokens.spacingHorizontalL}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
    maxWidth: "70ch",
  },
  rows: {
    display: "grid",
    gridTemplateColumns: "minmax(180px, max-content) 1fr",
    columnGap: tokens.spacingHorizontalL,
    rowGap: tokens.spacingVerticalXS,
    margin: 0,
  },
  pair: {
    display: "contents",
  },
  label: {
    color: tokens.colorNeutralForeground2,
  },
  value: {
    margin: 0,
  },
});

/** "2:30" for 150 000 ms. */
export function minutesText(ms: number): string {
  const s = Math.round(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** How long a run takes, for the button's note. */
export function lengthNote(meetingMs: number): string {
  const minutes = Math.max(1, Math.round(meetingMs / 60_000));
  return `It takes about ${minutes} minutes for the meeting, plus the time to transcribe and summarise it. Cancelling stops it once the live transcript in progress is finished.`;
}

/** "Ran Thu 5 Nov 2026 at 09:30 with Earshot 0.1.0." */
export function ranLine(r: BenchmarkResultView): string {
  const date = new Date(r.ran_at_ms).toLocaleDateString(undefined, {
    weekday: "short",
    day: "numeric",
    month: "short",
    year: "numeric",
  });
  return `Ran ${date} at ${clockTime(r.ran_at_ms)} with Earshot ${r.app_version}.`;
}

/** The line under the progress bar. */
export function progressText(run: BenchmarkRun): string {
  if (run.status === "starting") return "Loading the models";
  if (run.status === "cancelling") return "Stopping once the live transcript in progress is finished";
  if (run.status !== "running") return "";
  switch (run.phase) {
    case "Replaying":
      return `Playing the meeting: ${minutesText(run.heardMs)} of ${minutesText(run.totalMs)}`;
    case "FinishingLive":
      return "The meeting ended. Finishing the live transcript";
    case "Transcribing":
      return "Making the final transcript";
    case "Summarising":
      return "Writing the summary";
  }
}

function Rows({ label, rows }: { label: string; rows: BenchmarkRow[] }) {
  const styles = useStyles();
  return (
    <dl className={styles.rows} aria-label={label}>
      {rows.map((r) => (
        <div key={r.label} className={styles.pair}>
          <dt>
            <Body1 className={styles.label}>{r.label}</Body1>
          </dt>
          <dd className={styles.value}>
            <Body1>{r.value}</Body1>
          </dd>
        </div>
      ))}
    </dl>
  );
}

function Result({ result }: { result: BenchmarkResultView }) {
  const styles = useStyles();
  return (
    <div className={styles.result} role="group" aria-label="Last benchmark">
      <Body1Strong>{result.headline}</Body1Strong>
      <Rows label="Measured" rows={result.timings} />
      <Caption1 className={styles.muted}>With these settings:</Caption1>
      <Rows label="Settings used" rows={result.settings} />
      {result.notes.map((n) => (
        <Caption1 key={n} className={styles.muted}>
          {n}
        </Caption1>
      ))}
      <Caption1 className={styles.muted}>{ranLine(result)}</Caption1>
    </div>
  );
}

/** The benchmark, a section of the System page. */
export function BenchmarkSection() {
  const styles = useStyles();
  const state = useBenchmark();
  const headingId = useId();
  const { load, run } = state;

  if (load.status === "loading") return <Spinner label="Reading the last benchmark" />;
  if (load.status === "failed") {
    return (
      <MessageBar intent="error">
        <MessageBarBody>
          <MessageBarTitle>The benchmark could not be read</MessageBarTitle>
          {load.message}
        </MessageBarBody>
      </MessageBar>
    );
  }
  const { view } = load;
  const running = benchmarkRunning(run);
  const progress =
    run.status === "running" && run.phase === "Replaying" ? (
      <ProgressBar value={run.heardMs} max={Math.max(run.totalMs, 1)} thickness="large" aria-label="Meeting played so far" />
    ) : (
      <ProgressBar thickness="large" aria-label="Benchmark running" />
    );

  return (
    <section className={styles.section} aria-labelledby={headingId}>
      <Subtitle2 as="h2" id={headingId} className={styles.heading}>
        How fast Earshot is on this computer
      </Subtitle2>
      <Body1 className={styles.text}>{BENCHMARK_EXPLAINED}</Body1>
      <Body1 className={styles.text}>{BENCHMARK_PRIVATE}</Body1>
      {view.unreadable && (
        <MessageBar intent="warning">
          <MessageBarBody>
            An earlier benchmark was saved by another version of Earshot and cannot be read. Run the benchmark again.
          </MessageBarBody>
        </MessageBar>
      )}
      {view.last !== null ? (
        <Result result={view.last} />
      ) : (
        <Caption1 className={styles.muted}>The benchmark has not run on this computer yet.</Caption1>
      )}

      {running && (
        <div className={styles.running} role="group" aria-label="Benchmark running">
          {progress}
          <div className={styles.row}>
            <Caption1>{progressText(run)}</Caption1>
            <Button disabled={run.status === "cancelling"} onClick={() => void state.cancel()}>
              Cancel
            </Button>
          </div>
        </div>
      )}
      {run.status === "cancelled" && (
        <MessageBar intent="info">
          <MessageBarBody>The benchmark was cancelled. Nothing was saved.</MessageBarBody>
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
            <MessageBarTitle>The benchmark did not start</MessageBarTitle>
            {state.notice}
          </MessageBarBody>
        </MessageBar>
      )}

      {!running && (
        <>
          <Caption1 className={styles.muted}>{lengthNote(view.meeting_ms)}</Caption1>
          <div className={styles.row}>
            <Button appearance={view.last !== null ? "secondary" : "primary"} onClick={() => void state.start()}>
              {view.last !== null ? "Run benchmark again" : "Run benchmark"}
            </Button>
          </div>
        </>
      )}
      <Caption1 className={styles.muted}>{view.attribution}</Caption1>
    </section>
  );
}
