// The first-run wizard (D21, docs/settings-design.md "First run", T12.5a, T12.79): five short
// steps with a progress bar. It is a view of `AppShell`, so the shell's tabster hold, the
// meeting events and the quit dialog stay as they are. Every step is the Settings page's own parts and hooks;
// the wizard holds only the order, its own short words, what the Download step adds up, and the
// "done" call. Next saves what the step's parts registered (`saveOnNext`), so no part shows a Save
// button here. Nothing downloads, records, plays a sound or runs the model check or the audio
// check until the user presses its button, and the Sync step reads nothing until a way in is
// chosen and sends nothing until its button or Next (Not now, the default, asks the app nothing).
// The one thing that runs by itself: when Claude Code or Codex is ticked on the Summaries step,
// its card asks that agent CLI whether it is installed and signed in (`checkClaudeCode` /
// `checkCodex`), which sends no meeting content.
// The wizard holds its own `useModels`; `SettingsView` holds the other, and `AppShell` never
// shows both, so only one subscription to model job events is live at a time.

import {
  Body1,
  Body1Strong,
  Button,
  Caption1,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  ProgressBar,
  Radio,
  RadioGroup,
  Title2,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";

import { type ModelItem, errorText, useIpc } from "../ipc";
import { AudioCheckSection } from "../settings/AudioCheckSection";
import { LanguagesPicker, Notices, Recommendation } from "../settings/ModelsParts";
import { ModelChoices, ModelsNotLoaded } from "../settings/ModelSections";
import { ObsidianSection } from "../settings/ObsidianSection";
import {
  MyNameField,
  SpeakersNoticeBar,
  SpeakersNotLoaded,
  VOICE_NOTICES,
  VoiceSection,
} from "../settings/PeoplePage";
import { SummaryOrderChoices } from "../settings/SummariesPage";
import { JoinWithCodeForm, SYNC_INTRO, SetUpFirstForm } from "../settings/SyncPage";
import { formatSize } from "../settings/copy";
import { NotLoaded, UnknownSettings, usePageStyles } from "../settings/parts";
import { type SaveOnNext, type SaveStep, saveStep, useSaveOnNext } from "../settings/saveOnNext";
import { checkRunning, useAudioCheck } from "../settings/useAudioCheck";
import { type ModelsData, type ModelsLoad, type ModelsPageState, useModels } from "../settings/useModels";
import { sampleRunning, useSpeakers } from "../settings/useSpeakers";
import { useSync } from "../settings/useSync";
import { realTimer } from "../time";

/** The steps, in order, with what each one says first. */
export const STEPS = [
  {
    title: "Summaries",
    intro: "Choose who writes the summary after each meeting, in the order Earshot asks them.",
  },
  {
    title: "Sync",
    intro: "Do you use Earshot on another computer?",
  },
  {
    title: "Download",
    intro: "Earshot does its work on this computer, so it needs a few downloads. Nothing downloads until you press the button.",
  },
  {
    title: "About me",
    intro: "Summaries use your name, and a sample of your voice lets Earshot recognise you in meetings. Nothing records until you press the button.",
  },
  {
    title: "Optional extras",
    intro: "Both are optional, and both are in Settings if you want them later.",
  },
] as const;

/** A step's place in `STEPS`. */
function stepAt(title: (typeof STEPS)[number]["title"]): number {
  return STEPS.findIndex((s) => s.title === title);
}

const SUMMARIES_STEP = stepAt("Summaries");
const SYNC_STEP = stepAt("Sync");
const DOWNLOAD_STEP = stepAt("Download");
const ABOUT_ME_STEP = stepAt("About me");
const EXTRAS_STEP = stepAt("Optional extras");

/** The Sync step's question. */
export const SYNC_QUESTION = STEPS[SYNC_STEP].intro;

/** The Sync step's three answers. */
export const SYNC_JOIN = "Join with a pairing code";
export const SYNC_SET_UP = "Set up sync for the first time";
export const SYNC_LATER = "Not now";

/** What Not now means: only what the choice does, since the step does not read whether this
 * computer already syncs. */
export const SYNC_LATER_NOTE = "Not now sets nothing up. You can set up sync any time in Settings, Sync.";

/** Why Next waits on Join or Set up: the sync settings are not read yet. */
export const SYNC_STILL_READING = "Still reading this computer's sync settings.";

/** The Sync step once this computer syncs. */
export const SYNC_JOINED = "This computer syncs. You can change sync any time in Settings, Sync.";

/** Said after the wizard when it ended with no transcription model. */
export const NO_MODEL_NOTE = "Meetings need a transcription model. You can download one any time in Settings, Transcription.";

/** Why the voice sample waits: the speech helpers are on their way. */
export const HELPERS_ARRIVING = "The voice sample needs the speech helpers. They are still downloading, and this button turns on when they arrive.";

/** Why the voice sample waits: nothing is bringing the speech helpers. */
export const HELPERS_MISSING = "The voice sample needs the speech helpers. Go back to Download to get them, or skip this for now.";

/** The Download step when there is nothing to fetch. */
export const NOTHING_TO_DOWNLOAD = "Everything Earshot needs for this computer is already downloaded.";

/** The Download step when no recommended model covers the languages. */
export const NO_MODEL_FOR_LANGUAGES = "No transcription model covers all your meeting languages. Change the languages, or choose different models.";

/** What the Download step says about going on. */
export const DOWNLOADS_CARRY_ON = "It carries on while you go through the other steps, and after you finish.";

/** What Obsidian's row says before it opens. */
export const OBSIDIAN_ROW = "Choose the vault your meeting notes go to when you export them.";

/** What the audio check's row says before it opens. */
export const AUDIO_CHECK_ROW = "Play a few short sounds to see how your microphone and speakers work together.";

/** Where the devices are. */
export const DEVICES_NOTE = "The check uses the devices in Settings, Audio: the Windows defaults unless you chose others.";

/** Why Next stayed on the step. */
export const SAVE_REFUSED_NOTE =
  "Something on this step was not saved, so you are still on it. The reason is above.";

/** Why the buttons wait during the check or the voice sample. */
export const RUNNING_NOTE = "Wait for this to finish, or cancel it, before you move on.";

/** The line while models download on other steps. */
export function stillDownloading(titles: string[]): string {
  const names = titles.join(", ");
  return `Still downloading: ${names}. ${DOWNLOADS_CARRY_ON}`;
}

/** "A, B and C". */
function namesOf(items: ModelItem[]): string {
  const titles = items.map((m) => m.title);
  if (titles.length < 2) return titles.join("");
  return `${titles.slice(0, -1).join(", ")} and ${titles[titles.length - 1]}`;
}

/** What this computer still needs: the speech helpers, the transcription model (the one chosen,
 * else the recommended one) and, when this computer is one of the summary writers, the summary
 * model (the same way). `localWriter`: whether this computer is in the order of writers (T12.79:
 * first or a later step); unknown (null) goes by whether it writes first. */
export function neededDownloads({ models, settings }: ModelsData, localWriter: boolean | null = null): ModelItem[] {
  const transcription = settings.value.transcription ?? models.recommended?.model;
  const local = localWriter ?? models.in_use.summaries_here;
  const summary = local ? (settings.value.summary ?? models.summary_recommended.model) : null;
  return [
    models.helpers,
    ...models.transcription.filter((m) => m.name === transcription),
    ...models.summary.filter((m) => m.name === summary),
  ].filter((m) => m.installed !== "Yes" || m.downloading);
}

/** Why the voice sample cannot run yet, or null. A list that cannot be read holds nothing back:
 * the app then says why itself. */
export function voiceSampleBlocked(load: ModelsLoad): string | null {
  if (load.status !== "loaded") return null;
  const { helpers } = load.data.models;
  if (helpers.installed === "Yes") return null;
  return helpers.downloading ? HELPERS_ARRIVING : HELPERS_MISSING;
}

/** Whether the app knows there is no transcription model and none on its way. */
function noTranscriptionModel(load: ModelsLoad): boolean {
  return (
    load.status === "loaded" && load.data.models.transcription.every((m) => m.installed !== "Yes" && !m.downloading)
  );
}

const useStyles = makeStyles({
  root: {
    display: "flex",
    flexDirection: "column",
    height: "100%",
    minHeight: 0,
  },
  top: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    padding: `${tokens.spacingVerticalL} ${tokens.spacingHorizontalXXL}`,
    borderBottom: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    backgroundColor: tokens.colorNeutralBackground2,
  },
  topRow: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: tokens.spacingHorizontalM,
  },
  heading: {
    margin: 0,
    ":focus": { outlineStyle: "none" },
  },
  steps: {
    display: "grid",
    gridTemplateColumns: `repeat(${STEPS.length}, minmax(0, 1fr))`,
    columnGap: tokens.spacingHorizontalM,
    listStyleType: "none",
    margin: 0,
    padding: 0,
    color: tokens.colorNeutralForeground3,
  },
  current: {
    color: tokens.colorNeutralForeground1,
    fontWeight: tokens.fontWeightSemibold,
  },
  body: {
    flexGrow: 1,
    minHeight: 0,
    overflowY: "auto",
    padding: tokens.spacingHorizontalXXL,
  },
  intro: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
    maxWidth: "70ch",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: "70ch",
  },
  need: {
    display: "flex",
    flexDirection: "column",
    alignItems: "flex-start",
    gap: tokens.spacingVerticalS,
    maxWidth: "70ch",
  },
  bar: {
    alignSelf: "stretch",
  },
  name: {
    maxWidth: "520px",
  },
  row: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
    padding: `${tokens.spacingVerticalM} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  rowHead: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    gap: tokens.spacingHorizontalM,
  },
  rowWords: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    minWidth: 0,
  },
  foot: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    padding: `${tokens.spacingVerticalM} ${tokens.spacingHorizontalXXL}`,
    borderTop: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    backgroundColor: tokens.colorNeutralBackground2,
  },
  buttons: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  push: {
    marginLeft: "auto",
  },
});

/** One line and one button: what this computer still needs, and getting all of it. */
function WhatThisComputerNeeds({
  state,
  data,
  localWriter,
}: {
  state: ModelsPageState;
  data: ModelsData;
  localWriter: boolean | null;
}) {
  const styles = useStyles();
  const [starting, setStarting] = useState(false);
  const needed = neededDownloads(data, localWriter);
  const waiting = needed.filter((m) => !m.downloading);
  const held = needed.reduce((sum, m) => sum + m.held, 0);
  const total = needed.reduce((sum, m) => sum + m.size, 0);
  const downloading = waiting.length < needed.length;

  const start = async () => {
    setStarting(true);
    // In this order: a refusal of one (shown at the top of the step) leaves the others to go on.
    for (const m of waiting) await state.download(m.name);
    setStarting(false);
  };

  return (
    <div className={styles.need}>
      {data.models.recommended === null && data.settings.value.transcription === null && (
        <Body1>{NO_MODEL_FOR_LANGUAGES}</Body1>
      )}
      {needed.length === 0 ? (
        <Body1>{NOTHING_TO_DOWNLOAD}</Body1>
      ) : (
        <Body1Strong>
          Earshot needs {formatSize(total - held)} for this computer: {namesOf(needed)}.
        </Body1Strong>
      )}
      {downloading && (
        <>
          <ProgressBar
            className={styles.bar}
            value={total > 0 ? held / total : 0}
            aria-label="Downloads for this computer"
          />
          <Caption1 className={styles.muted} role="status">
            {formatSize(held)} of {formatSize(total)} downloaded. {DOWNLOADS_CARRY_ON}
          </Caption1>
        </>
      )}
      {waiting.length > 0 && (
        <Button appearance="primary" disabled={starting} onClick={() => void start()}>
          Download
        </Button>
      )}
    </div>
  );
}

/** The Sync step's answer. */
type SyncChoice = "join" | "setup" | "later";

/** The Sync step once a way in is chosen: the Sync page's own form for it, which the wizard's
 * Next (or the form's own button, which is Next) sends. A computer that already syncs tells the
 * wizard (`onJoined`) and sends nothing. Nothing is read before this shows. */
function SyncChosen({
  choice,
  saveOnNext,
  onNext,
  onJoined,
}: {
  choice: Exclude<SyncChoice, "later">;
  saveOnNext: SaveOnNext;
  onNext(): void;
  onJoined(): void;
}) {
  const state = useSync(realTimer);
  const { load } = state;
  // Next was pressed before the sync settings were read.
  const [early, setEarly] = useState(false);
  const setUp = load.status === "loaded" && load.view.set_up;
  useEffect(() => {
    if (setUp) onJoined();
  }, [setUp, onJoined]);
  useSaveOnNext(saveOnNext, async () => {
    // Nothing is sent over a view not read yet; a view that could not be read says why itself.
    if (load.status === "loading") setEarly(true);
    if (load.status !== "loaded") return false;
    if (load.view.set_up) return true;
    const ok = choice === "join" ? await state.joinWithCode() : await state.setUpFirst();
    if (ok) onJoined();
    return ok;
  });
  if (load.status !== "loaded") {
    return (
      <>
        <NotLoaded load={load} loading="Reading the sync settings" failed="The sync settings could not be read" />
        {early && load.status === "loading" && (
          <MessageBar intent="info">
            <MessageBarBody>{SYNC_STILL_READING}</MessageBarBody>
          </MessageBar>
        )}
      </>
    );
  }
  return choice === "join" ? (
    <JoinWithCodeForm state={state} onSubmit={onNext} />
  ) : (
    <SetUpFirstForm state={state} onSubmit={onNext} />
  );
}

/** Step 2: another computer, or not now. The choice and a join or setup that worked live in the
 * wizard, so Back shows them; once this computer syncs, that is all the step says, whatever the
 * choice. The choices wait while a join or setup is on its way. */
function SyncStep({
  choice,
  onChoice,
  joined,
  onJoined,
  busy,
  saveOnNext,
  onNext,
}: {
  choice: SyncChoice;
  onChoice(choice: SyncChoice): void;
  joined: boolean;
  onJoined(): void;
  busy: boolean;
  saveOnNext: SaveOnNext;
  onNext(): void;
}) {
  const styles = useStyles();
  return (
    <>
      <Body1 className={styles.muted}>{SYNC_INTRO}</Body1>
      <RadioGroup
        aria-label={SYNC_QUESTION}
        value={choice}
        disabled={busy}
        onChange={(_, d) => onChoice(d.value as SyncChoice)}
      >
        <Radio value="join" label={SYNC_JOIN} />
        <Radio value="setup" label={SYNC_SET_UP} />
        <Radio value="later" label={SYNC_LATER} />
      </RadioGroup>
      {joined ? (
        <MessageBar intent="success">
          <MessageBarBody>{SYNC_JOINED}</MessageBarBody>
        </MessageBar>
      ) : choice === "later" ? (
        <Caption1 className={styles.muted}>{SYNC_LATER_NOTE}</Caption1>
      ) : (
        <SyncChosen key={choice} choice={choice} saveOnNext={saveOnNext} onNext={onNext} onJoined={onJoined} />
      )}
    </>
  );
}

/** Step 3. The state lives in the wizard, so a download's progress outlives this step. */
function DownloadStep({ state, localWriter }: { state: ModelsPageState; localWriter: boolean | null }) {
  const [choosing, setChoosing] = useState(false);
  const { load } = state;
  if (load.status !== "loaded") return <ModelsNotLoaded load={load} />;
  const { models, settings } = load.data;
  return (
    <>
      <UnknownSettings unknown={settings.unknown} />
      <LanguagesPicker
        models={models}
        value={settings.value.languages}
        onChange={(languages) => void state.save({ languages })}
      />
      <WhatThisComputerNeeds state={state} data={load.data} localWriter={localWriter} />
      <div>
        <Button aria-expanded={choosing} onClick={() => setChoosing((c) => !c)}>
          Choose different models
        </Button>
      </div>
      {choosing && (
        <>
          <Recommendation models={models} />
          <ModelChoices state={state} data={load.data} />
        </>
      )}
    </>
  );
}

/** Step 4. Tells the wizard while the sample runs. */
function AboutMeStep({
  blocked,
  saveOnNext,
  onSkip,
  onRunningChange,
}: {
  blocked: string | null;
  saveOnNext: SaveOnNext;
  onSkip(): void;
  onRunningChange(running: boolean): void;
}) {
  const styles = useStyles();
  const speakers = useSpeakers();
  const running = sampleRunning(speakers.sample);
  useEffect(() => {
    onRunningChange(running);
    return () => onRunningChange(false);
  }, [running, onRunningChange]);
  if (speakers.load.status !== "loaded") return <SpeakersNotLoaded load={speakers.load} />;
  const { voice } = speakers.load.view;
  return (
    <>
      <SpeakersNoticeBar state={speakers} kinds={VOICE_NOTICES} />
      <div className={styles.name}>
        <MyNameField state={speakers} name={voice.name} saveOnNext={saveOnNext} />
      </div>
      <VoiceSection state={speakers} voice={voice} withName={false} recordBlocked={blocked} onSkip={onSkip} />
    </>
  );
}

/** A row of step 5: what it is, then, once pressed, the thing itself. */
function ExtraRow({ title, words, children }: { title: string; words: string; children: ReactNode }) {
  const styles = useStyles();
  const [open, setOpen] = useState(false);
  return (
    <section className={styles.row} aria-label={title}>
      <div className={styles.rowHead}>
        <div className={styles.rowWords}>
          <Body1Strong>{title}</Body1Strong>
          {!open && <Caption1 className={styles.muted}>{words}</Caption1>}
        </div>
        {!open && (
          <Button aria-label={`Open ${title}`} onClick={() => setOpen(true)}>
            Open
          </Button>
        )}
      </div>
      {open && children}
    </section>
  );
}

/** Step 5. Tells the wizard while the check runs. */
function OptionalExtrasStep({
  saveOnNext,
  onRunningChange,
}: {
  saveOnNext: SaveOnNext;
  onRunningChange(running: boolean): void;
}) {
  const styles = useStyles();
  const check = useAudioCheck("first-run");
  const running = checkRunning(check.run);
  useEffect(() => {
    onRunningChange(running);
    return () => onRunningChange(false);
  }, [running, onRunningChange]);
  return (
    <>
      <ExtraRow title="Obsidian" words={OBSIDIAN_ROW}>
        <ObsidianSection saveOnNext={saveOnNext} />
      </ExtraRow>
      <ExtraRow title="Audio check" words={AUDIO_CHECK_ROW}>
        <AudioCheckSection state={check} disabled={false} />
        <Caption1 className={styles.muted}>{DEVICES_NOTE}</Caption1>
      </ExtraRow>
    </>
  );
}

export interface FirstRunWizardProps {
  /** The wizard is done (stored), or the user went on after it could not be stored; `noModel`
   * says it ended with no transcription model, here or on its way. */
  onDone(noModel: boolean): void;
}

/** The first-run wizard. */
export function FirstRunWizard({ onDone }: FirstRunWizardProps) {
  const styles = useStyles();
  const page = usePageStyles();
  const ipc = useIpc();
  // Held here, not in the step: the one subscriber to model job events outlives the steps, so
  // the wizard can say what still downloads and the voice sample can follow the helpers.
  const models = useModels();
  const [step, setStep] = useState(0);
  const [syncChoice, setSyncChoice] = useState<SyncChoice>("later");
  // This computer syncs: joined or set up here, or found already set up.
  const [syncJoined, setSyncJoined] = useState(false);
  const onSyncJoined = useCallback(() => setSyncJoined(true), []);
  // Whether this computer is one of the summary writers, read when the Summaries step is left.
  const [localWriter, setLocalWriter] = useState<boolean | null>(null);
  const [running, setRunning] = useState(false);
  const [saving, setSaving] = useState(false);
  const [refused, setRefused] = useState(false);
  const [finishing, setFinishing] = useState(false);
  const [finishError, setFinishError] = useState<string | null>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const savers = useRef(new Set<SaveStep>());

  const saveOnNext = useCallback<SaveOnNext>((save) => {
    savers.current.add(save);
    return () => {
      savers.current.delete(save);
    };
  }, []);

  // A new step starts at its heading, for the keyboard and for screen readers.
  useEffect(() => {
    heading.current?.focus();
  }, [step]);

  const noModel = noTranscriptionModel(models.load);

  /** Whether the stored order holds this computer; null when it cannot be read. */
  const localInOrder = async (): Promise<boolean | null> => {
    try {
      return (await ipc.summaryOrder()).steps.some((s) => s.step.kind === "Local");
    } catch {
      return null;
    }
  };

  const finish = async () => {
    setFinishing(true);
    try {
      await ipc.finishFirstRun();
      onDone(noModel);
    } catch (e) {
      setFinishError(errorText(e));
      setFinishing(false);
    }
  };

  const go = (to: number) => {
    setRefused(false);
    if (to >= STEPS.length) void finish();
    else setStep(to);
  };

  /** Saves what is on the step, and moves on unless a part refused. */
  const next = async () => {
    setSaving(true);
    setRefused(false);
    try {
      if (!(await saveStep([...savers.current]))) {
        setRefused(true);
        return;
      }
      // The Download step reads who writes the summaries from the models view and the order.
      if (step === SUMMARIES_STEP) {
        await models.refresh();
        setLocalWriter(await localInOrder());
      }
    } finally {
      setSaving(false);
    }
    go(step + 1);
  };

  const last = step === STEPS.length - 1;
  const moving = running || finishing || saving;
  const downloading =
    models.load.status === "loaded"
      ? [
          models.load.data.models.helpers,
          ...models.load.data.models.transcription,
          ...models.load.data.models.summary,
          ...models.load.data.models.npu,
        ]
          .filter((m) => m.downloading)
          .map((m) => m.title)
      : [];

  return (
    <div className={styles.root}>
      <header className={styles.top}>
        <div className={styles.topRow}>
          <Body1>Welcome to Earshot. A few quick things, and you can change all of them later in Settings.</Body1>
          <Button appearance="subtle" disabled={moving} onClick={() => void finish()}>
            Skip setup
          </Button>
        </div>
        <ProgressBar
          value={step + 1}
          max={STEPS.length}
          thickness="large"
          aria-label={`Step ${step + 1} of ${STEPS.length}`}
        />
        <ol className={styles.steps}>
          {STEPS.map((s, i) => (
            <li key={s.title} aria-current={i === step ? "step" : undefined}>
              <Caption1 className={mergeClasses(i === step && styles.current)}>{s.title}</Caption1>
            </li>
          ))}
        </ol>
      </header>
      <main className={styles.body}>
        <div className={page.page}>
          <div className={styles.intro}>
            <Caption1 className={styles.muted}>
              Step {step + 1} of {STEPS.length}
            </Caption1>
            <h1 className={styles.heading} tabIndex={-1} ref={heading}>
              <Title2>{STEPS[step].title}</Title2>
            </h1>
            <Body1>{STEPS[step].intro}</Body1>
          </div>
          <Notices notices={models.notices.filter((n) => n.area === "page")} onDismiss={models.dismiss} />
          {step === SUMMARIES_STEP && <SummaryOrderChoices saveOnNext={saveOnNext} />}
          {step === SYNC_STEP && (
            <SyncStep
              choice={syncChoice}
              onChoice={setSyncChoice}
              joined={syncJoined}
              onJoined={onSyncJoined}
              busy={moving}
              saveOnNext={saveOnNext}
              onNext={() => void next()}
            />
          )}
          {step === DOWNLOAD_STEP && <DownloadStep state={models} localWriter={localWriter} />}
          {step === ABOUT_ME_STEP && (
            <AboutMeStep
              blocked={voiceSampleBlocked(models.load)}
              saveOnNext={saveOnNext}
              onSkip={() => void next()}
              onRunningChange={setRunning}
            />
          )}
          {step === EXTRAS_STEP && <OptionalExtrasStep saveOnNext={saveOnNext} onRunningChange={setRunning} />}
        </div>
      </main>
      <footer className={styles.foot}>
        {step !== DOWNLOAD_STEP && downloading.length > 0 && (
          <Caption1 className={styles.muted} role="status">
            {stillDownloading(downloading)}
          </Caption1>
        )}
        {refused && (
          <MessageBar intent="warning">
            <MessageBarBody>{SAVE_REFUSED_NOTE}</MessageBarBody>
          </MessageBar>
        )}
        {running && <Caption1 className={styles.muted}>{RUNNING_NOTE}</Caption1>}
        {finishError !== null && (
          <MessageBar intent="error">
            <MessageBarBody>
              <MessageBarTitle>Earshot could not note that setup is finished</MessageBarTitle>
              {finishError} You can try again, or go on: this guide then shows again next time.
            </MessageBarBody>
          </MessageBar>
        )}
        <div className={styles.buttons}>
          {finishError !== null && <Button onClick={() => onDone(noModel)}>Go on anyway</Button>}
          <Button className={styles.push} disabled={moving || step === 0} onClick={() => go(step - 1)}>
            Back
          </Button>
          <Button appearance="primary" disabled={moving} onClick={() => void next()}>
            {last ? "Finish" : "Next"}
          </Button>
        </div>
      </footer>
    </div>
  );
}
