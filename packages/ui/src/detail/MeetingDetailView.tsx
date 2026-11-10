// The meeting detail screen (docs/ui-design.md "Meeting detail", T7.3, T9.1o): a header that
// stays in view with Export, Copy summary, Re-transcribe and Re-summarise, Summary and Transcript
// tabs, and the who-is-who panel. Deleting starts from the meeting list. Every change goes through `useIpc()`;
// the detail is read again after each one.

import {
  Body1,
  Body1Strong,
  Button,
  Caption1,
  CompoundButton,
  Input,
  Link,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Spinner,
  Subtitle2,
  Tab,
  TabList,
  Textarea,
  Title2,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type KeyboardEvent, type ReactNode, useCallback, useEffect, useRef, useState } from "react";

import {
  type Citation,
  type JobEvent,
  type JobKind,
  type LinkChoice,
  type MeetingDetail,
  type MeetingListItem,
  type PersonItem,
  type SummaryFieldRef,
  type SummaryView,
  useIpc,
} from "../ipc";
import { stepLabel } from "../meeting/liveMeeting";
import type { MessageBarIntent } from "../meeting/types";
import { displayTitle } from "../shell/meetings";
import { type Timer, realTimer } from "../time";
import { KeptLocal } from "./KeptLocal";
import { WhoIsWho } from "./WhoIsWho";
import {
  COPIED_MS,
  failureText,
  handoverLine,
  headerLine,
  namesBanner,
  recordingsGone,
  rememberText,
  rowTime,
  summaryMarkdown,
  voicesToName,
} from "./detail";

const useStyles = makeStyles({
  root: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
    minWidth: 0,
  },
  // Stays in view while the pane scrolls: the buttons and the tabs live here. AppShell's pane
  // covers its own top padding, so this sticks just below it, where it starts.
  pinned: {
    position: "sticky",
    top: 0,
    zIndex: 1,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
    backgroundColor: tokens.colorNeutralBackground1,
    borderBottom: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
  },
  header: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "flex-start",
    gap: tokens.spacingHorizontalS,
    paddingBottom: tokens.spacingVerticalXS,
  },
  headerText: {
    flexGrow: 1,
    flexBasis: "240px",
    minWidth: 0,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
  },
  heading: {
    margin: 0,
    overflowWrap: "anywhere",
  },
  titleLine: {
    display: "flex",
    alignItems: "center",
    flexWrap: "wrap",
    gap: tokens.spacingHorizontalS,
    minWidth: 0,
  },
  // The title and transcript editors (T12.16): the field, then Save and Cancel.
  editor: {
    display: "flex",
    alignItems: "flex-start",
    flexWrap: "wrap",
    gap: tokens.spacingHorizontalS,
    minWidth: 0,
  },
  field: {
    flexGrow: 1,
    minWidth: "200px",
  },
  headerButtons: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "flex-start",
    gap: tokens.spacingHorizontalS,
    whiteSpace: "nowrap",
  },
  // Wide enough for "Copy summary", so the header's buttons stay put when it says "Copied".
  copyButton: {
    minWidth: "124px",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  columns: {
    display: "grid",
    gridTemplateColumns: "minmax(0, 1fr) 240px",
    columnGap: tokens.spacingHorizontalL,
    alignItems: "start",
  },
  main: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
    minWidth: 0,
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
  sectionHeading: {
    margin: 0,
  },
  list: {
    margin: 0,
    paddingLeft: tokens.spacingHorizontalL,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
  actions: {
    display: "grid",
    gridTemplateColumns: "120px minmax(0, 1fr)",
    columnGap: tokens.spacingHorizontalM,
    rowGap: tokens.spacingVerticalXS,
    alignItems: "baseline",
  },
  owner: {
    overflowWrap: "anywhere",
  },
  cite: {
    marginLeft: tokens.spacingHorizontalXS,
    fontSize: tokens.fontSizeBase200,
  },
  citeMissing: {
    marginLeft: tokens.spacingHorizontalXS,
    color: tokens.colorNeutralForegroundDisabled,
  },
  summaryButtons: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalM,
    flexWrap: "wrap",
  },
  transcript: {
    display: "flex",
    flexDirection: "column",
  },
  row: {
    display: "grid",
    gridTemplateColumns: "56px 120px minmax(0, 1fr) auto",
    columnGap: tokens.spacingHorizontalM,
    alignItems: "baseline",
    paddingTop: tokens.spacingVerticalXS,
    paddingBottom: tokens.spacingVerticalXS,
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
    borderRadius: tokens.borderRadiusMedium,
    // The Edit button shows on hover and whenever the row holds focus (T12.16).
    ":hover .earshot-line-edit": { opacity: 1 },
    ":focus-within .earshot-line-edit": { opacity: 1 },
  },
  lineEdit: {
    opacity: 0,
  },
  // A summary text with its Edit buttons at the end (T12.16). Hidden, a button takes no width,
  // so the text and citations read as before; hover or focus shows it.
  editable: {
    ":hover .earshot-inline-edit": {
      opacity: 1,
      maxWidth: "none",
      minWidth: "auto",
      paddingLeft: tokens.spacingHorizontalS,
      paddingRight: tokens.spacingHorizontalS,
    },
    ":focus-within .earshot-inline-edit": {
      opacity: 1,
      maxWidth: "none",
      minWidth: "auto",
      paddingLeft: tokens.spacingHorizontalS,
      paddingRight: tokens.spacingHorizontalS,
    },
  },
  inlineEdit: {
    opacity: 0,
    maxWidth: 0,
    minWidth: 0,
    paddingLeft: 0,
    paddingRight: 0,
    overflow: "hidden",
    whiteSpace: "nowrap",
    marginLeft: tokens.spacingHorizontalXS,
  },
  editedMark: {
    marginLeft: tokens.spacingHorizontalXS,
    color: tokens.colorNeutralForeground3,
  },
  rowCited: {
    backgroundColor: tokens.colorBrandBackground2,
  },
  time: {
    color: tokens.colorNeutralForeground3,
    fontVariantNumeric: "tabular-nums",
  },
  speaker: {
    overflowWrap: "anywhere",
  },
  text: {
    overflowWrap: "anywhere",
  },
});

type Tab = "summary" | "transcript";

export interface Notice {
  key: string;
  intent: MessageBarIntent;
  title?: string;
  text: string;
  busy?: boolean;
}

export interface MeetingDetailViewProps {
  /** The meeting's row in the list (title and state while the detail loads). */
  item: MeetingListItem;
  /** Increments to focus the first voice that needs a name ("Name voices"). */
  nameVoices?: number;
  /** Something the meeting list shows may have changed (title, state). */
  onChanged(): void;
  /** This meeting's job as the app follows it (`AppShell` hears every `JobEvent`, so a job
   * that ends while another meeting is shown is seen on return). */
  job?: JobView;
  /** A job was started from this screen. */
  onJobStarted(job: JobKind): void;
  /** The job's notice was dismissed. */
  onJobDismissed(): void;
  /** Timers (injected by tests). */
  timer?: Timer;
}

/** A meeting's job: which one, and its latest event (`null` until the first). `seq` changes
 * with every event. */
export interface JobView {
  job: JobKind;
  last: JobEvent | null;
  seq: number;
}

/** Whether `e` ends its job. */
export function isTerminal(e: JobEvent | null): boolean {
  return e !== null && (e.type === "Retranscribed" || e.type === "Summary" || e.type === "Failed");
}

/** The notice of a job. */
export function jobNotice(view: JobView): Notice {
  const e = view.last;
  if (e === null) {
    return view.job === "Retranscribe"
      ? { key: "job", intent: "info", text: "Re-transcribing from the recording", busy: true }
      : { key: "job", intent: "info", text: "Writing a new summary", busy: true };
  }
  switch (e.type) {
    case "Progress":
      return { key: "job", intent: "info", text: `Re-transcribing: ${stepLabel(e.step)}`, busy: true };
    case "Warning":
      return { key: "job", intent: "warning", text: e.message, busy: true };
    case "Retranscribed":
      return e.no_audio
        ? {
            key: "job",
            intent: "warning",
            text: "The recordings of this meeting are no longer kept, so it cannot be transcribed again.",
          }
        : e.warnings.length > 0
          ? {
              key: "job",
              intent: "warning",
              text: `The meeting was transcribed again. ${e.warnings.join(" ")}`,
            }
          : { key: "job", intent: "success", text: "The meeting was transcribed again." };
    case "Summary":
      switch (e.result.kind) {
        case "Stored":
          return {
            key: "job",
            intent: "success",
            text: "New summary ready. The previous one is kept until the next re-summary.",
          };
        case "Skipped":
          return { key: "job", intent: "info", title: "No summary", text: e.result.reason };
        case "Failed":
          return { key: "job", intent: "error", title: "Summary failed", text: e.result.message };
        // A job has no final pass to cancel, so `PostCancelled` is only here to be complete.
        case "Cancelled":
        case "PostCancelled":
          return { key: "job", intent: "info", text: "The summary was cancelled." };
      }
      break;
    case "Failed":
      return {
        key: "job",
        intent: "error",
        title: e.job === "Retranscribe" ? "Re-transcription failed" : "Summary failed",
        text: e.message,
      };
  }
  return { key: "job", intent: "info", text: "" };
}

/** The detail of a stored meeting. */
export function MeetingDetailView({
  item,
  nameVoices = 0,
  onChanged,
  job,
  onJobStarted,
  onJobDismissed,
  timer = realTimer,
}: MeetingDetailViewProps) {
  const styles = useStyles();
  const ipc = useIpc();
  const id = item.id;
  const [detail, setDetail] = useState<MeetingDetail | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [persons, setPersons] = useState<PersonItem[]>([]);
  const [tab, setTab] = useState<Tab>("summary");
  const [cited, setCited] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [noAudio, setNoAudio] = useState<Set<string>>(new Set());
  const [editingTitle, setEditingTitle] = useState(false);
  const rows = useRef(new Map<string, HTMLDivElement>());
  const nameField = useRef<HTMLInputElement | null>(null);
  const view = useRef<HTMLElement | null>(null);
  const audio = useRef<HTMLAudioElement | null>(null);
  const audioUrl = useRef<string | null>(null);

  const reload = useCallback(async () => {
    try {
      const [d, p] = await Promise.all([ipc.getMeeting(id), ipc.listPersons()]);
      setDetail(d);
      setPersons(p);
      setLoadError(null);
    } catch (e) {
      setLoadError(failureText(e));
    }
  }, [ipc, id]);

  useEffect(() => {
    void reload();
  }, [reload]);

  useEffect(
    () => () => {
      if (audioUrl.current) URL.revokeObjectURL?.(audioUrl.current);
    },
    [],
  );

  // A job of this meeting ended (now, or while another meeting was shown): read again.
  const ended = job !== undefined && isTerminal(job.last) ? job.seq : null;
  useEffect(() => {
    if (ended !== null) void reload();
  }, [ended, reload]);

  // "Name voices": focus the first name field.
  useEffect(() => {
    if (nameVoices > 0 && detail !== null) nameField.current?.focus();
  }, [nameVoices, detail]);

  // A citation shows its row. Any other tab change opens at the top: the pane's scroll position
  // would otherwise carry over, and the end of a long transcript is past the end of a summary.
  useEffect(() => {
    const row = tab === "transcript" && cited !== null ? rows.current.get(cited) : undefined;
    if (row !== undefined) row.scrollIntoView?.({ block: "center" });
    else view.current?.scrollIntoView?.({ block: "start" });
  }, [tab, cited]);

  const run = async (f: () => Promise<unknown>, failTitle: string) => {
    try {
      await f();
      await reload();
      onChanged();
    } catch (e) {
      setNotice({ key: "edit", intent: "error", title: failTitle, text: failureText(e) });
    }
  };

  /** Saves a typed edit (T12.16); false when it was refused, so the editor stays open. */
  const save = async (f: () => Promise<unknown>, failTitle: string): Promise<boolean> => {
    try {
      await f();
    } catch (e) {
      setNotice({ key: "edit", intent: "error", title: failTitle, text: failureText(e) });
      return false;
    }
    setNotice((n) => (n?.key === "edit" ? null : n));
    await reload();
    onChanged();
    return true;
  };

  const openCitation = (c: Citation) => {
    setCited(c.segment_id);
    setTab("transcript");
  };

  const play = async (speaker: string) => {
    try {
      const wav = await ipc.speakerSample(id, speaker);
      if (wav === null) {
        setNoAudio((s) => new Set(s).add(speaker));
        return;
      }
      const bytes = Uint8Array.from(atob(wav), (ch) => ch.charCodeAt(0));
      if (typeof URL.createObjectURL !== "function" || audio.current === null) return;
      if (audioUrl.current) URL.revokeObjectURL(audioUrl.current);
      audioUrl.current = URL.createObjectURL(new Blob([bytes], { type: "audio/wav" }));
      audio.current.src = audioUrl.current;
      await audio.current.play();
    } catch (e) {
      setNotice({ key: "edit", intent: "error", title: "The sample could not be played", text: failureText(e) });
    }
  };

  const exportNote = async () => {
    try {
      const done = await ipc.exportMeeting(id);
      setNotice({ key: "export", intent: "success", text: `Exported to ${done.note_path}` });
    } catch (e) {
      setNotice({ key: "export", intent: "warning", text: failureText(e) });
    }
  };

  const startJob = async (job: "retranscribe" | "resummarise") => {
    try {
      if (job === "retranscribe") {
        await ipc.retranscribe(id);
        onJobStarted("Retranscribe");
      } else {
        await ipc.resummarise(id);
        onJobStarted("Resummarise");
      }
      setNotice(null);
      onChanged();
    } catch (e) {
      setNotice({ key: "job", intent: "error", text: failureText(e) });
    }
  };

  const title = displayTitle(item);
  const jobBar = job !== undefined ? jobNotice(job) : null;
  const voices = detail ? voicesToName(detail) : 0;
  // A new summary being written replaces the one shown, so there is nothing settled to copy.
  const writingSummary = job !== undefined && job.job === "Resummarise" && !isTerminal(job.last);
  const summaryText = detail?.summary && !writingSummary ? summaryMarkdown(detail.summary) : "";

  return (
    <section ref={view} className={styles.root} aria-label="Meeting detail">
      <div className={styles.pinned}>
        <div className={styles.header}>
          <div className={styles.headerText}>
            {editingTitle && detail !== null ? (
              <TextEditor
                label="Meeting title"
                initial={detail.header.title}
                multiline={false}
                onSave={async (text) => {
                  if (await save(() => ipc.renameMeeting(id, text), "The title was not saved")) {
                    setEditingTitle(false);
                  }
                }}
                onCancel={() => setEditingTitle(false)}
              />
            ) : (
              <div className={styles.titleLine}>
                <Title2 as="h1" className={styles.heading}>
                  {title}
                </Title2>
                {detail !== null && (
                  <Button size="small" appearance="subtle" onClick={() => setEditingTitle(true)}>
                    Rename
                  </Button>
                )}
              </div>
            )}
            {detail !== null && <Caption1 className={styles.muted}>{headerLine(detail)}</Caption1>}
          </div>
          <div className={styles.headerButtons}>
            <Button onClick={() => void exportNote()} disabled={detail === null}>
              Export to Obsidian
            </Button>
            {summaryText !== "" && (
              <CopySummaryButton
                text={summaryText}
                timer={timer}
                onCopied={() => setNotice((n) => (n?.key === "copy" ? null : n))}
                onFailed={() =>
                  setNotice({ key: "copy", intent: "error", text: "The summary could not be copied." })
                }
              />
            )}
            {detail !== null && recordingsGone(detail) ? (
              // Focusable, so a keyboard reaches the reason.
              <CompoundButton disabledFocusable secondaryContent="Recording no longer kept">
                Re-transcribe
              </CompoundButton>
            ) : (
              <Button
                disabled={detail === null}
                onClick={() => void startJob("retranscribe")}
              >
                Re-transcribe
              </Button>
            )}
            <Button disabled={detail === null} onClick={() => void startJob("resummarise")}>
              {detail !== null && detail.summary === null ? "Summarise" : "Re-summarise"}
            </Button>
          </div>
        </div>
        {detail !== null && (
          <TabList selectedValue={tab} onTabSelect={(_, d) => setTab(d.value as Tab)}>
            <Tab value="summary">Summary</Tab>
            <Tab value="transcript">Transcript</Tab>
          </TabList>
        )}
      </div>

      {jobBar !== null && (
        <MessageBar intent={jobBar.intent} layout="multiline">
          <MessageBarBody>
            {jobBar.title !== undefined && <MessageBarTitle>{jobBar.title}</MessageBarTitle>}
            {jobBar.text}
          </MessageBarBody>
          <MessageBarActions>
            {jobBar.busy ? (
              <Spinner size="extra-tiny" aria-label="Working" />
            ) : (
              <Button size="small" appearance="transparent" onClick={onJobDismissed}>
                Dismiss
              </Button>
            )}
          </MessageBarActions>
        </MessageBar>
      )}
      {notice !== null && (
        <MessageBar intent={notice.intent} layout="multiline">
          <MessageBarBody>
            {notice.title !== undefined && <MessageBarTitle>{notice.title}</MessageBarTitle>}
            {notice.text}
          </MessageBarBody>
          <MessageBarActions>
            {notice.busy ? (
              <Spinner size="extra-tiny" aria-label="Working" />
            ) : (
              <Button size="small" appearance="transparent" onClick={() => setNotice(null)}>
                Dismiss
              </Button>
            )}
          </MessageBarActions>
        </MessageBar>
      )}

      {loadError !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>The meeting could not be loaded</MessageBarTitle>
            {loadError}
          </MessageBarBody>
        </MessageBar>
      )}
      {detail === null && loadError === null && <Spinner label="Loading the meeting" />}

      {detail !== null && (
        <>
          <div className={styles.columns}>
            <div className={styles.main}>
              {voices > 0 && (
                <MessageBar intent="info" layout="multiline">
                  <MessageBarBody>{namesBanner(voices)}</MessageBarBody>
                  <MessageBarActions>
                    <Button size="small" onClick={() => nameField.current?.focus()}>
                      Name voices
                    </Button>
                  </MessageBarActions>
                </MessageBar>
              )}
              {tab === "summary" ? (
                <SummaryTab
                  detail={detail}
                  onCite={openCitation}
                  onEdit={
                    writingSummary
                      ? undefined
                      : (summary, field, text) =>
                          save(() => ipc.editSummary(id, summary, field, text), "The summary was not changed")
                  }
                  onResummarise={() => void startJob("resummarise")}
                  onUndo={() => void run(() => ipc.undoSummary(id), "The previous summary could not be restored")}
                  onAllowWriter={() =>
                    void run(() => ipc.allowSummaryWriter(id), "The meeting could not be changed")
                  }
                />
              ) : (
                <TranscriptTab
                  detail={detail}
                  cited={cited}
                  onEdit={(segment, text) =>
                    save(() => ipc.editSegment(id, segment, text), "The line was not saved")
                  }
                  rowRef={(segment, el) => {
                    if (el) rows.current.set(segment, el);
                    else rows.current.delete(segment);
                  }}
                />
              )}
            </div>
            <WhoIsWho
              detail={detail}
              persons={persons}
              noAudio={noAudio}
              onPlay={(s) => void play(s)}
              onRename={(s, name) => run(() => ipc.renameSpeaker(id, s, name), "The name was not saved")}
              onRenamePerson={(person, name) =>
                run(() => ipc.renamePerson(person, name), "The person was not renamed")
              }
              onLink={(s, choice: LinkChoice) =>
                run(() => ipc.linkSpeaker(id, s, choice), "The speaker was not linked")
              }
              onMarkMe={(s) => run(() => ipc.markMe(id, s), "The voice was not marked as you")}
              onUnmarkMe={(s) => run(() => ipc.unmarkMe(id, s), "The voice was not changed")}
              onMerge={(keep, merge) =>
                run(() => ipc.mergeSpeakers(id, keep, merge), "The speakers were not merged")
              }
              onForget={(person) =>
                run(async () => {
                  await ipc.forgetPersonVoices(person);
                  setNotice({ key: "edit", intent: "success", text: "The voice was forgotten." });
                }, "The voice was not forgotten")
              }
              onRemember={(speakers) =>
                run(async () => {
                  const results = await ipc.rememberVoices(id, speakers);
                  const saved = results.filter((r) => r.outcome === "Remembered").length;
                  setNotice({
                    key: "edit",
                    intent: saved === results.length ? "success" : "info",
                    text: rememberText(results, detail.speakers),
                  });
                }, "The voices were not saved")
              }
              firstNameField={(el) => {
                nameField.current = el;
              }}
            />
          </div>
        </>
      )}

      {/* The play-sample audio (blob: URL, CSP media-src blob:). */}
      <audio ref={audio} hidden />
    </section>
  );
}

/** Copies `text` to the clipboard. The webview allows this without a plugin: the page is a
 * secure context and writing needs no permission. */
function CopySummaryButton({
  text,
  timer,
  onCopied,
  onFailed,
}: {
  text: string;
  timer: Timer;
  onCopied(): void;
  onFailed(): void;
}) {
  const styles = useStyles();
  const [copied, setCopied] = useState(false);
  const cancelReset = useRef<(() => void) | null>(null);

  useEffect(() => () => cancelReset.current?.(), []);

  const copy = async () => {
    cancelReset.current?.();
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      setCopied(false);
      onFailed();
      return;
    }
    setCopied(true);
    onCopied();
    cancelReset.current = timer.setTimeout(() => setCopied(false), COPIED_MS);
  };

  return (
    <Button className={styles.copyButton} onClick={() => void copy()}>
      <span aria-live="polite">{copied ? "Copied" : "Copy summary"}</span>
    </Button>
  );
}

function Citations({ citations, onCite }: { citations: Citation[]; onCite(c: Citation): void }) {
  const styles = useStyles();
  return (
    <>
      {citations.map((c) =>
        c.row === null ? (
          <Caption1 key={c.segment_id} className={styles.citeMissing} title="This line is no longer in the transcript">
            [unavailable]
          </Caption1>
        ) : (
          <Link
            key={c.segment_id}
            className={styles.cite}
            aria-label={`Show line ${c.row + 1} in the transcript`}
            onClick={() => onCite(c)}
          >
            [{c.row + 1}]
          </Link>
        ),
      )}
    </>
  );
}

/** One text of a summary the user may correct (T12.16). */
interface EditField {
  /** Names the field: "Edit {label}" and the editor's label. */
  label: string;
  /** The button's own text. */
  button: string;
  text: string;
  onSave(text: string): Promise<boolean>;
}

/** Summary content with Edit buttons at its end, shown on hover and focus; a field being
 * edited replaces the content with its editor. With no fields it is the content alone. */
function Editable({ fields, children }: { fields: EditField[]; children: ReactNode }) {
  const styles = useStyles();
  const [editing, setEditing] = useState<EditField | null>(null);
  if (editing !== null) {
    return (
      <TextEditor
        label={editing.label}
        initial={editing.text}
        multiline
        onSave={async (t) => {
          if (await editing.onSave(t)) setEditing(null);
        }}
        onCancel={() => setEditing(null)}
      />
    );
  }
  return (
    <span className={styles.editable}>
      {children}
      {fields.map((f) => (
        <Button
          key={f.label}
          size="small"
          appearance="subtle"
          className={mergeClasses(styles.inlineEdit, "earshot-inline-edit")}
          aria-label={`Edit ${f.label}`}
          onClick={() => setEditing(f)}
        >
          {f.button}
        </Button>
      ))}
    </span>
  );
}

function SummaryBody({
  summary,
  onCite,
  onEdit,
}: {
  summary: SummaryView;
  onCite(c: Citation): void;
  /** Saves one text; false when it was refused. Absent: read only. */
  onEdit?: (field: SummaryFieldRef, text: string) => Promise<boolean>;
}) {
  const styles = useStyles();
  /** The field as an Edit button, or none when read only. */
  const edit = (field: SummaryFieldRef, label: string, text: string, button = "Edit"): EditField[] =>
    onEdit === undefined ? [] : [{ label, button, text, onSave: (t) => onEdit(field, t) }];
  return (
    <>
      <div className={styles.section}>
        <Subtitle2 as="h2" className={styles.sectionHeading}>
          TL;DR
        </Subtitle2>
        <Body1>
          <Editable fields={edit({ kind: "Tldr" }, "TL;DR", summary.tldr)}>{summary.tldr}</Editable>
        </Body1>
      </div>
      {summary.decisions.length > 0 && (
        <div className={styles.section}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            Decisions
          </Subtitle2>
          <ul className={styles.list} aria-label="Decisions">
            {summary.decisions.map((d, i) => (
              <li key={i}>
                <Editable fields={edit({ kind: "Decision", index: i }, `decision ${i + 1}`, d.text)}>
                  <Body1>{d.text}</Body1>
                  <Citations citations={d.citations} onCite={onCite} />
                </Editable>
              </li>
            ))}
          </ul>
        </div>
      )}
      {summary.actions.length > 0 && (
        <div className={styles.section}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            Actions
          </Subtitle2>
          <div role="table" aria-label="Actions" className={styles.actions}>
            {summary.actions.map((a, i) => (
              <div role="row" key={i} style={{ display: "contents" }}>
                <Body1Strong role="cell" className={styles.owner}>
                  <Editable
                    fields={edit({ kind: "ActionOwner", index: i }, `who does action ${i + 1}`, a.owner)}
                  >
                    {a.owner}
                  </Editable>
                </Body1Strong>
                <span role="cell">
                  <Editable
                    fields={[
                      ...edit({ kind: "ActionTask", index: i }, `action ${i + 1}`, a.task),
                      ...(a.due === null
                        ? []
                        : edit({ kind: "ActionDue", index: i }, `when action ${i + 1} is due`, a.due, "Edit date")),
                    ]}
                  >
                    <Body1>
                      {a.task}
                      {a.due !== null && ` (by ${a.due})`}
                    </Body1>
                    <Citations citations={a.citations} onCite={onCite} />
                  </Editable>
                </span>
              </div>
            ))}
          </div>
          <Caption1 className={styles.muted}>Tick actions in the Obsidian note.</Caption1>
        </div>
      )}
      {summary.notes.map((n, i) => (
        <div className={styles.section} key={`notes-${i}`}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            <Editable fields={edit({ kind: "NoteHeading", section: i }, `heading ${n.heading}`, n.heading)}>
              {n.heading}
            </Editable>
          </Subtitle2>
          <ul className={styles.list}>
            {n.bullets.map((b, j) => (
              <li key={j}>
                <Editable
                  fields={edit(
                    { kind: "NoteBullet", section: i, bullet: j },
                    `note ${j + 1} under ${n.heading}`,
                    b.text,
                  )}
                >
                  <Body1>{b.text}</Body1>
                  <Citations citations={b.citations} onCite={onCite} />
                </Editable>
              </li>
            ))}
          </ul>
        </div>
      ))}
      {summary.open_questions.length > 0 && (
        <div className={styles.section}>
          <Subtitle2 as="h2" className={styles.sectionHeading}>
            Open questions
          </Subtitle2>
          <ul className={styles.list}>
            {summary.open_questions.map((q, i) => (
              <li key={i}>
                <Editable fields={edit({ kind: "OpenQuestion", index: i }, `open question ${i + 1}`, q)}>
                  <Body1>{q}</Body1>
                </Editable>
              </li>
            ))}
          </ul>
        </div>
      )}
    </>
  );
}

function SummaryTab({
  detail,
  onCite,
  onEdit,
  onResummarise,
  onUndo,
  onAllowWriter,
}: {
  detail: MeetingDetail;
  onCite(c: Citation): void;
  /** Saves one text of summary `summary`; false when refused. Absent while a new summary is
   * being written. */
  onEdit?: (summary: string, field: SummaryFieldRef, text: string) => Promise<boolean>;
  onResummarise(): void;
  onUndo(): void;
  onAllowWriter(): void;
}) {
  const styles = useStyles();
  const s = detail.summary;
  return (
    <>
      {detail.summary_stale && (
        <MessageBar intent="warning" layout="multiline">
          <MessageBarBody>
            This summary refers to lines of an earlier transcript. Re-summarise to match the
            current one.
          </MessageBarBody>
          <MessageBarActions>
            <Button size="small" onClick={onResummarise}>
              Re-summarise
            </Button>
          </MessageBarActions>
        </MessageBar>
      )}
      <KeptLocal keptLocal={detail.kept_local} onAllow={onAllowWriter} />
      {handoverLine(detail) !== null && <Caption1 className={styles.muted}>{handoverLine(detail)}</Caption1>}
      {s === null ? (
        <Body1 className={styles.muted}>
          {detail.state === "Processing"
            ? "The summary is being written."
            : detail.state === "Waiting"
              ? "The summary will be written when the previous meeting has finished."
              : "This meeting has no summary yet."}
        </Body1>
      ) : (
        <SummaryBody
          summary={s}
          onCite={onCite}
          onEdit={onEdit === undefined ? undefined : (field, text) => onEdit(s.id, field, text)}
        />
      )}
      {detail.can_undo_summary && (
        <div className={styles.summaryButtons}>
          <Link onClick={onUndo}>Switch to previous summary</Link>
        </div>
      )}
    </>
  );
}

/** A typed text with Save and Cancel (T12.16): Enter saves, Escape cancels. A title or a
 * transcript line holds no line break, so Enter never adds one. */
function TextEditor({
  label,
  initial,
  multiline,
  onSave,
  onCancel,
}: {
  label: string;
  initial: string;
  multiline: boolean;
  onSave(text: string): Promise<void>;
  onCancel(): void;
}) {
  const styles = useStyles();
  const [text, setText] = useState(initial);
  const [saving, setSaving] = useState(false);
  const submit = async () => {
    if (saving) return;
    setSaving(true);
    try {
      await onSave(text);
    } finally {
      setSaving(false);
    }
  };
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void submit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onCancel();
    }
  };
  return (
    <div className={styles.editor}>
      {multiline ? (
        <Textarea
          className={styles.field}
          aria-label={label}
          value={text}
          resize="vertical"
          autoFocus
          onChange={(_, d) => setText(d.value)}
          onKeyDown={onKeyDown}
        />
      ) : (
        <Input
          className={styles.field}
          aria-label={label}
          value={text}
          autoFocus
          onChange={(_, d) => setText(d.value)}
          onKeyDown={onKeyDown}
        />
      )}
      <Button appearance="primary" disabled={saving} onClick={() => void submit()}>
        Save
      </Button>
      <Button disabled={saving} onClick={onCancel}>
        Cancel
      </Button>
    </div>
  );
}

function TranscriptTab({
  detail,
  cited,
  onEdit,
  rowRef,
}: {
  detail: MeetingDetail;
  cited: string | null;
  /** Saves a line's new text; false when it was refused. */
  onEdit(segment: string, text: string): Promise<boolean>;
  rowRef(segment: string, el: HTMLDivElement | null): void;
}) {
  const styles = useStyles();
  const [editing, setEditing] = useState<string | null>(null);
  // Only the final transcript can be corrected: the final pass replaces a draft.
  const canEdit = detail.header.transcript === "Final";
  if (detail.transcript.length === 0) {
    return <Body1 className={styles.muted}>No speech was transcribed in this meeting.</Body1>;
  }
  return (
    <div role="table" aria-label="Transcript" className={styles.transcript}>
      {detail.transcript.map((r, i) => (
        <div
          role="row"
          key={r.segment_id}
          ref={(el) => rowRef(r.segment_id, el)}
          aria-current={r.segment_id === cited ? "true" : undefined}
          className={mergeClasses(styles.row, r.segment_id === cited && styles.rowCited)}
        >
          <Caption1 role="cell" className={styles.time}>
            {rowTime(r.start_ms)}
          </Caption1>
          <Body1Strong role="cell" className={styles.speaker}>
            {r.speaker}
          </Body1Strong>
          {editing === r.segment_id ? (
            <div role="cell" className={styles.text}>
              <TextEditor
                label={`Line ${i + 1}`}
                initial={r.text}
                multiline
                onSave={async (text) => {
                  if (await onEdit(r.segment_id, text)) setEditing(null);
                }}
                onCancel={() => setEditing(null)}
              />
            </div>
          ) : (
            <Body1 role="cell" className={styles.text}>
              {r.text}
              {r.edited && <Caption1 className={styles.editedMark}>(edited)</Caption1>}
            </Body1>
          )}
          {/* Not a cell: a row's cells are its time, speaker and words. */}
          <span>
            {canEdit && editing !== r.segment_id && (
              <Button
                size="small"
                appearance="subtle"
                className={mergeClasses(styles.lineEdit, "earshot-line-edit")}
                aria-label={`Edit line ${i + 1}`}
                onClick={() => setEditing(r.segment_id)}
              >
                Edit
              </Button>
            )}
          </span>
        </div>
      ))}
    </div>
  );
}
