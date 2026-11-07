// One model as a radio card: name, tags, why, and its install state with Download / Pause /
// Resume / Remove (docs/ui-mockups/t7-4.html).

import {
  Badge,
  Body1Strong,
  Button,
  Caption1,
  ProgressBar,
  Radio,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import type { ReactNode } from "react";

import type { ModelItem } from "../ipc";
import { formatSize, verdictTag } from "./copy";

const useStyles = makeStyles({
  card: {
    display: "grid",
    gridTemplateColumns: "minmax(0, 1fr) auto",
    columnGap: tokens.spacingHorizontalM,
    rowGap: tokens.spacingVerticalXS,
    alignItems: "start",
    padding: `${tokens.spacingVerticalS} ${tokens.spacingHorizontalM}`,
    border: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke1}`,
    borderRadius: tokens.borderRadiusMedium,
    backgroundColor: tokens.colorNeutralBackground1,
  },
  selected: {
    border: `${tokens.strokeWidthThin} solid ${tokens.colorBrandStroke1}`,
    boxShadow: `inset 0 0 0 ${tokens.strokeWidthThin} ${tokens.colorBrandStroke1}`,
  },
  main: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    minWidth: 0,
  },
  title: {
    display: "inline-flex",
    flexWrap: "wrap",
    alignItems: "center",
    gap: tokens.spacingHorizontalS,
  },
  why: {
    color: tokens.colorNeutralForeground2,
    paddingLeft: "32px",
    maxWidth: "62ch",
  },
  warn: {
    color: tokens.colorPaletteMarigoldForeground2,
    paddingLeft: "32px",
  },
  actions: {
    display: "flex",
    flexDirection: "column",
    alignItems: "flex-end",
    gap: tokens.spacingVerticalXS,
    textAlign: "right",
    paddingTop: tokens.spacingVerticalXS,
  },
  muted: {
    color: tokens.colorNeutralForeground3,
  },
  bar: {
    width: "140px",
  },
});

export interface ModelCardProps {
  item: ModelItem;
  /** The tag next to the name ("Recommended", "Lighter", ...). */
  label: string;
  /** Why you would pick it. */
  why: string;
  selected: boolean;
  onDownload(model: string): void;
  onPause(model: string): void;
  onRemove(model: string): void;
  /** Shown under the install state (the first-run wizard's later / never). */
  offer?: ReactNode;
}

/** The speech helpers: what every meeting needs, so a card with nothing to choose or remove. */
export function HelpersCard({
  item,
  why,
  onDownload,
  onPause,
}: {
  item: ModelItem;
  why: string;
  onDownload(model: string): void;
  onPause(model: string): void;
}) {
  const styles = useStyles();
  return (
    <div className={styles.card} data-model={item.name}>
      <div className={styles.main}>
        <Body1Strong>{item.title}</Body1Strong>
        <Caption1>{why}</Caption1>
      </div>
      <div className={styles.actions}>
        <InstallState item={item} title={item.title} onDownload={onDownload} onPause={onPause} />
      </div>
    </div>
  );
}

/** A model as a radio card; must sit inside a `RadioGroup`. */
export function ModelCard({ item, label, why, selected, onDownload, onPause, onRemove, offer }: ModelCardProps) {
  const styles = useStyles();
  const title = item.title;
  const verdict = verdictTag(item.check);
  const removable = item.installed !== "No" && !item.protected && !item.downloading && item.held > 0;
  return (
    <div className={mergeClasses(styles.card, selected && styles.selected)} data-model={item.name}>
      <div className={styles.main}>
        <Radio
          value={item.name}
          label={
            <span className={styles.title}>
              <Body1Strong>{title}</Body1Strong>
              <Badge appearance="tint" color="brand">
                {label}
              </Badge>
              {verdict !== null && (
                <Badge appearance="tint" color={verdict.color}>
                  {verdict.text}
                </Badge>
              )}
            </span>
          }
        />
        {why !== "" && <Caption1 className={styles.why}>{why}</Caption1>}
        {verdict?.line !== undefined && <Caption1 className={styles.warn}>{verdict.line}</Caption1>}
      </div>
      <div className={styles.actions}>
        <InstallState item={item} title={title} onDownload={onDownload} onPause={onPause} />
        {offer}
        {removable && (
          <Button
            size="small"
            appearance="transparent"
            aria-label={`Remove ${title} (frees ${formatSize(item.held)})`}
            onClick={() => onRemove(item.name)}
          >
            Remove (frees {formatSize(item.held)})
          </Button>
        )}
      </div>
    </div>
  );
}

function InstallState({
  item,
  title,
  onDownload,
  onPause,
}: {
  item: ModelItem;
  title: string;
  onDownload(model: string): void;
  onPause(model: string): void;
}) {
  const styles = useStyles();
  if (item.installed === "Yes" && !item.downloading) {
    return (
      <>
        <Badge appearance="tint" color="success">
          Installed
        </Badge>
        <Caption1 className={styles.muted}>{formatSize(item.size)}</Caption1>
      </>
    );
  }
  if (item.downloading || item.installed === "Partly") {
    const progress = `${formatSize(item.held)} of ${formatSize(item.size)}`;
    return (
      <>
        <Caption1 className={styles.muted}>
          {item.downloading ? progress : `Partly downloaded: ${progress}`}
        </Caption1>
        <ProgressBar
          className={styles.bar}
          value={item.size > 0 ? item.held / item.size : 0}
          aria-label={`${title} download`}
        />
        {item.downloading ? (
          <Button size="small" aria-label={`Pause ${title}`} onClick={() => onPause(item.name)}>
            Pause
          </Button>
        ) : (
          <Button size="small" aria-label={`Resume ${title}`} onClick={() => onDownload(item.name)}>
            Resume
          </Button>
        )}
      </>
    );
  }
  return (
    <Button size="small" aria-label={`Download ${title} (${formatSize(item.size)})`} onClick={() => onDownload(item.name)}>
      Download {formatSize(item.size)}
    </Button>
  );
}
