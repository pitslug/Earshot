// The first-run wizard (D21, docs/ui-design.md "First run", T9.1d): four skippable steps with
// a progress bar. It is a view of `AppShell`, so the shell's tabster hold, the meeting events
// and the quit dialog stay as they are. Every step is the Settings page's own parts and hooks;
// the wizard holds only the order, its own short words and the "done" call.

import {
  Body1,
  Button,
  Caption1,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  ProgressBar,
  Title2,
  ToggleButton,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { useCallback, useEffect, useRef, useState } from "react";

import { type ModelItem, type WriterChoice, useIpc } from "../ipc";
import { AudioCheckSection } from "../settings/AudioCheckSection";
import {
  LanguagesPicker,
  Notices,
  HardwareSection,
  Recommendation,
} from "../settings/ModelsParts";
import { ModelChoices, ModelsNotLoaded, Section, UnknownModelSettings } from "../settings/ModelsPage";
import { ObsidianPage } from "../settings/ObsidianPage";
import { SpeakersNoticeBar, SpeakersNotLoaded, VoiceSection } from "../settings/SpeakersPage";
import { SummaryWriterChoices } from "../settings/SummariesPage";
import { checkRunning, useAudioCheck } from "../settings/useAudioCheck";
import { type ModelsPageState, errorText, useModels } from "../settings/useModels";
import { sampleRunning, useSpeakers } from "../settings/useSpeakers";

/** The steps, in order, with what each one says first. */
export const STEPS = [
  {
    title: "Obsidian vault",
    intro: "Choose the Obsidian vault your meeting notes go to. If you do not use Obsidian, skip this step.",
  },
  {
    title: "Models",
    intro: "Earshot does its work on this computer, so it needs a few downloads: the speech helpers and a transcription model. You choose when; nothing downloads until you press the button.",
  },
  {
    title: "Summaries",
    intro: "Choose who writes the summary after each meeting.",
  },
  {
    title: "Audio check and your voice",
    intro: "Both are optional. Nothing plays and nothing records until you press the button.",
  },
] as const;

/** What now / later / never mean for an optional model. */
export const OFFER_EXPLAINED =
  "Optional models: download one now, or leave it for later and get it any time from Settings, Models. Never records that you do not want it, and it stays in Settings if you change your mind.";

/** The line under the summary models in this step. */
export const SUMMARY_NOTE =
  "You only need a summary model if this computer writes your summaries. You choose that in the next step.";

/** Where the other summary choices are. */
export const OTHER_WRITERS_NOTE =
  "Claude through Claude Code and OpenAI through Codex are in Settings, Summaries, with the summary style and summarising during the meeting.";

/** Where the devices are. */
export const DEVICES_NOTE = "The check uses the devices in Settings, Audio: the Windows defaults unless you chose others.";

/** Why Next waits. */
export const UNSAVED_NOTE = "This step has something that is not saved. Save it first, or skip this step to leave it.";

/** Why the buttons wait during the check or the voice sample. */
export const RUNNING_NOTE = "Wait for this to finish, or cancel it, before you move on.";

/** The line while models download on other steps. */
export function stillDownloading(titles: string[]): string {
  const names = titles.join(", ");
  return `Still downloading: ${names}. It carries on while you go through the other steps, and after you finish.`;
}

/** The summary choices this step offers (docs/ui-design.md "First run"). */
const WIZARD_WRITERS: WriterChoice[] = ["OnThisComputer", "OwnEndpoint", "NoSummary"];

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
    gridTemplateColumns: "repeat(4, minmax(0, 1fr))",
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
  column: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
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
  offer: {
    display: "flex",
    gap: tokens.spacingHorizontalXS,
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

/** Later / never for a model that is not on this computer; the card's Download is "now". */
function ModelOffer({
  item,
  declined,
  onChange,
}: {
  item: ModelItem;
  declined: boolean;
  onChange(declined: boolean): void;
}) {
  const styles = useStyles();
  return (
    <div className={styles.offer} role="group" aria-label={`When to download ${item.title}`}>
      <ToggleButton size="small" appearance="subtle" checked={!declined} onClick={() => onChange(false)}>
        Later
      </ToggleButton>
      <ToggleButton size="small" appearance="subtle" checked={declined} onClick={() => onChange(true)}>
        Never
      </ToggleButton>
    </div>
  );
}

/** Step 2. The state lives in the wizard, so a download's progress outlives this step. */
function ModelsStep({
  state,
  declined,
  setDeclined,
}: {
  state: ModelsPageState;
  declined: string[];
  setDeclined(model: string, declined: boolean): void;
}) {
  const styles = useStyles();
  const { load } = state;
  if (load.status !== "loaded") return <ModelsNotLoaded load={load} />;
  const { hardware, models, settings } = load.data;
  const picked = [models.recommended?.model, models.summary_recommended.model];
  const offer = (item: ModelItem) =>
    picked.includes(item.name) || item.installed !== "No" || item.downloading ? null : (
      <ModelOffer
        item={item}
        declined={declined.includes(item.name)}
        onChange={(no) => setDeclined(item.name, no)}
      />
    );
  // Downloading a model answers "never" the other way.
  const answering: ModelsPageState = {
    ...state,
    download: async (model) => {
      if (declined.includes(model)) setDeclined(model, false);
      await state.download(model);
    },
  };
  return (
    <>
      <UnknownModelSettings unknown={settings.unknown} />
      <Section title="This is your hardware">
        <HardwareSection hardware={hardware} models={models} />
      </Section>
      <Recommendation models={models} />
      <Caption1 className={styles.muted}>{OFFER_EXPLAINED}</Caption1>
      <ModelChoices state={answering} data={load.data} summaryNote={SUMMARY_NOTE} offer={offer} />
      <Section title="Meeting languages">
        <LanguagesPicker
          models={models}
          value={settings.value.languages}
          onChange={(languages) => void state.save({ languages })}
        />
      </Section>
    </>
  );
}

/** Step 4. Tells the wizard while the check or the sample runs. */
function AudioAndVoiceStep({ onRunningChange }: { onRunningChange(running: boolean): void }) {
  const styles = useStyles();
  const check = useAudioCheck("first-run");
  const speakers = useSpeakers();
  const running = checkRunning(check.run) || sampleRunning(speakers.sample);
  useEffect(() => {
    onRunningChange(running);
    return () => onRunningChange(false);
  }, [running, onRunningChange]);
  return (
    <>
      <Section title="Audio check">
        <AudioCheckSection state={check} disabled={sampleRunning(speakers.sample)} />
        <Caption1 className={styles.muted}>{DEVICES_NOTE}</Caption1>
      </Section>
      <SpeakersNoticeBar state={speakers} />
      {speakers.load.status === "loaded" ? (
        <VoiceSection state={speakers} voice={speakers.load.view.voice} />
      ) : (
        <SpeakersNotLoaded load={speakers.load} />
      )}
    </>
  );
}

export interface FirstRunWizardProps {
  /** The wizard is done (stored), or the user went on after it could not be stored. */
  onDone(): void;
}

/** The first-run wizard. */
export function FirstRunWizard({ onDone }: FirstRunWizardProps) {
  const styles = useStyles();
  const ipc = useIpc();
  // Held here, not in the step: the one subscriber to model job events outlives the steps, so
  // the wizard can say what still downloads.
  const models = useModels();
  const [step, setStep] = useState(0);
  const [declined, setDeclined] = useState<string[]>([]);
  const [answerError, setAnswerError] = useState<string | null>(null);
  const [unsaved, setUnsaved] = useState(false);
  const [unsavedAsked, setUnsavedAsked] = useState(false);
  const [running, setRunning] = useState(false);
  const [finishing, setFinishing] = useState(false);
  const [finishError, setFinishError] = useState<string | null>(null);
  const heading = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    let live = true;
    // Only the "never" answers matter here; a store that cannot be read shows none.
    ipc.firstRunState().then(
      (s) => {
        if (live) setDeclined(s.declined_models);
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  // A new step starts at its heading, for the keyboard and for screen readers.
  useEffect(() => {
    heading.current?.focus();
  }, [step]);

  const answer = useCallback(
    (model: string, no: boolean) => {
      ipc.setModelDeclined(model, no).then(
        (s) => {
          setDeclined(s.declined_models);
          setAnswerError(null);
        },
        (e: unknown) => setAnswerError(errorText(e)),
      );
    },
    [ipc],
  );

  const onUnsavedChange = useCallback((u: boolean) => {
    setUnsaved(u);
    if (!u) setUnsavedAsked(false);
  }, []);

  const finish = async () => {
    setFinishing(true);
    try {
      await ipc.finishFirstRun();
      onDone();
    } catch (e) {
      setFinishError(errorText(e));
      setFinishing(false);
    }
  };

  /** Moves on without looking at unsaved fields (skipping says to leave them). */
  const go = (to: number) => {
    setUnsaved(false);
    setUnsavedAsked(false);
    if (to >= STEPS.length) void finish();
    else setStep(to);
  };
  const next = () => {
    if (unsaved) setUnsavedAsked(true);
    else go(step + 1);
  };

  const last = step === STEPS.length - 1;
  const moving = running || finishing;
  const downloading =
    models.load.status === "loaded"
      ? [models.load.data.models.helpers, ...models.load.data.models.transcription, ...models.load.data.models.summary]
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
        <div className={styles.column}>
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
          {answerError !== null && (
            <MessageBar intent="error">
              <MessageBarBody>
                <MessageBarTitle>Your answer was not saved</MessageBarTitle>
                {answerError}
              </MessageBarBody>
            </MessageBar>
          )}
          {step === 0 && <ObsidianPage noHeading onUnsavedChange={onUnsavedChange} />}
          {step === 1 && <ModelsStep state={models} declined={declined} setDeclined={answer} />}
          {step === 2 && (
            <>
              <SummaryWriterChoices writers={WIZARD_WRITERS} onUnsavedChange={onUnsavedChange} />
              <Caption1 className={styles.muted}>{OTHER_WRITERS_NOTE}</Caption1>
            </>
          )}
          {step === 3 && <AudioAndVoiceStep onRunningChange={setRunning} />}
        </div>
      </main>
      <footer className={styles.foot}>
        {step !== 1 && downloading.length > 0 && (
          <Caption1 className={styles.muted} role="status">
            {stillDownloading(downloading)}
          </Caption1>
        )}
        {unsavedAsked && unsaved && (
          <MessageBar intent="warning">
            <MessageBarBody>{UNSAVED_NOTE}</MessageBarBody>
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
          <Button appearance="subtle" disabled={moving} onClick={() => go(step + 1)}>
            Skip this step
          </Button>
          {finishError !== null && <Button onClick={onDone}>Go on anyway</Button>}
          <Button className={styles.push} disabled={moving || step === 0} onClick={() => go(step - 1)}>
            Back
          </Button>
          <Button appearance="primary" disabled={moving} onClick={next}>
            {last ? "Finish" : "Next"}
          </Button>
        </div>
      </footer>
    </div>
  );
}
