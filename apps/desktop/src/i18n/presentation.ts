import type {
  ActivityEntry,
  DesktopError,
  ProjectReadiness,
  ReadinessNextActionKind,
  ReadinessSummaryKind,
  RunnerReadiness,
  ServerReadiness,
} from "../models/topology";
import type { MessageKey } from "./locale";

type Translate = (key: MessageKey, params?: Record<string, string | number>) => string;

const summaryKeys: Record<ReadinessSummaryKind, MessageKey> = {
  ready_for_chat_gpt: "readiness.readyForChatGpt",
  service_needs_attention: "readiness.serviceNeedsAttention",
  runner_disconnected: "readiness.runnerDisconnected",
  project_not_ready: "readiness.projectNotReady",
  runtime_ready_local_only: "readiness.runtimeReadyLocalOnly",
  tunnel_ready_waiting_for_chat_gpt: "readiness.tunnelReadyWaitingForChatGpt",
  connection_unverified: "readiness.connectionUnverified",
  quick_share_stopped: "readiness.quickShareStopped",
};

const nextActionKeys: Record<ReadinessNextActionKind, MessageKey> = {
  start_or_reconnect_service: "action.startOrReconnectService",
  start_runner: "action.startRunner",
  add_or_reload_project: "action.addOrReloadProject",
  choose_connection: "action.chooseConnection",
  check_connection: "action.checkConnection",
  restart_quick_share: "action.restartQuickShare",
  restore_clipboard_handoff: "action.restoreClipboard",
  restart_secure_tunnel: "action.restartSecureTunnel",
};

export function readinessSummary(
  kind: ReadinessSummaryKind,
  fallback: string,
  t: Translate,
) {
  return summaryKeys[kind] ? t(summaryKeys[kind]) : fallback;
}

export function readinessNextAction(
  kind: ReadinessNextActionKind | null | undefined,
  fallback: string | null | undefined,
  t: Translate,
) {
  return kind ? t(nextActionKeys[kind]) : fallback ?? null;
}

const activityKeys: Record<ActivityEntry["event_kind"], MessageKey> = {
  process_started: "activity.processStarted",
  process_exited: "activity.processExited",
  process_observation_failed: "activity.processObservationFailed",
  process_stopping: "activity.processStopping",
  process_stopped: "activity.processStopped",
  local_setup_preparing: "activity.localSetupPreparing",
  local_runtime_ready: "activity.localRuntimeReady",
  remote_connecting: "activity.remoteConnecting",
  remote_connected: "activity.remoteConnected",
  quick_share_starting: "activity.quickShareStarting",
  quick_share_ready: "activity.quickShareReady",
  quick_share_stopped: "activity.quickShareStopped",
  regular_tunnel_starting: "activity.regularTunnelStarting",
  regular_tunnel_ready: "activity.regularTunnelReady",
  regular_tunnel_stopped: "activity.regularTunnelStopped",
  runtime_stopped: "activity.runtimeStopped",
  operation_started: "activity.operationStarted",
  operation_cancel_requested: "activity.operationCancelRequested",
  operation_cancelled: "activity.operationCancelled",
  operation_failed: "activity.operationFailed",
};

export function activityMessage(entry: ActivityEntry, t: Translate) {
  return t(activityKeys[entry.event_kind]);
}

export function activitySource(source: string, t: Translate) {
  const key = `activity.source.${source}` as MessageKey;
  return key in {
    "activity.source.desktop": true,
    "activity.source.service": true,
    "activity.source.runner": true,
    "activity.source.quick_share": true,
    "activity.source.regular_tunnel": true,
  } ? t(key) : source.replaceAll("_", " ");
}

type ErrorPresentation = { title: string; action: string };

const binaryErrors = new Set([
  "binaries_not_checked",
  "binary_directory_invalid",
  "binary_directory_missing",
  "binary_missing",
  "binary_version_mismatch",
  "binary_version_unverifiable",
  "binary_probe_failed",
]);
const serverErrors = new Set([
  "server_unreachable",
  "server_start_failed",
  "server_unavailable",
  "local_port_unavailable",
]);
const runnerErrors = new Set(["runner_offline"]);
const projectErrors = new Set([
  "project_unavailable",
  "project_not_directory",
  "project_not_loaded",
]);
const enrollmentErrors = new Set([
  "webcodex_command_failed",
  "webcodex_command_start_failed",
  "webcodex_command_input_failed",
  "webcodex_command_wait_failed",
  "webcodex_command_timeout",
]);
const tunnelErrors = new Set([
  "tunnel_unavailable",
  "tunnel_auth_invalid",
  "regular_tunnel_not_ready",
  "regular_tunnel_already_running",
  "quick_share_not_ready",
]);
const processErrors = new Set([
  "process_start_failed",
  "process_already_running",
  "quick_share_already_running",
]);

export function desktopErrorPresentation(error: DesktopError, t: Translate): ErrorPresentation {
  if (binaryErrors.has(error.code)) return { title: t("error.binaryTitle"), action: t("error.binaryAction") };
  if (serverErrors.has(error.code) || error.code === "server_url_invalid") return { title: t("error.serverTitle"), action: t("error.serverAction") };
  if (error.code === "runtime_not_ready") return { title: t("error.runtimeTitle"), action: t("error.runtimeAction") };
  if (runnerErrors.has(error.code)) return { title: t("error.runnerTitle"), action: t("error.runnerAction") };
  if (projectErrors.has(error.code)) return { title: t("error.projectTitle"), action: t("error.projectAction") };
  if (error.code === "pairing_code_invalid") return { title: t("error.pairingTitle"), action: t("error.pairingAction") };
  if (enrollmentErrors.has(error.code)) return { title: t("error.enrollmentTitle"), action: t("error.enrollmentAction") };
  if (tunnelErrors.has(error.code)) return { title: t("error.tunnelTitle"), action: t("error.tunnelAction") };
  if (processErrors.has(error.code)) return { title: t("error.processTitle"), action: t("error.processAction") };
  if (error.code === "webcodex_contract_invalid") return { title: t("error.contractTitle"), action: t("error.contractAction") };
  if (error.code === "unsupported_topology" || error.code === "quick_share_provider_invalid") return { title: t("error.topologyTitle"), action: t("error.topologyAction") };
  if (error.code === "desktop_operation_busy" || error.code === "desktop_operation_not_cancellable") return { title: t("error.operationBusyTitle"), action: t("error.operationBusyAction") };
  if (error.code === "desktop_operation_not_current") return { title: t("error.operationStaleTitle"), action: t("error.operationStaleAction") };
  if (error.code === "desktop_operation_cancelled") return { title: t("error.operationCancelledTitle"), action: t("error.operationCancelledAction") };
  return { title: t("error.fallbackTitle"), action: t("error.fallbackAction") };
}

export function normalizeDesktopError(value: unknown): DesktopError {
  if (value && typeof value === "object") {
    const candidate = value as Partial<DesktopError>;
    if (candidate.code && candidate.message && candidate.next_action) {
      return candidate as DesktopError;
    }
  }
  return {
    code: "desktop_operation_failed",
    message: "Desktop could not complete the operation.",
    next_action: "Retry the operation or open Activity for safe diagnostics.",
  };
}

export function serverReadinessLabel(value: ServerReadiness, t: Translate) {
  if (value === "ready") return t("common.running");
  if (value === "stopped") return t("common.stopped");
  if (value === "starting") return t("common.starting");
  if (value === "error") return t("common.error");
  return t("common.unknown");
}

export function runnerReadinessLabel(value: RunnerReadiness, t: Translate) {
  if (value === "ready") return t("common.connected");
  if (value === "stopped") return t("common.stopped");
  if (value === "connecting") return t("common.starting");
  if (value === "error") return t("common.error");
  return t("common.unknown");
}

export function projectReadinessLabel(value: ProjectReadiness, t: Translate) {
  if (value === "ready") return t("common.ready");
  if (value === "configured") return t("common.configured");
  if (value === "reload_required") return t("readiness.projectNotReady");
  if (value === "error") return t("common.error");
  if (value === "none") return t("project.none");
  return t("common.unknown");
}
