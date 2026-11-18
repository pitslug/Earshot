// Settings, Transcription (docs/settings-design.md "Settings", T12.5b): the meeting languages
// first, because they decide the recommendation; the transcription models and their NPU
// versions; whose words show while someone is talking (T9.1m, was on General); and when speakers
// are found (T6.13d4, was on Speakers). The model parts share the `useModels` state Settings holds.

import {
  Body1,
  Caption1,
  MessageBar,
  MessageBarBody,
  Radio,
  RadioGroup,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useEffect, useId, useState } from "react";

import { type PreviewsChoice, type PreviewsView, errorText, useIpc } from "../ipc";
import { ModelNotices, ModelsNotLoaded, NpuVersions, TranscriptionChoices } from "./ModelSections";
import { LANGUAGES_LABEL, LanguagesPicker } from "./ModelsParts";
import { SpeakerTimingSection } from "./PeoplePage";
import { Section, usePageStyles } from "./parts";
import { type SaveOnNext, useSaveOnNext } from "./saveOnNext";
import { type ModelsPageState } from "./useModels";

const useStyles = makeStyles({
  heading: {
    margin: 0,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
});

/** The name of the provisional words choice. */
export const PREVIEWS_HEADING = "Show words while someone is talking";

/** What provisional words are, on the Transcription page. */
export const PREVIEWS_EXPLAINED =
  "Shows grey words within a few seconds while someone is talking, then replaces them with the settled text. It uses more of your computer during a meeting and changes nothing that is saved. Only with a Parakeet model, and paused while the computer is on battery.";

/** Whose words "Other people only" shows. */
export const PREVIEWS_OTHER_PEOPLE_EXPLAINED =
  "Other people only: in a call, grey words for the people you're talking to. In the room, for everyone.";

/** Whose words "Everyone" shows. */
export const PREVIEWS_EVERYONE_EXPLAINED =
  "Everyone: in a call, also for your own voice. Uses more of your computer. Without headphones, your grey words can briefly show what the other people said, until the settled text replaces them.";

/** How the wizard marks the option Rust suggests for this computer (T12.29). */
export const PREVIEWS_SUGGESTED = "suggested for this computer";

/** The three options, in order. */
const PREVIEWS_OPTIONS: { value: PreviewsChoice; label: string }[] = [
  { value: "off", label: "Off" },
  { value: "other_people", label: "Other people only" },
  { value: "everyone", label: "Everyone" },
];

export interface PreviewsSectionProps {
  /** In the first-run wizard: the choice is saved on Next, not at once, and only when it differs
   * from what the app shows now. */
  saveOnNext?: SaveOnNext;
  /** Marks the option the app suggests for this computer (the wizard). */
  showSuggested?: boolean;
}

/** Whose provisional words meetings show, from the next meeting: on the Transcription page a
 * choice applies at once; in the wizard it waits for Next. */
export function PreviewsSection({ saveOnNext, showSuggested = false }: PreviewsSectionProps) {
  const styles = useStyles();
  const ipc = useIpc();
  const [view, setView] = useState<PreviewsView | null>(null);
  // The wizard's choice until Next saves it.
  const [picked, setPicked] = useState<PreviewsChoice | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    ipc.previewsSettings().then(
      (v) => {
        if (live) setView(v);
      },
      (e: unknown) => {
        if (live) setProblem(errorText(e));
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const headingId = useId();

  const set = async (choice: PreviewsChoice): Promise<boolean> => {
    setBusy(true);
    try {
      setView(await ipc.setPreviews(choice));
      setPicked(null);
      setProblem(null);
      return true;
    } catch (e) {
      setProblem(errorText(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  // Only a choice the user changed is sent: an untouched part stores nothing.
  useSaveOnNext(saveOnNext, async () => {
    if (view === null || picked === null || picked === view.choice) return true;
    return set(picked);
  });

  const shown = picked ?? view?.choice ?? "";
  const labelOf = (o: (typeof PREVIEWS_OPTIONS)[number]) =>
    showSuggested && view?.suggested === o.value ? `${o.label} (${PREVIEWS_SUGGESTED})` : o.label;

  return (
    <Section title="During a meeting">
      <Body1 id={headingId}>{PREVIEWS_HEADING}</Body1>
      <RadioGroup
        aria-labelledby={headingId}
        value={shown}
        disabled={busy || view === null || !view.can_change}
        onChange={(_, d) => {
          const choice = d.value as PreviewsChoice;
          if (saveOnNext) setPicked(choice);
          else void set(choice);
        }}
      >
        {PREVIEWS_OPTIONS.map((o) => (
          <Radio key={o.value} value={o.value} label={labelOf(o)} />
        ))}
      </RadioGroup>
      <Caption1 className={styles.muted}>{PREVIEWS_EXPLAINED}</Caption1>
      <Caption1 className={styles.muted}>{PREVIEWS_OTHER_PEOPLE_EXPLAINED}</Caption1>
      <Caption1 className={styles.muted}>{PREVIEWS_EVERYONE_EXPLAINED}</Caption1>
      {view?.note != null && <Caption1 className={styles.muted}>{view.note}</Caption1>}
      {problem !== null && (
        <MessageBar intent="error">
          <MessageBarBody>{problem}</MessageBarBody>
        </MessageBar>
      )}
    </Section>
  );
}

/** The languages and the transcription models, or why they cannot be shown. */
function TranscriptionModels({ models }: { models: ModelsPageState }) {
  const { load } = models;
  if (load.status !== "loaded") return <ModelsNotLoaded load={load} />;
  const { settings } = load.data;
  return (
    <>
      <ModelNotices state={models} data={load.data} />
      <Section title={LANGUAGES_LABEL}>
        <LanguagesPicker
          hideLabel
          models={load.data.models}
          value={settings.value.languages}
          onChange={(languages) => void models.save({ languages })}
        />
      </Section>
      <Section title="Transcription model">
        <TranscriptionChoices state={models} data={load.data} />
      </Section>
      <NpuVersions state={models} data={load.data} />
    </>
  );
}

export interface TranscriptionPageProps {
  /** The model state Settings holds for its pages. */
  models: ModelsPageState;
}

/** The Transcription page of Settings. */
export function TranscriptionPage({ models }: TranscriptionPageProps) {
  const styles = useStyles();
  const page = usePageStyles();
  return (
    <div className={page.page}>
      <Title2 as="h1" className={styles.heading}>
        Transcription
      </Title2>
      <TranscriptionModels models={models} />
      <PreviewsSection />
      <SpeakerTimingSection />
    </div>
  );
}
