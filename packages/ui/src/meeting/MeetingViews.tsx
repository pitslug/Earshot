// The meeting screen's views (docs/ui-design.md "During the meeting"): the start view, the
// recording view, the compact card and the view after the meeting. Each takes its state as
// props; `AppShell` owns the meeting and the IPC calls.

import {
  Accordion,
  AccordionHeader,
  AccordionItem,
  AccordionPanel,
  Body1,
  Button,
  Caption1,
  Card,
  Checkbox,
  Field,
  Input,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  ProgressBar,
  Radio,
  RadioGroup,
  Spinner,
  Subtitle2,
  Title2,
  Title3,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type FormEvent, useEffect, useLayoutEffect, useRef, useState } from "react";

import { useIpc } from "../ipc";
import type { CaptureMode, LiveTrack, MessageBarIntent } from "./types";
import {
  type Banner,
  type LiveMeeting,
  MODE_LABEL,
  atTheEnd,
  afterMeetingBanners,
  bannersNewestFirst,
  transcriptBanner,
  clock,
  lagText,
  meterValue,
  runningOn,
  stepLabel,
  summaryCounts,
  wordCount,
} from "./liveMeeting";

const useStyles = makeStyles({
  view: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalL,
    maxWidth: "760px",
    width: "100%",
  },
  // The recording view fills the window: only the transcript scrolls.
  recording: {
    flexGrow: 1,
    minHeight: 0,
  },
  // Stays in view even if a short window makes the whole page scroll.
  header: {
    position: "sticky",
    top: 0,
    zIndex: 1,
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalM,
    paddingBlock: tokens.spacingVerticalXS,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  headerStatus: {
    flexGrow: 1,
  },
  transcript: {
    flexGrow: 1,
    minHeight: "220px",
    display: "flex",
    flexDirection: "column",
  },
  transcriptItem: {
    flexGrow: 1,
    minHeight: 0,
    display: "flex",
    flexDirection: "column",
  },
  transcriptPanel: {
    flexGrow: 1,
    minHeight: 0,
    position: "relative",
    display: "flex",
    flexDirection: "column",
  },
  transcriptScroll: {
    flexGrow: 1,
    minHeight: 0,
    overflowY: "auto",
  },
  jump: {
    position: "absolute",
    bottom: tokens.spacingVerticalS,
    left: "50%",
    transform: "translateX(-50%)",
  },
  heading: {
    margin: 0,
  },
  form: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalL,
    maxWidth: "480px",
  },
  buttons: {
    display: "flex",
    gap: tokens.spacingHorizontalS,
    alignItems: "center",
  },
  status: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
    minWidth: 0,
  },
  statusMode: {
    color: tokens.colorNeutralForeground3,
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    minWidth: 0,
  },
  clock: {
    marginLeft: "auto",
    fontVariantNumeric: "tabular-nums",
    flexShrink: 0,
  },
  recordingDot: {
    width: "10px",
    height: "10px",
    borderRadius: tokens.borderRadiusCircular,
    backgroundColor: tokens.colorStatusDangerBackground3,
    flexShrink: 0,
  },
  banners: {
    display: "flex",
    flexDirection: "column",
    flexShrink: 0,
    gap: tokens.spacingVerticalS,
  },
  meters: {
    display: "grid",
    gridTemplateColumns: "48px minmax(0, 1fr) 12px",
    alignItems: "center",
    columnGap: tokens.spacingHorizontalM,
    rowGap: tokens.spacingVerticalS,
  },
  voiceDot: {
    width: "10px",
    height: "10px",
    borderRadius: tokens.borderRadiusCircular,
    backgroundColor: tokens.colorNeutralStroke1,
    justifySelf: "center",
  },
  voiceDotOn: {
    backgroundColor: tokens.colorStatusSuccessBackground3,
  },
  tiles: {
    display: "grid",
    gridTemplateColumns: "repeat(3, minmax(0, 1fr))",
    gap: tokens.spacingHorizontalM,
  },
  tile: {
    gap: tokens.spacingVerticalXXS,
  },
  tileLabel: {
    color: tokens.colorNeutralForeground3,
  },
  tileValue: {
    fontVariantNumeric: "tabular-nums",
    overflowWrap: "anywhere",
  },
  grid: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  row: {
    display: "grid",
    gridTemplateColumns: "44px 48px minmax(0, 1fr)",
    columnGap: tokens.spacingHorizontalM,
    alignItems: "baseline",
  },
  time: {
    color: tokens.colorNeutralForeground3,
    fontVariantNumeric: "tabular-nums",
  },
  speaker: {
    fontWeight: tokens.fontWeightSemibold,
  },
  text: {
    overflowWrap: "anywhere",
  },
  // Not yet settled: quieter and slanted, so it never reads as the transcript.
  provisional: {
    overflowWrap: "anywhere",
    color: tokens.colorNeutralForeground3,
    fontStyle: "italic",
  },
  provisionalSpeaker: {
    fontWeight: tokens.fontWeightSemibold,
    color: tokens.colorNeutralForeground3,
  },
  pending: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
    color: tokens.colorNeutralForeground3,
  },
  danger: {
    backgroundColor: tokens.colorStatusDangerBackground3,
    color: tokens.colorNeutralForegroundOnBrand,
    ":hover": {
      backgroundColor: tokens.colorStatusDangerBackground3Hover,
      color: tokens.colorNeutralForegroundOnBrand,
    },
    ":hover:active": {
      backgroundColor: tokens.colorStatusDangerBackground3Pressed,
      color: tokens.colorNeutralForegroundOnBrand,
    },
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  compact: {
    boxSizing: "border-box",
    height: "100%",
    display: "flex",
    flexDirection: "column",
    justifyContent: "space-between",
    gap: tokens.spacingVerticalXS,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    overflow: "hidden",
  },
  compactMeters: {
    display: "grid",
    gridTemplateColumns: "40px minmax(0, 1fr) 10px",
    alignItems: "center",
    columnGap: tokens.spacingHorizontalS,
    rowGap: tokens.spacingVerticalXXS,
  },
  progress: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    maxWidth: "480px",
  },
});

/** What "Keep this meeting on this computer" means, under the box. */
export const KEEP_LOCAL_HINT =
  "Its summary is written by the summary model on this computer or not at all, never sent " +
  "elsewhere. This can only be chosen now.";

/** The longest title the start view accepts. */
export const MAX_TITLE = 120;

export interface NewMeetingViewProps {
  /** Starts the meeting; rejects with the reason it could not start. `keepLocal` keeps its
   * summary on this computer (T12.13). */
  onStart(mode: CaptureMode, title: string | null, keepLocal: boolean): Promise<void>;
  onCancel(): void;
}

/** "New meeting": what to record (preselected from the Audio page's capture mode, T7.4i),
 * an optional title, and whether its summary stays on this computer (T12.13: asked only here,
 * off by default). */
export function NewMeetingView({ onStart, onCancel }: NewMeetingViewProps) {
  const styles = useStyles();
  const ipc = useIpc();
  const [mode, setMode] = useState<CaptureMode>("Call");
  // The stored default applies until the user picks a mode themselves.
  const picked = useRef(false);
  useEffect(() => {
    let live = true;
    ipc.defaultCaptureMode().then(
      (stored) => {
        if (live && !picked.current) setMode(stored);
      },
      () => {
        // Without the setting the screen keeps "Call"; the Audio page shows the failure.
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);
  const [title, setTitle] = useState("");
  const [keepLocal, setKeepLocal] = useState(false);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const tooLong = title.trim().length > MAX_TITLE;

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (tooLong || starting) return;
    setStarting(true);
    setError(null);
    const trimmed = title.trim();
    onStart(mode, trimmed === "" ? null : trimmed, keepLocal).catch((e: unknown) => {
      setStarting(false);
      setError(e instanceof Error ? e.message : String(e));
    });
  };

  return (
    <div className={styles.view}>
      <Title2 as="h1" className={styles.heading}>
        New meeting
      </Title2>
      {error !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>The meeting could not start</MessageBarTitle>
            {error}
          </MessageBarBody>
        </MessageBar>
      )}
      <form className={styles.form} onSubmit={submit} aria-label="New meeting">
        <Field label="What are you recording?">
          <RadioGroup
            value={mode}
            onChange={(_, d) => {
              picked.current = true;
              setMode(d.value as CaptureMode);
            }}
          >
            <Radio value="Call" label={MODE_LABEL.Call} />
            <Radio value="InRoom" label={MODE_LABEL.InRoom} />
          </RadioGroup>
        </Field>
        <Field
          label="Title (optional)"
          validationState={tooLong ? "error" : "none"}
          validationMessage={
            tooLong ? `Keep the title to ${MAX_TITLE} characters or fewer.` : undefined
          }
          hint={tooLong ? undefined : "Without a title, the summary names the meeting."}
        >
          <Input
            value={title}
            onChange={(_, d) => setTitle(d.value)}
            placeholder="Untitled meeting"
          />
        </Field>
        <Field hint={KEEP_LOCAL_HINT}>
          <Checkbox
            checked={keepLocal}
            onChange={(_, d) => setKeepLocal(d.checked === true)}
            label="Keep this meeting on this computer"
          />
        </Field>
        <div className={styles.buttons}>
          <Button appearance="primary" type="submit" disabled={tooLong || starting}>
            Start
          </Button>
          <Button onClick={onCancel} disabled={starting}>
            Cancel
          </Button>
          {starting && <Spinner size="tiny" label="Checking models and devices" />}
        </div>
      </form>
    </div>
  );
}

function bannerIntent(b: Banner): MessageBarIntent {
  switch (b.kind) {
    case "DeviceRestored":
      return "success";
    case "SpeakersOff":
    case "SummariesOff":
    case "Backend":
    case "CallEnded":
      return "info";
    case "Recording":
    case "Storage":
      return "error";
    default:
      return "warning";
  }
}

function Banners({ banners, onDismiss }: { banners: Banner[]; onDismiss(key: string): void }) {
  const styles = useStyles();
  if (banners.length === 0) return null;
  return (
    <div className={styles.banners}>
      {banners.map((b) => (
        <MessageBar key={b.key} intent={bannerIntent(b)}>
          <MessageBarBody>{b.message}</MessageBarBody>
          <MessageBarActions
            containerAction={
              <Button
                appearance="transparent"
                aria-label="Dismiss"
                size="small"
                onClick={() => onDismiss(b.key)}
              >
                Dismiss
              </Button>
            }
          />
        </MessageBar>
      ))}
    </div>
  );
}

/** The question when the call seems to have ended. The app stops the recording if it stays open
 * for 3 minutes; anything that closes it starts that over. */
function CallEndOffer({
  onStop,
  onKeep,
  stopping,
}: {
  onStop(): void;
  onKeep(): void;
  stopping: boolean;
}) {
  const styles = useStyles();
  return (
    <div className={styles.banners}>
      <MessageBar intent="info" layout="multiline" role="status">
        <MessageBarBody>
          <MessageBarTitle>The call seems to have ended.</MessageBarTitle>
          Earshot will stop recording within 3 minutes unless you keep it going.
        </MessageBarBody>
        <MessageBarActions>
          <Button onClick={onStop} disabled={stopping}>
            Stop recording
          </Button>
          <Button onClick={onKeep}>Keep recording</Button>
        </MessageBarActions>
      </MessageBar>
    </div>
  );
}

/** What is happening to the live transcript, at its top; not dismissable, it goes by itself. */
function TranscriptBanner({ meeting }: { meeting: LiveMeeting }) {
  const styles = useStyles();
  const banner = transcriptBanner(meeting.transcript);
  if (banner === null) return null;
  return (
    <div className={styles.banners}>
      <MessageBar intent="info" layout="multiline" role="status">
        <MessageBarBody>
          {banner.title !== null && <MessageBarTitle>{banner.title}</MessageBarTitle>}
          {banner.body}
        </MessageBarBody>
      </MessageBar>
    </div>
  );
}

function Meter({
  track,
  meeting,
  compact = false,
}: {
  track: LiveTrack;
  meeting: LiveMeeting;
  compact?: boolean;
}) {
  const styles = useStyles();
  const speaking = meeting.speaking[track];
  const Label = compact ? Caption1 : Body1;
  return (
    <>
      <Label>{track}</Label>
      <ProgressBar
        aria-label={`${track} level`}
        value={meterValue(meeting.levels[track])}
        max={1}
        thickness="large"
      />
      <span
        role="img"
        aria-label={speaking ? `${track} speaking` : `${track} quiet`}
        className={mergeClasses(styles.voiceDot, speaking && styles.voiceDotOn)}
      />
    </>
  );
}

function StatusLine({ meeting, now }: { meeting: LiveMeeting; now: number }) {
  const styles = useStyles();
  return (
    <div className={mergeClasses(styles.status, styles.headerStatus)}>
      <span className={styles.recordingDot} aria-hidden="true" />
      <Subtitle2>Listening</Subtitle2>
      <Caption1 className={styles.statusMode}>{MODE_LABEL[meeting.mode]}</Caption1>
      <Subtitle2 className={styles.clock} aria-label="Elapsed">
        {clock(now - meeting.startedAt)}
      </Subtitle2>
    </div>
  );
}

function Tile({ label, value }: { label: string; value: string }) {
  const styles = useStyles();
  return (
    <Card appearance="filled-alternative" className={styles.tile} role="group" aria-label={label}>
      <Caption1 className={styles.tileLabel}>{label}</Caption1>
      <Title3 className={styles.tileValue}>{value}</Title3>
    </Card>
  );
}

/** The transcript with the newest line in view: it follows new lines until the user scrolls
 * up, then "Jump to latest" goes back to following. */
function FollowingTranscript({ meeting }: { meeting: LiveMeeting }) {
  const styles = useStyles();
  const scroller = useRef<HTMLDivElement>(null);
  const [following, setFollowing] = useState(true);
  const lines = meeting.lines.length;
  const provisional = meeting.provisional.You.length + meeting.provisional.Them.length;
  useLayoutEffect(() => {
    const el = scroller.current;
    if (following && el) el.scrollTop = el.scrollHeight;
  }, [lines, provisional, following]);
  return (
    <div className={styles.transcriptPanel}>
      <div
        ref={scroller}
        className={styles.transcriptScroll}
        role="group"
        aria-label="Transcript scroll area"
        tabIndex={0}
        onScroll={(e) => {
          const el = e.currentTarget;
          setFollowing(atTheEnd(el.scrollTop, el.clientHeight, el.scrollHeight));
        }}
      >
        <TranscriptGrid meeting={meeting} />
      </div>
      {!following && (
        <Button
          className={styles.jump}
          size="small"
          appearance="primary"
          onClick={() => setFollowing(true)}
        >
          Jump to latest
        </Button>
      )}
    </div>
  );
}

function TranscriptGrid({ meeting }: { meeting: LiveMeeting }) {
  const styles = useStyles();
  const provisional = (["You", "Them"] as const)
    .map((t) => [t, meeting.provisional[t]] as const)
    .filter(([, words]) => words.length > 0);
  return (
    <div role="table" aria-label="Transcript as it lands" className={styles.grid}>
      {meeting.lines.map((line) => (
        <div role="row" key={line.seq} className={styles.row}>
          <Caption1 role="cell" className={styles.time}>
            {clock(line.startMs)}
          </Caption1>
          <Body1 role="cell" className={styles.speaker}>
            {line.track}
          </Body1>
          <Body1 role="cell" className={styles.text}>
            {line.text}
          </Body1>
        </div>
      ))}
      {provisional.map(([track, words]) => (
        <div role="row" key={track} aria-label={`${track}, provisional`} className={styles.row}>
          <Caption1 role="cell" className={styles.time}>
            {clock(words[0].startMs)}
          </Caption1>
          <Body1 role="cell" className={styles.provisionalSpeaker}>
            {track}
          </Body1>
          <Body1 role="cell" className={styles.provisional}>
            {words.map((w) => w.text).join(" ")}
          </Body1>
        </div>
      ))}
      {provisional.length === 0 && (
        <div role="row" className={styles.row}>
          <span role="cell" />
          <span role="cell" />
          <span role="cell" className={styles.pending}>
            <Spinner size="extra-tiny" aria-hidden="true" />
            <Caption1>Next stretch transcribing</Caption1>
          </span>
        </div>
      )}
    </div>
  );
}

export interface RecordingViewProps {
  meeting: LiveMeeting;
  /** The injected clock's time, for the elapsed clock. */
  now: number;
  onStop(): void;
  onCompact(): void;
  onDismiss(key: string): void;
  /** "Keep recording" on the offer to stop when the call seems to have ended. */
  onKeepRecording(): void;
  /** True while the stop request is on its way. */
  stopping?: boolean;
}

/** The meeting while it records. */
export function RecordingView({
  meeting,
  now,
  onStop,
  onCompact,
  onDismiss,
  onKeepRecording,
  stopping = false,
}: RecordingViewProps) {
  const styles = useStyles();
  return (
    <section className={mergeClasses(styles.view, styles.recording)} aria-label="Meeting">
      <div className={styles.header} role="group" aria-label="Meeting controls">
        <StatusLine meeting={meeting} now={now} />
        <div className={styles.buttons}>
          <Button
            appearance="primary"
            className={styles.danger}
            onClick={onStop}
            disabled={stopping}
          >
            Stop meeting
          </Button>
          <Button onClick={onCompact}>Compact</Button>
        </div>
      </div>
      {meeting.callEndOffer && (
        <CallEndOffer onStop={onStop} onKeep={onKeepRecording} stopping={stopping} />
      )}
      <Banners banners={bannersNewestFirst(meeting)} onDismiss={onDismiss} />
      <div className={styles.meters} role="group" aria-label="Levels">
        {meeting.tracks.map((t) => (
          <Meter key={t} track={t} meeting={meeting} />
        ))}
      </div>
      <div className={styles.tiles}>
        <Tile label="Words so far" value={wordCount(meeting.words)} />
        <Tile label="Transcript behind" value={lagText(meeting.lagSeconds)} />
        <Tile label="Running on" value={runningOn(meeting)} />
      </div>
      <TranscriptBanner meeting={meeting} />
      <Accordion className={styles.transcript} collapsible defaultOpenItems={["transcript"]}>
        <AccordionItem className={styles.transcriptItem} value="transcript">
          <AccordionHeader>Transcript as it lands</AccordionHeader>
          <AccordionPanel className={styles.transcriptPanel}>
            <FollowingTranscript meeting={meeting} />
          </AccordionPanel>
        </AccordionItem>
      </Accordion>
    </section>
  );
}

export interface CompactCardProps {
  meeting: LiveMeeting;
  now: number;
  onStop(): void;
  onExpand(): void;
  onKeepRecording(): void;
  stopping?: boolean;
}

/** The small always-on-top listening card. */
export function CompactCard({
  meeting,
  now,
  onStop,
  onExpand,
  onKeepRecording,
  stopping = false,
}: CompactCardProps) {
  const styles = useStyles();
  return (
    <section className={styles.compact} aria-label="Meeting">
      <div className={styles.status}>
        <span className={styles.recordingDot} aria-hidden="true" />
        <Subtitle2>Listening</Subtitle2>
        <Subtitle2 className={styles.clock} aria-label="Elapsed">
          {clock(now - meeting.startedAt)}
        </Subtitle2>
      </div>
      <div className={styles.compactMeters} role="group" aria-label="Levels">
        {meeting.tracks.map((t) => (
          <Meter key={t} track={t} meeting={meeting} compact />
        ))}
      </div>
      <Caption1 className={styles.muted}>
        {meeting.callEndOffer
          ? "The call seems to have ended. Stopping within 3 minutes."
          : `${wordCount(meeting.words)} words so far`}
      </Caption1>
      <div className={styles.buttons}>
        <Button
          appearance="primary"
          size="small"
          className={styles.danger}
          onClick={onStop}
          disabled={stopping}
        >
          Stop
        </Button>
        {meeting.callEndOffer && (
          <Button size="small" onClick={onKeepRecording}>
            Keep recording
          </Button>
        )}
        <Button size="small" onClick={onExpand}>
          Expand
        </Button>
      </div>
    </section>
  );
}

export interface AfterMeetingViewProps {
  meeting: LiveMeeting;
  onOpen(meetingId: string): void;
  onNameVoices(meetingId: string): void;
  onDismissBanner(key: string): void;
  /** Stops the work after the recording: the audio stays and the meeting can be re-transcribed. */
  onCancel(): void;
  /** True from the press until the meeting's result: a model call may have to end first. */
  cancelling?: boolean;
  /** Re-transcribes the meeting, offered when its transcript was not finished (T9.23). */
  onRetranscribe(meetingId: string): void;
  /** Writes the summary again, offered when it failed (T9.23). */
  onResummarise(meetingId: string): void;
  /** True while a re-transcription or re-summary asked for here is starting. */
  retrying?: boolean;
  /** Why the one asked for here did not start, in the app's words. */
  retryError?: string | null;
}

function afterMeetingHeading(s: LiveMeeting["summary"]): string {
  if (s === null) return "Finishing the meeting";
  return s.kind === "PostCancelled" || s.kind === "PostFailed" ? "Meeting saved" : "Meeting finished";
}

/** After the stop: the post-meeting steps, then the summary result. */
export function AfterMeetingView({
  meeting,
  onOpen,
  onNameVoices,
  onDismissBanner,
  onCancel,
  cancelling = false,
  onRetranscribe,
  onResummarise,
  retrying = false,
  retryError = null,
}: AfterMeetingViewProps) {
  const styles = useStyles();
  const p = meeting.progress;
  const s = meeting.summary;
  return (
    <section className={styles.view} aria-label="Meeting">
      <Title2 as="h1" className={styles.heading}>
        {afterMeetingHeading(s)}
      </Title2>
      {meeting.endedReason === "Failed" && (
        <MessageBar intent="error">
          <MessageBarBody>
            The recording stopped because audio capture failed. What was recorded is kept.
          </MessageBarBody>
        </MessageBar>
      )}
      {s === null && (
        <div className={styles.progress}>
          <ProgressBar
            aria-label="Progress"
            value={p?.total ? p.done / p.total : undefined}
            max={1}
          />
          <Body1 aria-live="polite">
            {cancelling
              ? "Stopping. This can take a moment if a model is still working or loading."
              : p
                ? stepLabel(p.step)
                : "Finalising the recording"}
          </Body1>
          {p?.total ? (
            <Caption1 className={styles.muted}>
              {p.done} of {p.total} steps
            </Caption1>
          ) : null}
          <div className={styles.buttons}>
            <Button onClick={onCancel} disabled={cancelling}>
              Cancel
            </Button>
          </div>
        </div>
      )}
      <Banners banners={afterMeetingBanners(meeting)} onDismiss={onDismissBanner} />
      {s !== null && (
        <SummaryNotice
          meeting={meeting}
          onOpen={onOpen}
          onNameVoices={onNameVoices}
          onRetranscribe={onRetranscribe}
          onResummarise={onResummarise}
          retrying={retrying}
        />
      )}
      {retryError !== null && (
        <MessageBar intent="error" layout="multiline" role="alert">
          <MessageBarBody>{retryError}</MessageBarBody>
        </MessageBar>
      )}
    </section>
  );
}

function SummaryNotice({
  meeting,
  onOpen,
  onNameVoices,
  onRetranscribe,
  onResummarise,
  retrying,
}: Pick<
  AfterMeetingViewProps,
  "meeting" | "onOpen" | "onNameVoices" | "onRetranscribe" | "onResummarise" | "retrying"
>) {
  const s = meeting.summary;
  if (s === null) return null;
  const open = (
    <Button appearance="primary" size="small" onClick={() => onOpen(meeting.meetingId)}>
      Open
    </Button>
  );
  // The way forward on the spot (T9.23): the job starts and the meeting's page shows it.
  const retranscribe = (
    <Button
      appearance="primary"
      size="small"
      disabled={retrying}
      onClick={() => onRetranscribe(meeting.meetingId)}
    >
      Re-transcribe
    </Button>
  );
  const resummarise = (
    <Button
      appearance="primary"
      size="small"
      disabled={retrying}
      onClick={() => onResummarise(meeting.meetingId)}
    >
      Re-summarise
    </Button>
  );
  const openSecondary = (
    <Button size="small" onClick={() => onOpen(meeting.meetingId)}>
      Open
    </Button>
  );
  switch (s.kind) {
    case "Stored":
      return (
        <MessageBar intent="success" layout="multiline" role="status">
          <MessageBarBody>
            <MessageBarTitle>Summary ready</MessageBarTitle>
            {s.title} · {summaryCounts(s)}
          </MessageBarBody>
          <MessageBarActions>
            {open}
            {s.voices_to_name > 0 && (
              <Button size="small" onClick={() => onNameVoices(meeting.meetingId)}>
                Name voices
              </Button>
            )}
          </MessageBarActions>
        </MessageBar>
      );
    case "Skipped":
      return (
        <MessageBar intent="info" layout="multiline" role="status">
          <MessageBarBody>
            <MessageBarTitle>No summary</MessageBarTitle>
            {s.reason} The transcript is saved.
          </MessageBarBody>
          <MessageBarActions>{open}</MessageBarActions>
        </MessageBar>
      );
    case "Failed":
      return (
        <MessageBar intent="error" layout="multiline" role="status">
          <MessageBarBody>
            <MessageBarTitle>Summary failed</MessageBarTitle>
            {s.message}{" "}
            {s.retryable
              ? "Trying again may work: re-summarise the meeting."
              : "The recording and transcript are saved."}
          </MessageBarBody>
          <MessageBarActions>
            {resummarise}
            {openSecondary}
          </MessageBarActions>
        </MessageBar>
      );
    case "PostFailed":
      return (
        <MessageBar intent="error" layout="multiline" role="status">
          <MessageBarBody>
            <MessageBarTitle>Transcript not finished</MessageBarTitle>
            {s.message}
          </MessageBarBody>
          <MessageBarActions>
            {retranscribe}
            {openSecondary}
          </MessageBarActions>
        </MessageBar>
      );
    case "Cancelled":
      return (
        <MessageBar intent="info" layout="multiline" role="status">
          <MessageBarBody>
            <MessageBarTitle>Summary cancelled</MessageBarTitle>
            The recording and transcript are saved. You can summarise the meeting from its page.
          </MessageBarBody>
          <MessageBarActions>{open}</MessageBarActions>
        </MessageBar>
      );
    case "PostCancelled":
      return (
        <MessageBar intent="info" layout="multiline" role="status">
          <MessageBarBody>
            <MessageBarTitle>Transcript not finished</MessageBarTitle>
            The recording and any draft transcript are saved. Re-transcribe the meeting to get the
            full transcript.
          </MessageBarBody>
          <MessageBarActions>
            {retranscribe}
            {openSecondary}
          </MessageBarActions>
        </MessageBar>
      );
  }
}
