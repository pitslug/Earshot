// What the window shows when Earshot could not open its data folder at the start (T9.23): what
// happened and what to check, in fixed words, and Try again. The store is opened once, when the
// app starts, so trying again restarts Earshot.

import {
  Body1,
  Button,
  MessageBar,
  MessageBarBody,
  Title2,
  makeStyles,
  tokens,
} from "@fluentui/react-components";
import { useState } from "react";

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

/** The plain screen for a data folder that could not be opened. `message` is the app's copy. */
export function DataFolderScreen({
  message,
  onTryAgain,
}: {
  message: string;
  /** Restarts Earshot; rejects with the app's words when it cannot (a download is running). */
  onTryAgain: () => Promise<void>;
}) {
  const styles = useStyles();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const tryAgain = async () => {
    setPending(true);
    setError(null);
    try {
      await onTryAgain();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
      setPending(false);
    }
  };
  return (
    <main className={styles.root}>
      <Title2 as="h1" className={styles.heading}>
        Earshot could not open its data
      </Title2>
      <Body1 className={styles.text}>{message}</Body1>
      <Button appearance="primary" disabled={pending} onClick={() => void tryAgain()}>
        Try again
      </Button>
      {error !== null && (
        <MessageBar intent="error" className={styles.text}>
          <MessageBarBody>{error}</MessageBarBody>
        </MessageBar>
      )}
    </main>
  );
}
