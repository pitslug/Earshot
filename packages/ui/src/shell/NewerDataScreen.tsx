// What the window shows when the database was saved by a newer Earshot (T9.5c,
// docs/folders-design.md "Downgrade"): the data is left untouched, and the way out is to
// install the newer version, so the update check in Settings, General stays one click away.

import { Body1, Button, Title2, makeStyles, tokens } from "@fluentui/react-components";

const useStyles = makeStyles({
  root: {
    height: "100%",
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    justifyContent: "center",
    gap: tokens.spacingVerticalM,
    padding: tokens.spacingHorizontalXXL,
    textAlign: "center",
  },
  heading: {
    margin: 0,
  },
  text: {
    maxWidth: "440px",
  },
});

/** The plain screen for data saved by a newer Earshot. */
export function NewerDataScreen({ onCheckForUpdates }: { onCheckForUpdates: () => void }) {
  const styles = useStyles();
  return (
    <main className={styles.root}>
      <Title2 as="h1" className={styles.heading}>
        Earshot needs updating
      </Title2>
      <Body1 className={styles.text}>
        This data was saved by a newer version of Earshot. Install the latest version to open it.
      </Body1>
      <Button appearance="primary" onClick={onCheckForUpdates}>
        Check for updates
      </Button>
    </main>
  );
}
