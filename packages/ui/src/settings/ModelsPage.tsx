// Settings, Models (docs/ui-mockups/t7-4.html, PLAN T7.4 answers): this is your hardware,
// the recommendation, the four transcription choices with the check, the summary models,
// where it runs and the meeting languages. Everything shown comes from the IPC.

import {
  Caption1,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  RadioGroup,
  Spinner,
  Subtitle2,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useId } from "react";

import type { ModelItem } from "../ipc";
import { CudaParts } from "./CudaSection";
import { HelpersCard, ModelCard, NpuCard } from "./ModelCard";
import {
  CheckLine,
  HardwareSection,
  LanguagesPicker,
  Notices,
  Recommendation,
  WhereItRuns,
} from "./ModelsParts";
import { HELPERS_WHY, NPU_WHY, ROLE_LABEL, choiceWhy, summaryLabel, summaryWhy, titleOf } from "./copy";
import { useCuda } from "./useCuda";
import { type ModelsData, type ModelsLoad, type ModelsPageState, useModels } from "./useModels";

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
  muted: {
    color: tokens.colorNeutralForeground3,
  },
});

export function Section({ title, children }: { title: string; children: ReactNode }) {
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

/** What shows until the models are read: the spinner or the failure. */
export function ModelsNotLoaded({ load }: { load: Exclude<ModelsLoad, { status: "loaded" }> }) {
  if (load.status === "loading") return <Spinner label="Reading this computer and the models" />;
  return (
    <MessageBar intent="error">
      <MessageBarBody>
        <MessageBarTitle>The models could not be read</MessageBarTitle>
        {load.message}
      </MessageBarBody>
    </MessageBar>
  );
}

/** Stored settings this version does not know. */
export function UnknownModelSettings({ unknown }: { unknown: string[] }) {
  if (unknown.length === 0) return null;
  return (
    <MessageBar intent="warning">
      <MessageBarBody>
        <MessageBarTitle>Some stored settings are not known to this version</MessageBarTitle>
        {unknown.join(" ")}
      </MessageBarBody>
    </MessageBar>
  );
}

/** Said under the summary models when this computer does not write the summaries. */
export const SUMMARIES_ELSEWHERE =
  "Summaries are not written on this computer now, so no model here is in use for them. The one selected is used if you switch back on the Summaries page.";

/** What every meeting needs, the transcription choices with the check, and the summary models. */
export function ModelChoices({
  state,
  data,
  summaryNote,
  offer,
}: {
  state: ModelsPageState;
  data: ModelsData;
  /** The line under the summary models. */
  summaryNote: string;
  /** Shown under a model's install state (the first-run wizard's later / never). */
  offer?: (item: ModelItem) => ReactNode;
}) {
  const styles = useStyles();
  const transcriptionLabel = useId();
  const summaryLabelId = useId();
  const { checking, notices } = state;
  const { hardware, models } = data;
  const best = hardware.accelerators[0] ?? "Processor";
  const byName = new Map(models.transcription.map((m) => [m.name, m]));
  const choices = models.choices.flatMap((c) => {
    const item = byName.get(c.model);
    return item ? [{ role: c.role, item }] : [];
  });
  const recommendedSummary = models.summary.find((m) => m.name === models.summary_recommended.model);
  const summaries = [...models.summary].sort((a, b) =>
    a.name === models.summary_recommended.model ? -1 : b.name === models.summary_recommended.model ? 1 : 0,
  );
  const inUseIsAChoice = choices.some((c) => c.item.name === models.in_use.transcription);
  const card = (item: ModelItem, label: string, why: string, selected: boolean) => (
    <ModelCard
      key={item.name}
      item={item}
      label={label}
      why={why}
      selected={selected}
      onDownload={(m) => void state.download(m)}
      onPause={(m) => void state.pause(m)}
      onRemove={(m) => void state.remove(m)}
      offer={offer?.(item)}
    />
  );

  return (
    <>
      <Section title="What every meeting needs">
        <HelpersCard
          item={models.helpers}
          why={HELPERS_WHY}
          onDownload={(m) => void state.download(m)}
          onPause={(m) => void state.pause(m)}
        />
      </Section>

      <Section title="Transcription">
        <span id={transcriptionLabel} hidden>
          Transcription model
        </span>
        <RadioGroup
          aria-labelledby={transcriptionLabel}
          value={inUseIsAChoice ? models.in_use.transcription : ""}
          onChange={(_, d) => void state.save({ transcription: d.value })}
          className={styles.choices}
        >
          {choices.map(({ role, item }) =>
            card(item, ROLE_LABEL[role], choiceWhy(role, item, best), item.name === models.in_use.transcription),
          )}
        </RadioGroup>
        {!inUseIsAChoice && (
          <Caption1 className={styles.muted}>
            In use now: {titleOf(models, models.in_use.transcription)} ({models.in_use.transcription_source}).
          </Caption1>
        )}
        <CheckLine
          models={models}
          checking={checking}
          onRun={() => void state.runCheck()}
          onCancel={() => void state.cancelCheck()}
        />
        <Notices notices={notices.filter((n) => n.area === "check")} onDismiss={state.dismiss} />
      </Section>

      <Section title="Summaries on this computer">
        <span id={summaryLabelId} hidden>
          Summary model
        </span>
        <RadioGroup
          aria-labelledby={summaryLabelId}
          value={models.in_use.summary}
          onChange={(_, d) => void state.save({ summary: d.value })}
          className={styles.choices}
        >
          {summaries.map((item) =>
            card(
              item,
              summaryLabel(item, recommendedSummary),
              summaryWhy(item),
              models.in_use.summaries_here && item.name === models.in_use.summary,
            ),
          )}
        </RadioGroup>
        {!models.in_use.summaries_here && (
          <Caption1 className={styles.muted}>{SUMMARIES_ELSEWHERE}</Caption1>
        )}
        <Caption1 className={styles.muted}>{summaryNote}</Caption1>
      </Section>
    </>
  );
}

/** The Models page of Settings. */
export function ModelsPage() {
  const styles = useStyles();
  const state = useModels();
  const cuda = useCuda(state.refresh);
  const { load, notices } = state;

  if (load.status !== "loaded") return <ModelsNotLoaded load={load} />;
  const { hardware, models, settings } = load.data;

  return (
    <div className={styles.page}>
      <Title2 as="h1" className={styles.heading}>
        Models
      </Title2>
      <Notices notices={notices.filter((n) => n.area === "page")} onDismiss={state.dismiss} />
      <UnknownModelSettings unknown={settings.unknown} />

      <Section title="This is your hardware">
        <HardwareSection hardware={hardware} models={models} />
      </Section>

      <Recommendation models={models} />

      <ModelChoices
        state={state}
        data={load.data}
        summaryNote="Your own endpoint or no summary: Settings, Summaries."
      />

      {models.npu.length > 0 && (
        <Section title="NPU versions">
          {models.npu.map((item) => (
            <NpuCard
              key={item.name}
              item={item}
              why={NPU_WHY}
              onDownload={(m) => void state.download(m)}
              onPause={(m) => void state.pause(m)}
              onRemove={(m) => void state.remove(m)}
            />
          ))}
        </Section>
      )}

      <Section title="Where it runs">
        <WhereItRuns
          hardware={hardware}
          models={models}
          transcription={settings.value.transcription_part}
          summary={settings.value.summary_part}
          onChange={(task, part) => void state.save({ [task]: part })}
        />
      </Section>

      {cuda.view?.offered === true && (
        <Section title="NVIDIA CUDA libraries">
          <CudaParts state={cuda} view={cuda.view} />
        </Section>
      )}

      <Section title="Meeting languages">
        <LanguagesPicker
          models={models}
          value={settings.value.languages}
          onChange={(languages) => void state.save({ languages })}
        />
      </Section>
    </div>
  );
}
