import {
  AppShell,
  EarshotThemeProvider,
  IpcProvider,
  createMockIpc,
  createTauriIpc,
  isTauri,
} from "@earshot/ui";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

// Inside the Tauri webview the app talks to Rust; in a plain browser
// (`pnpm --filter @earshot/desktop dev`) it previews the UI on sample data, and "New
// meeting" plays a scripted meeting in real time. The preview's Models page shows a
// Surface-like computer (NPU); `?machine=desktop` shows a desktop with an NVIDIA card.
// `?firstRun=1` opens on the first-run wizard. `?inRoom=1` shows finished meetings as recorded in the room. `?callEnded=1` asks whether to stop 3 s into a call. `?cuda=unavailable` shows the CUDA section as it is
// until the download exists, `?cuda=checking` while the start tries its files. `?newerData=1`
// shows the screen for a database saved by a newer Earshot.
const inTauri = isTauri();
const query = new URLSearchParams(window.location.search);
const machine = query.get("machine") === "desktop" ? "desktop" : "surface";
const ipc = inTauri
  ? createTauriIpc()
  : createMockIpc({
      now: Date.now(),
      machine,
      firstRun: { show: query.get("firstRun") === "1" },
      cleanInstall: query.get("clean") === "1",
      noModels: query.get("clean") === "1",
      inRoom: query.get("inRoom") === "1",
      callEndOfferAfterMs: query.get("callEnded") === "1" ? 3_000 : undefined,
      newerData: query.get("newerData") === "1",
      cuda:
        query.get("cuda") === "unavailable"
          ? { notAvailable: "This download is not available yet, so there is nothing to install. Earshot keeps running as it does now." }
          : query.get("cuda") === "checking"
            ? {
                installed: ["Summaries"],
                switch: { summaries: true, transcription: false },
                summariesOnCuda: true,
                checkingMs: 15_000,
              }
            : {},
    });

const root = document.getElementById("root");
if (!root) throw new Error("index.html has no #root");

createRoot(root).render(
  <StrictMode>
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell notice={inTauri ? undefined : "Preview: sample data"} />
      </IpcProvider>
    </EarshotThemeProvider>
  </StrictMode>,
);
