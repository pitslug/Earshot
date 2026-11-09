// Settings, Summaries (docs/ui-mockups/t7-4.html, PLAN T7.4g): who writes the summary (this
// computer, your own endpoint, Claude through Claude Code (T7.4ga), OpenAI through Codex
// (T7.4gc) or no summary), the endpoint with its API key for Anthropic, Claude Code's and
// Codex's status and model,
// summarising during the meeting, and the summary style (T7.4b, `SummaryStyleSection`). Choices
// save as they change; the endpoint saves with its button. The API key is only ever sent
// to the app, never shown or read back.

import {
  Badge,
  Body1Strong,
  Button,
  Caption1,
  Field,
  Input,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Radio,
  RadioGroup,
  Select,
  Spinner,
  Subtitle2,
  Switch,
  Title2,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type ReactNode, useEffect, useId, useState } from "react";

import type { ProviderChoice, SummarySettingsView, WriterChoice } from "../ipc";
import { SummaryStyleSection } from "./SummaryStyleSection";
import { partName, titleOf } from "./copy";
import { type SaveOnNext, useSaveOnNext } from "./saveOnNext";
import { type CliCheck, type SummariesPageState, useSummaries } from "./useSummaries";

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
  title: {
    display: "inline-flex",
    flexWrap: "wrap",
    alignItems: "baseline",
    gap: tokens.spacingHorizontalS,
  },
  why: {
    color: tokens.colorNeutralForeground2,
    paddingLeft: "32px",
    maxWidth: "62ch",
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

/** The Claude Code model field's placeholder (never a stored default). */
export const CLAUDE_MODEL_PLACEHOLDER = "opus";

/** What the Codex choice says about where the meeting goes (PLAN.md 3.9 exception, T7.4gc). */
export const CODEX_PRIVACY =
  "Your meeting's transcript, title, speaker names and notes are handed to Codex, which sends them to OpenAI on your ChatGPT sign-in. Earshot cannot see that request; it records only when and how much it handed over.";

/** The Codex model field's placeholder (never a stored default). */
export const CODEX_MODEL_PLACEHOLDER = "gpt-5.6-luna";

/** The text of "Summarise during the meeting" (mockup). */
export const LIVE_DESCRIPTION =
  "The summary is ready sooner after you hang up. It may miss some points. Uses this computer while you talk.";

/** The warning for an endpoint that is not this computer or the local network. */
export function remoteWarning(host: string): string {
  return `The meeting's transcript, title, speaker names and notes are sent to ${host}. Nothing else leaves this computer.`;
}

function Choice({
  value,
  label,
  detail,
  why,
  selected,
}: {
  value: WriterChoice;
  label: string;
  detail?: string;
  why: string;
  selected: boolean;
}) {
  const styles = useStyles();
  return (
    <div className={mergeClasses(styles.card, selected && styles.selected)} data-writer={value}>
      <Radio
        value={value}
        label={
          <span className={styles.title}>
            <Body1Strong>{label}</Body1Strong>
            {detail !== undefined && <Caption1 className={styles.muted}>{detail}</Caption1>}
          </span>
        }
      />
      <Caption1 className={styles.why}>{why}</Caption1>
    </div>
  );
}

/** The endpoint form, the remote warning and the API key. */
function EndpointForm({
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

  const inUse = stored.writer === "OwnEndpoint";
  const anthropic = provider === "Anthropic";
  const changed = !inUse || provider !== stored.provider || url !== stored.url || model !== stored.model;
  const showWarning = inUse && settings.endpoint_host !== null && !settings.endpoint_local;
  const wizard = saveOnNext !== undefined;
  useSaveOnNext(saveOnNext, async () => {
    if (changed && !(await state.save({ writer: "OwnEndpoint", provider, url, model }))) return false;
    if (!anthropic || key.trim() === "") return true;
    const saved = await state.saveKey(key);
    if (saved) setKey("");
    return saved;
  });

  return (
    <div className={styles.endpoint} role="group" aria-label="Your own endpoint">
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
            disabled={state.busy || !changed}
            onClick={() => void state.save({ writer: "OwnEndpoint", provider, url, model })}
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

/** An agent CLI's status and the model it is asked for: Claude Code (T7.4ga) or Codex
 * (T7.4gc). */
function AgentCliCard({
  state,
  writer,
  inUse,
  name,
  group,
  check,
  recheck,
  stored,
  placeholder,
  hint,
  saveModel,
  saveOnNext,
}: {
  state: SummariesPageState;
  /** The choice this card belongs to. */
  writer: WriterChoice;
  /** Whether that choice is the stored one. */
  inUse: boolean;
  /** The CLI's product name ("Claude Code", "Codex"). */
  name: string;
  /** The card's accessible name. */
  group: string;
  check: CliCheck<{ state: string; message: string; fix: string | null }>;
  recheck(): Promise<void>;
  /** The stored model name. */
  stored: string;
  placeholder: string;
  hint: string;
  /** The settings change that stores `model`. */
  saveModel(model: string): Partial<SummarySettingsView["value"]>;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const [model, setModel] = useState(stored);
  useEffect(() => setModel(stored), [stored]);
  // Asked once when the card first shows; again on "Check again".
  useEffect(() => {
    if (check.status === "idle") void recheck();
  }, [check.status, recheck]);
  const changed = model.trim() !== stored;
  useSaveOnNext(saveOnNext, async () => !changed || state.save({ writer, ...saveModel(model) }));
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
        <Field label="Model" hint={hint}>
          <Input value={model} placeholder={placeholder} onChange={(_, d) => setModel(d.value)} spellCheck={false} />
        </Field>
      </div>
      {saveOnNext === undefined && (
        <div className={styles.row}>
          <Button
            appearance="primary"
            disabled={state.busy || !changed}
            onClick={() => void state.save({ writer, ...saveModel(model) })}
          >
            Save model
          </Button>
          {inUse && !changed && <Caption1 className={styles.muted}>Saved.</Caption1>}
        </div>
      )}
    </div>
  );
}

/** The props of the cards under a writer choice. */
interface WriterCardProps {
  state: SummariesPageState;
  settings: SummarySettingsView;
  saveOnNext?: SaveOnNext;
}

/** Claude Code's status and the model it is asked for (T7.4ga). */
function ClaudeCodeCard({ state, settings, saveOnNext }: WriterCardProps) {
  return (
    <AgentCliCard
      state={state}
      writer="ClaudeCode"
      inUse={settings.value.writer === "ClaudeCode"}
      name="Claude Code"
      group="Claude through Claude Code"
      check={state.claudeCode}
      recheck={state.checkClaudeCode}
      stored={settings.value.claude_model}
      placeholder={CLAUDE_MODEL_PLACEHOLDER}
      hint="Optional. A model name Claude Code accepts, for example opus or sonnet. Leave it empty for Claude Code's own default."
      saveModel={(m) => ({ claude_model: m })}
      saveOnNext={saveOnNext}
    />
  );
}

/** Codex's status and the model it is asked for (T7.4gc). */
function CodexCard({ state, settings, saveOnNext }: WriterCardProps) {
  return (
    <AgentCliCard
      state={state}
      writer="Codex"
      inUse={settings.value.writer === "Codex"}
      name="Codex"
      group="OpenAI through Codex"
      check={state.codex}
      recheck={state.checkCodex}
      stored={settings.value.codex_model}
      placeholder={CODEX_MODEL_PLACEHOLDER}
      hint="Optional. A model name Codex accepts. Leave it empty for Codex's own default."
      saveModel={(m) => ({ codex_model: m })}
      saveOnNext={saveOnNext}
    />
  );
}

/** "Who writes the summary": the choices and, under the chosen one, its form or card. With
 * `endpointUnderOther` (the first-run wizard) your own endpoint comes last, behind "Other". */
function WriterSection({
  state,
  settings,
  keySaved,
  endpointUnderOther = false,
  saveOnNext,
}: {
  state: SummariesPageState;
  settings: SummarySettingsView;
  keySaved: boolean | null;
  endpointUnderOther?: boolean;
  saveOnNext?: SaveOnNext;
}) {
  const styles = useStyles();
  const writerLabel = useId();
  const [pendingEndpoint, setPendingEndpoint] = useState(false);
  const [otherOpen, setOtherOpen] = useState(false);
  const value = settings.value;
  const shown: WriterChoice = pendingEndpoint ? "OwnEndpoint" : value.writer;
  const models = state.models;
  const local =
    models === null
      ? undefined
      : `${titleOf(models, models.in_use.summary)} on the ${partName(models.in_use.summary_runs_on)}`;

  const choose = (w: WriterChoice) => {
    if (w === "OwnEndpoint" && !(value.url !== "" && value.model !== "")) {
      // No endpoint yet: show the form; it is used once saved.
      setPendingEndpoint(true);
      return;
    }
    setPendingEndpoint(false);
    void state.save({ writer: w });
  };
  const ownEndpoint = (
    <Choice
      value="OwnEndpoint"
      label="Your own endpoint"
      why="An OpenAI-compatible server or Claude. The meeting's transcript, title, speaker names and notes are sent to the address you enter, and only there."
      selected={shown === "OwnEndpoint"}
    />
  );
  // What is stored always shows.
  const endpointOpen = otherOpen || shown === "OwnEndpoint";

  return (
    <Section title="Who writes the summary">
      <span id={writerLabel} hidden>
        Who writes the summary
      </span>
      <RadioGroup
        aria-labelledby={writerLabel}
        value={shown}
        disabled={state.busy}
        onChange={(_, d) => choose(d.value as WriterChoice)}
        className={styles.choices}
      >
        <Choice
          value="OnThisComputer"
          label="On this computer"
          detail={local}
          why="Nothing leaves this computer."
          selected={shown === "OnThisComputer"}
        />
        {!endpointUnderOther && ownEndpoint}
        <Choice
          value="ClaudeCode"
          label="Claude (through Claude Code)"
          detail="Uses your Claude sign-in"
          why={CLAUDE_CODE_PRIVACY}
          selected={shown === "ClaudeCode"}
        />
        <Choice
          value="Codex"
          label="OpenAI (through Codex)"
          detail="Uses your ChatGPT sign-in"
          why={CODEX_PRIVACY}
          selected={shown === "Codex"}
        />
        <Choice
          value="NoSummary"
          label="No summary"
          why="Transcript and speakers only."
          selected={shown === "NoSummary"}
        />
        {endpointUnderOther && shown !== "OwnEndpoint" && (
          <Button
            appearance="subtle"
            className={styles.other}
            aria-expanded={otherOpen}
            onClick={() => setOtherOpen((open) => !open)}
          >
            Other
          </Button>
        )}
        {endpointUnderOther && endpointOpen && ownEndpoint}
      </RadioGroup>
      {state.modelsError !== null && (
        <Caption1 className={styles.muted}>The model in use could not be read: {state.modelsError}</Caption1>
      )}
      {shown === "OwnEndpoint" && (
        <EndpointForm state={state} settings={settings} keySaved={keySaved} saveOnNext={saveOnNext} />
      )}
      {shown === "ClaudeCode" && <ClaudeCodeCard state={state} settings={settings} saveOnNext={saveOnNext} />}
      {shown === "Codex" && <CodexCard state={state} settings={settings} saveOnNext={saveOnNext} />}
    </Section>
  );
}

/** What shows until the settings are read: the spinner or the failure. */
function SummariesNotLoaded({ load }: { load: Exclude<SummariesPageState["load"], { status: "loaded" }> }) {
  if (load.status === "loading") return <Spinner label="Reading the summary settings" />;
  return (
    <MessageBar intent="error">
      <MessageBarBody>
        <MessageBarTitle>The summary settings could not be read</MessageBarTitle>
        {load.message}
      </MessageBarBody>
    </MessageBar>
  );
}

/** A refusal, and stored settings this version does not know. */
function SummariesNotices({ state, settings }: { state: SummariesPageState; settings: SummarySettingsView }) {
  return (
    <>
      {state.notice !== null && (
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
      )}
      {settings.unknown.length > 0 && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>Some stored settings are not known to this version</MessageBarTitle>
            {settings.unknown.join(" ")}
          </MessageBarBody>
        </MessageBar>
      )}
    </>
  );
}

/** Who writes the summary, on its own, for the first-run wizard: its Next saves what is typed. */
export function SummaryWriterChoices({ saveOnNext }: { saveOnNext: SaveOnNext }) {
  const styles = useStyles();
  const state = useSummaries();
  const { load } = state;
  if (load.status !== "loaded") return <SummariesNotLoaded load={load} />;
  return (
    <div className={styles.page}>
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

/** The Summaries page of Settings. */
export function SummariesPage() {
  const styles = useStyles();
  const state = useSummaries();
  const { load } = state;

  if (load.status !== "loaded") return <SummariesNotLoaded load={load} />;
  const { settings, keySaved } = load;
  const value = settings.value;

  return (
    <div className={styles.page}>
      <Title2 as="h1" className={styles.heading}>
        Summaries
      </Title2>
      <SummariesNotices state={state} settings={settings} />

      <WriterSection state={state} settings={settings} keySaved={keySaved} />

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
