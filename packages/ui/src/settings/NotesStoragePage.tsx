// Settings, Notes and storage (docs/settings-design.md "Settings", T12.5b): where exported notes go
// (Obsidian), how long recordings are kept, and where the crash logs are. Each part keeps its own
// state; this page only places them.

import { Title2, makeStyles } from "@fluentui/react-components";

import { CrashLogsFolder } from "./CrashLogsFolder";
import { ObsidianSection } from "./ObsidianSection";
import { RetentionSection } from "./RetentionSection";
import { Section, usePageStyles } from "./parts";

const useStyles = makeStyles({
  heading: {
    margin: 0,
  },
});

/** The Notes and storage page of Settings. */
export function NotesStoragePage() {
  const styles = useStyles();
  const page = usePageStyles();
  return (
    <div className={page.page}>
      <Title2 as="h1" className={styles.heading}>
        Notes and storage
      </Title2>
      <Section title="Obsidian">
        <ObsidianSection />
      </Section>
      <RetentionSection />
      <Section title="Crash logs">
        <CrashLogsFolder />
      </Section>
    </div>
  );
}
