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

import { type PreviewsChoice, type PreviewsView, useIpc } from "../ipc";
import { ModelNotices, ModelsNotLoaded, NpuVersions, Section, TranscriptionChoices } from "./ModelSections";
import { LANGUAGES_LABEL, LanguagesPicker } from "./ModelsParts";
import { SpeakerTimingSection } from "./PeoplePage";
import { type ModelsPageState, errorText } from "./useModels";

const useStyles = makeStyles({
  page: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXL,
    maxWidth: "860px",
  },
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
  "Shows grey words within a few seconds while someone is talking, then replaces them with the settled text. It uses more of your computer during a meeting and changes nothing that is saved. Only with a Parakeet model, and not on battery.";

/** Whose words "Other people only" shows. */
export const PREVIEWS_OTHER_PEOPLE_EXPLAINED =
  "Other people only: in a call, grey words for the people you're talking to. In the room, for everyone.";

/** Whose words "Everyone" shows. */
export const PREVIEWS_EVERYONE_EXPLAINED =
  "Everyone: in a call, also for your own voice. Uses more of your computer. Without headphones, your grey words can briefly show what the other people said, until the settled text replaces them.";

/** Whose provisional words meetings show: read and changed on its own, from the next meeting. */
function PreviewsSection() {
  const styles = useStyles();
  const ipc = useIpc();
  const [view, setView] = useState<PreviewsView | null>(null);
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

  const set = async (choice: PreviewsChoice) => {
    setBusy(true);
    try {
      setView(await ipc.setPreviews(choice));
      setProblem(null);
    } catch (e) {
      setProblem(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Section title="During a meeting">
      <Body1 id={headingId}>{PREVIEWS_HEADING}</Body1>
      <RadioGroup
        aria-labelledby={headingId}
        value={view?.choice ?? ""}
        disabled={busy || view === null || !view.can_change}
        onChange={(_, d) => void set(d.value as PreviewsChoice)}
      >
        <Radio value="off" label="Off" />
        <Radio value="other_people" label="Other people only" />
        <Radio value="everyone" label="Everyone" />
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
  return (
    <div className={styles.page}>
      <Title2 as="h1" className={styles.heading}>
        Transcription
      </Title2>
      <TranscriptionModels models={models} />
      <PreviewsSection />
      <SpeakerTimingSection />
    </div>
  );
}
