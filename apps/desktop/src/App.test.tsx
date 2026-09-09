import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { DesktopState } from "./models/topology";
import { LocaleProvider } from "./i18n/locale";

const api = vi.hoisted(() => ({
  getState: vi.fn(),
  openPowerShellInstallGuide: vi.fn(),
  refresh: vi.fn(),
  resumeSavedRuntime: vi.fn(),
  updateTunnelProxy: vi.fn(),
  activity: vi.fn(),
  configureLocal: vi.fn(),
  configureRemote: vi.fn(),
  startQuickShare: vi.fn(),
  stopQuickShare: vi.fn(),
  stopLocalRuntime: vi.fn(),
  startRegularTunnel: vi.fn(),
  stopRegularTunnel: vi.fn(),
  cancelOperation: vi.fn(),
  inspectProject: vi.fn(),
  getLaunchAtLogin: vi.fn(),
  setLaunchAtLogin: vi.fn(),
}));

const tauriEvents = vi.hoisted(() => ({
  handler: null as null | ((event: { payload: unknown }) => void),
  listen: vi.fn(),
}));

vi.mock("./lib/desktop-api", () => ({
  desktopApi: api,
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: tauriEvents.listen,
}));

import App from "./App";
import { open } from "@tauri-apps/plugin-dialog";

const readyState: DesktopState = {
  topology: {
    experience: "full",
    server: { kind: "local" },
    runner: { kind: "local" },
    exposure: { kind: "none" },
    enrollment: { kind: "managed_pairing" },
  },
  readiness: {
    server: "ready",
    runner: "ready",
    exposure: "local_ready",
    project: "ready",
    runtime_ready: true,
    ready_for_chatgpt: false,
    summary: "Runtime ready on this computer",
    summary_kind: "runtime_ready_local_only",
    next_action: "Choose a ChatGPT connection in Connection.",
    next_action_kind: "choose_connection",
  },
  project: {
    path: "C:\\fixture\\repo",
    allowed_root: "C:\\fixture",
    is_git_repository: true,
    runtime_project_id: "agent:desktop:repo",
  },
  binaries: {
    directory: "C:\\fixture\\bin",
    version: "0.3.9",
    git_commit: "0123456789abcdef",
    source: "WEBCODEX_DESKTOP_BIN_DIR",
  },
  quick_share: null,
  regular_tunnel: null,
  activity_sequence: 0,
  openai_tunnel_configured: true,
  openai_tunnel_config: {
    tunnel_id_present: true,
    api_key_present: true,
  },
  regular_tunnel_available: true,
  runtime_autostart: false,
  preferred_connection: "no_chat_gpt",
  tunnel_proxy: {
    mode: "auto",
    custom_url: null,
    effective_source: "direct",
    effective_url: null,
    detected_url: null,
  },
};

const firstRunState: DesktopState = {
  readiness: {
    server: "unknown",
    runner: "unknown",
    exposure: "unknown",
    project: "none",
    runtime_ready: false,
    ready_for_chatgpt: false,
    summary: "WebCodex Service needs attention",
    summary_kind: "service_needs_attention",
    next_action: "Start or reconnect the WebCodex Service.",
    next_action_kind: "start_or_reconnect_service",
  },
  activity_sequence: 0,
  openai_tunnel_configured: true,
  openai_tunnel_config: {
    tunnel_id_present: true,
    api_key_present: true,
  },
  regular_tunnel_available: true,
  runtime_autostart: false,
  preferred_connection: "no_chat_gpt",
  tunnel_proxy: {
    mode: "auto",
    custom_url: null,
    effective_source: "direct",
    effective_url: null,
    detected_url: null,
  },
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function setupState(): DesktopState {
  return {
    ...readyState,
    readiness: {
      ...readyState.readiness,
      server: "stopped",
      runner: "stopped",
      exposure: "disabled",
      project: "configured",
      runtime_ready: false,
      ready_for_chatgpt: false,
      summary: "WebCodex Service needs attention",
      summary_kind: "service_needs_attention",
      next_action: "Start or reconnect the WebCodex Service.",
      next_action_kind: "start_or_reconnect_service",
    },
    current_operation: null,
  };
}

function localSetupOperationState(
  phase: "running" | "cancelling" = "running",
  activitySequence = 1,
): DesktopState {
  return {
    ...setupState(),
    activity_sequence: activitySequence,
    current_operation: {
      id: "desktop-operation-a",
      kind: "local_setup",
      phase,
      started_at_ms: 1_000,
      cancellable: true,
    },
  };
}

function renderApp() {
  return render(
    <LocaleProvider>
      <App />
    </LocaleProvider>,
  );
}

describe("semantic Desktop UI", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    tauriEvents.handler = null;
    tauriEvents.listen.mockImplementation(
      async (_eventName: string, handler: (event: { payload: unknown }) => void) => {
        tauriEvents.handler = handler;
        return () => {
          if (tauriEvents.handler === handler) tauriEvents.handler = null;
        };
      },
    );
    api.activity.mockResolvedValue([]);
    api.getLaunchAtLogin.mockResolvedValue(false);
    api.openPowerShellInstallGuide.mockResolvedValue(undefined);
    api.setLaunchAtLogin.mockImplementation(async (enabled: boolean) => enabled);
    api.resumeSavedRuntime.mockResolvedValue(readyState);
    api.updateTunnelProxy.mockResolvedValue(readyState);
    api.startRegularTunnel.mockResolvedValue(readyState);
    api.stopRegularTunnel.mockResolvedValue(readyState);
  });

  it("opens dashboard shortcuts and moves keyboard focus into the destination", async () => {
    api.getState.mockResolvedValue(readyState);
    renderApp();
    fireEvent.click(await screen.findByRole("button", { name: /查看项目/ }));
    expect(screen.getByRole("heading", { level: 1, name: "此电脑上的项目" })).toBeInTheDocument();
    expect(screen.getByRole("main")).toHaveFocus();
    fireEvent.click(screen.getByRole("button", { name: "选择其他项目" }));
    expect(screen.getByRole("button", { name: /在此电脑使用 WebCodex/ })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /返回运行概览/ }));
    fireEvent.click(screen.getByRole("button", { name: "首页" }));
    fireEvent.click(screen.getByRole("button", { name: /管理连接/ }));
    expect(screen.getByRole("heading", { level: 1, name: "ChatGPT 连接" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "首页" }));
    fireEvent.click(screen.getByRole("button", { name: /查看活动/ }));
    expect(screen.getByRole("button", { name: "活动" })).toHaveAttribute("aria-current", "page");
    await waitFor(() => expect(api.activity).toHaveBeenCalled());
    expect(api.configureLocal).not.toHaveBeenCalled();
  });

  it("takes Add project to setup and returns without reconfiguring the runtime", async () => {
    api.getState.mockResolvedValue({ ...readyState, project: null });
    renderApp();
    fireEvent.click(await screen.findByRole("button", { name: "项目" }));
    fireEvent.click(screen.getByRole("button", { name: /添加项目/ }));
    expect(await screen.findByRole("button", { name: /在此电脑使用 WebCodex/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "首页" })).toHaveAttribute("aria-current", "page");
    fireEvent.click(screen.getByRole("button", { name: /返回运行概览/ }));
    expect(screen.getByRole("heading", { name: "WebCodex" })).toBeInTheDocument();
    expect(api.configureLocal).not.toHaveBeenCalled();
  });

  it("navigates by accessible role/name and marks the current page", async () => {
    api.getState.mockResolvedValue(readyState);
    api.refresh.mockResolvedValue(readyState);
    renderApp();

    await screen.findByRole("heading", { level: 1, name: "WebCodex" });
    const home = screen.getByRole("button", { name: "首页" });
    const connection = screen.getByRole("button", { name: "连接" });
    expect(home).toHaveAttribute("aria-current", "page");
    expect(screen.getAllByRole("status")).toHaveLength(1);

    fireEvent.click(connection);
    expect(await screen.findByRole("heading", { level: 1, name: "ChatGPT 连接" })).toBeInTheDocument();
    expect(connection).toHaveAttribute("aria-current", "page");
    expect(home).not.toHaveAttribute("aria-current");

    expect(screen.getByRole("radiogroup", { name: "连接方式" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /OpenAI Secure Tunnel/ })).not.toBeChecked();
    expect(screen.getByRole("radio", { name: /Cloudflare/ })).toBeDisabled();
  });

  it("surfaces Tunnel configuration as presence-only current-process diagnostics", async () => {
    const missingKey: DesktopState = {
      ...readyState,
      openai_tunnel_configured: false,
      openai_tunnel_config: {
        tunnel_id_present: true,
        api_key_present: false,
      },
    };
    const missingId: DesktopState = {
      ...missingKey,
      openai_tunnel_config: {
        tunnel_id_present: false,
        api_key_present: true,
      },
    };
    const bothMissing: DesktopState = {
      ...missingKey,
      openai_tunnel_config: {
        tunnel_id_present: false,
        api_key_present: false,
      },
    };
    const configured: DesktopState = {
      ...missingKey,
      openai_tunnel_configured: true,
      openai_tunnel_config: {
        tunnel_id_present: true,
        api_key_present: true,
      },
    };
    api.getState.mockResolvedValue(missingKey);
    renderApp();
    await screen.findByRole("heading", { level: 1, name: "WebCodex" });
    fireEvent.click(screen.getByRole("button", { name: "连接" }));

    const diagnostics = screen.getByText("OpenAI Tunnel 配置检测").closest("article");
    expect(diagnostics).not.toBeNull();
    expect(within(diagnostics!).getByText("Tunnel ID").parentElement).toHaveTextContent("已检测");
    expect(within(diagnostics!).getByText("Tunnel API key").parentElement).toHaveTextContent("未检测");
    expect(within(diagnostics!).getByText(/关闭窗口只会隐藏到托盘\/菜单栏/)).toBeInTheDocument();
    expect(diagnostics).not.toHaveTextContent("CONTROL_PLANE_API_KEY");

    api.getState.mockResolvedValueOnce(missingId);
    fireEvent.click(within(diagnostics!).getByRole("button", { name: "重新检测配置" }));
    await waitFor(() => {
      expect(screen.getByText("Tunnel ID").parentElement).toHaveTextContent("未检测");
      expect(screen.getByText("Tunnel API key").parentElement).toHaveTextContent("已检测");
    });

    api.getState.mockResolvedValueOnce(bothMissing);
    fireEvent.click(within(diagnostics!).getByRole("button", { name: "重新检测配置" }));
    await waitFor(() => {
      expect(screen.getByText("Tunnel ID").parentElement).toHaveTextContent("未检测");
      expect(screen.getByText("Tunnel API key").parentElement).toHaveTextContent("未检测");
    });

    api.getState.mockResolvedValueOnce(configured);
    fireEvent.click(within(diagnostics!).getByRole("button", { name: "重新检测配置" }));
    await waitFor(() => {
      expect(screen.getByText("Tunnel ID").parentElement).toHaveTextContent("已检测");
      expect(screen.getByText("Tunnel API key").parentElement).toHaveTextContent("已检测");
    });
    expect(api.startRegularTunnel).not.toHaveBeenCalled();
  });

  it("navigates to existing Activity and Settings pages from the tray host event", async () => {
    api.getState.mockResolvedValue(readyState);
    renderApp();
    await screen.findByRole("heading", { level: 1, name: "WebCodex" });
    await waitFor(() => expect(tauriEvents.handler).not.toBeNull());

    act(() => {
      tauriEvents.handler?.({ payload: "settings" });
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Desktop 设置" })).toBeInTheDocument();

    act(() => {
      tauriEvents.handler?.({ payload: "activity" });
    });
    expect(await screen.findByRole("heading", { level: 1, name: "最近的运行活动" })).toBeInTheDocument();
    await waitFor(() => expect(api.activity).toHaveBeenCalled());

    act(() => {
      tauriEvents.handler?.({ payload: "https://example.invalid" });
    });
    expect(screen.getByRole("button", { name: "活动" })).toHaveAttribute("aria-current", "page");
  });

  it("reads and updates Launch at Login through the narrow Desktop host API", async () => {
    api.getState.mockResolvedValue(readyState);
    renderApp();
    await screen.findByRole("heading", { level: 1, name: "WebCodex" });
    fireEvent.click(screen.getByRole("button", { name: "设置" }));

    const launchAtLogin = await screen.findByRole("checkbox", { name: "登录时启动 WebCodex" });
    await waitFor(() => expect(launchAtLogin).toBeEnabled());
    expect(launchAtLogin).not.toBeChecked();
    expect(screen.getByText(/WebCodex 会在后台启动/)).toBeInTheDocument();

    fireEvent.click(launchAtLogin);
    await waitFor(() => expect(api.setLaunchAtLogin).toHaveBeenCalledWith(true));
    await waitFor(() => expect(launchAtLogin).toBeChecked());
  });

  it("keeps a fresh Desktop in product setup until the user chooses its real project", async () => {
    api.getState.mockResolvedValue(firstRunState);
    renderApp();

    expect(await screen.findByRole("button", { name: /在此电脑使用 WebCodex/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /连接现有 Server/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /快速共享项目/ })).toBeInTheDocument();
    expect(api.configureLocal).not.toHaveBeenCalled();
    expect(api.resumeSavedRuntime).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: /在此电脑使用 WebCodex/ }));
    expect(await screen.findByRole("heading", { level: 1, name: "在此电脑配置 WebCodex" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "选择文件夹" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "配置 WebCodex" })).toBeDisabled();
    fireEvent.submit(screen.getByRole("button", { name: "配置 WebCodex" }).closest("form")!);
    expect(api.configureLocal).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "← 返回全部配置方式" }));
    fireEvent.click(screen.getByRole("button", { name: /快速共享项目/ }));
    expect(screen.getByRole("radiogroup", { name: "Quick Share 连接方式" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /Cloudflare/ })).toBeChecked();
    expect(screen.getByRole("radio", { name: /OpenAI Secure Tunnel/ })).not.toBeChecked();
  });

  it("guides Windows users to PowerShell 7 without blocking the 5.1 fallback", async () => {
    const missingPwsh: DesktopState = {
      ...firstRunState,
      powershell_runtime: {
        pwsh_available: false,
        windows_powershell_available: true,
      },
    };
    const detectedPwsh: DesktopState = {
      ...missingPwsh,
      powershell_runtime: {
        pwsh_available: true,
        windows_powershell_available: true,
      },
    };
    api.getState.mockResolvedValueOnce(missingPwsh).mockResolvedValueOnce(detectedPwsh);
    renderApp();

    fireEvent.click(await screen.findByRole("button", { name: /在此电脑使用 WebCodex/ }));
    const guidance = screen.getByText("建议安装 PowerShell 7").closest("article");
    expect(guidance).not.toBeNull();
    expect(guidance).toHaveTextContent("Windows PowerShell 5.1");
    expect(guidance).toHaveTextContent("winget install --id Microsoft.PowerShell --source winget");
    expect(screen.getByRole("button", { name: "配置 WebCodex" })).toBeDisabled();

    vi.mocked(open).mockResolvedValue(readyState.project!.path);
    api.inspectProject.mockResolvedValue(readyState.project);
    fireEvent.click(screen.getByRole("button", { name: "选择文件夹" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "配置 WebCodex" })).toBeEnabled());

    fireEvent.click(within(guidance!).getByRole("button", { name: "打开 Microsoft 安装说明" }));
    await waitFor(() => expect(api.openPowerShellInstallGuide).toHaveBeenCalledTimes(1));
    fireEvent.click(within(guidance!).getByRole("button", { name: "重新检测" }));
    await waitFor(() => expect(screen.queryByText("建议安装 PowerShell 7")).not.toBeInTheDocument());
    expect(api.configureLocal).not.toHaveBeenCalled();
  });

  it("keeps first-run errors and the selected project through intermediate polling and Tunnel failure", async () => {
    vi.useFakeTimers();
    try {
      const setupResult = deferred<DesktopState>();
      api.getState.mockResolvedValue(firstRunState);
      api.configureLocal.mockReturnValueOnce(setupResult.promise).mockResolvedValue(readyState);
      api.startRegularTunnel.mockRejectedValue({ code: "tunnel_unavailable", message: "Tunnel failed", next_action: "Retry." });
      vi.mocked(open).mockResolvedValue(readyState.project!.path);
      api.inspectProject.mockResolvedValue(readyState.project);
      const view = renderApp();
      await act(async () => {});
      fireEvent.click(screen.getByRole("button", { name: /在此电脑使用 WebCodex/ }));
      fireEvent.click(screen.getByRole("button", { name: "选择文件夹" }));
      await act(async () => {});
      fireEvent.click(screen.getByRole("button", { name: "配置 WebCodex" }));
      expect(api.configureLocal).toHaveBeenCalledWith(readyState.project!.path);

      api.getState.mockResolvedValue(localSetupOperationState());
      await act(async () => { await vi.advanceTimersByTimeAsync(1_500); });
      expect(screen.getByRole("heading", { level: 1, name: "在此电脑配置 WebCodex" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "更改文件夹" })).toBeDisabled();

      api.getState.mockResolvedValue(setupState());
      await act(async () => {
        setupResult.reject({ code: "project_not_loaded", message: "Project failed", next_action: "Retry." });
        await vi.advanceTimersByTimeAsync(1_000);
      });
      expect(screen.getByRole("alert")).toHaveTextContent("project_not_loaded");
      fireEvent.click(screen.getByRole("button", { name: "重新加载项目" }));
      await act(async () => {});
      expect(screen.getByRole("alert")).toHaveTextContent("tunnel_unavailable");
      expect(screen.getByRole("heading", { level: 1, name: "在此电脑配置 WebCodex" })).toBeInTheDocument();
      api.startRegularTunnel.mockResolvedValue(readyState);
      fireEvent.click(screen.getByRole("button", { name: "配置 WebCodex" }));
      await act(async () => {});
      expect(screen.getByRole("heading", { level: 1, name: "WebCodex" })).toBeInTheDocument();
      view.unmount();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows an initial status failure and retries the complete fresh-start bootstrap", async () => {
    const retryState = deferred<DesktopState>();
    api.getState
      .mockRejectedValueOnce({
        code: "desktop_state_unavailable",
        message: "Desktop status is temporarily unavailable",
        next_action: "Retry reading the Desktop status.",
      })
      .mockReturnValueOnce(retryState.promise)
      .mockResolvedValue(readyState);
    renderApp();

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("desktop_state_unavailable");
    expect(screen.queryByText("正在加载 WebCodex…")).not.toBeInTheDocument();
    expect(api.configureLocal).not.toHaveBeenCalled();
    expect(api.resumeSavedRuntime).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(await screen.findByRole("status")).toHaveTextContent("正在加载 WebCodex…");
    expect(screen.queryByRole("button", { name: "重试" })).not.toBeInTheDocument();
    await waitFor(() => expect(api.getState).toHaveBeenCalledTimes(2));

    await act(async () => {
      retryState.resolve(firstRunState);
    });
    expect(await screen.findByRole("button", { name: /在此电脑使用 WebCodex/ })).toBeInTheDocument();
    expect(api.configureLocal).not.toHaveBeenCalled();
    expect(api.resumeSavedRuntime).not.toHaveBeenCalled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("keeps repeated initial status failures visible without automatically retrying", async () => {
    api.getState.mockRejectedValue({
      code: "desktop_state_unavailable",
      message: "Desktop status is unavailable",
      next_action: "Retry reading the Desktop status.",
    });
    renderApp();

    expect(await screen.findByRole("alert")).toHaveTextContent("desktop_state_unavailable");
    expect(api.getState).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("desktop_state_unavailable");
    expect(screen.getByRole("button", { name: "重试" })).toBeEnabled();
    expect(api.getState).toHaveBeenCalledTimes(2);
    expect(api.configureLocal).not.toHaveBeenCalled();
    expect(api.resumeSavedRuntime).not.toHaveBeenCalled();
  });

  it("renders localized failures as an alert while keeping safe diagnostics available", async () => {
    const setupState: DesktopState = {
      ...readyState,
      readiness: {
        ...readyState.readiness,
        runtime_ready: false,
        ready_for_chatgpt: false,
        server: "stopped",
        runner: "stopped",
        project: "configured",
        exposure: "disabled",
        summary_kind: "service_needs_attention",
        next_action_kind: "start_or_reconnect_service",
      },
    };
    api.getState.mockResolvedValue(setupState);
    api.refresh.mockResolvedValue(setupState);
    api.resumeSavedRuntime.mockRejectedValue({
      code: "server_unreachable",
      message: "WebCodex Service did not become ready",
      next_action: "Check diagnostics.",
    });

    renderApp();
    const submit = await screen.findByRole("button", { name: "恢复运行环境" });
    fireEvent.click(submit);

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("WebCodex 服务不可用");
    expect(alert).toHaveTextContent("server_unreachable");
  });

  it("offers a product recovery action when the selected project did not load", async () => {
    api.getState.mockResolvedValue(readyState);
    api.configureLocal
      .mockRejectedValueOnce({
        code: "project_not_loaded",
        message: "The selected project did not become ready in the Desktop-owned Runner",
        next_action: "Retry project setup.",
      })
      .mockResolvedValueOnce(readyState);
    renderApp();
    await screen.findByRole("heading", { level: 1, name: "WebCodex" });
    fireEvent.click(screen.getByRole("button", { name: "更改运行方式" }));
    fireEvent.click(screen.getByRole("button", { name: /在此电脑使用 WebCodex/ }));
    fireEvent.click(screen.getByRole("button", { name: "配置 WebCodex" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("项目尚未就绪");
    expect(alert).not.toHaveTextContent("registry");
    fireEvent.click(screen.getByRole("button", { name: "重新加载项目" }));
    await waitFor(() => expect(api.configureLocal).toHaveBeenCalledTimes(2));
  });

  it("uses the backend restart_quick_share presentation identity after a stopped share", async () => {
    const stoppedShare: DesktopState = {
      ...readyState,
      topology: {
        experience: "quick_share",
        server: { kind: "local" },
        runner: { kind: "local" },
        exposure: { kind: "cloudflare" },
        enrollment: { kind: "existing_profile", profile: "temporary_share" },
      },
      readiness: {
        server: "stopped",
        runner: "stopped",
        exposure: "error",
        project: "configured",
        runtime_ready: false,
        ready_for_chatgpt: false,
        summary: "Quick Share stopped",
        summary_kind: "quick_share_stopped",
        next_action: "Start Quick Share again.",
        next_action_kind: "restart_quick_share",
      },
      quick_share: {
        provider: "cloudflare",
        project: "C:\\fixture\\repo",
        mcp_url: null,
        clipboard_state: "unavailable",
        clipboard_contains: "bearer_credential",
        ready_for_chatgpt: false,
      },
    };
    api.getState.mockResolvedValue(stoppedShare);
    api.refresh.mockResolvedValue(stoppedShare);
    renderApp();

    expect(await screen.findByText("重新启动 Quick Share。")).toBeInTheDocument();
  });

  it("does not offer a duplicate start when the regular tunnel is running but handoff is degraded", async () => {
    const degradedTunnel: DesktopState = {
      ...readyState,
      topology: {
        ...readyState.topology!,
        exposure: { kind: "open_ai_tunnel" },
      },
      readiness: {
        ...readyState.readiness,
        exposure: "degraded",
        ready_for_chatgpt: false,
        summary: "ChatGPT connection is not verified",
        summary_kind: "connection_unverified",
        next_action: "Restore clipboard access, then restart the secure tunnel handoff.",
        next_action_kind: "restore_clipboard_handoff",
      },
      regular_tunnel: {
        provider: "openai",
        status: "ready",
        clipboard_state: "unavailable",
        clipboard_contains: "tunnel_id",
        ready_for_chatgpt: false,
      },
    };
    api.getState.mockResolvedValue(degradedTunnel);
    api.refresh.mockResolvedValue(degradedTunnel);
    renderApp();
    await screen.findByRole("heading", { level: 1, name: "WebCodex" });

    fireEvent.click(screen.getByRole("button", { name: "连接" }));
    expect(await screen.findByRole("heading", { level: 2, name: "安全隧道正在运行，但连接信息仍需要处理" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "启动安全隧道" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "停止安全隧道" })).toBeInTheDocument();
  });

  it("keeps navigation and Activity usable while a pending setup is globally observable", async () => {
    vi.useFakeTimers();
    try {
      const initial = setupState();
      const running = localSetupOperationState("running", 1);
      const runningWithNewActivity = localSetupOperationState("running", 2);
      const setupResult = deferred<DesktopState>();
      api.getState
        .mockResolvedValueOnce(initial)
        .mockResolvedValueOnce(running)
        .mockResolvedValueOnce(runningWithNewActivity)
        .mockResolvedValue(runningWithNewActivity);
      api.refresh.mockResolvedValue(initial);
      api.resumeSavedRuntime.mockReturnValue(setupResult.promise);
      api.activity.mockResolvedValue([]);

      const view = renderApp();
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
      });

      fireEvent.click(screen.getByRole("button", { name: "恢复运行环境" }));
      expect(api.resumeSavedRuntime).toHaveBeenCalledTimes(1);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_500);
      });

      const operationStatus = screen.getByRole("status", { name: "当前 Desktop 操作" });
      expect(operationStatus).toHaveTextContent("正在配置本机 WebCodex");
      expect(operationStatus).toHaveTextContent("停止当前操作不会自动重复执行尚未确认的步骤");

      fireEvent.click(screen.getByRole("button", { name: "活动" }));
      expect(screen.getByRole("heading", { level: 1, name: "最近的运行活动" })).toBeInTheDocument();
      expect(api.activity).toHaveBeenCalledTimes(1);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_000);
      });
      expect(api.activity).toHaveBeenCalledTimes(2);

      setupResult.resolve(readyState);
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(screen.queryByRole("status", { name: "当前 Desktop 操作" })).not.toBeInTheDocument();
      view.unmount();
    } finally {
      vi.useRealTimers();
    }
  });

  it("cancels only the observed operation id and presents the cancelling phase", async () => {
    const running = localSetupOperationState("running", 1);
    const cancelling = localSetupOperationState("cancelling", 2);
    api.getState.mockResolvedValueOnce(running).mockResolvedValue(cancelling);
    api.cancelOperation.mockResolvedValue(cancelling);

    renderApp();
    const cancel = await screen.findByRole("button", { name: "停止当前操作" });
    fireEvent.click(cancel);

    await waitFor(() => {
      expect(api.cancelOperation).toHaveBeenCalledWith("desktop-operation-a");
    });
    const cancellingStatus = screen.getByRole("status", { name: "当前 Desktop 操作" });
    expect(cancellingStatus).toHaveTextContent("正在停止当前操作…");
    const disabledCancel = screen.getByRole("button", { name: "正在停止当前操作…" });
    expect(disabledCancel).toBeDisabled();
    fireEvent.click(disabledCancel);
    expect(api.cancelOperation).toHaveBeenCalledTimes(1);
  });

  it("does not let an older polling response resurrect a completed operation", async () => {
    vi.useFakeTimers();
    try {
      const running = localSetupOperationState("running", 1);
      const stalePoll = deferred<DesktopState>();
      const terminal = { ...readyState, current_operation: null, activity_sequence: 2 };
      api.getState
        .mockResolvedValueOnce(running)
        .mockReturnValueOnce(stalePoll.promise)
        .mockResolvedValue(terminal);
      api.cancelOperation.mockResolvedValue(terminal);

      const view = renderApp();
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(screen.getByRole("status", { name: "当前 Desktop 操作" })).toBeInTheDocument();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_000);
      });
      expect(api.getState).toHaveBeenCalledTimes(2);

      fireEvent.click(screen.getByRole("button", { name: "停止当前操作" }));
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(screen.queryByRole("status", { name: "当前 Desktop 操作" })).not.toBeInTheDocument();

      stalePoll.resolve(running);
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(screen.queryByRole("status", { name: "当前 Desktop 操作" })).not.toBeInTheDocument();
      view.unmount();
    } finally {
      vi.useRealTimers();
    }
  });

  it("never promotes a locally ready Regular Tunnel to ChatGPT-connected state", async () => {
    vi.useFakeTimers();
    try {
      const locallyReadyTunnel: DesktopState = {
        ...readyState,
        topology: {
          ...readyState.topology!,
          exposure: { kind: "open_ai_tunnel" },
        },
        readiness: {
          ...readyState.readiness,
          exposure: "local_ready",
          ready_for_chatgpt: false,
          summary: "OpenAI Secure Tunnel is ready; waiting for ChatGPT to connect",
          summary_kind: "tunnel_ready_waiting_for_chat_gpt",
          next_action: "Connect the Tunnel in ChatGPT, then verify it with one real project read.",
          next_action_kind: "check_connection",
        },
        regular_tunnel: {
          provider: "openai",
          status: "ready",
          clipboard_state: "copied",
          clipboard_contains: "tunnel_id",
          ready_for_chatgpt: true,
        },
      };
      const failedTunnel: DesktopState = {
        ...locallyReadyTunnel,
        readiness: {
          ...locallyReadyTunnel.readiness,
          exposure: "error",
          ready_for_chatgpt: false,
          summary: "ChatGPT connection is not verified",
          summary_kind: "connection_unverified",
          next_action: "Restart the secure tunnel.",
          next_action_kind: "restart_secure_tunnel",
        },
        regular_tunnel: {
          ...locallyReadyTunnel.regular_tunnel!,
          status: "error",
          ready_for_chatgpt: false,
        },
      };

      api.getState
        .mockResolvedValueOnce(locallyReadyTunnel)
        .mockResolvedValueOnce(failedTunnel);
      api.refresh.mockResolvedValue(locallyReadyTunnel);
      const view = renderApp();
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
      });

      const readyStatus = screen.getByRole("status");
      expect(readyStatus).toHaveTextContent("OpenAI Secure Tunnel 已就绪，等待 ChatGPT 连接");
      expect(readyStatus).not.toHaveTextContent("可以使用");
      expect(screen.queryByText("外部连接已验证")).not.toBeInTheDocument();
      expect(screen.getByText("Tunnel 已就绪，等待 ChatGPT")).toBeInTheDocument();
      const connectionCard = screen.getByText("ChatGPT 连接").closest("article");
      expect(connectionCard).not.toBeNull();
      expect(connectionCard!.querySelector(".status-dot")).toHaveClass("pending");
      expect(api.getState).toHaveBeenCalledTimes(1);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_500);
      });

      expect(api.getState).toHaveBeenCalledTimes(2);
      expect(api.refresh).toHaveBeenCalledTimes(0);
      const failedStatus = screen.getByRole("status");
      expect(failedStatus).toHaveTextContent("ChatGPT 连接尚未验证");
      expect(failedStatus).toHaveTextContent("重新启动安全隧道。");
      expect(screen.queryByText("外部连接已验证")).not.toBeInTheDocument();
      expect(screen.getAllByText("ChatGPT 连接尚未验证").length).toBeGreaterThan(0);

      view.unmount();
      const callsAfterUnmount = api.getState.mock.calls.length;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3_000);
      });
      expect(api.getState).toHaveBeenCalledTimes(callsAfterUnmount);
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps regular tunnel controls off a remote Server connection page", async () => {
    const remote: DesktopState = {
      ...readyState,
      topology: {
        experience: "full",
        server: { kind: "remote", url: "https://server.example.test" },
        runner: { kind: "local" },
        exposure: { kind: "existing_https", url: "https://server.example.test" },
        enrollment: { kind: "managed_pairing" },
      },
      readiness: {
        ...readyState.readiness,
        exposure: "unknown",
        ready_for_chatgpt: false,
        summary_kind: "connection_unverified",
        next_action_kind: "check_connection",
      },
    };
    api.getState.mockResolvedValue(remote);
    api.refresh.mockResolvedValue(remote);
    renderApp();
    await screen.findByRole("heading", { level: 1, name: "WebCodex" });

    fireEvent.click(screen.getByRole("button", { name: "连接" }));
    expect(await screen.findByRole("heading", { level: 2, name: "远程 WebCodex Server" })).toBeInTheDocument();
    expect(screen.getByText("由远程 Server 管理")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "启动安全隧道" })).not.toBeInTheDocument();
  });

  it("automatically resumes a saved full runtime on Desktop launch", async () => {
    const stopped = { ...setupState(), runtime_autostart: true };
    api.getState.mockResolvedValue(stopped);
    api.resumeSavedRuntime.mockResolvedValue(readyState);

    renderApp();

    await waitFor(() => expect(api.resumeSavedRuntime).toHaveBeenCalledTimes(1));
    expect(await screen.findByRole("heading", { level: 1, name: "WebCodex" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "配置 WebCodex" })).not.toBeInTheDocument();
  });

  it("reconnects the remembered OpenAI Tunnel after restoring the runtime", async () => {
    const stopped: DesktopState = {
      ...setupState(),
      runtime_autostart: true,
      preferred_connection: "open_ai_tunnel",
    };
    const resumed: DesktopState = {
      ...readyState,
      runtime_autostart: true,
      preferred_connection: "open_ai_tunnel",
    };
    api.getState.mockResolvedValue(stopped);
    api.resumeSavedRuntime.mockResolvedValue(resumed);
    api.startRegularTunnel.mockResolvedValue(resumed);

    renderApp();

    await waitFor(() => expect(api.resumeSavedRuntime).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(api.startRegularTunnel).toHaveBeenCalledTimes(1));
  });

  it("keeps Remote Server as a first-class runtime choice after local setup", async () => {
    api.getState.mockResolvedValue(readyState);
    renderApp();
    await screen.findByRole("heading", { level: 1, name: "WebCodex" });

    fireEvent.click(screen.getByRole("button", { name: "更改运行方式" }));

    expect(await screen.findByRole("button", { name: /连接现有 Server/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /在此电脑使用 WebCodex/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /快速共享项目/ })).toBeInTheDocument();
  });
});
