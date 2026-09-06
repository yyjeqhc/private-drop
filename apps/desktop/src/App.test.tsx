import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { DesktopState } from "./models/topology";
import { LocaleProvider } from "./i18n/locale";

const api = vi.hoisted(() => ({
  getState: vi.fn(),
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
}));

vi.mock("./lib/desktop-api", () => ({
  desktopApi: api,
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn(),
}));

import App from "./App";

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
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
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
    api.activity.mockResolvedValue([]);
    api.resumeSavedRuntime.mockResolvedValue(readyState);
    api.updateTunnelProxy.mockResolvedValue(readyState);
    api.startRegularTunnel.mockResolvedValue(readyState);
    api.stopRegularTunnel.mockResolvedValue(readyState);
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

  it("exposes Chinese first-run actions and native Quick Share radio state", async () => {
    api.getState.mockResolvedValue(firstRunState);
    renderApp();

    expect(await screen.findByRole("button", { name: /在此电脑使用 WebCodex/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /连接现有 Server/ })).toBeInTheDocument();
    const quickShare = screen.getByRole("button", { name: /快速共享项目/ });
    fireEvent.click(quickShare);

    expect(screen.getByRole("radiogroup", { name: "Quick Share 连接方式" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: /Cloudflare/ })).toBeChecked();
    expect(screen.getByRole("radio", { name: /OpenAI Secure Tunnel/ })).not.toBeChecked();
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
    expect(await screen.findByRole("heading", { level: 2, name: "安全隧道已建立，连接信息需要处理" })).toBeInTheDocument();
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

  it("revokes stale connected state from background tunnel ownership observation without manual refresh", async () => {
    vi.useFakeTimers();
    try {
      const connectedTunnel: DesktopState = {
        ...readyState,
        topology: {
          ...readyState.topology!,
          exposure: { kind: "open_ai_tunnel" },
        },
        readiness: {
          ...readyState.readiness,
          exposure: "remote_ready",
          ready_for_chatgpt: true,
          summary: "Ready to use with ChatGPT",
          summary_kind: "ready_for_chat_gpt",
          next_action: null,
          next_action_kind: null,
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
        ...connectedTunnel,
        readiness: {
          ...connectedTunnel.readiness,
          exposure: "error",
          ready_for_chatgpt: false,
          summary: "ChatGPT connection is not verified",
          summary_kind: "connection_unverified",
          next_action: "Restart the secure tunnel.",
          next_action_kind: "restart_secure_tunnel",
        },
        regular_tunnel: {
          ...connectedTunnel.regular_tunnel!,
          status: "error",
          ready_for_chatgpt: false,
        },
      };

      api.getState
        .mockResolvedValueOnce(connectedTunnel)
        .mockResolvedValueOnce(failedTunnel);
      api.refresh.mockResolvedValue(connectedTunnel);
      const view = renderApp();
      await act(async () => {
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
      });

      const readyStatus = screen.getByRole("status");
      expect(readyStatus).toHaveTextContent("可以使用");
      expect(screen.getByText("ChatGPT 已就绪")).toBeInTheDocument();
      expect(api.getState).toHaveBeenCalledTimes(1);

      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_500);
      });

      expect(api.getState).toHaveBeenCalledTimes(2);
      expect(api.refresh).toHaveBeenCalledTimes(0);
      const failedStatus = screen.getByRole("status");
      expect(failedStatus).toHaveTextContent("ChatGPT 连接尚未验证");
      expect(failedStatus).toHaveTextContent("重新启动安全隧道。");
      expect(screen.queryByText("ChatGPT 已就绪")).not.toBeInTheDocument();
      expect(screen.getByText("ChatGPT 连接未完成")).toBeInTheDocument();

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
