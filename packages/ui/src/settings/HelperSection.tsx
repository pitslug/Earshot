// Summaries, "Another computer" (T11.3db, docs/sync.md section 10): each of the user's other
// computers that writes summaries for the others, as "On <device> (<writer kind>)", beside this
// computer's writer, which still runs when the chosen one is not available. A choice applies at
// once. Shown only once sync can ask another computer (or one is still chosen).

import { Caption1, MessageBar, MessageBarBody, Radio, RadioGroup, makeStyles, tokens } from "@fluentui/react-components";
import { useCallback, useEffect, useId, useState } from "react";

import { helperLabel } from "../detail/ResummariseControl";
import { type SummaryHelperView, type SummaryHelpersView, errorText, useIpc } from "../ipc";
import { Section } from "./parts";

const useStyles = makeStyles({
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  choices: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
  },
});

/** What the section says the choice does. */
export const HELPER_EXPLAINED =
  "The chosen computer writes this computer's summaries with its own summary writer, so the meeting's transcript goes wherever that writer sends it. If it is not online, not idle or no longer writes summaries, the writer above writes the summary here and the meeting says so.";

/** The choice that asks no other computer. */
export const HERE_ONLY = "No, write it here";

/** A computer's state under its choice. */
export function helperState(h: SummaryHelperView): string {
  if (!h.offers) return h.online ? "No longer writes summaries" : "Not online";
  if (!h.online) return "Not online";
  if (h.recording) return "Recording a meeting now";
  if (h.writer === null) return "No summary writer set up";
  return "Online";
}

/** The Summaries page's "Another computer" section. */
export function HelperSection() {
  const ipc = useIpc();
  const styles = useStyles();
  const label = useId();
  const [view, setView] = useState<SummaryHelpersView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const load = useCallback(async () => {
    try {
      setView(await ipc.summaryHelpers());
    } catch (e) {
      setError(errorText(e));
    }
  }, [ipc]);
  useEffect(() => {
    void load();
  }, [load]);

  if (view === null || !(view.sync_ready || view.chosen !== null)) return null;
  const choose = async (value: string) => {
    setBusy(true);
    try {
      setView(await ipc.setSummaryHelper(value === "" ? null : value));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Section title="Another computer">
      <span id={label} hidden>
        Ask another computer to write the summary
      </span>
      <RadioGroup
        aria-labelledby={label}
        value={view.chosen ?? ""}
        disabled={busy}
        onChange={(_, d) => void choose(d.value)}
        className={styles.choices}
      >
        <Radio value="" label={HERE_ONLY} />
        {view.helpers.map((h) => (
          <Radio
            key={h.id}
            value={h.id}
            label={{
              children: (
                <>
                  {helperLabel(h.name, h.short_id, h.writer)}
                  <br />
                  <Caption1 className={styles.muted}>{helperState(h)}</Caption1>
                </>
              ),
            }}
          />
        ))}
      </RadioGroup>
      {view.sync_ready && view.helpers.length === 0 && (
        <Caption1 className={styles.muted}>
          None of your other computers writes summaries yet. Turn on "Write summaries for my other computers" on
          the Sync page of the computer that should.
        </Caption1>
      )}
      <Caption1 className={styles.muted}>{HELPER_EXPLAINED}</Caption1>
      {view.problem !== null && (
        <MessageBar intent="warning">
          <MessageBarBody>{view.problem}</MessageBarBody>
        </MessageBar>
      )}
      {error !== null && (
        <MessageBar intent="error">
          <MessageBarBody>{error}</MessageBarBody>
        </MessageBar>
      )}
    </Section>
  );
}
