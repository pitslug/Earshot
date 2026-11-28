import { EarshotThemeProvider } from "@earshot/ui";
import { Body1, Title2, makeStyles, tokens } from "@fluentui/react-components";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

// The Android app's first screen (T10.2a): the project and its APK build exist; the recorder's
// routes and the Rust commands behind them come in T10.2b. Nothing here talks to Rust yet.

/** What the window says until the recorder exists. */
const COMING = "The recorder is on its way. This build checks that Earshot installs and opens on this phone.";

const useStyles = makeStyles({
  page: {
    minHeight: "100%",
    boxSizing: "border-box",
    padding: "24px 16px",
    display: "flex",
    flexDirection: "column",
    gap: "12px",
    backgroundColor: tokens.colorNeutralBackground1,
    color: tokens.colorNeutralForeground1,
  },
});

function Coming() {
  const styles = useStyles();
  return (
    <main className={styles.page}>
      <h1 style={{ margin: 0 }}>
        <Title2>Earshot</Title2>
      </h1>
      <Body1>{COMING}</Body1>
    </main>
  );
}

const root = document.getElementById("root");
if (!root) throw new Error("index.html has no #root");

createRoot(root).render(
  <StrictMode>
    <EarshotThemeProvider>
      <Coming />
    </EarshotThemeProvider>
  </StrictMode>,
);