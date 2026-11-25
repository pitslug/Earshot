// @earshot/ui: React components shared by the desktop app (and later Android).

export * from "./ipc";
export { AppShell } from "./shell/AppShell";
export { suppressBrowserKeys } from "./shell/browserKeys";
export { suppressBrowserMenu } from "./shell/browserMenu";
export { RootErrorBoundary } from "./shell/RootErrorBoundary";
export type { RootErrorBoundaryProps } from "./shell/RootErrorBoundary";
export type { AppShellProps } from "./shell/AppShell";
export { SETTINGS_SECTIONS, SettingsView } from "./settings/SettingsView";
export type { SettingsSection, SettingsViewProps } from "./settings/SettingsView";
export {
  EarshotThemeProvider,
  type ThemeChoice,
  usePrefersDark,
  useThemeChoice,
} from "./theme/EarshotThemeProvider";
export { createManualTimer, realTimer } from "./time";
export type { ManualTimer, Timer } from "./time";
