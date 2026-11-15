import {
  AppShell,
  EarshotThemeProvider,
  type Ipc,
  IpcProvider,
  createTauriIpc,
  isTauri,
  suppressBrowserMenu,
} from "@earshot/ui";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

// Inside the Tauri webview the app talks to Rust; in a plain browser
// (`pnpm --filter @earshot/desktop dev`) it previews the UI on sample data, and "New
// meeting" plays a scripted meeting in real time. The preview's Models page shows a
// Surface-like computer (NPU); `?machine=desktop` shows a desktop with an NVIDIA card.
// `?firstRun=1` opens on the first-run wizard. `?inRoom=1` shows finished meetings as recorded in the room, `?inRoom=recognised` with the first voice named by the user's voiceprint; `?selfNamed=1` adds a separate person with the user's name (T12.44). `?callEnded=1` asks whether to stop 3 s into a call. `?cuda=unavailable` shows the CUDA section as it is
// until the download exists, `?cuda=checking` while the start tries its files. `?newerData=1`
// shows the screen for a database saved by a newer Earshot, `?dataFolder=1` the one for a data
// folder that could not be opened, `?summary=failed` / `?summary=postFailed` a meeting whose
// summary or transcript did not finish (T9.23). `?keptLocal=1` keeps the weekly sync
// on this computer with Claude Code as the summary writer (T12.13). `?previews=battery` shows a
// meeting whose provisional words are off on battery, `?previews=waiting` one where they wait
// for the transcript and the banner is taken back (T12.33). `?sync=1` shows a computer set up for
// sync with other computers that write summaries, `?summary=offloaded` a meeting whose summary
// another computer writes (T11.3db). `?notInUse=3` shows a Dictionary whose words joined from
// the user's computers pass a limit, three of them not in use (T11.3e). The mock is loaded only here, with a
// dynamic import, so the app's own bundle starts without it (T12.54).
async function previewIpc(): Promise<Ipc> {
  const { createMockIpc, samplePersons } = await import("@earshot/ui/preview");
  const query = new URLSearchParams(window.location.search);
  const machine = query.get("machine") === "desktop" ? "desktop" : "surface";
  const previews = query.get("previews");
  return createMockIpc({
    now: Date.now(),
    machine,
    firstRun: { show: query.get("firstRun") === "1" },
    cleanInstall: query.get("clean") === "1",
    noModels: query.get("clean") === "1",
    inRoom: query.get("inRoom") === "recognised" ? "recognised" : query.get("inRoom") === "1",
    ...(query.get("selfNamed") === "1"
      ? { persons: [...samplePersons(), { id: "9e000000-0000-7000-8000-000000000009", name: "Casey", has_voiceprint: true, is_me: false }] }
      : {}),
    callEndOfferAfterMs: query.get("callEnded") === "1" ? 3_000 : undefined,
    newerData: query.get("newerData") === "1",
    dataFolderFails: query.get("dataFolder") === "1",
    provisionalWords: previews === "battery" || previews === "waiting" ? previews : undefined,
    ...(query.get("summary") === "failed"
      ? {
          summary: {
            kind: "Failed" as const,
            message:
              "The summary could not be written: Earshot could not reach the summary service. Check the network and the address in Settings, Summaries, then try again.",
            retryable: false,
          },
        }
      : query.get("summary") === "offloaded"
        ? {
            summary: {
              kind: "Offloaded" as const,
              message: "Waiting for Studio to write the summary.",
            },
          }
      : query.get("summary") === "postFailed"
        ? {
            summary: {
              kind: "PostFailed" as const,
              message:
                "The transcript could not be finished. The recording and the draft transcript are saved. The transcription did not finish. Try again; if it keeps failing, choose another transcription model in Settings, Transcription.",
              retryable: false,
            },
          }
        : {}),
    ...(query.get("sync") === "1" ? { sync: { setUp: true, lastSuccessUnixMs: Date.now() - 60_000 } } : {}),
    ...(query.get("notInUse") !== null
      ? {
          summaries: {
            vocabulary: ["Quillmarsh", "QXR-7", "Velmora Ridge"],
            notInUse: Number(query.get("notInUse")),
          },
        }
      : {}),
    ...(query.get("keptLocal") === "1"
      ? {
          keptLocal: ["0190f1a2-0000-7000-8000-000000000003"],
          summaries: { settings: { writer: "ClaudeCode" as const } },
        }
      : {}),
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
}

const inTauri = isTauri();
const ipc = inTauri ? createTauriIpc() : await previewIpc();

// In the app the webview's own right-click menu (Back, Refresh, Print) is hidden except in text
// fields and over selected text; the browser preview keeps it, for inspecting.
if (inTauri) suppressBrowserMenu(document);

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
