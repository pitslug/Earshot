// The smaller model parts of Settings (System and Transcription) and the first-run wizard:
// hardware rows, the recommendation, the check line, "where it runs", meeting languages and the
// notices.

import {
  Badge,
  Body1,
  Body1Strong,
  Button,
  Caption1,
  Combobox,
  Field,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
  Option,
  Select,
  Spinner,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { useId } from "react";

import type { HardwareView, ModelsView, PartChoice, RunsOn } from "../ipc";
import { hardwareRows, languageName, partName, partOf, titleOf, verdictTag } from "./copy";
import type { Checking, Notice } from "./useModels";

const useStyles = makeStyles({
  hw: {
    display: "grid",
    gridTemplateColumns: "repeat(auto-fit, minmax(160px, 1fr))",
    gap: tokens.strokeWidthThin,
    backgroundColor: tokens.colorNeutralStroke2,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    borderRadius: tokens.borderRadiusMedium,
    overflow: "hidden",
  },
  cell: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    backgroundColor: tokens.colorNeutralBackground2,
    minWidth: 0,
  },
  label: {
    color: tokens.colorNeutralForeground3,
  },
  on: {
    color: tokens.colorPaletteGreenForeground1,
  },
  off: {
    color: tokens.colorNeutralForeground3,
  },
  rec: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    padding: `${tokens.spacingVerticalM} ${tokens.spacingHorizontalL}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorBrandBackground2,
    color: tokens.colorNeutralForeground1,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  check: {
    display: "flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalM,
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    borderRadius: tokens.borderRadiusMedium,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    backgroundColor: tokens.colorNeutralBackground2,
  },
  push: {
    marginLeft: "auto",
  },
  runs: {
    display: "grid",
    gridTemplateColumns: "max-content minmax(0, 320px)",
    columnGap: tokens.spacingHorizontalL,
    rowGap: tokens.spacingVerticalS,
    alignItems: "center",
  },
  notices: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalS,
  },
  languages: {
    maxWidth: "420px",
  },
});

/** "This is your hardware". */
export function HardwareSection({ hardware, models }: { hardware: HardwareView; models: ModelsView }) {
  const styles = useStyles();
  return (
    <div className={styles.hw} role="list" aria-label="This is your hardware">
      {hardwareRows(hardware, models).map((row) => (
        <div key={row.label} className={styles.cell} role="listitem" aria-label={row.label}>
          <Caption1 className={styles.label}>{row.label}</Caption1>
          {row.names.map((n) => (
            <Body1Strong key={n}>{n}</Body1Strong>
          ))}
          {row.use !== "" && (
            <Caption1 className={mergeClasses(row.used ? styles.on : styles.off)}>{row.use}</Caption1>
          )}
          {row.through !== undefined && row.through !== "" && (
            <Caption1 className={styles.label}>{row.through}</Caption1>
          )}
        </div>
      ))}
    </div>
  );
}

/** "On this computer we recommend ...". */
export function Recommendation({ models }: { models: ModelsView }) {
  const styles = useStyles();
  const r = models.recommended;
  const s = models.summary_recommended;
  return (
    <section className={styles.rec} aria-label="Recommendation">
      <Body1Strong>On this computer we recommend</Body1Strong>
      <Body1>
        {r === null ? (
          "No transcription model covers all your meeting languages. "
        ) : (
          <>
            Transcription with{" "}
            <strong>
              {titleOf(models, r.model)} on the {partName(r.runs_on)}
            </strong>
            ,{" "}
          </>
        )}
        {r === null ? "Summaries with " : "summaries with "}
        <strong>
          {titleOf(models, s.model)} on the {partName(s.runs_on)}
        </strong>
        .
      </Body1>
      <Caption1>
        {r !== null && `Transcription: ${r.reason}. `}Summaries: {s.reason}.
      </Caption1>
    </section>
  );
}

/** The check line: the stored result, a running check, Check again / Cancel. */
export function CheckLine({
  models,
  checking,
  onRun,
  onCancel,
}: {
  models: ModelsView;
  checking: Checking | null;
  onRun(): void;
  onCancel(): void;
}) {
  const styles = useStyles();
  if (checking !== null) {
    const label =
      checking.model !== null && checking.runs_on !== null
        ? `Checking ${titleOf(models, checking.model)} on the ${partName(checking.runs_on)}`
        : "Starting the check";
    return (
      <div className={styles.check} role="status" aria-label="Model check">
        <Spinner size="tiny" label={label} />
        <Button className={styles.push} onClick={onCancel}>
          Cancel
        </Button>
      </div>
    );
  }
  const c = models.check;
  if (c === null) {
    return (
      <div className={styles.check} role="status" aria-label="Model check">
        <Body1>Not checked on this computer yet.</Body1>
        <Button className={styles.push} onClick={onRun}>
          Check now
        </Button>
      </div>
    );
  }
  const item = models.transcription.find((m) => m.name === c.model);
  const verdict = item ? verdictTag(item.check) : null;
  return (
    <div className={styles.check} role="status" aria-label="Model check">
      {verdict !== null && (
        <Badge appearance="tint" color={verdict.color}>
          {verdict.text}
        </Badge>
      )}
      <Body1>
        Last check: {titleOf(models, c.model)} on the {partName(c.runs_on)}.
        {c.fallback !== null && ` If a meeting falls behind, Earshot switches to ${titleOf(models, c.fallback)}.`}
      </Body1>
      {!c.current && (
        <Caption1 className={styles.muted}>
          Something changed since (hardware, models or languages): check again.
        </Caption1>
      )}
      <Button className={styles.push} onClick={onRun}>
        Check again
      </Button>
    </div>
  );
}

/** "Automatic (processor)", or "Automatic (graphics card or processor)" when there is more than
 * one part to pick from, best first: one of them, not one after the other. */
export function automaticLabel(parts: string[]): string {
  const unique = [...new Set(parts)];
  if (unique.length < 2) return `Automatic (${unique.join("")})`;
  return `Automatic (${unique.slice(0, -1).join(", ")} or ${unique[unique.length - 1]})`;
}

const PART_LABEL: Record<PartChoice, string> = {
  Automatic: "Automatic",
  GraphicsCard: "Graphics card",
  Npu: "NPU",
  Processor: "Processor",
};

function partChoices(order: RunsOn[], allowNpu: boolean, current: PartChoice): PartChoice[] {
  const out: PartChoice[] = ["Automatic"];
  if (order.some((o) => partOf(o) === "graphics")) out.push("GraphicsCard");
  if (allowNpu && order.includes("Npu")) out.push("Npu");
  out.push("Processor");
  if (!out.includes(current)) out.splice(out.length - 1, 0, current);
  return out;
}

/** "Where it runs": one choice per task and the IPC's notes. */
export function WhereItRuns({
  hardware,
  models,
  transcription,
  summary,
  onChange,
}: {
  hardware: HardwareView;
  models: ModelsView;
  transcription: PartChoice;
  summary: PartChoice;
  onChange(task: "transcription_part" | "summary_part", part: PartChoice): void;
}) {
  const styles = useStyles();
  const asrId = useId();
  const sumId = useId();
  const order = hardware.accelerators;
  const sumAuto =
    partOf(models.summary_recommended.runs_on) === "graphics"
      ? automaticLabel(["graphics card", "processor"])
      : automaticLabel(["processor"]);
  return (
    <>
      <div className={styles.runs}>
        <label htmlFor={asrId}>Transcription</label>
        <Select
          id={asrId}
          value={transcription}
          onChange={(_, d) => onChange("transcription_part", d.value as PartChoice)}
        >
          {partChoices(order, true, transcription).map((p) => (
            <option key={p} value={p}>
              {p === "Automatic" ? automaticLabel(order.map(partName)) : PART_LABEL[p]}
            </option>
          ))}
        </Select>
        <label htmlFor={sumId}>Summaries</label>
        <Select
          id={sumId}
          value={summary}
          onChange={(_, d) => onChange("summary_part", d.value as PartChoice)}
        >
          {partChoices(order, false, summary).map((p) => (
            <option key={p} value={p}>
              {p === "Automatic" ? sumAuto : PART_LABEL[p]}
            </option>
          ))}
        </Select>
      </div>
      {models.in_use.notes.map((n) => (
        <MessageBar key={n} intent="warning">
          <MessageBarBody>{n}</MessageBarBody>
        </MessageBar>
      ))}
      <Caption1 className={styles.muted}>
        Automatic uses the fastest part of this computer that passed the check. If you pick a slower
        one, Earshot says so during the meeting when it falls behind. A model that does not fit on
        the part you picked runs on the processor instead, and this page says so. "This is your
        hardware" above follows these choices.
      </Caption1>
    </>
  );
}

/** The languages field's name: its label, or, under a heading of the same words
 * (`hideLabel`), only its accessible name. */
export const LANGUAGES_LABEL = "Meeting languages";

/** The meeting languages: every language a transcription model covers. */
export function LanguagesPicker({
  models,
  value,
  onChange,
  hideLabel = false,
}: {
  models: ModelsView;
  value: string[];
  onChange(languages: string[]): void;
  /** The page shows a heading with the same words, so the field shows no label of its own. */
  hideLabel?: boolean;
}) {
  const styles = useStyles();
  const codes = [...new Set(models.transcription.flatMap((m) => m.languages))];
  const named = codes
    .map((code) => ({ code, name: languageName(code) }))
    .sort((a, b) => a.name.localeCompare(b.name));
  const shown = value.map(languageName).join(", ");
  return (
    <Field
      className={styles.languages}
      label={hideLabel ? undefined : LANGUAGES_LABEL}
      hint="The recommendation and the check follow the languages spoken in your meetings."
    >
      <Combobox
        aria-label={hideLabel ? LANGUAGES_LABEL : undefined}
        multiselect
        value={shown}
        selectedOptions={value}
        placeholder="Choose languages"
        onOptionSelect={(_, d) => onChange(d.selectedOptions)}
      >
        {named.map((l) => (
          <Option key={l.code} value={l.code}>
            {l.name}
          </Option>
        ))}
      </Combobox>
    </Field>
  );
}

/** The notices of one area. */
export function Notices({ notices, onDismiss }: { notices: Notice[]; onDismiss(key: string): void }) {
  const styles = useStyles();
  if (notices.length === 0) return null;
  return (
    <div className={styles.notices}>
      {notices.map((n) => (
        <MessageBar key={n.key} intent={n.intent}>
          <MessageBarBody>
            <MessageBarTitle>{n.title}</MessageBarTitle>
            {n.message}
          </MessageBarBody>
          <MessageBarActions
            containerAction={
              <Button size="small" appearance="transparent" onClick={() => onDismiss(n.key)}>
                Dismiss
              </Button>
            }
          />
        </MessageBar>
      ))}
    </div>
  );
}
