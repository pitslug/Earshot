// Settings, Summaries (docs/ui-mockups/t7-4.html, PLAN T7.4g): who writes the summary (this
// computer, your own endpoint, Claude through Claude Code (T7.4ga), OpenAI through Codex
// (T7.4gc) or no summary), the summary model under "On this computer" (T12.5b, was on Models),
// the endpoint with its API key for Anthropic, Claude Code's and Codex's status and model (a
// list with Other, T12.18) and Claude Code's effort, summarising during the meeting, and the
// summary style (T7.4b, `SummaryStyleSection`). Choices save as they change; the endpoint and a
// model name typed under Other save with their buttons. The API key is only ever sent
// to the app, never shown or read back.

import {
  Badge,
  Body1Strong,
  Button,
  Caption1,
  Field,
  Input,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Radio,
  RadioGroup,
  Select,
  Spinner,
  Switch,
  Title2,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useEffect, useId, useRef, useState } from "react";

import {
  type EffortChoice,
  ORDER_COPY,
  type ProviderChoice,
  type StepChoice,
  type StepView,
  type SummarySettingsValue,
  type SummarySettingsView,
  fillCopy,
  sameStep,
  stepKey,
} from "../ipc";
import { ModelsNotLoaded, SummaryModelChoices } from "./ModelSections";
import { SummaryStyleSection } from "./SummaryStyleSection";
import { partName, titleOf } from "./copy";
import { NotLoaded, NoticeBar, Section, UnknownSettings, usePageStyles } from "./parts";
import { type SaveOnNext, useSaveOnNext } from "./saveOnNext";
import type { ModelsPageState } from "./useModels";
import { type CliCheck, type SummariesPageState, useSummaries } from "./useSummaries";

const useStyles = makeStyles({
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
  title: {
    display: "inline-flex",
    flexWrap: "wrap",
    alignItems: "baseline",
    gap: tokens.spacingHorizontalS,
  },
  why: {
    color: tokens.colorNeutralForeground2,
    paddingLeft: "32px",
  },
  destination: {
    color: tokens.colorNeutralForeground3,
    paddingLeft: "32px",
  },
  later: {
    display: "grid",
    gap: tokens.spacingVerticalM,
  },
  // The list itself is capped; its hint (where the transcript goes) fills the page.
  laterSelect: {
    maxWidth: "520px",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  endpoint: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
    padding: tokens.spacingHorizontalM,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground2,
  },
  fields: {
    display: "grid",
    gap: tokens.spacingVerticalM,
    maxWidth: "520px",
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  other: {
    justifySelf: "start",
  },
});

/** The model field's placeholder per provider (never a stored default). */
export const MODEL_PLACEHOLDER: Record<ProviderChoice, string> = {
  OpenAiCompatible: "The model name your server lists",
  Anthropic: "claude-opus-5-5",
};

/** The provider names in the select. */
export const PROVIDER_LABEL: Record<ProviderChoice, string> = {
  OpenAiCompatible: "OpenAI-compatible server",
  Anthropic: "Anthropic (Claude)",
};

/** What the Claude Code choice says about where the meeting goes (PLAN.md 3.9 exception). */
export const CLAUDE_CODE_PRIVACY =
  "Your meeting's transcript, title, speaker names and notes are handed to Claude Code, which sends them to Anthropic on your Claude sign-in. Earshot cannot see that request; it records only when and how much it handed over.";

/** The placeholder of Claude Code's typed model name under Other (never a stored default). */
export const CLAUDE_MODEL_PLACEHOLDER = "claude-opus-5-5";

/** What the Codex choice says about where the meeting goes (PLAN.md 3.9 exception, T7.4gc). */
export const CODEX_PRIVACY =
  "Your meeting's transcript, title, speaker names and notes are handed to Codex, which sends them to OpenAI on your ChatGPT sign-in. Earshot cannot see that request; it records only when and how much it handed over.";

/** The placeholder of Codex's typed model name under Other (never a stored default). */
export const CODEX_MODEL_PLACEHOLDER = "gpt-5.6-luna";

/** The text of "Summarise during the meeting" (mockup). */
export const LIVE_DESCRIPTION =
  "The summary is ready sooner after you hang up. It may miss some points. Uses this computer while you talk.";

/** The warning for an endpoint that is not this computer or the local network. */
export function remoteWarning(host: string): string {
  return `The meeting's transcript, title, speaker names and notes are sent to ${host}. Nothing else leaves this computer.`;
}

/** One choice of step 1: a writer card with what it is and where the transcript goes. */
function Choice({
  value,
  label,
  detail,
  why,
  destination,
  selected,
}: {
  /** A step's key (`stepKey`), or `NoSummary`. */
  value: string;
  label: string;
  detail?: string;
  why?: string;
  /** Where the transcript goes (the order's copy). */
  destination?: string;
  selected: boolean;
}) {
  const styles = useStyles();
  return (
    <div className={mergeClasses(styles.card, selected && styles.selected)} data-step={value}>
      <Radio
        value={value}
        label={
          <span className={styles.title}>
            <Body1Strong>{label}</Body1Strong>
            {detail !== undefined && <Caption1 className={styles.muted}>{detail}</Caption1>}
          </span>
        }
      />
      {why !== undefined && <Caption1 className={styles.why}>{why}</Caption1>}
      {destination !== undefined && <Caption1 className={styles.destination}>{destination}</Caption1>}
    </div>
  );
}

/** The value of "No summary" among step 1's choices. */
const NO_SUMMARY = "NoSummary";

/** The endpoint form, the remote warning and the API key. `inUse`: a step of the order is the
 * endpoint; `onUse` stores the fields with the endpoint in its step. */
function EndpointForm({
  state,
  settings,
  keySaved,
  inUse,
  onUse,
  locked,
  saveOnNext,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  inUse: boolean;
  onUse(change: Partial<SummarySettingsValue>): Promise<boolean>;
  /** The order is not read yet (or could not be): nothing may store the endpoint in it. */
  locked: boolean;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const stored = settings.value;
  const [provider, setProvider] = useState<ProviderChoice>(stored.provider);
  const [url, setUrl] = useState(stored.url);
  const [model, setModel] = useState(stored.model);
  const [key, setKey] = useState("");
  const providerId = useId();
  // A stored change (a save) resets the drafts to what was stored.
  useEffect(() => {
    setProvider(stored.provider);
    setUrl(stored.url);
    setModel(stored.model);
  }, [stored.provider, stored.url, stored.model]);

  const anthropic = provider === "Anthropic";
  const changed = !inUse || provider !== stored.provider || url !== stored.url || model !== stored.model;
  const showWarning = inUse && settings.endpoint_host !== null && !settings.endpoint_local;
  const wizard = saveOnNext !== undefined;
  useSaveOnNext(saveOnNext, async () => {
    if (changed && (locked || !(await onUse({ provider, url, model })))) return false;
    if (!anthropic || key.trim() === "") return true;
    const saved = await state.saveKey(key);
    if (saved) setKey("");
    return saved;
  });

  return (
    <div className={styles.endpoint} role="group" aria-label="Your own server">
      <div className={styles.fields}>
        <Field label="Provider">
          <Select
            id={providerId}
            value={provider}
            onChange={(_, d) => {
              const p = d.value as ProviderChoice;
              setProvider(p);
              if (p === "Anthropic" && url.trim() === "") setUrl(settings.anthropic_url);
            }}
          >
            {(["OpenAiCompatible", "Anthropic"] as const).map((p) => (
              <option key={p} value={p}>
                {PROVIDER_LABEL[p]}
              </option>
            ))}
          </Select>
        </Field>
        <Field
          label="Address"
          hint={
            anthropic
              ? `The API address without a path, usually ${settings.anthropic_url}.`
              : "The server's API address without /chat/completions, for example http://127.0.0.1:8080/v1 for a server on this computer. Plain http only to this computer or the local network."
          }
        >
          <Input value={url} onChange={(_, d) => setUrl(d.value)} spellCheck={false} />
        </Field>
        <Field label="Model" hint="The model name the endpoint expects.">
          <Input
            value={model}
            placeholder={MODEL_PLACEHOLDER[provider]}
            onChange={(_, d) => setModel(d.value)}
            spellCheck={false}
          />
        </Field>
      </div>
      {!wizard && (
        <div className={styles.row}>
          <Button
            appearance="primary"
            disabled={state.busy || locked || !changed}
            onClick={() => void onUse({ provider, url, model })}
          >
            {inUse ? "Save endpoint" : "Use this endpoint"}
          </Button>
          {inUse && !changed && <Caption1 className={styles.muted}>Saved.</Caption1>}
        </div>
      )}
      {showWarning && (
        <MessageBar intent="warning">
          <MessageBarBody>{remoteWarning(settings.endpoint_host ?? "")}</MessageBarBody>
        </MessageBar>
      )}
      {anthropic && (
        <div className={styles.fields}>
          {keySaved === true ? (
            <div className={styles.row}>
              <Badge appearance="tint" color="success">
                Key saved
              </Badge>
              <Button disabled={state.busy} onClick={() => void state.removeKey()}>
                Remove key
              </Button>
            </div>
          ) : (
            <Field
              label="API key"
              hint="Kept in Windows Credential Manager, never in Earshot's database. It is sent only to the Anthropic address above."
            >
              <div className={styles.row}>
                <Input
                  type="password"
                  value={key}
                  autoComplete="off"
                  onChange={(_, d) => setKey(d.value)}
                />
                {!wizard && (
                  <Button
                    disabled={state.busy || key.trim() === ""}
                    onClick={() =>
                      void state.saveKey(key).then((ok) => {
                        if (ok) setKey("");
                      })
                    }
                  >
                    Save key
                  </Button>
                )}
              </div>
            </Field>
          )}
          {state.keyError !== null && (
            <MessageBar intent="error">
              <MessageBarBody>
                <MessageBarTitle>Earshot cannot tell whether a key is saved</MessageBarTitle>
                {state.keyError}
              </MessageBarBody>
            </MessageBar>
          )}
          {inUse && stored.provider === "Anthropic" && keySaved === false && (
            <MessageBar intent="warning">
              <MessageBarBody>No API key is saved, so Anthropic cannot write the summary. Save your key.</MessageBarBody>
            </MessageBar>
          )}
        </div>
      )}
    </div>
  );
}

/** A model in an agent CLI's list: the stored name ("" for the CLI's own default) and its
 * label. */
export interface ModelOption {
  value: string;
  label: string;
}

/** Claude Code's models (T12.18): its default and the aliases its `--model` takes for the
 * latest model of each family, so the list does not go stale. */
export const CLAUDE_MODELS: ModelOption[] = [
  { value: "", label: "Claude Code's default" },
  { value: "fable", label: "Fable" },
  { value: "opus", label: "Opus" },
  { value: "sonnet", label: "Sonnet" },
  { value: "haiku", label: "Haiku" },
];

/** Codex's models: its default only; any other is typed under Other. */
export const CODEX_MODELS: ModelOption[] = [{ value: "", label: "Codex's default" }];

/** Claude Code's effort levels (`--effort`), after its default. */
export const EFFORT_LABEL: Record<EffortChoice, string> = {
  Low: "Low",
  Medium: "Medium",
  High: "High",
  ExtraHigh: "Extra high",
  Max: "Max",
};

/** The list value of Other: never a model name (both CLIs refuse brackets). */
const OTHER_MODEL = "(other)";

/** How the stored model reads: its list label, or the typed name it was saved as. */
export function storedModelLabel(models: ModelOption[], stored: string): string {
  return models.find((m) => m.value === stored)?.label ?? stored;
}

/** Under Other while its name is empty (T12.45): choosing Other stores nothing, so the line
 * names the model still stored. */
export function otherKeepsText(label: string): string {
  return `Until you save a name, the model stays ${label}.`;
}

/** The effort list (Claude Code only, T12.18): a choice, applied at once. */
function EffortField({
  state,
  stored,
  note,
}: {
  state: SummariesPageState;
  stored: EffortChoice | null;
  /** Why the choice does nothing with the installed CLI, when it does nothing. */
  note: string | null;
}) {
  return (
    <Field
      label="Effort"
      hint={
        note ??
        "How much thought Claude puts into the summary. Higher can take longer and uses more of your Claude plan."
      }
    >
      <Select
        value={stored ?? ""}
        disabled={state.busy}
        onChange={(_, d) =>
          void state.save({ claude_effort: d.value === "" ? null : (d.value as EffortChoice) })
        }
      >
        <option value="">Claude Code's default</option>
        {(Object.keys(EFFORT_LABEL) as EffortChoice[]).map((e) => (
          <option key={e} value={e}>
            {EFFORT_LABEL[e]}
          </option>
        ))}
      </Select>
    </Field>
  );
}

/** An agent CLI's status, the model it is asked for and, for Claude Code, the effort: Claude
 * Code (T7.4ga) or Codex (T7.4gc). The lists apply at once; a name typed under Other is saved
 * with its button (T12.18). */
function AgentCliCard({
  state,
  inUse,
  name,
  group,
  check,
  recheck,
  stored,
  models,
  placeholder,
  hint,
  saveModel,
  effort,
  saveOnNext,
}: {
  state: SummariesPageState;
  /** Whether a step of the order is this CLI. */
  inUse: boolean;
  /** The CLI's product name ("Claude Code", "Codex"). */
  name: string;
  /** The card's accessible name. */
  group: string;
  check: CliCheck<{ state: string; message: string; fix: string | null }>;
  recheck(): Promise<void>;
  /** The stored model name. */
  stored: string;
  /** The listed models, the CLI's default first. */
  models: ModelOption[];
  /** The placeholder of Other's name. */
  placeholder: string;
  /** The hint of Other's name. */
  hint: string;
  /** The settings change that stores `model`. */
  saveModel(model: string): Partial<SummarySettingsView["value"]>;
  /** The effort list, when the CLI has one. */
  effort?: ReactNode;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const listed = models.some((m) => m.value === stored);
  const [otherOpen, setOtherOpen] = useState(!listed);
  const [typed, setTyped] = useState(listed ? "" : stored);
  const otherLabel = useId();
  // A stored change (a save) shows what was stored.
  useEffect(() => {
    setOtherOpen(!listed);
    setTyped(listed ? "" : stored);
  }, [stored, listed]);
  // Asked once when the card first shows; again on "Check again".
  useEffect(() => {
    if (check.status === "idle") void recheck();
  }, [check.status, recheck]);
  // An empty name under Other is no choice yet: saving it would store the CLI's default, which
  // has its own entry in the list.
  const changed = otherOpen && typed.trim() !== "" && typed.trim() !== stored;
  useSaveOnNext(saveOnNext, async () => !changed || state.save(saveModel(typed)));
  return (
    <div className={styles.endpoint} role="group" aria-label={group}>
      {check.status === "checking" && <Spinner size="tiny" label={`Checking ${name}`} />}
      {check.status === "failed" && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>Earshot could not check {name}</MessageBarTitle>
            {check.message}
          </MessageBarBody>
        </MessageBar>
      )}
      {check.status === "done" && (
        <MessageBar intent={check.view.state === "SignedIn" ? "success" : "warning"}>
          <MessageBarBody>
            <MessageBarTitle>{check.view.message}</MessageBarTitle>
            {check.view.fix}
          </MessageBarBody>
        </MessageBar>
      )}
      <div className={styles.row}>
        <Button disabled={check.status === "checking"} onClick={() => void recheck()}>
          Check again
        </Button>
      </div>
      <div className={styles.fields}>
        <Field label="Model">
          <Select
            value={otherOpen ? OTHER_MODEL : stored}
            disabled={state.busy}
            onChange={(_, d) => {
              if (d.value === OTHER_MODEL) {
                setOtherOpen(true);
                return;
              }
              setOtherOpen(false);
              void state.save(saveModel(d.value));
            }}
          >
            {models.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
            <option value={OTHER_MODEL}>Other</option>
          </Select>
        </Field>
        {otherOpen && (
          <div className={styles.fields} role="group" aria-labelledby={otherLabel}>
            <span id={otherLabel} hidden>
              Other model
            </span>
            <Field label="Model name" hint={hint}>
              <Input
                value={typed}
                placeholder={placeholder}
                onChange={(_, d) => setTyped(d.value)}
                spellCheck={false}
              />
            </Field>
            {typed.trim() === "" && (
              <Caption1 className={styles.muted}>{otherKeepsText(storedModelLabel(models, stored))}</Caption1>
            )}
            {saveOnNext === undefined && (
              <div className={styles.row}>
                <Button
                  appearance="primary"
                  disabled={state.busy || !changed}
                  onClick={() => void state.save(saveModel(typed))}
                >
                  Save model
                </Button>
                {inUse && !changed && stored !== "" && typed.trim() !== "" && (
                  <Caption1 className={styles.muted}>Saved.</Caption1>
                )}
              </div>
            )}
          </div>
        )}
        {effort}
      </div>
    </div>
  );
}

/** The props of the cards under a writer choice. */
interface WriterCardProps {
  state: SummariesPageState;
  settings: SummarySettingsView;
  /** Whether a step of the order is this writer. */
  inUse: boolean;
  saveOnNext?: SaveOnNext;
}

/** Claude Code's status, the model it is asked for and its effort (T7.4ga, T12.18). */
function ClaudeCodeCard({ state, settings, inUse, saveOnNext }: WriterCardProps) {
  const check = state.claudeCode;
  return (
    <AgentCliCard
      state={state}
      inUse={inUse}
      name="Claude Code"
      group="Claude through Claude Code"
      check={check}
      recheck={state.checkClaudeCode}
      stored={settings.value.claude_model}
      models={CLAUDE_MODELS}
      placeholder={CLAUDE_MODEL_PLACEHOLDER}
      hint="A model name Claude Code accepts, for example a full model name."
      saveModel={(m) => ({ claude_model: m })}
      effort={
        <EffortField
          state={state}
          stored={settings.value.claude_effort}
          note={check.status === "done" ? check.view.effort_note : null}
        />
      }
      saveOnNext={saveOnNext}
    />
  );
}

/** Codex's status and the model it is asked for (T7.4gc). */
function CodexCard({ state, settings, inUse, saveOnNext }: WriterCardProps) {
  return (
    <AgentCliCard
      state={state}
      inUse={inUse}
      name="Codex"
      group="OpenAI through Codex"
      check={state.codex}
      recheck={state.checkCodex}
      stored={settings.value.codex_model}
      models={CODEX_MODELS}
      placeholder={CODEX_MODEL_PLACEHOLDER}
      hint="A model name Codex accepts."
      saveModel={(m) => ({ codex_model: m })}
      saveOnNext={saveOnNext}
    />
  );
}

/** "Who writes the summary" (T12.63b, docs/summary-order.md 2): the order of summary writers.
 * Step 1 is a choice among the writers on this computer, the user's other computers that write
 * summaries, and "No summary" (which clears the order); steps 2 and 3 ("If it can't") are
 * optional and offer only writers not already used. Under each step the page says where the
 * transcript goes, and under the order each writer in it shows its form or card. Choices apply at
 * once; an endpoint without its address waits for its form's button. With `endpointUnderOther`
 * (the first-run wizard) there is one step, no computers, and your own endpoint comes last,
 * behind "Other". */
function WriterSection({
  state,
  settings,
  keySaved,
  endpointUnderOther = false,
  saveOnNext,
  localModels,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  endpointUnderOther?: boolean;
  saveOnNext?: SaveOnNext;
  /** The model state Settings holds: with it, the summary models show under "On this computer". */
  localModels?: ModelsPageState;
}) {
  const styles = useStyles();
  const writerLabel = useId();
  const localLabel = useId();
  const oneStep = endpointUnderOther;
  // The step whose endpoint waits for its form (null: none).
  const [pending, setPending] = useState<number | null>(null);
  const [otherOpen, setOtherOpen] = useState(false);
  const value = settings.value;
  const order = state.order;
  // Nothing is offered before the order is read: a choice then would replace an order nobody
  // has seen (the old writer setting holds only its first writer step).
  const locked = order === null;
  const stored: StepChoice[] = order !== null ? order.steps.map((s) => s.step) : [];
  const offered: StepView[] = order?.offered ?? [];
  const viewOf = (step: StepChoice) => offered.find((o) => sameStep(o.step, step));
  const hasEndpoint = value.url !== "" && value.model !== "";
  const maxSteps = oneStep ? 1 : (order?.max_steps ?? 3);
  const shown = locked ? "" : pending === 0 ? "Endpoint" : stored.length > 0 ? stepKey(stored[0]) : NO_SUMMARY;
  const loaded = localModels?.load.status === "loaded" ? localModels.load.data : null;
  const models = loaded?.models ?? state.models;
  const local =
    models === null
      ? undefined
      : `${titleOf(models, models.in_use.summary)} on the ${partName(models.in_use.summary_runs_on)}`;
  const inOrder = (kind: StepChoice["kind"]) => stored.some((s) => s.kind === kind);

  // `step` at `index`, a later copy of it removed (a writer appears once).
  const placed = (index: number, step: StepChoice): StepChoice[] => {
    const next = [...stored];
    next[index] = step;
    return next.filter((s, i) => !next.slice(0, i).some((t) => sameStep(s, t))).slice(0, maxSteps);
  };
  const chooseAt = (index: number, step: StepChoice | null) => {
    if (locked) return;
    if (step === null) {
      // "No summary" for step 1; "Nothing else" for a later step (the steps after it go too).
      setPending(null);
      void state.saveOrder(stored.slice(0, index));
      return;
    }
    if (step.kind === "Endpoint" && !hasEndpoint) {
      // No endpoint yet: show the form; the step is stored once it is saved.
      setPending(index);
      return;
    }
    setPending(null);
    void state.saveOrder(oneStep ? [step] : placed(index, step));
  };
  const stepOfKey = (key: string): StepChoice | null =>
    key === NO_SUMMARY || key === "" ? null : (offered.find((o) => stepKey(o.step) === key)?.step ?? null);
  const useEndpoint = async (change: Partial<SummarySettingsValue>) => {
    if (locked) return false;
    const at = pending ?? stored.findIndex((s) => s.kind === "Endpoint");
    const index = at < 0 ? stored.length : at;
    const steps = oneStep ? [{ kind: "Endpoint" } as StepChoice] : placed(index, { kind: "Endpoint" });
    const ok = await state.saveOrder(steps, change);
    if (ok) setPending(null);
    return ok;
  };
  const dest = (step: StepChoice) => viewOf(step)?.destination;
  const ownEndpoint = (
    <Choice
      value="Endpoint"
      label="Your own server"
      detail={hasEndpoint ? viewOf({ kind: "Endpoint" })?.label : undefined}
      why="An OpenAI-compatible server or Claude. The meeting's transcript, title, speaker names and notes are sent to the address you enter, and only there."
      destination={hasEndpoint ? dest({ kind: "Endpoint" }) : undefined}
      selected={shown === "Endpoint"}
    />
  );
  // What is stored always shows.
  const endpointOpen = otherOpen || shown === "Endpoint";
  const computers = oneStep ? [] : offered.filter((o) => o.step.kind === "Computer");
  // The later steps: each shown once the one before it is chosen.
  const later = oneStep
    ? []
    : Array.from({ length: maxSteps - 1 }, (_, k) => k + 1).filter((i) => stored.length >= i && stored.length > 0);
  const endpointShown = inOrder("Endpoint") || pending !== null;

  return (
    <Section title="Who writes the summary">
      <span id={writerLabel} hidden>
        Who writes the summary
      </span>
      {!oneStep && <Caption1 className={styles.muted}>{ORDER_COPY.page.explained}</Caption1>}
      {state.orderError !== null && (
        <MessageBar intent="error">
          <MessageBarBody>
            <MessageBarTitle>{ORDER_COPY.page.not_read}</MessageBarTitle>
            {state.orderError}
          </MessageBarBody>
        </MessageBar>
      )}
      {locked && state.orderError === null && <Spinner size="tiny" label={ORDER_COPY.page.reading} />}
      <RadioGroup
        aria-labelledby={writerLabel}
        value={shown}
        disabled={state.busy || locked}
        onChange={(_, d) => chooseAt(0, stepOfKey(d.value))}
        className={styles.choices}
      >
        <Choice
          value="Local"
          label="On this computer"
          detail={local}
          why="Nothing leaves this computer."
          destination={dest({ kind: "Local" })}
          selected={shown === "Local"}
        />
        {!endpointUnderOther && ownEndpoint}
        <Choice
          value="ClaudeCode"
          label="Claude (through Claude Code)"
          detail="Uses your Claude sign-in"
          why={CLAUDE_CODE_PRIVACY}
          destination={dest({ kind: "ClaudeCode" })}
          selected={shown === "ClaudeCode"}
        />
        <Choice
          value="Codex"
          label="OpenAI (through Codex)"
          detail="Uses your ChatGPT sign-in"
          why={CODEX_PRIVACY}
          destination={dest({ kind: "Codex" })}
          selected={shown === "Codex"}
        />
        {computers.map((c) => (
          <Choice
            key={stepKey(c.step)}
            value={stepKey(c.step)}
            label={c.label}
            destination={c.destination}
            selected={shown === stepKey(c.step)}
          />
        ))}
        <Choice
          value={NO_SUMMARY}
          label={ORDER_COPY.page.no_summary}
          why={ORDER_COPY.page.no_summary_why}
          selected={shown === NO_SUMMARY}
        />
        {endpointUnderOther && endpointOpen && ownEndpoint}
      </RadioGroup>
      {endpointUnderOther && shown !== "Endpoint" && (
        <Button
          appearance="subtle"
          className={styles.other}
          aria-expanded={otherOpen}
          onClick={() => setOtherOpen((open) => !open)}
        >
          Other
        </Button>
      )}
      {later.length > 0 && (
        <div className={styles.later} role="group" aria-label={ORDER_COPY.page.if_cant}>
          <Body1Strong>{ORDER_COPY.page.if_cant}</Body1Strong>
          {later.map((i) => {
            const current = stored[i];
            const value = current !== undefined ? stepKey(current) : pending === i ? "Endpoint" : "";
            const choices = offered.filter((o) => !stored.some((s, j) => j !== i && sameStep(s, o.step)));
            return (
              <Field
                key={i}
                label={fillCopy(ORDER_COPY.page.step, { n: String(i + 1) })}
                hint={current !== undefined ? dest(current) : undefined}
              >
                <Select
                  className={styles.laterSelect}
                  value={value}
                  disabled={state.busy || locked}
                  onChange={(_, d) => chooseAt(i, stepOfKey(d.value))}
                >
                  <option value="">{ORDER_COPY.page.nothing_else}</option>
                  {choices.map((o) => (
                    <option key={stepKey(o.step)} value={stepKey(o.step)}>
                      {o.label}
                    </option>
                  ))}
                </Select>
              </Field>
            );
          })}
        </div>
      )}
      {order !== null && order.notes.length > 0 && <UnknownSettings unknown={order.notes} />}
      {state.modelsError !== null && (
        <Caption1 className={styles.muted}>The model in use could not be read: {state.modelsError}</Caption1>
      )}
      {inOrder("Local") && localModels !== undefined && (
        <div className={styles.endpoint} role="group" aria-labelledby={localLabel}>
          <Body1Strong id={localLabel}>Which model writes the summary</Body1Strong>
          {localModels.load.status === "loaded" ? (
            <SummaryModelChoices state={localModels} data={localModels.load.data} />
          ) : (
            <ModelsNotLoaded load={localModels.load} />
          )}
        </div>
      )}
      {endpointShown && (
        <EndpointForm
          state={state}
          settings={settings}
          keySaved={keySaved}
          inUse={inOrder("Endpoint")}
          onUse={useEndpoint}
          locked={locked}
          saveOnNext={saveOnNext}
        />
      )}
      {inOrder("ClaudeCode") && (
        <ClaudeCodeCard state={state} settings={settings} inUse saveOnNext={saveOnNext} />
      )}
      {inOrder("Codex") && <CodexCard state={state} settings={settings} inUse saveOnNext={saveOnNext} />}
    </Section>
  );
}

/** What shows until the settings are read: the spinner or the failure. */
function SummariesNotLoaded({ load }: { load: Exclude<SummariesPageState["load"], { status: "loaded" }> }) {
  return (
    <NotLoaded load={load} loading="Reading the summary settings" failed="The summary settings could not be read" />
  );
}

/** A refusal, and stored settings this version does not know. */
function SummariesNotices({ state, settings }: { state: SummariesPageState; settings: SummarySettingsView }) {
  return (
    <>
      {state.notice !== null && (
        <NoticeBar title={state.notice.title} message={state.notice.message} onDismiss={state.dismiss} />
      )}
      <UnknownSettings unknown={settings.unknown} />
    </>
  );
}

/** Who writes the summary, on its own, for the first-run wizard: its Next saves what is typed. */
export function SummaryWriterChoices({ saveOnNext }: { saveOnNext: SaveOnNext }) {
  const page = usePageStyles();
  const state = useSummaries();
  const { load } = state;
  if (load.status !== "loaded") return <SummariesNotLoaded load={load} />;
  return (
    <div className={page.page}>
      <SummariesNotices state={state} settings={load.settings} />
      <WriterSection
        state={state}
        settings={load.settings}
        keySaved={load.keySaved}
        endpointUnderOther
        saveOnNext={saveOnNext}
      />
    </div>
  );
}

export interface SummariesPageProps {
  /** The model state Settings holds for its pages: the summary models show under "On this
   * computer", and a change of writer reads the models again (it changes what runs where). */
  models?: ModelsPageState;
}

/** The Summaries page of Settings. */
export function SummariesPage({ models }: SummariesPageProps) {
  const styles = useStyles();
  const page = usePageStyles();
  const state = useSummaries();
  const { load } = state;
  const writer = load.status === "loaded" ? load.settings.value.writer : null;
  const refresh = models?.refresh;
  const lastWriter = useRef<string | null>(null);
  useEffect(() => {
    if (writer === null) return;
    const before = lastWriter.current;
    lastWriter.current = writer;
    if (before !== null && before !== writer) void refresh?.();
  }, [writer, refresh]);

  if (load.status !== "loaded") return <SummariesNotLoaded load={load} />;
  const { settings, keySaved } = load;
  const value = settings.value;

  return (
    <div className={page.page}>
      <Title2 as="h1" className={styles.heading}>
        Summaries
      </Title2>
      <SummariesNotices state={state} settings={settings} />

      <WriterSection state={state} settings={settings} keySaved={keySaved} localModels={models} />

      <Section title="During the meeting">
        <Switch
          label="Summarise during the meeting"
          checked={value.live && settings.live_unavailable === null}
          disabled={state.busy || settings.live_unavailable !== null}
          onChange={(_, d) => void state.save({ live: d.checked })}
        />
        <Caption1 className={styles.muted}>{LIVE_DESCRIPTION}</Caption1>
        {settings.live_unavailable !== null && (
          <Caption1 className={styles.muted}>{settings.live_unavailable} Choose "On this computer" to use it.</Caption1>
        )}
      </Section>

      <Section title="Summary style">
        <SummaryStyleSection />
      </Section>
    </div>
  );
}
