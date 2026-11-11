// Settings, Notes and storage, Recordings (PLAN T7.4k, T12.5b, docs/ui-design.md "Settings"): how
// long a meeting's recordings are kept after its transcript is ready. A choice that deletes
// recordings now is confirmed first with how many go.

import {
  Body1,
  Button,
  Caption1,
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  DialogTitle,
  Field,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Select,
  Spinner,
  Subtitle2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useId } from "react";

import { deleteText, useStorage } from "./useStorage";

/** What the page says the setting does. */
export const RETENTION_DESCRIPTION =
  "Each meeting's recordings are kept for this long after its transcript is ready. Transcripts, summaries and speaker names are always kept.";

/** Why the recordings matter. */
export const RETENTION_NEEDED =
  "The recordings are needed to transcribe a meeting again and to play speaker samples. A meeting whose transcript is not ready yet keeps its recordings.";

const useStyles = makeStyles({
  section: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  heading: {
    margin: 0,
  },
  text: {
    maxWidth: "62ch",
  },
  muted: {
    color: tokens.colorNeutralForeground3,
    maxWidth: "62ch",
  },
  field: {
    maxWidth: "240px",
  },
});

/** How long recordings are kept, a section of the Notes and storage page. */
export function RetentionSection() {
  const styles = useStyles();
  const state = useStorage();
  const headingId = useId();
  const { load, pending } = state;

  if (load.status === "loading") return <Spinner label="Reading the storage settings" />;
  if (load.status === "failed") {
    return (
      <MessageBar intent="error">
        <MessageBarBody>
          <MessageBarTitle>The storage settings could not be read</MessageBarTitle>
          {load.message}
        </MessageBarBody>
      </MessageBar>
    );
  }
  const { view } = load;

  return (
    <>
      {state.notice !== null && (
        <MessageBar intent={state.notice.intent}>
          <MessageBarBody>
            {state.notice.title !== undefined && <MessageBarTitle>{state.notice.title}</MessageBarTitle>}
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
      {view.problem !== null && (
        <MessageBar intent="warning">
          <MessageBarBody>{view.problem}</MessageBarBody>
        </MessageBar>
      )}
      {view.unknown.length > 0 && (
        <MessageBar intent="warning">
          <MessageBarBody>
            <MessageBarTitle>Some stored settings are not known to this version</MessageBarTitle>
            {view.unknown.join(" ")}
          </MessageBarBody>
        </MessageBar>
      )}
      <section className={styles.section} aria-labelledby={headingId}>
        <Subtitle2 as="h2" id={headingId} className={styles.heading}>
          Recordings
        </Subtitle2>
        <Body1 className={styles.text}>{RETENTION_DESCRIPTION}</Body1>
        <Field label="Keep recordings for" className={styles.field}>
          <Select
            value={pending?.value ?? view.retention}
            disabled={state.busy}
            onChange={(_, d) => void state.choose(d.value)}
          >
            {view.options.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </Select>
        </Field>
        <Caption1 className={styles.muted}>{RETENTION_NEEDED}</Caption1>
      </section>

      <Dialog
        modalType="alert"
        open={pending !== null}
        onOpenChange={(_, d) => {
          if (!d.open) state.cancel();
        }}
      >
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Delete recordings?</DialogTitle>
            <DialogContent>
              {pending !== null &&
                `${deleteText(pending.count)} Keeping recordings for ${pending.label.toLowerCase()} applies to past meetings too. Their transcripts, summaries and speaker names stay.`}
            </DialogContent>
            <DialogActions>
              <Button appearance="primary" disabled={state.busy} onClick={() => void state.confirm()}>
                Delete
              </Button>
              <Button disabled={state.busy} onClick={state.cancel}>
                Cancel
              </Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </>
  );
}
