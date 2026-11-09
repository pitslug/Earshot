// The model parts of Settings (docs/settings-design.md "Settings", T12.5b): the speech helpers,
// the transcription choices, the summary models and the NPU versions, each one section, placed
// by the System, Transcription and Summaries pages and, through `ModelChoices`, by the first-run
// wizard. Everything shown comes from the IPC, through the one `useModels` state they share.

import {
  Caption1,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  RadioGroup,
  Spinner,
  Subtitle2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useId } from "react";

import type { ModelItem } from "../ipc";
import { HelpersCard, ModelCard, NpuCard } from "./ModelCard";
import { CheckLine, Notices } from "./ModelsParts";
import { HELPERS_WHY, NPU_WHY, ROLE_LABEL, choiceWhy, summaryLabel, summaryWhy, titleOf } from "./copy";
import type { ModelsData, ModelsLoad, ModelsPageState } from "./useModels";

const useStyles = makeStyles({
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

/** The notices of the whole model state and the stored settings this version does not know: the
 * top of every page that shows a model part. */
export function ModelNotices({ state, data }: { state: ModelsPageState; data: ModelsData }) {
  return (
    <>
      <Notices notices={state.notices.filter((n) => n.area === "page")} onDismiss={state.dismiss} />
      <UnknownModelSettings unknown={data.settings.unknown} />
    </>
  );
}

/** Said under the summary models when this computer does not write the summaries. */
export const SUMMARIES_ELSEWHERE =
  "Summaries are not written on this computer now, so no model here is in use for them. The one selected is used if you switch back on the Summaries page.";

function cardOf(state: ModelsPageState) {
  return (item: ModelItem, label: string, why: string, selected: boolean) => (
    <ModelCard
      key={item.name}
      item={item}
      label={label}
      why={why}
      selected={selected}
      onDownload={(m) => void state.download(m)}
      onPause={(m) => void state.pause(m)}
      onRemove={(m) => void state.remove(m)}
    />
  );
}

/** The speech helpers every meeting needs. */
export function HelpersSection({ state, data }: { state: ModelsPageState; data: ModelsData }) {
  return (
    <Section title="What every meeting needs">
      <HelpersCard
        item={data.models.helpers}
        why={HELPERS_WHY}
        onDownload={(m) => void state.download(m)}
        onPause={(m) => void state.pause(m)}
      />
    </Section>
  );
}

/** The four transcription choices and, when the model in use is none of them, what is in use. */
export function TranscriptionChoices({ state, data }: { state: ModelsPageState; data: ModelsData }) {
  const styles = useStyles();
  const label = useId();
  const card = cardOf(state);
  const { hardware, models } = data;
  const best = hardware.accelerators[0] ?? "Processor";
  const byName = new Map(models.transcription.map((m) => [m.name, m]));
  const choices = models.choices.flatMap((c) => {
    const item = byName.get(c.model);
    return item ? [{ role: c.role, item }] : [];
  });
  const inUseIsAChoice = choices.some((c) => c.item.name === models.in_use.transcription);
  const european = choices.find((c) => c.role === "European")?.item.title;
  return (
    <>
      <span id={label} hidden>
        Transcription model
      </span>
      <RadioGroup
        aria-labelledby={label}
        value={inUseIsAChoice ? models.in_use.transcription : ""}
        onChange={(_, d) => void state.save({ transcription: d.value })}
        className={styles.choices}
      >
        {choices.map(({ role, item }) =>
          card(item, ROLE_LABEL[role], choiceWhy(role, item, best, european), item.name === models.in_use.transcription),
        )}
      </RadioGroup>
      {!inUseIsAChoice && (
        <Caption1 className={styles.muted}>
          In use now: {titleOf(models, models.in_use.transcription)} ({models.in_use.transcription_source}).
        </Caption1>
      )}
    </>
  );
}

/** What the model check does, above its line. */
export const CHECK_EXPLAINED =
  "Earshot tries the transcription models on this computer to find which ones keep up with live speech.";

/** The model check's line and its notices. */
export function CheckSection({ state, data }: { state: ModelsPageState; data: ModelsData }) {
  const styles = useStyles();
  return (
    <>
      <Caption1 className={styles.muted}>{CHECK_EXPLAINED}</Caption1>
      <CheckLine
        models={data.models}
        checking={state.checking}
        onRun={() => void state.runCheck()}
        onCancel={() => void state.cancelCheck()}
      />
      <Notices notices={state.notices.filter((n) => n.area === "check")} onDismiss={state.dismiss} />
    </>
  );
}

/** The summary models, recommended first. */
export function SummaryModelChoices({
  state,
  data,
  note,
}: {
  state: ModelsPageState;
  data: ModelsData;
  /** The line under the models, if any. */
  note?: string;
}) {
  const styles = useStyles();
  const label = useId();
  const card = cardOf(state);
  const { models } = data;
  const recommended = models.summary.find((m) => m.name === models.summary_recommended.model);
  const summaries = [...models.summary].sort((a, b) =>
    a.name === models.summary_recommended.model ? -1 : b.name === models.summary_recommended.model ? 1 : 0,
  );
  return (
    <>
      <span id={label} hidden>
        Summary model
      </span>
      <RadioGroup
        aria-labelledby={label}
        value={models.in_use.summary}
        onChange={(_, d) => void state.save({ summary: d.value })}
        className={styles.choices}
      >
        {summaries.map((item) =>
          card(
            item,
            summaryLabel(item, recommended),
            summaryWhy(item),
            models.in_use.summaries_here && item.name === models.in_use.summary,
          ),
        )}
      </RadioGroup>
      {!models.in_use.summaries_here && <Caption1 className={styles.muted}>{SUMMARIES_ELSEWHERE}</Caption1>}
      {note !== undefined && <Caption1 className={styles.muted}>{note}</Caption1>}
    </>
  );
}

/** The NPU versions of the transcription models, on a build that offers them. */
export function NpuVersions({ state, data }: { state: ModelsPageState; data: ModelsData }) {
  if (data.models.npu.length === 0) return null;
  return (
    <Section title="NPU versions">
      {data.models.npu.map((item) => (
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
  );
}

/** What every meeting needs, the transcription choices with the check, and the summary models:
 * the first-run wizard's "Choose different models", all in one place. */
export function ModelChoices({ state, data }: { state: ModelsPageState; data: ModelsData }) {
  return (
    <>
      <HelpersSection state={state} data={data} />
      <Section title="Transcription">
        <TranscriptionChoices state={state} data={data} />
        <CheckSection state={state} data={data} />
      </Section>
      <Section title="Summaries on this computer">
        <SummaryModelChoices state={state} data={data} />
      </Section>
    </>
  );
}
