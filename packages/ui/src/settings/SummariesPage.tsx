// Settings, Summaries (docs/ui-mockups/t7-4.html, PLAN T7.4g): who writes the summary (one
// ordered list of this computer, your own endpoint, Claude through Claude Code (T7.4ga), OpenAI
// through Codex (T7.4gc) and your other computers, or no summary; T12.68), the summary model
// when this computer writes (T12.5b, was on Models),
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
  Checkbox,
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
  // The writers (T12.68): one bordered list, a row per writer. No width caps: the names and
  // where each writer sends the transcript wrap with the page.
  writerList: {
    listStyleType: "none",
    margin: 0,
    padding: 0,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  writerListOff: {
    opacity: 0.55,
  },
  writerRow: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    "&:not(:first-child)": {
      borderTop: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    },
  },
  writerOff: {
    color: tokens.colorNeutralForegroundDisabled,
  },
  rank: {
    flexShrink: 0,
    display: "inline-flex",
    alignItems: "center",
    justifyContent: "center",
    width: "24px",
    height: "24px",
    borderRadius: tokens.borderRadiusCircular,
    backgroundColor: tokens.colorBrandBackground,
    color: tokens.colorNeutralForegroundOnBrand,
    fontSize: tokens.fontSizeBase200,
    fontWeight: tokens.fontWeightSemibold,
  },
  rankEmpty: {
    backgroundColor: "transparent",
    border: `${tokens.strokeWidthThin} dashed ${tokens.colorNeutralStroke1}`,
    boxSizing: "border-box",
  },
  writerText: {
    flexGrow: 1,
    minWidth: 0,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
  },
  moves: {
    flexShrink: 0,
    display: "flex",
    gap: tokens.spacingHorizontalXXS,
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

/** The value of "Write summaries" on the Summaries page. */
const WRITE = "Write";

/** The endpoint form, the remote warning and the API key. `inUse`: a step of the order is the
 * endpoint; `onUse` stores the fields with the endpoint in its step. */
function EndpointForm({
  state,
  settings,
  keySaved,
  inUse,
  saveOnly = false,
  onUse,
  locked,
  saveOnNext,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  inUse: boolean;
  /** A set-up endpoint that is not in the order: saving changes only its fields (T12.68). */
  saveOnly?: boolean;
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
  const saves = inUse || saveOnly;
  const changed = !saves || provider !== stored.provider || url !== stored.url || model !== stored.model;
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
      <Body1Strong>Your own server</Body1Strong>
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
            {saves ? "Save endpoint" : "Use this endpoint"}
          </Button>
          {saves && !changed && <Caption1 className={styles.muted}>Saved.</Caption1>}
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

/** The parts under the writers: the summary models when this computer writes, the endpoint form
 * (also while the endpoint is not set up, so it can be), and the Claude Code and Codex cards for
 * the writers in the order. Nothing shows while the order is unread (`locked`). */
function WriterForms({
  state,
  settings,
  keySaved,
  stored,
  locked,
  endpointShown,
  endpointSaveOnly = false,
  localShown,
  useEndpoint,
  localModels,
  saveOnNext,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  stored: StepChoice[];
  locked: boolean;
  endpointShown: boolean;
  /** The endpoint form saves only its fields (a set-up endpoint not in the order). */
  endpointSaveOnly?: boolean;
  /** Whether the summary models show (this computer writes, or no summary model is installed). */
  localShown: boolean;
  useEndpoint(change: Partial<SummarySettingsValue>): Promise<boolean>;
  localModels?: ModelsPageState;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const localLabel = useId();
  const inOrder = (kind: StepChoice["kind"]) => stored.some((s) => s.kind === kind);
  return (
    <>
      {state.modelsError !== null && (
        <Caption1 className={styles.muted}>The model in use could not be read: {state.modelsError}</Caption1>
      )}
      {localShown && localModels !== undefined && (
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
          saveOnly={endpointSaveOnly}
          onUse={useEndpoint}
          locked={locked}
          saveOnNext={saveOnNext}
        />
      )}
      {inOrder("ClaudeCode") && (
        <ClaudeCodeCard state={state} settings={settings} inUse saveOnNext={saveOnNext} />
      )}
      {inOrder("Codex") && (
        <CodexCard state={state} settings={settings} inUse saveOnNext={saveOnNext} />
      )}
    </>
  );
}

/** "Who writes the summary" in the first-run wizard: one writer among this computer, Claude
 * Code, Codex and "No summary", with your own endpoint last, behind "Other". A choice applies at
 * once; an endpoint without its address waits for its form's button. */
function WriterSection({
  state,
  settings,
  keySaved,
  saveOnNext,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const writerLabel = useId();
  // Whether the endpoint waits for its form.
  const [pending, setPending] = useState(false);
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
  const shown = locked ? "" : pending ? "Endpoint" : stored.length > 0 ? stepKey(stored[0]) : NO_SUMMARY;
  const models = state.models;
  const local =
    models === null
      ? undefined
      : `${titleOf(models, models.in_use.summary)} on the ${partName(models.in_use.summary_runs_on)}`;

  const choose = (key: string) => {
    if (locked) return;
    const step = key === NO_SUMMARY || key === "" ? null : (offered.find((o) => stepKey(o.step) === key)?.step ?? null);
    if (step !== null && step.kind === "Endpoint" && !hasEndpoint) {
      // No endpoint yet: show the form; the step is stored once it is saved.
      setPending(true);
      return;
    }
    setPending(false);
    void state.saveOrder(step === null ? [] : [step]);
  };
  const useEndpoint = async (change: Partial<SummarySettingsValue>) => {
    if (locked) return false;
    const ok = await state.saveOrder([{ kind: "Endpoint" }], change);
    if (ok) setPending(false);
    return ok;
  };
  const dest = (step: StepChoice) => viewOf(step)?.destination;
  // What is stored always shows.
  const endpointOpen = otherOpen || shown === "Endpoint";

  return (
    <Section title="Who writes the summary">
      <span id={writerLabel} hidden>
        Who writes the summary
      </span>
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
        onChange={(_, d) => choose(d.value)}
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
        <Choice
          value={NO_SUMMARY}
          label={ORDER_COPY.page.no_summary}
          why={ORDER_COPY.page.no_summary_why}
          selected={shown === NO_SUMMARY}
        />
        {endpointOpen && (
          <Choice
            value="Endpoint"
            label="Your own server"
            detail={hasEndpoint ? viewOf({ kind: "Endpoint" })?.label : undefined}
            why="An OpenAI-compatible server or Claude. The meeting's transcript, title, speaker names and notes are sent to the address you enter, and only there."
            destination={hasEndpoint ? dest({ kind: "Endpoint" }) : undefined}
            selected={shown === "Endpoint"}
          />
        )}
      </RadioGroup>
      {shown !== "Endpoint" && (
        <Button
          appearance="subtle"
          className={styles.other}
          aria-expanded={otherOpen}
          onClick={() => setOtherOpen((open) => !open)}
        >
          Other
        </Button>
      )}
      {order !== null && order.notes.length > 0 && <UnknownSettings unknown={order.notes} />}
      <WriterForms
        state={state}
        settings={settings}
        keySaved={keySaved}
        stored={stored}
        locked={locked}
        endpointShown={stored.some((s) => s.kind === "Endpoint") || pending}
        localShown={stored.some((s) => s.kind === "Local")}
        useEndpoint={useEndpoint}
        saveOnNext={saveOnNext}
      />
    </Section>
  );
}

/** The writers the list always holds, in its order (the user's other computers follow). */
const WRITERS: StepChoice[] = [{ kind: "Local" }, { kind: "Endpoint" }, { kind: "ClaudeCode" }, { kind: "Codex" }];

/** A writer's row while the order is unread: the table's label and destination for a writer
 * not yet set up (nothing in it can be used until the order is read). */
function unreadView(step: StepChoice): StepView {
  const key =
    step.kind === "Local"
      ? "local"
      : step.kind === "ClaudeCode"
        ? "claude_code"
        : step.kind === "Codex"
          ? "codex"
          : "endpoint_unset";
  return { step, label: ORDER_COPY.steps[key], destination: ORDER_COPY.destinations[key] };
}

function ArrowIcon({ up }: { up: boolean }) {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d={up ? "M4 10l4-4 4 4" : "M4 6l4 4 4-4"} strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

/** Which control of a row keeps the keyboard focus after a change (T12.68). */
type RowControl = "tick" | "up" | "down";

/** One writer in the list: its rank while ticked ("Step n" to a screen reader), its tick box,
 * its name and kind, where it sends the transcript, what the writer does with the meeting (the
 * agent CLIs' privacy line, shown before it is ticked), why it cannot be ticked yet, and up and
 * down while ticked. */
function WriterRow({
  view,
  kind,
  privacy,
  rank,
  first,
  last,
  disabled,
  reason,
  onTick,
  onMove,
}: {
  view: StepView;
  /** What kind of writer it is, in a lighter span after its name. */
  kind?: string;
  /** Where the meeting goes, said in full (PLAN 3.9: before it is chosen). */
  privacy?: string;
  /** Its place in the order (1-based), or null when not ticked. */
  rank: number | null;
  first: boolean;
  last: boolean;
  /** Nothing in the row can be used (unread, no summary). */
  disabled: boolean;
  /** Why it cannot be ticked yet (null: it can). */
  reason: string | null;
  onTick(on: boolean): void;
  onMove(by: -1 | 1, from: RowControl): void;
}) {
  const styles = useStyles();
  const ticked = rank !== null;
  const writer = view.label;
  const stepId = useId();
  const destId = useId();
  const privacyId = useId();
  const described = [ticked ? stepId : null, destId, privacy !== undefined ? privacyId : null]
    .filter((id) => id !== null)
    .join(" ");
  return (
    <li
      className={mergeClasses(styles.writerRow, reason !== null && !ticked && styles.writerOff)}
      data-rank={rank ?? undefined}
      data-step={stepKey(view.step)}
    >
      {ticked ? (
        <>
          <span className={styles.rank} aria-hidden="true">
            {rank}
          </span>
          <span id={stepId} hidden>
            {fillCopy(ORDER_COPY.page.step, { n: String(rank) })}
          </span>
        </>
      ) : (
        <span className={mergeClasses(styles.rank, styles.rankEmpty)} aria-hidden="true" />
      )}
      <Checkbox
        aria-label={fillCopy(ORDER_COPY.page.use, { writer })}
        aria-describedby={described}
        data-control="tick"
        checked={ticked}
        disabled={disabled || (reason !== null && !ticked)}
        onChange={(_, d) => onTick(d.checked === true)}
      />
      <div className={styles.writerText}>
        <span className={styles.title}>
          <Body1Strong>{writer}</Body1Strong>
          {kind !== undefined && <Caption1 className={styles.muted}>{kind}</Caption1>}
          {reason !== null && (
            <Badge appearance="outline" color="informative" size="small">
              {reason}
            </Badge>
          )}
        </span>
        <Caption1 id={destId} className={styles.muted}>
          {view.destination}
        </Caption1>
        {privacy !== undefined && (
          <Caption1 id={privacyId} className={styles.muted}>
            {privacy}
          </Caption1>
        )}
      </div>
      {ticked && (
        <div className={styles.moves}>
          <Button
            appearance="subtle"
            size="small"
            icon={<ArrowIcon up />}
            data-control="up"
            aria-label={fillCopy(ORDER_COPY.page.move_up, { writer })}
            disabled={disabled || first}
            onClick={() => onMove(-1, "up")}
          />
          <Button
            appearance="subtle"
            size="small"
            icon={<ArrowIcon up={false} />}
            data-control="down"
            aria-label={fillCopy(ORDER_COPY.page.move_down, { writer })}
            disabled={disabled || last}
            onClick={() => onMove(1, "down")}
          />
        </div>
      )}
    </li>
  );
}

/** "Who writes the summary" on the Summaries page (T12.68, docs/summary-order.md 2): "Write
 * summaries" or "No summary", then one list of every writer this computer can use (this
 * computer, your own server, Claude Code, Codex, each other computer that offers summaries). The
 * ticked writers come first, in the order Earshot asks them, each with its rank and up and down
 * buttons; at most `max_steps` are ticked. A writer that cannot run yet (an endpoint not set up,
 * this computer with no summary model) is listed, greyed, with why, and cannot be ticked; "No
 * summary" greys the whole list. Every change saves the order at once through
 * `set_summary_order`, nothing changes while the order is unread, and the keyboard focus stays
 * on the control used, on its row wherever the row moved. */
function OrderSection({
  state,
  settings,
  keySaved,
  localModels,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  /** The model state Settings holds: with it, the summary models show under the list. */
  localModels?: ModelsPageState;
}) {
  const styles = useStyles();
  const writerLabel = useId();
  const listLabel = useId();
  const list = useRef<HTMLUListElement>(null);
  // The control to focus once the change it made is shown.
  const focusNext = useRef<{ key: string; control: RowControl } | null>(null);
  // A tick refused because the order is full.
  const [full, setFull] = useState(false);
  const value = settings.value;
  const order = state.order;
  // Nothing is offered before the order is read: a change then would replace an order nobody
  // has seen.
  const locked = order === null;
  const stored: StepChoice[] = order !== null ? order.steps.map((s) => s.step) : [];
  const offered: StepView[] = order?.offered ?? WRITERS.map(unreadView);
  const hasEndpoint = value.url !== "" && value.model !== "";
  const maxSteps = order?.max_steps ?? 3;
  const none = !locked && stored.length === 0;
  // Busy (a save in flight) does not disable anything, so the focused control keeps the focus;
  // a change while busy is ignored instead.
  const off = locked || none;
  const loaded = localModels?.load.status === "loaded" ? localModels.load.data : null;
  const models = loaded?.models ?? state.models;
  // No summary model on this computer (unknown while the models are not read: then nothing is
  // said).
  const noLocalModel = models !== null && !models.summary.some((m) => m.installed === "Yes");
  const local =
    models === null || noLocalModel
      ? undefined
      : `${titleOf(models, models.in_use.summary)} on the ${partName(models.in_use.summary_runs_on)}`;
  const kindOf = (step: StepChoice): string | undefined => {
    switch (step.kind) {
      case "Local":
        return local;
      case "ClaudeCode":
        return "Uses your Claude sign-in";
      case "Codex":
        return "Uses your ChatGPT sign-in";
      default:
        return undefined;
    }
  };
  const privacyOf = (step: StepChoice): string | undefined =>
    step.kind === "ClaudeCode" ? CLAUDE_CODE_PRIVACY : step.kind === "Codex" ? CODEX_PRIVACY : undefined;
  // Claude Code and Codex stay tickable whatever their sign-in: the CLI is asked only once its
  // card shows, and a run that finds it signed out moves to the next writer.
  const reasonOf = (step: StepChoice): string | null =>
    step.kind === "Endpoint" && !hasEndpoint
      ? ORDER_COPY.page.not_set_up
      : step.kind === "Local" && noLocalModel
        ? ORDER_COPY.page.not_downloaded
        : null;
  const viewOf = (step: StepChoice): StepView =>
    order?.steps.find((v) => sameStep(v.step, step)) ??
    offered.find((o) => sameStep(o.step, step)) ??
    unreadView(step);
  const rows: StepView[] = [
    ...stored.map(viewOf),
    ...offered.filter((o) => !stored.some((s) => sameStep(s, o.step))),
  ];

  // Back to the control used, on its row, once the change shows; an arrow that reached its end
  // hands the focus to the other one.
  useEffect(() => {
    const next = focusNext.current;
    if (next === null || state.busy || list.current === null) return;
    focusNext.current = null;
    const row = [...list.current.querySelectorAll<HTMLElement>("li[data-step]")].find(
      (r) => r.dataset.step === next.key,
    );
    const control = (c: RowControl) => row?.querySelector<HTMLElement>(`[data-control="${c}"]`) ?? null;
    let target = control(next.control);
    if (next.control !== "tick" && (target === null || (target as HTMLButtonElement).disabled)) {
      target = control(next.control === "up" ? "down" : "up");
      if (target !== null && (target as HTMLButtonElement).disabled) target = null;
    }
    (target ?? control("tick"))?.focus();
  });

  const save = (steps: StepChoice[], focus: { key: string; control: RowControl }) => {
    if (locked || state.busy) return;
    setFull(false);
    focusNext.current = focus;
    void state.saveOrder(steps);
  };
  const tickOne = (step: StepChoice, on: boolean) => {
    if (locked || state.busy) return;
    const focus = { key: stepKey(step), control: "tick" as const };
    if (!on) {
      save(
        stored.filter((s) => !sameStep(s, step)),
        focus,
      );
      return;
    }
    if (stored.length >= maxSteps) {
      setFull(true);
      return;
    }
    save([...stored, step], focus);
  };
  const move = (index: number, by: -1 | 1, from: RowControl) => {
    const to = index + by;
    if (to < 0 || to >= stored.length) return;
    const next = [...stored];
    [next[index], next[to]] = [next[to], next[index]];
    save(next, { key: stepKey(stored[index]), control: from });
  };
  const choose = (key: string) => {
    if (locked || state.busy) return;
    setFull(false);
    if (key === NO_SUMMARY) void state.saveOrder([]);
    // "Write summaries" from no summary starts with this computer.
    else if (stored.length === 0) void state.saveOrder([{ kind: "Local" }]);
  };
  const endpointTicked = stored.some((s) => s.kind === "Endpoint");
  const useEndpoint = async (change: Partial<SummarySettingsValue>) => {
    if (locked) return false;
    // A set-up endpoint that is not ticked: only its fields change.
    if (endpointTicked || hasEndpoint) {
      setFull(false);
      return state.saveOrder(stored, change);
    }
    // An endpoint the user just set up is ticked last; with the order full that is refused
    // before anything is stored, and the form keeps what was typed.
    if (stored.length >= maxSteps) {
      setFull(true);
      return false;
    }
    setFull(false);
    return state.saveOrder([...stored, { kind: "Endpoint" }], change);
  };

  return (
    <Section title="Who writes the summary">
      <span id={writerLabel} hidden>
        Who writes the summary
      </span>
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
        value={locked ? "" : none ? NO_SUMMARY : WRITE}
        disabled={locked}
        onChange={(_, d) => choose(d.value)}
      >
        <Radio value={WRITE} label={ORDER_COPY.page.write} />
        <Radio
          value={NO_SUMMARY}
          label={
            <span className={styles.title}>
              <span>{ORDER_COPY.page.no_summary}</span>
              <Caption1 className={styles.muted}>{ORDER_COPY.page.no_summary_why}</Caption1>
            </span>
          }
        />
      </RadioGroup>
      <Caption1 className={styles.muted}>{ORDER_COPY.page.explained}</Caption1>
      <span id={listLabel} hidden>
        {ORDER_COPY.page.list}
      </span>
      <ul
        ref={list}
        className={mergeClasses(styles.writerList, none && styles.writerListOff)}
        aria-labelledby={listLabel}
      >
        {rows.map((view) => {
          const at = stored.findIndex((s) => sameStep(s, view.step));
          return (
            <WriterRow
              key={stepKey(view.step)}
              view={view}
              kind={kindOf(view.step)}
              privacy={privacyOf(view.step)}
              rank={at < 0 ? null : at + 1}
              first={at === 0}
              last={at === stored.length - 1}
              disabled={off}
              reason={reasonOf(view.step)}
              onTick={(on) => tickOne(view.step, on)}
              onMove={(by, from) => move(at, by, from)}
            />
          );
        })}
      </ul>
      {full && (
        <MessageBar intent="warning">
          <MessageBarBody>{fillCopy(ORDER_COPY.page.too_many, { n: String(maxSteps) })}</MessageBarBody>
        </MessageBar>
      )}
      <Caption1 className={styles.muted}>{ORDER_COPY.page.kept_local}</Caption1>
      {order !== null && order.notes.length > 0 && <UnknownSettings unknown={order.notes} />}
      {!none && (
        <WriterForms
          state={state}
          settings={settings}
          keySaved={keySaved}
          stored={stored}
          locked={locked}
          endpointShown={!locked}
          endpointSaveOnly={hasEndpoint && !endpointTicked}
          localShown={stored.some((s) => s.kind === "Local") || noLocalModel}
          useEndpoint={useEndpoint}
          localModels={localModels}
        />
      )}
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
      <WriterSection state={state} settings={load.settings} keySaved={load.keySaved} saveOnNext={saveOnNext} />
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

      <OrderSection state={state} settings={settings} keySaved={keySaved} localModels={models} />

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
