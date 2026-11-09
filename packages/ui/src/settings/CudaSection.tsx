// Settings, System: NVIDIA's CUDA libraries (T8.4af). Shown only on a computer the app offers them
// to; every decision comes from the IPC (`cuda_settings`), this file only shows it.

import {
  Badge,
  Body1,
  Body1Strong,
  Button,
  Caption1,
  Checkbox,
  Link,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  ProgressBar,
  Spinner,
  Switch,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useState } from "react";

import type { CudaPartView, CudaView } from "../ipc";
import { Notices } from "./ModelsParts";
import { formatSize } from "./copy";
import type { CudaState } from "./useCuda";

/** What the libraries are, for the card that earned the offer. */
export function cudaIntro(card: string | null): string {
  const who = card ?? "Your graphics card";
  return `${who} can also run on NVIDIA's CUDA libraries. They are optional, a separate download and off unless you switch them on. Earshot works without them.`;
}

/** What the summaries part does, against what summaries run on without it (from the app). */
export function cudaSummariesWhy(staysOn: string): string {
  return `Summaries run through NVIDIA's libraries instead of ${staysOn}. At each start Earshot first tries them on your card, and if that does not work it stays on ${staysOn} and says so.`;
}

/** What the transcription part does and does not gain. */
export const CUDA_TRANSCRIPTION_WHY =
  "Transcription already keeps up on the processor, and accuracy is the same. What this gains is less work for the processor.";

/** The transcription part cannot do without the other. */
export const CUDA_TRANSCRIPTION_NEEDS = "It uses files from summaries on CUDA, so that part comes first.";

/** When a change applies. */
export const CUDA_APPLIES =
  "A change applies the next time Earshot starts, because a library cannot be unloaded while the app runs. Switching off never removes the files.";

/** What the section says when the trial at start worked. */
export const CUDA_RUNNING =
  "Summaries are on CUDA now: the trial at this start ran on your card.";

/** What the section says while the start verifies and tries the files. */
export const CUDA_CHECKING_LINE =
  "Earshot is trying the CUDA libraries on your card. Until that has finished, this page shows what runs without them.";

const TITLE = { Summaries: "Summaries on CUDA", Transcription: "Transcription on CUDA" };

const useStyles = makeStyles({
  parts: {
    display: "grid",
    gap: tokens.spacingVerticalS,
  },
  card: {
    display: "grid",
    gridTemplateColumns: "minmax(0, 1fr) auto",
    columnGap: tokens.spacingHorizontalL,
    rowGap: tokens.spacingVerticalXS,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  main: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
    minWidth: 0,
  },
  title: {
    display: "inline-flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  why: {
    color: tokens.colorNeutralForeground2,
    maxWidth: "62ch",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: "62ch",
  },
  actions: {
    display: "flex",
    flexDirection: "column",
    alignItems: "flex-end",
    gap: tokens.spacingVerticalXS,
    textAlign: "right",
    paddingTop: tokens.spacingVerticalXS,
  },
  // Fluent slides a Switch's knob from where text alignment puts it, so it must not inherit the
  // column's right alignment: the knob would start on the right and slide past it.
  toggle: {
    textAlign: "left",
  },
  terms: {
    display: "flex",
    flexDirection: "column",
    alignItems: "flex-start",
    gap: tokens.spacingVerticalXXS,
  },
  term: {
    display: "flex",
    flexWrap: "wrap",
    columnGap: tokens.spacingHorizontalS,
    alignItems: "baseline",
  },
  checking: {
    justifyContent: "flex-start",
  },
  bar: {
    width: "140px",
  },
});

function Part({
  part,
  why,
  sizes,
  state,
  switchDisabled,
  onSwitch,
}: {
  part: CudaPartView;
  why: string;
  sizes: string;
  state: CudaState;
  switchDisabled: boolean;
  onSwitch(on: boolean): void;
}) {
  const styles = useStyles();
  const [accepted, setAccepted] = useState(false);
  const title = TITLE[part.part];
  const canFetch = part.not_available === null && part.installed !== "Yes" && !part.downloading;
  const removable = part.installed !== "No" && !part.downloading;
  const progress = `${formatSize(part.held)} of ${formatSize(part.download_size)}`;
  const frees = part.installed === "Yes" ? part.disk_size : part.held;
  return (
    <div className={styles.card} data-part={part.part}>
      <div className={styles.main}>
        <span className={styles.title}>
          <Body1Strong>{title}</Body1Strong>
          {part.installed === "Yes" && !part.downloading && (
            <Badge appearance="tint" color="success">
              Installed
            </Badge>
          )}
        </span>
        <Caption1 className={styles.why}>{why}</Caption1>
        <Caption1 className={styles.muted}>{sizes}</Caption1>
        {part.not_available !== null && part.installed !== "Yes" && (
          <Body1 role="status">{part.not_available}</Body1>
        )}
        {canFetch && (
          <div className={styles.terms}>
            <Caption1 className={styles.why}>This download is under NVIDIA's terms:</Caption1>
            {part.terms.map((t) => (
              <span key={t.terms} className={styles.term}>
                <Link as="button" onClick={() => void state.openTerms(t.terms)}>
                  {t.title}
                </Link>
                <Caption1 className={styles.muted}>{t.url}</Caption1>
              </span>
            ))}
            <Checkbox
              checked={accepted}
              onChange={(_, d) => setAccepted(d.checked === true)}
              label="I accept NVIDIA's terms for this download"
            />
          </div>
        )}
      </div>
      <div className={styles.actions}>
        <Switch
          className={styles.toggle}
          checked={part.on}
          disabled={switchDisabled}
          label={part.on ? "On" : "Off"}
          labelPosition="before"
          aria-label={title}
          onChange={(_, d) => onSwitch(d.checked)}
        />
        {part.downloading || part.installed === "Partly" ? (
          <>
            <Caption1 className={styles.muted}>
              {part.downloading ? progress : `Partly downloaded: ${progress}`}
            </Caption1>
            <ProgressBar
              className={styles.bar}
              value={part.download_size > 0 ? part.held / part.download_size : 0}
              aria-label={`${title} download`}
            />
            {part.downloading ? (
              <Button size="small" aria-label={`Pause ${title}`} onClick={() => void state.pause(part.part)}>
                Pause
              </Button>
            ) : (
              <Button
                size="small"
                disabled={part.not_available !== null}
                aria-label={`Resume ${title}`}
                onClick={() => void state.download(part.part, accepted)}
              >
                Resume
              </Button>
            )}
          </>
        ) : (
          canFetch && (
            <Button
              size="small"
              aria-label={`Download ${title} (${formatSize(part.download_size)})`}
              onClick={() => void state.download(part.part, accepted)}
            >
              Download {formatSize(part.download_size)}
            </Button>
          )
        )}
        {removable && (
          <Button
            size="small"
            appearance="transparent"
            disabled={part.removal_refused !== null}
            aria-label={`Remove ${title} (frees ${formatSize(frees)})`}
            onClick={() => void state.remove(part.part)}
          >
            Remove (frees {formatSize(frees)})
          </Button>
        )}
        {removable && part.removal_refused !== null && (
          <Caption1 className={styles.muted}>{part.removal_refused}</Caption1>
        )}
      </div>
    </div>
  );
}

function sizesLine(part: CudaPartView): string {
  const about = part.not_available !== null;
  const download = `${about ? "About " : ""}${formatSize(part.download_size)} to download`;
  const disk = `${about ? "about " : ""}${formatSize(part.disk_size)} on disk`;
  return `${download}, ${disk}.`;
}

function transcriptionWhy(view: CudaView): string {
  const saved = view.transcription_cpu_saved;
  return `${CUDA_TRANSCRIPTION_WHY}${saved !== null ? ` ${saved}` : ""} ${CUDA_TRANSCRIPTION_NEEDS}`;
}

/** The section's content (the page gives it its heading, only when `view.offered`). */
export function CudaParts({ state, view }: { state: CudaState; view: CudaView }) {
  const styles = useStyles();
  const { summaries, transcription, switch: stored } = view;
  return (
    <div className={styles.parts}>
      <Caption1 className={styles.why}>{cudaIntro(view.card)}</Caption1>
      <Notices notices={state.notices} onDismiss={state.dismiss} />
      {view.unknown.length > 0 && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>Some stored settings are not known to this version</MessageBarTitle>
            {view.unknown.join(" ")}
          </MessageBarBody>
        </MessageBar>
      )}
      {view.this_run.checking && (
        <div role="status">
          <Spinner className={styles.checking} size="tiny" labelPosition="after" label={CUDA_CHECKING_LINE} />
        </div>
      )}
      {view.this_run.summaries_on_cuda && <Caption1 role="status">{CUDA_RUNNING}</Caption1>}
      <Part
        part={summaries}
        why={cudaSummariesWhy(view.summaries_stay_on)}
        sizes={sizesLine(summaries)}
        state={state}
        switchDisabled={!summaries.on && summaries.installed !== "Yes"}
        onSwitch={(on) =>
          // Transcription cannot stay on without it.
          void state.setSwitch({ summaries: on, transcription: on && stored.transcription })
        }
      />
      <Part
        part={transcription}
        why={transcriptionWhy(view)}
        sizes={sizesLine(transcription)}
        state={state}
        switchDisabled={
          !transcription.on &&
          (transcription.not_available !== null || transcription.installed !== "Yes" || !stored.summaries)
        }
        onSwitch={(on) => void state.setSwitch({ summaries: stored.summaries, transcription: on })}
      />
      <Caption1 className={styles.muted}>{CUDA_APPLIES}</Caption1>
      {view.restart_needed && (
        <MessageBar intent="info">
          <MessageBarBody>
            <MessageBarTitle>Restart to apply this</MessageBarTitle>
            {view.restart_blocked ?? "The change applies when Earshot starts again."}
          </MessageBarBody>
          <MessageBarActions>
            <Button size="small" disabled={view.restart_blocked !== null} onClick={() => void state.restart()}>
              Restart Earshot
            </Button>
          </MessageBarActions>
        </MessageBar>
      )}
    </div>
  );
}
