// Settings, Summaries (docs/ui-mockups/t7-4.html, PLAN T7.4g): who writes the summary (one
// ordered list of this computer, your other computers and one Online service row whose picker
// names Claude through Claude Code (T7.4ga), OpenAI through Codex (T7.4gc), the Anthropic API or
// your own server (T14.72, docs/summary-order.md 12), or no summary; T12.68), the summary model
// when this computer writes (T12.5b, was on Models), the setup of the service picked (the
// endpoint with its API key for Anthropic, or Claude Code's or Codex's status and model, a list
// with Other, T12.18, and Claude Code's effort), summarising during the meeting, and the summary
// style (T7.4b, `SummaryStyleSection`). The setup section folds away (T12.96). Choices, the
// picker included, save as they change; the endpoint and a model name typed under Other save
// with their buttons. The API key is only ever sent to the app, never shown or read back.

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
  type OnlineRowView,
  type ProviderChoice,
  SERVICES,
  type ServiceChoice,
  type StepChoice,
  type StepView,
  type SummarySettingsValue,
  type SummarySettingsView,
  fillCopy,
  isEndpointService,
  isRemoteWriter,
  sameStep,
  serviceName,
  stepKey,
} from "../ipc";
import { endpointHost } from "../ipc/localHost";
import { ModelsNotLoaded, SummaryModelChoices } from "./ModelSections";
import { SummaryStyleSection } from "./SummaryStyleSection";
import { partName, titleOf } from "./copy";
import { FIELD_WIDTH, NotLoaded, NoticeBar, Section, TEXT_WIDTH, UnknownSettings, usePageStyles } from "./parts";
import { type SaveOnNext, useSaveOnNext } from "./saveOnNext";
import type { ModelsPageState } from "./useModels";
import { type CliCheck, type SummariesPageState, useSummaries } from "./useSummaries";

const useStyles = makeStyles({
  heading: {
    margin: 0,
  },
  title: {
    display: "inline-flex",
    flexWrap: "wrap",
    alignItems: "baseline",
    gap: tokens.spacingHorizontalS,
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
  // The Online service row's picker, under the row's name inside the text column, so it wraps
  // with the row and never leaves the bordered list.
  picker: {
    maxWidth: FIELD_WIDTH,
  },
  moves: {
    flexShrink: 0,
    display: "flex",
    gap: tokens.spacingHorizontalXXS,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: TEXT_WIDTH,
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
    maxWidth: FIELD_WIDTH,
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  // A writer's setup section (T12.96): the header that folds it, then what it holds.
  setupHead: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  setupToggle: {
    display: "flex",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
    width: "100%",
    margin: 0,
    padding: 0,
    border: "none",
    background: "none",
    color: "inherit",
    font: "inherit",
    textAlign: "start",
    cursor: "pointer",
    borderRadius: tokens.borderRadiusSmall,
    ":focus-visible": {
      outline: `${tokens.strokeWidthThick} solid ${tokens.colorStrokeFocus2}`,
      outlineOffset: tokens.strokeWidthThick,
    },
  },
  caret: {
    flexShrink: 0,
    transitionProperty: "transform",
    transitionDuration: tokens.durationFaster,
  },
  caretOpen: {
    transform: "rotate(90deg)",
  },
  setupBody: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
  },
});

function CaretIcon({ open }: { open: boolean }) {
  const styles = useStyles();
  return (
    <svg
      className={mergeClasses(styles.caret, open && styles.caretOpen)}
      width="16"
      height="16"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      aria-hidden="true"
    >
      <path d="M6 4l4 4-4 4" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

/** A writer's setup section under the order list (T12.96): a header with the writer's name and
 * a one-line state, folded away by default and opened by its caret. It opens by itself when
 * `attention` turns true (the writer is ticked but cannot run) and when `openRequest` changes
 * (the order list's "Set up"), which also focuses its first control. In the first-run wizard
 * (`fixed`) it is always open and has no caret. Its contents are not rendered while it is closed;
 * the component holding the drafts stays mounted, so a close loses nothing typed. */
export function SetupSection({
  name,
  state,
  group,
  attention,
  openRequest = 0,
  revealRequest = 0,
  fixed,
  children,
}: {
  name: string;
  state: string;
  /** The section's accessible name. */
  group: string;
  attention: boolean;
  openRequest?: number;
  /** Changes when the section must open without taking the focus (T14.72: a pick that left the
   * endpoint unticked because it is not set up keeps the focus on the picker). */
  revealRequest?: number;
  fixed: boolean;
  children: ReactNode;
}) {
  const styles = useStyles();
  const bodyId = useId();
  const body = useRef<HTMLDivElement>(null);
  const focusPending = useRef(false);
  const [openState, setOpen] = useState(false);
  const open = fixed || openState;
  useEffect(() => {
    if (attention) setOpen(true);
  }, [attention]);
  useEffect(() => {
    if (openRequest === 0) return;
    setOpen(true);
    focusPending.current = true;
  }, [openRequest]);
  useEffect(() => {
    if (revealRequest !== 0) setOpen(true);
  }, [revealRequest]);
  // The control to focus once a requested open shows. The request is spent on the first render
  // that shows the body, whether or not it holds a control: a control that appears later (a
  // reload) never takes the focus from where the user has gone since.
  useEffect(() => {
    if (!focusPending.current || !open || body.current === null) return;
    focusPending.current = false;
    const first = body.current.querySelector<HTMLElement>("select, input, textarea, button");
    if (first !== null) first.focus();
  });
  const head = (
    <span className={styles.title}>
      <Body1Strong>{name}</Body1Strong>
      <Caption1 className={styles.muted}>{state}</Caption1>
    </span>
  );
  return (
    <div className={styles.endpoint} role="group" aria-label={group}>
      {fixed ? (
        <div className={styles.setupHead}>{head}</div>
      ) : (
        <button
          type="button"
          className={styles.setupToggle}
          aria-label={setupHeaderName(name, state)}
          aria-expanded={open}
          aria-controls={open ? bodyId : undefined}
          onClick={() => setOpen((o) => !o)}
        >
          <CaretIcon open={open} />
          {head}
        </button>
      )}
      {open && (
        <div id={bodyId} ref={body} className={styles.setupBody}>
          {children}
        </div>
      )}
    </div>
  );
}

/** The model field's placeholder per provider (never a stored default). */
export const MODEL_PLACEHOLDER: Record<ProviderChoice, string> = {
  OpenAiCompatible: "The model name your server lists",
  Anthropic: "claude-opus-5-5",
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

/** The value of "No summary" on the Summaries page. */
const NO_SUMMARY = "NoSummary";

/** The value of "Write summaries" on the Summaries page. */
const WRITE = "Write";

/** The endpoint form, the remote warning and the API key: the setup section of the service
 * picked when it is Anthropic API or your own server (T14.72: the picker is the provider, so the
 * form has no provider of its own). `inUse`: a step of the order is the endpoint; `onUse` stores
 * the fields with the endpoint in its step. */
function EndpointForm({
  state,
  settings,
  keySaved,
  inUse,
  saveOnly = false,
  onUse,
  locked,
  openRequest,
  revealRequest,
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
  /** Changes when the order list's "Set up" asks for the form: it opens and takes the focus. */
  openRequest: number;
  /** Changes when a pick left the endpoint unticked because it is not set up: it opens and the
   * focus stays where it is (docs/summary-order.md 12.6). */
  revealRequest: number;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const stored = settings.value;
  // The provider is the service picked, as stored; nothing here changes it (12.7a).
  const anthropic = stored.provider === "Anthropic";
  // Anthropic's address, filled in as a draft over an empty address (12.7): nothing is stored
  // until the form is saved, and the fill alone is not an edit.
  const draftUrl = anthropic && stored.url === "" ? settings.anthropic_url : stored.url;
  const [url, setUrl] = useState(draftUrl);
  const [model, setModel] = useState(stored.model);
  const [key, setKey] = useState("");
  // The address is Anthropic's, filled in by picking Anthropic API over an empty address.
  const [prefilled, setPrefilled] = useState(draftUrl !== stored.url);
  // A stored change (a save, or another service picked) resets the drafts to what was stored.
  useEffect(() => {
    setUrl(draftUrl);
    setModel(stored.model);
    setPrefilled(draftUrl !== stored.url);
  }, [draftUrl, stored.url, stored.model]);

  const saves = inUse || saveOnly;
  const changed = !saves || url !== stored.url || model !== stored.model;
  const wizard = saveOnNext !== undefined;
  // Next saves the fields only when they differ from what is stored: an endpoint form nobody
  // typed in never holds Next. Picking Anthropic API alone fills in its address; with nothing
  // else typed (no model, no API key) that is not an edit. A typed key makes it one, so Next asks
  // for the model.
  const onlyPrefill =
    prefilled && url === settings.anthropic_url && model.trim() === "" && key.trim() === "" && stored.url === "";
  const edited = !onlyPrefill && (url !== stored.url || model !== stored.model);
  // Where the transcript would go: in the wizard the address being typed, as soon as it is one
  // (T12.79 review), else what is stored and in use.
  const typed = wizard && edited ? endpointHost(url) : null;
  const warnHost =
    typed !== null
      ? typed.local
        ? null
        : typed.host
      : inUse && settings.endpoint_host !== null && !settings.endpoint_local
        ? settings.endpoint_host
        : null;
  useSaveOnNext(saveOnNext, async () => {
    if (edited && (locked || !(await onUse({ url, model })))) return false;
    if (!anthropic || key.trim() === "") return true;
    const saved = await state.saveKey(key);
    if (saved) setKey("");
    return saved;
  });

  const setUp = stored.url !== "" && stored.model !== "";
  const noKey = stored.provider === "Anthropic" && keySaved === false;
  const stateLine = !setUp
    ? ORDER_COPY.page.not_set_up
    : setupState([stored.model, noKey ? SETUP_COPY.noApiKey : null]);
  // The section is named as the picker names the service.
  const name = serviceName(anthropic ? "AnthropicApi" : "OwnServer");

  return (
    <SetupSection
      name={name}
      state={stateLine}
      group={name}
      attention={inUse && (!setUp || noKey)}
      openRequest={openRequest}
      revealRequest={revealRequest}
      fixed={wizard}
    >
      <div className={styles.fields}>
        <Field
          label="Address"
          hint={
            anthropic
              ? `The API address without a path, usually ${settings.anthropic_url}.`
              : "The server's API address without /chat/completions, for example http://127.0.0.1:8080/v1 for a server on this computer. Plain http only to this computer or the local network."
          }
        >
          <Input
            value={url}
            onChange={(_, d) => {
              setUrl(d.value);
              setPrefilled(false);
            }}
            spellCheck={false}
          />
        </Field>
        <Field label="Model" hint="The model name the endpoint expects.">
          <Input
            value={model}
            placeholder={MODEL_PLACEHOLDER[stored.provider]}
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
            onClick={() => void onUse({ url, model })}
          >
            {saves ? "Save endpoint" : "Use this endpoint"}
          </Button>
          {saves && !changed && <Caption1 className={styles.muted}>Saved.</Caption1>}
        </div>
      )}
      {warnHost !== null && (
        <MessageBar intent="warning">
          <MessageBarBody>{remoteWarning(warnHost)}</MessageBarBody>
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
    </SetupSection>
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

/** The words of a writer's setup section header (T12.96): the parts of its state line, joined
 * by `separator`, and the separator between the writer's name and the state in the header's
 * accessible name. An endpoint not set up says `ORDER_COPY.page.not_set_up`, as its row does. */
export const SETUP_COPY = {
  separator: ", ",
  checking: "Checking",
  couldNotCheck: "Could not check",
  signedIn: "Signed in",
  notSignedIn: "Not signed in",
  notInstalled: "Not installed",
  signInUnknown: "Sign-in not known",
  noApiKey: "no API key",
  /** Claude Code's chosen effort ({effort}: its list label in lower case). */
  effort: "{effort} effort",
} as const;

/** A header's state line from its parts (null parts left out). */
export function setupState(parts: (string | null)[]): string {
  return parts.filter((p) => p !== null).join(SETUP_COPY.separator);
}

/** A header's accessible name: the writer, then its state. */
export function setupHeaderName(name: string, state: string): string {
  return `${name}${SETUP_COPY.separator}${state}`;
}

/** Claude Code's effort in its header's state line. */
export function effortStateText(effort: EffortChoice): string {
  return SETUP_COPY.effort.replace("{effort}", EFFORT_LABEL[effort].toLowerCase());
}

/** An agent CLI's state in its section's header (T12.96). */
export function cliStateText(check: CliCheck<{ state: string; message: string }>): string {
  switch (check.status) {
    case "idle":
    case "checking":
      return SETUP_COPY.checking;
    case "failed":
      return SETUP_COPY.couldNotCheck;
    case "done":
      switch (check.view.state) {
        case "SignedIn":
          return SETUP_COPY.signedIn;
        case "NotSignedIn":
          return SETUP_COPY.notSignedIn;
        case "NotInstalled":
          return SETUP_COPY.notInstalled;
        default:
          return SETUP_COPY.signInUnknown;
      }
  }
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
  effortState = null,
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
  /** The effort in the header's state line ("medium effort"), when one is chosen. */
  effortState?: string | null;
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
  const stateLine = setupState([
    cliStateText(check),
    stored === "" ? null : storedModelLabel(models, stored),
    effortState,
  ]);
  // Ticked but cannot run: not installed, signed out, or its state could not be read.
  const attention =
    inUse && (check.status === "failed" || (check.status === "done" && check.view.state !== "SignedIn"));
  return (
    <SetupSection name={name} state={stateLine} group={group} attention={attention} fixed={saveOnNext !== undefined}>
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
    </SetupSection>
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
      effortState={
        settings.value.claude_effort === null ? null : effortStateText(settings.value.claude_effort)
      }
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

/** The parts under the writers: the summary models when this computer writes, and the setup
 * section of the one online service picked (T14.72, docs/summary-order.md 12.8: the endpoint form
 * for Anthropic API or your own server, also while it is not set up so it can be, or Claude
 * Code's or Codex's card), folded away until opened (T12.96). The services not picked have no
 * section. Nothing shows while the order is unread (`locked`). */
function WriterForms({
  state,
  settings,
  keySaved,
  pick,
  onlineTicked,
  locked,
  endpointSaveOnly = false,
  localShown,
  useEndpoint,
  localModels,
  endpointRequest,
  endpointReveal,
  saveOnNext,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  /** The online service picked (null: none, or the order is unread). */
  pick: ServiceChoice | null;
  /** Whether the order holds the online service picked. */
  onlineTicked: boolean;
  locked: boolean;
  /** The endpoint form saves only its fields (a set-up endpoint not in the order). */
  endpointSaveOnly?: boolean;
  /** Whether the summary models show (this computer writes, or no summary model is installed). */
  localShown: boolean;
  useEndpoint(change: Partial<SummarySettingsValue>): Promise<boolean>;
  localModels?: ModelsPageState;
  /** Changes when the order list asks to set the endpoint up. */
  endpointRequest: number;
  /** Changes when a pick left the endpoint unticked because it is not set up. */
  endpointReveal: number;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const localLabel = useId();
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
      {!locked && isEndpointService(pick) && (
        <EndpointForm
          state={state}
          settings={settings}
          keySaved={keySaved}
          inUse={onlineTicked}
          saveOnly={endpointSaveOnly}
          onUse={useEndpoint}
          locked={locked}
          openRequest={endpointRequest}
          revealRequest={endpointReveal}
          saveOnNext={saveOnNext}
        />
      )}
      {!locked && pick === "ClaudeCode" && (
        <ClaudeCodeCard state={state} settings={settings} inUse={onlineTicked} saveOnNext={saveOnNext} />
      )}
      {!locked && pick === "Codex" && (
        <CodexCard state={state} settings={settings} inUse={onlineTicked} saveOnNext={saveOnNext} />
      )}
    </>
  );
}

/** This computer's row while the order is unread (nothing in it can be used until the order is
 * read). */
const UNREAD_LOCAL: StepView = {
  step: { kind: "Local" },
  label: ORDER_COPY.steps.local,
  destination: ORDER_COPY.destinations.local,
};

/** The Online service row while the order is unread: no service, nothing usable. */
const UNREAD_ONLINE: OnlineRowView = { service: null, step: null, reason: null, label: null, destination: null };

/** The Online service row's React key and `data-step`: a constant, so a change of service keeps
 * the row, and the picker its focus (docs/summary-order.md 12.8, T12.68's focus rule). */
export const ONLINE_ROW_KEY = "online";

function ArrowIcon({ up }: { up: boolean }) {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d={up ? "M4 10l4-4 4 4" : "M4 6l4 4 4-4"} strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

/** The order list's button that opens a writer's setup form (T12.96). */
export const SET_UP = "Set up";

/** Which control of a row keeps the keyboard focus after a change (T12.68; the Online service
 * row's picker, T14.72). */
type RowControl = "tick" | "up" | "down" | "picker";

/** One writer in the list: its rank while ticked ("Step n" to a screen reader), its tick box,
 * its name and kind, where it sends the transcript, what the writer does with the meeting (the
 * agent CLIs' privacy line, shown before it is ticked), why it cannot be ticked yet, the Online
 * service row's picker, and up and down while ticked. */
function WriterRow({
  view,
  rowKey,
  kind,
  privacy,
  rank,
  first,
  last,
  disabled,
  reason,
  showReason = true,
  note = null,
  picker,
  onTick,
  onMove,
  onSetUp,
}: {
  view: StepView;
  /** The row's `data-step` (default: its step's key). */
  rowKey?: string;
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
  /** Whether the reason shows as a badge beside the name (false: something else in the row
   * already says it, as the picker's "Choose a service"). */
  showReason?: boolean;
  /** A state shown beside its name that leaves it tickable (an agent CLI's sign-in). */
  note?: string | null;
  /** The Online service row's picker. */
  picker?: ReactNode;
  onTick(on: boolean): void;
  onMove(by: -1 | 1, from: RowControl): void;
  /** Opens and focuses the writer's setup form, for a writer not set up yet (T12.96). */
  onSetUp?: () => void;
}) {
  const styles = useStyles();
  const ticked = rank !== null;
  const writer = view.label;
  const stepId = useId();
  const destId = useId();
  const privacyId = useId();
  const hasDestination = view.destination !== "";
  const described = [ticked ? stepId : null, hasDestination ? destId : null, privacy !== undefined ? privacyId : null]
    .filter((id) => id !== null)
    .join(" ");
  return (
    <li
      className={mergeClasses(styles.writerRow, reason !== null && !ticked && styles.writerOff)}
      data-rank={rank ?? undefined}
      data-step={rowKey ?? stepKey(view.step)}
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
        aria-describedby={described === "" ? undefined : described}
        data-control="tick"
        checked={ticked}
        disabled={disabled || (reason !== null && !ticked)}
        onChange={(_, d) => onTick(d.checked === true)}
      />
      <div className={styles.writerText}>
        <span className={styles.title}>
          <Body1Strong>{writer}</Body1Strong>
          {kind !== undefined && <Caption1 className={styles.muted}>{kind}</Caption1>}
          {(showReason ? (reason ?? note) : note) !== null && (
            <Badge appearance="outline" color="informative" size="small">
              {showReason ? (reason ?? note) : note}
            </Badge>
          )}
          {reason !== null && !ticked && onSetUp !== undefined && (
            <Button
              appearance="transparent"
              size="small"
              aria-label={`${SET_UP} ${writer.toLowerCase()}`}
              disabled={disabled}
              onClick={onSetUp}
            >
              {SET_UP}
            </Button>
          )}
        </span>
        {hasDestination && (
          <Caption1 id={destId} className={styles.muted}>
            {view.destination}
          </Caption1>
        )}
        {privacy !== undefined && (
          <Caption1 id={privacyId} className={styles.muted}>
            {privacy}
          </Caption1>
        )}
        {picker}
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

/** The Online service row's picker (T14.72, docs/summary-order.md 12.8): the four services by
 * their step names, applied at once. With no service picked it reads "Choose a service". Not
 * disabled while a save is in flight (a change then is ignored), so it keeps the focus. */
function ServicePicker({
  service,
  disabled,
  onPick,
}: {
  service: ServiceChoice | null;
  disabled: boolean;
  onPick(service: ServiceChoice): void;
}) {
  const styles = useStyles();
  return (
    <Field label={ORDER_COPY.page.service_label} orientation="horizontal" className={styles.picker}>
      <Select
        data-control="picker"
        value={service ?? ""}
        disabled={disabled}
        onChange={(_, d) => {
          if (d.value !== "") onPick(d.value as ServiceChoice);
        }}
      >
        {service === null && (
          <option value="" disabled>
            {ORDER_COPY.page.choose_service}
          </option>
        )}
        {SERVICES.map((s) => (
          <option key={s} value={s}>
            {serviceName(s)}
          </option>
        ))}
      </Select>
    </Field>
  );
}

/** A row of the list: a step (this computer or another computer), or the one Online service
 * row. */
type Row = { type: "step"; view: StepView } | { type: "online" };

/** "Who writes the summary" on the Summaries page (T12.68, docs/summary-order.md 2): "Write
 * summaries" or "No summary", then one list of every writer this computer can use (this
 * computer, each other computer that offers summaries, and one Online service row with its picker,
 * T14.72). The ticked writers come first, in the order Earshot asks them, each with its rank and
 * up and down buttons; at most `max_steps` are ticked. A writer that cannot run yet (no service
 * picked, an endpoint not set up, this computer with no summary model) is listed, greyed, with
 * why, and cannot be ticked; "No summary" greys the whole list. Every change saves at once
 * (`set_summary_order`, the picker `set_online_service`), nothing changes while the order is
 * unread, and the keyboard focus stays on the control used, on its row wherever the row moved. */
function OrderSection({
  state,
  settings,
  keySaved,
  localModels,
  modelComesLater = false,
  saveOnNext,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  /** The model state Settings holds: with it, the summary models show under the list. */
  localModels?: ModelsPageState;
  /** This computer can be ticked with no summary model installed: the first-run wizard's
   * Download step fetches it (T12.79). */
  modelComesLater?: boolean;
  /** The first-run wizard: the forms save what was typed with its Next (no Save buttons). */
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const writerLabel = useId();
  const listLabel = useId();
  const list = useRef<HTMLUListElement>(null);
  // The control to focus once the change it made is shown.
  const focusNext = useRef<{ key: string; control: RowControl } | null>(null);
  // A tick refused because the order is full.
  const [full, setFull] = useState(false);
  // Counts the order list's requests to open the endpoint's form (T12.96), and the picks that
  // left the endpoint unticked because it is not set up (T14.72). Both go back to 0 at each pick,
  // so a form that mounts for a newly picked service neither opens nor takes the focus by itself.
  const [endpointRequest, setEndpointRequest] = useState(0);
  const [endpointReveal, setEndpointReveal] = useState(0);
  const reveals = useRef(0);
  const value = settings.value;
  const order = state.order;
  // Nothing is offered before the order is read: a change then would replace an order nobody
  // has seen.
  const locked = order === null;
  const stored: StepChoice[] = order !== null ? order.steps.map((s) => s.step) : [];
  const offered: StepView[] = order?.offered ?? [UNREAD_LOCAL];
  const online = order?.online ?? UNREAD_ONLINE;
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
  const noLocalModel = !modelComesLater && models !== null && !models.summary.some((m) => m.installed === "Yes");
  const local =
    models === null || !models.summary.some((m) => m.installed === "Yes")
      ? undefined
      : `${titleOf(models, models.in_use.summary)} on the ${partName(models.in_use.summary_runs_on)}`;
  // The online writer in the order (at most one, and it is the service picked: 12.5).
  const onlineAt = stored.findIndex(isRemoteWriter);
  const onlineTicked = onlineAt >= 0;
  const kindOfService = (service: ServiceChoice | null): string | undefined =>
    service === "ClaudeCode" ? "Uses your Claude sign-in" : service === "Codex" ? "Uses your ChatGPT sign-in" : undefined;
  const privacyOf = (service: ServiceChoice | null): string | undefined =>
    service === "ClaudeCode" ? CLAUDE_CODE_PRIVACY : service === "Codex" ? CODEX_PRIVACY : undefined;
  // What the last sign-in check found, said on the row too; the writer stays tickable. Only a
  // definite answer is said: a check not run (the CLI is asked once its card shows), running or
  // unreadable says nothing here.
  const noteOf = (service: ServiceChoice | null): string | null => {
    const check = service === "ClaudeCode" ? state.claudeCode : service === "Codex" ? state.codex : null;
    if (check?.status !== "done") return null;
    return check.view.state === "NotSignedIn"
      ? SETUP_COPY.notSignedIn
      : check.view.state === "NotInstalled"
        ? SETUP_COPY.notInstalled
        : null;
  };
  const viewOf = (step: StepChoice): StepView =>
    order?.steps.find((v) => sameStep(v.step, step)) ?? offered.find((o) => sameStep(o.step, step)) ?? UNREAD_LOCAL;
  const rows: Row[] = [
    ...stored.map((s): Row => (isRemoteWriter(s) ? { type: "online" } : { type: "step", view: viewOf(s) })),
    ...offered.filter((o) => !stored.some((s) => sameStep(s, o.step))).map((view): Row => ({ type: "step", view })),
    ...(onlineTicked ? [] : [{ type: "online" } as const]),
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
    if ((next.control === "up" || next.control === "down") && (target === null || (target as HTMLButtonElement).disabled)) {
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
  const keyOf = (step: StepChoice) => (isRemoteWriter(step) ? ONLINE_ROW_KEY : stepKey(step));
  const tickOne = (step: StepChoice, on: boolean) => {
    if (locked || state.busy) return;
    const focus = { key: keyOf(step), control: "tick" as const };
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
    save(next, { key: keyOf(stored[index]), control: from });
  };
  const choose = (key: string) => {
    if (locked || state.busy) return;
    setFull(false);
    if (key === NO_SUMMARY) void state.saveOrder([]);
    // "Write summaries" from no summary starts with this computer.
    else if (stored.length === 0) void state.saveOrder([{ kind: "Local" }]);
  };
  // The picker (12.6): applied at once. A pick that leaves the endpoint unticked because it is
  // not set up opens its setup section, the focus staying on the picker.
  const pick = (service: ServiceChoice) => {
    if (locked || state.busy || service === online.service) return;
    setFull(false);
    setEndpointRequest(0);
    setEndpointReveal(0);
    focusNext.current = { key: ONLINE_ROW_KEY, control: "picker" };
    void state.pickService(service).then((saved) => {
      if (saved === null || !onlineTicked || !isEndpointService(service)) return;
      if (!saved.order.steps.some((v) => isRemoteWriter(v.step))) {
        reveals.current += 1;
        setEndpointReveal(reveals.current);
      }
    });
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

  const onlineRow = () => {
    const ticked = onlineTicked ? stored[onlineAt] : null;
    const step = ticked ?? online.step;
    const view: StepView = {
      step: step ?? { kind: "Endpoint" },
      label: online.label ?? ORDER_COPY.page.online_service,
      destination: online.destination ?? "",
    };
    const service = online.service;
    return (
      <WriterRow
        key={ONLINE_ROW_KEY}
        view={view}
        rowKey={ONLINE_ROW_KEY}
        kind={kindOfService(service)}
        privacy={privacyOf(service)}
        rank={onlineTicked ? onlineAt + 1 : null}
        first={onlineAt === 0}
        last={onlineAt === stored.length - 1}
        disabled={off}
        reason={onlineTicked ? null : online.reason}
        // With no service picked the picker itself says "Choose a service": said once.
        showReason={service !== null}
        note={noteOf(service)}
        picker={<ServicePicker service={service} disabled={off} onPick={pick} />}
        onTick={(on) => {
          if (step !== null) tickOne(step, on);
        }}
        onMove={(by, from) => move(onlineAt, by, from)}
        onSetUp={
          isEndpointService(service) && online.step === null ? () => setEndpointRequest((n) => n + 1) : undefined
        }
      />
    );
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
        {rows.map((row) => {
          if (row.type === "online") return onlineRow();
          const view = row.view;
          const at = stored.findIndex((s) => sameStep(s, view.step));
          return (
            <WriterRow
              key={stepKey(view.step)}
              view={view}
              kind={view.step.kind === "Local" ? local : undefined}
              rank={at < 0 ? null : at + 1}
              first={at === 0}
              last={at === stored.length - 1}
              disabled={off}
              reason={view.step.kind === "Local" && noLocalModel ? ORDER_COPY.page.not_downloaded : null}
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
          pick={locked ? null : online.service}
          onlineTicked={onlineTicked}
          locked={locked}
          endpointSaveOnly={hasEndpoint && !endpointTicked}
          localShown={stored.some((s) => s.kind === "Local") || noLocalModel}
          useEndpoint={useEndpoint}
          localModels={localModels}
          endpointRequest={endpointRequest}
          endpointReveal={endpointReveal}
          saveOnNext={saveOnNext}
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

/** Who writes the summary, on its own, for the first-run wizard (T12.79): the Summaries page's
 * list of writers, its ticks and moves saved at once as on the page, and what is typed in a
 * writer's form saved with the wizard's Next. This computer can be ticked before its model is
 * downloaded, because the wizard's Download step fetches it. */
export function SummaryOrderChoices({ saveOnNext }: { saveOnNext: SaveOnNext }) {
  const page = usePageStyles();
  const state = useSummaries();
  const { load } = state;
  if (load.status !== "loaded") return <SummariesNotLoaded load={load} />;
  return (
    <div className={page.page}>
      <SummariesNotices state={state} settings={load.settings} />
      <OrderSection
        state={state}
        settings={load.settings}
        keySaved={load.keySaved}
        modelComesLater
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
