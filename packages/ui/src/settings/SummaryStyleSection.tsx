// Settings, Summaries, "Summary style" (T7.4b): the user's own words about tone, length,
// emphasis and jargon, used when the summary is written. The fixed rules (what a summary
// must contain, its sources) and the note layout are not part of it.

import {
  Button,
  Caption1,
  Field,
  MessageBar,
  MessageBarBody,
  MessageBarTitle,
  Spinner,
  Textarea,
  makeStyles,
  tokens,
} from "@fluentui/react-components";

import { useSummaryStyle } from "./useSummaryStyle";

const useStyles = makeStyles({
  body: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
    maxWidth: "520px",
  },
  row: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
});

/** The field's example text. */
export const STYLE_PLACEHOLDER =
  "For example: short and plain. Lead with risks and decisions. Spell out acronyms.";

/** What the field changes and what it cannot. */
export const STYLE_HINT =
  "Changes the tone, length and emphasis of the summary. It cannot change what a summary must contain: every decision and action item, with its sources, stays.";

/** The style field, Save and Reset to default. */
export function SummaryStyleSection() {
  const styles = useStyles();
  const state = useSummaryStyle();
  const { load } = state;

  if (load.status === "loading") return <Spinner size="small" label="Reading the summary style" />;
  if (load.status === "failed") {
    return (
      <MessageBar intent="error">
        <MessageBarBody>
          <MessageBarTitle>The summary style could not be read</MessageBarTitle>
          {load.message}
        </MessageBarBody>
      </MessageBar>
    );
  }
  const { view } = load;
  const count = [...state.draft].length;
  const isDefault = view.style === "" && state.draft === "";

  return (
    <div className={styles.body}>
      {view.unknown.length > 0 && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>The stored summary style is not used</MessageBarTitle>
            {view.unknown.join(" ")}
          </MessageBarBody>
        </MessageBar>
      )}
      <Field
        label="How the summary should read"
        hint={`${STYLE_HINT} ${count} of ${view.max_chars} characters.`}
        validationState={state.error !== null ? "error" : "none"}
        validationMessage={state.error ?? undefined}
      >
        <Textarea
          value={state.draft}
          placeholder={STYLE_PLACEHOLDER}
          resize="vertical"
          rows={4}
          onChange={(_, d) => state.setDraft(d.value)}
        />
      </Field>
      <div className={styles.row}>
        <Button
          appearance="primary"
          disabled={state.busy || !state.changed}
          onClick={() => void state.save()}
        >
          Save
        </Button>
        <Button disabled={state.busy || isDefault} onClick={() => void state.reset()}>
          Reset to default
        </Button>
        {state.saved && !state.changed && <Caption1 className={styles.muted}>Saved.</Caption1>}
      </div>
    </div>
  );
}
