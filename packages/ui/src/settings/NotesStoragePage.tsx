// Settings, Notes and storage (docs/settings-design.md "Settings", T12.5b): where exported notes go
// (Obsidian), how long recordings are kept, and where the crash logs are. Each part keeps its own
// state; this page only places them.

import { Subtitle2, Title2, makeStyles, tokens } from "@fluentui/react-components";
import { type ReactNode, useId } from "react";

import { CrashLogsFolder } from "./CrashLogsFolder";
import { ObsidianSection } from "./ObsidianSection";
import { RetentionSection } from "./RetentionSection";

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

/** The Notes and storage page of Settings. */
export function NotesStoragePage() {
  const styles = useStyles();
  return (
    <div className={styles.page}>
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
