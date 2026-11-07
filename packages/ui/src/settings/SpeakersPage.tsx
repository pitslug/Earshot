// Settings, Speakers (docs/ui-design.md "Settings", PLAN T7.4h): when speaker detection runs
// (the T6.13d4 setting, used from the next meeting) and your voice: record a short sample and
// save it as your voiceprint (the page asks first: voiceprints are opt-in), or forget it. Also the
// people Earshot knows from your meetings, to rename or to forget their voices (T9.1mc).

import {
  Body1,
  Body1Strong,
  Button,
  Caption1,
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  DialogTitle,
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
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useEffect, useId, useState } from "react";

import { type PersonItem, type SpeakerTimingChoice, type VoiceView, useIpc } from "../ipc";
import { type SpeakersPageState, sampleRunning, useSpeakers } from "./useSpeakers";

const useStyles = makeStyles({
  page: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
  },
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  heading: {
    margin: 0,
  },
  choices: {
    display: "grid",
    gap: tokens.spacingVerticalS,
  },
  card: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  selected: {
    border: `${tokens.strokeWidthThin} solid ${tokens.colorBrandStroke1}`,
    boxShadow: `inset 0 0 0 ${tokens.strokeWidthThin} ${tokens.colorBrandStroke1}`,
  },
  why: {
    color: tokens.colorNeutralForeground2,
    paddingLeft: "32px",
    maxWidth: "62ch",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  text: {
    maxWidth: "70ch",
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  recording: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
    padding: tokens.spacingHorizontalM,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground2,
    maxWidth: "70ch",
  },
  person: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    justifyContent: "space-between",
    gap: tokens.spacingHorizontalS,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  personName: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    minWidth: 0,
    overflowWrap: "anywhere",
  },
  passage: {
    margin: 0,
    fontSize: tokens.fontSizeBase400,
    lineHeight: tokens.lineHeightBase400,
  },
});

/** What the page says a voiceprint is, before anything is recorded. */
/** What the name is for. */
export const NAME_EXPLAINED =
  "Summaries and exports use this name for you, in every meeting. Summaries already written keep their wording until you summarise them again.";

export const VOICE_EXPLAINED =
  "Earshot can name you \"Me\" in meetings by recognising your voice. To do that it saves a voiceprint: a short list of numbers that describes how your voice sounds, made from a 30 second recording of you reading aloud. The recording itself is never saved. The voiceprint stays on this computer and never leaves it.";

/** The text to read aloud while recording (about 30 seconds at an easy pace). */
export const READ_ALOUD =
  "Good morning, everyone. Thanks for joining. Today I would like to go through three things: where the project stands, what we learned from the last test, and who will take the next steps. First, the schedule. We are about a week behind, mostly because the parts arrived late. Second, the test went well overall, but the battery ran down faster than we hoped. Third, I will write up the notes this afternoon and send them round, and we can meet again on Thursday to agree the plan.";

/** What the dialog says before recording (the opt-in question). */
export const RECORD_CONSENT =
  "Earshot will record 30 seconds from your microphone while you read a short text aloud, then save a voiceprint of your voice on this computer. The recording is not kept. You can forget the voiceprint here at any time.";

/** What the dialog says before forgetting. */
export const FORGET_EXPLAINED =
  "Earshot deletes your voiceprint from this computer and stops recognising you from your voice. Names already given in past meetings stay as they are.";

/** The page's line for "a change applies from the next meeting". */
export const NEXT_MEETING = "A change applies from the next meeting.";

function Section({ title, children }: { title: string; children: ReactNode }) {
  const styles = useStyles();
  const id = useId();
  return (
    <section className={styles.section} aria-labelledby={id}>
      <Subtitle2 as="h2" id={id} className={styles.heading}>
        {title}
      </Subtitle2>
      {children}
    </section>
  );
}

function secondsOf(ms: number): number {
  return Math.floor(ms / 1000);
}

/** The voice status line. */
export function voiceStatus(voice: VoiceView): string {
  return voice.saved
    ? `Your voice is saved, from ${secondsOf(voice.speech_ms)} seconds of speech.`
    : "No voice is saved.";
}

/** "Your name": saved when the field is left or Enter is pressed, if it changed. */
function MyNameField({ state, name }: { state: SpeakersPageState; name: string }) {
  const [value, setValue] = useState(name);
  useEffect(() => setValue(name), [name]);
  const save = () => {
    if (value.trim() !== name) void state.setName(value);
  };
  return (
    <Field label="Your name" hint={NAME_EXPLAINED}>
      <Input
        value={value}
        onChange={(_, d) => setValue(d.value)}
        onBlur={save}
        onKeyDown={(e) => {
          if (e.key === "Enter") save();
        }}
      />
    </Field>
  );
}

/** "Your voice": your name, the status, the sample and forgetting. */
export function VoiceSection({ state, voice }: { state: SpeakersPageState; voice: VoiceView }) {
  const styles = useStyles();
  const [asking, setAsking] = useState(false);
  const [forgetting, setForgetting] = useState(false);
  const { sample } = state;
  const running = sampleRunning(sample);

  return (
    <Section title="Your voice">
      <MyNameField state={state} name={voice.name} />
      <Body1 className={styles.text}>{VOICE_EXPLAINED}</Body1>
      <Body1Strong>{voiceStatus(voice)}</Body1Strong>

      {sample.status === "starting" && <Spinner size="small" label="Opening the microphone" labelPosition="after" />}
      {sample.status === "recording" && (
        <div className={styles.recording} role="group" aria-label="Recording your voice">
          <Body1Strong>Read this aloud at your normal pace:</Body1Strong>
          <p className={styles.passage}>{READ_ALOUD}</p>
          <ProgressBar
            value={sample.recordedMs}
            max={sample.totalMs}
            thickness="large"
            aria-label="Recorded so far"
          />
          <div className={styles.row}>
            <Caption1>
              Recording: {secondsOf(sample.recordedMs)} of {secondsOf(sample.totalMs)} seconds
            </Caption1>
            <Button onClick={() => void state.cancel()}>Cancel</Button>
          </div>
        </div>
      )}
      {sample.status === "saving" && <Spinner size="small" label="Saving your voice" labelPosition="after" />}
      {sample.status === "done" && (
        <MessageBar intent={sample.result === "Saved" ? "success" : "warning"}>
          <MessageBarBody>{sample.message}</MessageBarBody>
        </MessageBar>
      )}
      {sample.status === "cancelled" && (
        <MessageBar intent="info">
          <MessageBarBody>The recording was cancelled. Nothing was saved.</MessageBarBody>
        </MessageBar>
      )}
      {sample.status === "failed" && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>Your voice was not saved</MessageBarTitle>
            {sample.message}
          </MessageBarBody>
        </MessageBar>
      )}
      {state.forgotten !== null && (
        <MessageBar intent="success">
          <MessageBarBody>{state.forgotten}</MessageBarBody>
        </MessageBar>
      )}

      {!running && (
        <div className={styles.row}>
          <Button appearance={voice.saved ? "secondary" : "primary"} onClick={() => setAsking(true)}>
            {voice.saved ? "Add another sample" : "Record my voice"}
          </Button>
          {voice.saved && (
            <Button disabled={state.busy} onClick={() => setForgetting(true)}>
              Forget my voice
            </Button>
          )}
        </div>
      )}
      {!running && voice.saved && (
        <Caption1 className={styles.muted}>Another sample adds to the saved voiceprint and makes it more reliable.</Caption1>
      )}

      <Dialog modalType="alert" open={asking} onOpenChange={(_, d) => setAsking(d.open)}>
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Save a voiceprint of your voice?</DialogTitle>
            <DialogContent>{RECORD_CONSENT}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  setAsking(false);
                  void state.record();
                }}
              >
                Record and save
              </Button>
              <Button onClick={() => setAsking(false)}>Cancel</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
      <Dialog modalType="alert" open={forgetting} onOpenChange={(_, d) => setForgetting(d.open)}>
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Forget your voice?</DialogTitle>
            <DialogContent>{FORGET_EXPLAINED}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  setForgetting(false);
                  void state.forget();
                }}
              >
                Forget my voice
              </Button>
              <Button onClick={() => setForgetting(false)}>Cancel</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </Section>
  );
}

/** What the people section says it is. */
export const PEOPLE_EXPLAINED =
  "Everyone you have named in a meeting. They stay here, with any saved voice, after their meetings are deleted, until you remove them. A rename changes their name in every meeting. Summaries already written keep their wording until you summarise them again.";

/** What the page says before it forgets someone's voice. */
export const FORGET_PERSON_EXPLAINED =
  "Earshot deletes this person's voiceprint from this computer and stops recognising them from their voice. Their name stays in your meetings.";

/** What the page says before it removes someone. */
export const REMOVE_PERSON_EXPLAINED =
  "Earshot deletes this name and any saved voice from this computer. In your meetings their lines go back to \"Speaker 1\" and so on, ready to be named again. Summaries, notes and exported files that already mention them do not change.";

/** Everyone Earshot knows from meetings, other than you (the section above is yours). */
function PeopleSection() {
  const styles = useStyles();
  const ipc = useIpc();
  const [people, setPeople] = useState<PersonItem[] | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<{ id: string; name: string } | null>(null);
  const [forgetting, setForgetting] = useState<PersonItem | null>(null);
  const [removing, setRemoving] = useState<PersonItem | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    ipc
      .listPersons()
      .then((all) => {
        if (live) setPeople(all.filter((p) => !p.is_me));
      })
      .catch(() => {
        if (live) setProblem("The people Earshot knows could not be read.");
      });
    return () => {
      live = false;
    };
  }, [ipc]);

  const run = async (work: () => Promise<unknown>, failed: string) => {
    setBusy(true);
    setProblem(null);
    try {
      await work();
      const all = await ipc.listPersons();
      setPeople(all.filter((p) => !p.is_me));
      return true;
    } catch {
      setProblem(failed);
      return false;
    } finally {
      setBusy(false);
    }
  };

  const saveName = async (id: string, name: string) => {
    const saved = await run(() => ipc.renamePerson(id, name), "The name was not saved. Try another name.");
    if (saved) setRenaming(null);
  };

  return (
    <Section title="People">
      <Body1 className={styles.text}>{PEOPLE_EXPLAINED}</Body1>
      {problem !== null && (
        <MessageBar intent="error">
          <MessageBarBody>{problem}</MessageBarBody>
        </MessageBar>
      )}
      {people === null && problem === null && <Spinner size="tiny" label="Reading the people" />}
      {people !== null && people.length === 0 && (
        <Caption1 className={styles.muted}>Nobody yet. People appear here when you name a speaker in a meeting.</Caption1>
      )}
      {people !== null &&
        people.map((p) =>
          renaming?.id === p.id ? (
            <form
              key={p.id}
              className={styles.person}
              onSubmit={(e) => {
                e.preventDefault();
                void saveName(p.id, renaming.name);
              }}
            >
              <Field label={`New name for ${p.name}`}>
                <Input
                  autoFocus
                  value={renaming.name}
                  onChange={(_, d) => setRenaming({ id: p.id, name: d.value })}
                />
              </Field>
              <div className={styles.row}>
                <Button appearance="primary" type="submit" disabled={busy || renaming.name.trim() === ""}>
                  Save
                </Button>
                <Button onClick={() => setRenaming(null)}>Cancel</Button>
              </div>
            </form>
          ) : (
            <div key={p.id} className={styles.person} data-person={p.id}>
              <div className={styles.personName}>
                <Body1Strong>{p.name}</Body1Strong>
                <Caption1 className={styles.muted}>{p.has_voiceprint ? "Voice saved" : "No saved voice"}</Caption1>
              </div>
              <div className={styles.row}>
                <Button
                  aria-label={`Rename ${p.name}`}
                  disabled={busy}
                  onClick={() => setRenaming({ id: p.id, name: p.name })}
                >
                  Rename
                </Button>
                {p.has_voiceprint && (
                  <Button aria-label={`Forget the voice of ${p.name}`} disabled={busy} onClick={() => setForgetting(p)}>
                    Forget voice
                  </Button>
                )}
                <Button aria-label={`Remove ${p.name}`} disabled={busy} onClick={() => setRemoving(p)}>
                  Remove
                </Button>
              </div>
            </div>
          ),
        )}
      <Dialog modalType="alert" open={forgetting !== null} onOpenChange={(_, d) => !d.open && setForgetting(null)}>
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Forget the voice of {forgetting?.name}?</DialogTitle>
            <DialogContent>{FORGET_PERSON_EXPLAINED}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  const person = forgetting;
                  setForgetting(null);
                  if (person !== null) {
                    void run(() => ipc.forgetPersonVoices(person.id), "The voice was not forgotten.");
                  }
                }}
              >
                Forget voice
              </Button>
              <Button onClick={() => setForgetting(null)}>Cancel</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
      <Dialog modalType="alert" open={removing !== null} onOpenChange={(_, d) => !d.open && setRemoving(null)}>
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Remove {removing?.name}?</DialogTitle>
            <DialogContent>{REMOVE_PERSON_EXPLAINED}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  const person = removing;
                  setRemoving(null);
                  if (person !== null) {
                    void run(() => ipc.removePerson(person.id), "This person was not removed.");
                  }
                }}
              >
                Remove
              </Button>
              <Button onClick={() => setRemoving(null)}>Cancel</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </Section>
  );
}

/** What shows until the settings are read: the spinner or the failure. */
export function SpeakersNotLoaded({ load }: { load: Exclude<SpeakersPageState["load"], { status: "loaded" }> }) {
  if (load.status === "loading") return <Spinner label="Reading the speaker settings" />;
  return (
    <MessageBar intent="error">
      <MessageBarBody>
        <MessageBarTitle>The speaker settings could not be read</MessageBarTitle>
        {load.message}
      </MessageBarBody>
    </MessageBar>
  );
}

/** A refusal or failure, until dismissed. */
export function SpeakersNoticeBar({ state }: { state: SpeakersPageState }) {
  if (state.notice === null) return null;
  return (
    <MessageBar intent="error">
      <MessageBarBody>
        <MessageBarTitle>{state.notice.title}</MessageBarTitle>
        {state.notice.message}
      </MessageBarBody>
      <MessageBarActions
        containerAction={
          <Button size="small" appearance="transparent" onClick={state.dismiss}>
            Dismiss
          </Button>
        }
      />
    </MessageBar>
  );
}

/** The Speakers page of Settings. */
export function SpeakersPage() {
  const styles = useStyles();
  const state = useSpeakers();
  const timingLabel = useId();
  const { load } = state;

  if (load.status !== "loaded") return <SpeakersNotLoaded load={load} />;
  const { view } = load;

  return (
    <div className={styles.page}>
      <Title2 as="h1" className={styles.heading}>
        Speakers
      </Title2>
      <SpeakersNoticeBar state={state} />
      {view.unknown.length > 0 && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>A stored setting is not known to this version</MessageBarTitle>
            {view.unknown.join(" ")}
          </MessageBarBody>
        </MessageBar>
      )}

      <Section title="When speakers are found">
        <span id={timingLabel} hidden>
          When speakers are found
        </span>
        <RadioGroup
          aria-labelledby={timingLabel}
          value={view.timing}
          disabled={state.busy}
          onChange={(_, d) => void state.setTiming(d.value as SpeakerTimingChoice)}
          className={styles.choices}
        >
          {view.options.map((o) => (
            <div
              key={o.value}
              className={mergeClasses(styles.card, o.value === view.timing && styles.selected)}
              data-timing={o.value}
            >
              <Radio value={o.value} label={<Body1Strong>{o.label}</Body1Strong>} />
              <Caption1 className={styles.why}>{o.description}</Caption1>
            </div>
          ))}
        </RadioGroup>
        <Caption1 className={styles.muted}>{NEXT_MEETING}</Caption1>
      </Section>

      <VoiceSection state={state} voice={view.voice} />
      <PeopleSection />
    </div>
  );
}
