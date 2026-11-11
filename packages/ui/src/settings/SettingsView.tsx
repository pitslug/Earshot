// The Settings screen (docs/ui-design.md "Settings", docs/settings-design.md "Settings: eight
// pages", T12.5b): the section list on the left, one page on the right: General, Audio (T7.4i),
// System, Transcription, Summaries (T7.4g), Dictionary (T12.15a), People and Notes and storage.
// It opens on General.
// It holds the one `useModels` state for every page that shows a model part (System,
// Transcription, Summaries), so a download or the model check carries on across pages.

import { Button, makeStyles, mergeClasses, tokens } from "@fluentui/react-components";
import { useState } from "react";

import { AudioPage } from "./AudioPage";
import { DictionaryPage } from "./DictionaryPage";
import { GeneralPage } from "./GeneralPage";
import { NotesStoragePage } from "./NotesStoragePage";
import { PeoplePage } from "./PeoplePage";
import { SummariesPage } from "./SummariesPage";
import { SystemPage } from "./SystemPage";
import { TranscriptionPage } from "./TranscriptionPage";
import { useModels } from "./useModels";

/** The sections of Settings, in order. */
export const SETTINGS_SECTIONS = [
  "General",
  "Audio",
  "System",
  "Transcription",
  "Summaries",
  "Dictionary",
  "People",
  "Notes and storage",
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
  /** The section shown first (default General). */
  initialSection?: SettingsSection;
  /** Opens the first-run wizard again (General). */
  onOpenSetup?: () => void;
}

/** The Settings screen. */
export function SettingsView({ onBack, initialSection = "General", onOpenSetup }: SettingsViewProps) {
  const styles = useStyles();
  const [section, setSection] = useState<SettingsSection>(initialSection);
  // Held here, not in a page: the one subscriber to model job events outlives a page switch.
  const models = useModels();
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
        {section === "General" ? (
          <GeneralPage onOpenSetup={onOpenSetup} />
        ) : section === "Audio" ? (
          <AudioPage />
        ) : section === "System" ? (
          <SystemPage models={models} />
        ) : section === "Transcription" ? (
          <TranscriptionPage models={models} />
        ) : section === "Summaries" ? (
          <SummariesPage models={models} />
        ) : section === "Dictionary" ? (
          <DictionaryPage />
        ) : section === "People" ? (
          <PeoplePage />
        ) : (
          <NotesStoragePage />
        )}
      </main>
    </div>
  );
}
