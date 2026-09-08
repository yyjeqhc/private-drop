import { invoke } from "@tauri-apps/api/core";
import type {
  ActivityEntry,
  DesktopState,
  ProjectSelection,
  TunnelProxyMode,
} from "../models/topology";

export const desktopApi = {
  getState: () => invoke<DesktopState>("get_desktop_state"),
  getLaunchAtLogin: () => invoke<boolean>("get_launch_at_login"),
  setLaunchAtLogin: (enabled: boolean) =>
    invoke<boolean>("set_launch_at_login", { request: { enabled } }),
  refresh: () => invoke<DesktopState>("refresh_runtime_status"),
  resumeSavedRuntime: () => invoke<DesktopState>("resume_saved_runtime"),
  updateTunnelProxy: (mode: TunnelProxyMode, customUrl?: string | null) =>
    invoke<DesktopState>("update_tunnel_proxy", {
      request: { mode, customUrl: customUrl ?? null },
    }),
  inspectProject: (projectPath: string) =>
    invoke<ProjectSelection>("inspect_project", {
      request: { projectPath },
    }),
  configureLocal: (projectPath?: string | null) =>
    invoke<DesktopState>("configure_local_setup", {
      request: { projectPath: projectPath ?? null },
    }),
  configureRemote: (
    serverUrl: string,
    pairingCode: string,
    projectPath: string,
  ) =>
    invoke<DesktopState>("configure_remote_setup", {
      request: { serverUrl, pairingCode, projectPath },
    }),
  startQuickShare: (projectPath: string, provider: QuickShareProvider) =>
    invoke<DesktopState>("start_quick_share", {
      request: { projectPath, provider },
    }),
  stopQuickShare: () => invoke<DesktopState>("stop_quick_share"),
  startRegularTunnel: () => invoke<DesktopState>("start_regular_tunnel"),
  stopRegularTunnel: () => invoke<DesktopState>("stop_regular_tunnel"),
  stopLocalRuntime: () => invoke<DesktopState>("stop_local_runtime"),
  cancelOperation: (operationId: string) =>
    invoke<DesktopState>("cancel_desktop_operation", {
      request: { operationId },
    }),
  activity: () => invoke<ActivityEntry[]>("get_bounded_activity"),
};

export type QuickShareProvider = "cloudflare" | "openai" | "none";

