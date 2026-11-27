// The app as the UI tests render it, and the way they reach a Settings page (T12.55): one copy
// of the render tree and the Settings navigation for every test file.

import { type RenderResult, render, screen, waitFor, within } from "@testing-library/react";
import { expect } from "vitest";
import type userEvent from "@testing-library/user-event";

import { type Ipc, IpcProvider } from "../ipc";
import { AppShell, type AppShellProps } from "../shell/AppShell";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";

/** Renders the whole app (`AppShell` under the theme and `ipc`), as the window does. */
export function renderApp(ipc: Ipc, props: AppShellProps = {}): RenderResult {
  return render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <AppShell {...props} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
}

/** Waits until the home screen has the window's first meeting-list read (T14.49: that read waits
 * for the sync subscription, so right after a render the home screen shows only the "Loading
 * meetings" spinner). Call it before reading home or list content synchronously, and before
 * asserting that something there is absent, which would otherwise pass while still loading. */
export async function listLoaded(): Promise<void> {
  const main = await screen.findByRole("main");
  await waitFor(() => expect(within(main).queryByRole("progressbar", { name: "Loading meetings" })).toBeNull());
}

/** Opens Settings from the app and picks `section` in its navigation; resolves with the
 * navigation. Waiting for the page itself is the caller's. */
export async function openSettings(
  user: ReturnType<typeof userEvent.setup>,
  section: string,
): Promise<HTMLElement> {
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  const nav = await screen.findByRole("navigation", { name: "Settings sections" });
  await user.click(within(nav).getByRole("button", { name: section }));
  return nav;
}

/** The header button that folds a Summaries writer's setup section (T12.96). */
export function setupToggle(section: HTMLElement): HTMLElement {
  const toggle = section.querySelector<HTMLElement>(":scope > button[aria-expanded]");
  if (toggle === null) throw new Error("the section has no fold button");
  return toggle;
}

/** Finds the Summaries writer's setup section named `name` (T12.96) and opens it when it is
 * folded away; resolves with the section. The sections start closed, so a test that reads or
 * types in a writer's form opens it first, as a person would. */
export async function openSetup(user: ReturnType<typeof userEvent.setup>, name: string): Promise<HTMLElement> {
  const section = await screen.findByRole("group", { name });
  const toggle = setupToggle(section);
  if (toggle.getAttribute("aria-expanded") !== "true") await user.click(toggle);
  return section;
}
