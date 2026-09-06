import type { DesktopState } from "../../models/topology";
import { useLocale } from "../../i18n/locale";
import {
  projectReadinessLabel,
  readinessNextAction,
  readinessSummary,
  runnerReadinessLabel,
  serverReadinessLabel,
} from "../../i18n/presentation";

interface DashboardProps {
  state: DesktopState;
  refreshing: boolean;
  onRefresh: () => void;
  onResumeRuntime: () => void;
  onConnectChatGpt: () => void;
  onChangeSetup: () => void;
  onStopQuickShare: () => void;
  onStopRuntime: () => void;
}

export function Dashboard({
  state,
  refreshing,
  onRefresh,
  onResumeRuntime,
  onConnectChatGpt,
  onChangeSetup,
  onStopQuickShare,
  onStopRuntime,
}: DashboardProps) {
  const { t } = useLocale();
  const isQuickShare = state.topology?.experience === "quick_share";
  const operationBusy = Boolean(state.current_operation);
  const canResumeRuntime = !isQuickShare && Boolean(state.topology) && !state.readiness.runtime_ready;
  const canConnectChatGpt = !isQuickShare &&
    state.topology?.server.kind === "local" &&
    state.readiness.runtime_ready &&
    state.openai_tunnel_configured &&
    !state.regular_tunnel;
  const summary = readinessSummary(state.readiness.summary_kind, state.readiness.summary, t);
  const nextAction = readinessNextAction(
    state.readiness.next_action_kind,
    state.readiness.next_action,
    t,
  );
  return (
    <section
      className="page-section"
      aria-labelledby="home-title"
      aria-busy={refreshing}
      data-webcodex-page="home"
    >
      <div className="page-heading-row">
        <div>
          <div className="eyebrow">{t("home.eyebrow")}</div>
          <h1 id="home-title">WebCodex</h1>
          <p className="lede">{summary}</p>
        </div>
        <button
          className="secondary-button"
          onClick={onRefresh}
          disabled={refreshing || operationBusy}
          data-webcodex-action="refresh-runtime"
        >
          {refreshing ? t("home.checking") : t("home.refresh")}
        </button>
      </div>

      <div className="status-grid">
        <StatusCard
          title={t("home.service")}
          value={serviceLabel(state, t)}
          state={state.readiness.server}
          explanation={serviceExplanation(state, t)}
        />
        <StatusCard
          title={t("home.runner")}
          value={runnerReadinessLabel(state.readiness.runner, t)}
          state={state.readiness.runner}
          explanation={t("home.runnerExplanation")}
        />
        <StatusCard
          title={t("home.projects")}
          value={state.readiness.project === "ready" ? t("home.projectReady") : projectReadinessLabel(state.readiness.project, t)}
          state={state.readiness.project}
          explanation={state.project?.path ?? t("home.noProject")}
        />
        <StatusCard
          title={t("home.connection")}
          value={connectionLabel(state, t)}
          state={state.readiness.exposure}
          explanation={connectionExplanation(state, t)}
        />
      </div>

      <div
        className={`readiness-banner ${state.readiness.ready_for_chatgpt ? "ready" : "pending"}`}
        role="status"
        aria-live="polite"
        aria-atomic="true"
      >
        <div>
          <span className="section-kicker">{t("home.overall")}</span>
          <strong>{state.readiness.ready_for_chatgpt ? t("home.readyToUse") : summary}</strong>
        </div>
        <div className="readiness-actions">
          {canResumeRuntime && (
            <button className="primary-button" onClick={onResumeRuntime} disabled={operationBusy || refreshing} data-webcodex-action="resume-runtime">
              {t("home.resumeRuntime")}
            </button>
          )}
          {canConnectChatGpt && (
            <button className="primary-button" onClick={onConnectChatGpt} disabled={operationBusy} data-webcodex-action="connect-chatgpt">
              {t("home.connectChatGpt")}
            </button>
          )}
          {nextAction && !canResumeRuntime && !canConnectChatGpt && <span>{nextAction}</span>}
        </div>
      </div>

      {state.quick_share && (
        <div className="handoff-card">
          <div>
            <span className="section-kicker">{t("home.quickShareHandoff")}</span>
            <strong>{state.quick_share.ready_for_chatgpt ? t("home.handoffReady") : t("home.handoffAction")}</strong>
            {state.quick_share.mcp_url && <code>{state.quick_share.mcp_url}</code>}
            <span>{quickShareClipboardLabel(state.quick_share.clipboard_state, state.quick_share.clipboard_contains, t)}</span>
          </div>
          <button className="danger-button" onClick={onStopQuickShare} disabled={operationBusy} data-webcodex-action="stop-quick-share">{t("home.stopShare")}</button>
        </div>
      )}

      {!isQuickShare && state.topology && (
        <div className="runtime-actions">
          <span>{t("home.runtimeOwnership")}</span>
          <div className="runtime-action-buttons">
            <button className="secondary-button" onClick={onChangeSetup} disabled={operationBusy} data-webcodex-action="change-runtime-setup">{t("home.changeSetup")}</button>
            {state.readiness.runtime_ready && (
              <button className="secondary-button" onClick={onStopRuntime} disabled={operationBusy} data-webcodex-action="stop-runtime">{t("home.stopRuntime")}</button>
            )}
          </div>
        </div>
      )}
    </section>
  );
}

function StatusCard({
  title,
  value,
  state,
  explanation,
}: {
  title: string;
  value: string;
  state: string;
  explanation: string;
}) {
  const tone = state === "ready" || state === "remote_ready" ? "ready" : state === "error" ? "error" : state === "unknown" ? "unknown" : "pending";
  return (
    <article className="status-card">
      <span className="section-kicker">{title}</span>
      <div className="status-value"><i className={`status-dot ${tone}`} aria-hidden="true" />{value}</div>
      <p>{explanation}</p>
    </article>
  );
}

type Translate = ReturnType<typeof useLocale>["t"];

function serviceLabel(state: DesktopState, t: Translate) {
  if (state.readiness.server === "ready" && state.topology?.server.kind === "remote") return t("common.connected");
  return serverReadinessLabel(state.readiness.server, t);
}

function serviceExplanation(state: DesktopState, t: Translate) {
  if (state.topology?.server.kind === "remote") return t("home.serviceRemote", { url: state.topology.server.url });
  return t("home.serviceLocal");
}

function quickShareClipboardLabel(state: string, contains: string, t: Translate) {
  if (state !== "copied") return t("clipboard.unavailable");
  if (contains === "tunnel_id") return t("clipboard.tunnelId");
  if (contains === "bearer_credential") return t("clipboard.bearer");
  if (contains === "sensitive_mcp_url") return t("clipboard.sensitiveUrl");
  return t("clipboard.copied");
}

function connectionLabel(state: DesktopState, t: Translate) {
  const exposure = state.topology?.exposure;
  if (!exposure || exposure.kind === "none") return t("common.noChatGpt");
  if (exposure.kind === "cloudflare") return "Cloudflare";
  if (exposure.kind === "open_ai_tunnel") return "OpenAI Secure Tunnel";
  return "Existing HTTPS";
}

function connectionExplanation(state: DesktopState, t: Translate) {
  if (state.readiness.exposure === "remote_ready") return t("home.connectionRemoteReady");
  if (state.readiness.exposure === "local_ready") return t("home.connectionLocalReady");
  return t("home.connectionUnverified");
}

