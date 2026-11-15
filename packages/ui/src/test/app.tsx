// The app as the UI tests render it, and the way they reach a Settings page (T12.55): one copy
// of the render tree and the Settings navigation for every test file.

import { type RenderResult, render, screen, within } from "@testing-library/react";
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
