// The Settings screen (docs/ui-design.md "Settings", docs/ui-mockups/t7-4.html): the section
// list on the left, one page on the right: General, Speakers and Obsidian (T7.4h), Audio
// (T7.4i), Summaries (T7.4g), Models (T7.4f), Benchmark (T7.4c) and Storage (T7.4k).

import { Button, makeStyles, mergeClasses, tokens } from "@fluentui/react-components";
import { useState } from "react";

import { GeneralPage } from "./GeneralPage";
import { AudioPage } from "./AudioPage";
import { BenchmarkPage } from "./BenchmarkPage";
import { ModelsPage } from "./ModelsPage";
import { StoragePage } from "./StoragePage";
import { ObsidianPage } from "./ObsidianPage";
import { SpeakersPage } from "./SpeakersPage";
import { SummariesPage } from "./SummariesPage";

/** The sections of Settings, in order. */
export const SETTINGS_SECTIONS = [
  "General",
  "Audio",
  "Summaries",
  "Models",
  "Benchmark",
  "Speakers",
  "Obsidian",
  "Storage",
] as const;

export type SettingsSection = (typeof SETTINGS_SECTIONS)[number];

const useStyles = makeStyles({
  root: {
    display: "flex",
    height: "100%",
    minHeight: 0,
  },
  nav: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXXS,
    width: "240px",
    flexShrink: 0,
    padding: tokens.spacingVerticalM,
    borderRight: `${tokens.strokeWidthThin} solid ${tokens.colorNeutralStroke2}`,
    backgroundColor: tokens.colorNeutralBackground2,
  },
  back: {
    justifyContent: "flex-start",
    marginBottom: tokens.spacingVerticalM,
  },
  item: {
    width: "100%",
    justifyContent: "flex-start",
  },
  current: {
    backgroundColor: tokens.colorSubtleBackgroundSelected,
    color: tokens.colorBrandForeground1,
  },
  page: {
    flexGrow: 1,
    minWidth: 0,
    overflowY: "auto",
    padding: tokens.spacingHorizontalXXL,
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalM,
  },
});

export interface SettingsViewProps {
  /** Back to the meetings. */
  onBack(): void;
  /** The section shown first (default Models). */
  initialSection?: SettingsSection;
  /** Opens the first-run wizard again (General). */
  onOpenSetup?: () => void;
}

/** The Settings screen. */
export function SettingsView({ onBack, initialSection = "Models", onOpenSetup }: SettingsViewProps) {
  const styles = useStyles();
  const [section, setSection] = useState<SettingsSection>(initialSection);
  return (
    <div className={styles.root}>
      <nav className={styles.nav} aria-label="Settings sections">
        <Button appearance="subtle" className={styles.back} onClick={onBack}>
          Back to meetings
        </Button>
        {SETTINGS_SECTIONS.map((s) => (
          <Button
            key={s}
            appearance="subtle"
            className={mergeClasses(styles.item, s === section && styles.current)}
            aria-current={s === section ? "page" : undefined}
            onClick={() => setSection(s)}
          >
            {s}
          </Button>
        ))}
      </nav>
      <main className={styles.page}>
        {section === "Models" ? (
          <ModelsPage />
        ) : section === "Summaries" ? (
          <SummariesPage />
        ) : section === "Benchmark" ? (
          <BenchmarkPage />
        ) : section === "Storage" ? (
          <StoragePage />
        ) : section === "General" ? (
          <GeneralPage onOpenSetup={onOpenSetup} />
        ) : section === "Speakers" ? (
          <SpeakersPage />
        ) : section === "Obsidian" ? (
          <ObsidianPage />
        ) : (
          <AudioPage />
        )}
      </main>
    </div>
  );
}
