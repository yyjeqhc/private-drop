import { useCallback, useEffect, useRef, useState } from "react";
import { desktopApi } from "./lib/desktop-api";
import type {
  ActivityEntry,
  DesktopError,
  DesktopOperationKind,
  DesktopState,
} from "./models/topology";
import { FirstRun } from "./features/onboarding/FirstRun";
import { Dashboard } from "./features/dashboard/Dashboard";
import { ProjectsPanel } from "./features/projects/ProjectsPanel";
import { ConnectionPanel } from "./features/connection/ConnectionPanel";
import { ActivityPanel } from "./features/activity/ActivityPanel";
import { SettingsPanel } from "./features/settings/SettingsPanel";
import { useLocale } from "./i18n/locale";
import { desktopErrorPresentation, normalizeDesktopError } from "./i18n/presentation";

type Navigation = "home" | "projects" | "connection" | "activity" | "settings";

const REGULAR_TUNNEL_OBSERVATION_INTERVAL_MS = 1_500;
const ACTIVE_OPERATION_OBSERVATION_INTERVAL_MS = 1_000;

export default function App() {
  const { locale, setLocale, t } = useLocale();
  const [state, setState] = useState<DesktopState | null>(null);
  const [activity, setActivity] = useState<ActivityEntry[]>([]);
  const [navigation, setNavigation] = useState<Navigation>("home");
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<DesktopError | null>(null);
  const [cancelSubmittingId, setCancelSubmittingId] = useState<string | null>(null);
  const [showSetup, setShowSetup] = useState(false);
  const [startupAttempt, setStartupAttempt] = useState(0);
  const stateVersionRef = useRef(0);
  const mainRef = useRef<HTMLElement>(null);
  const hasRegularTunnel = Boolean(state?.regular_tunnel);
  const hasCurrentOperation = Boolean(state?.current_operation);
  const hasLoadedState = Boolean(state);

  useEffect(() => {
    mainRef.current?.focus({ preventScroll: true });
    mainRef.current?.scrollTo?.({ top: 0 });
  }, [navigation, showSetup]);

  const openSetup = () => {
    setShowSetup(true);
    setNavigation("home");
  };

  const commitState = useCallback((next: DesktopState) => {
    stateVersionRef.current += 1;
    setState(next);
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const initial = await desktopApi.getState();
        if (cancelled) return;
        commitState(initial);
        if (initial.current_operation) return;
        const bootstrapLocal = !initial.topology;
        const resumeExisting = Boolean(
          initial.topology
          && initial.runtime_autostart
          && initial.topology.experience === "full",
        );
        if (!bootstrapLocal && !resumeExisting) return;

        setRefreshing(true);
        try {
          let next = bootstrapLocal
            ? await desktopApi.configureLocal(null)
            : await desktopApi.resumeSavedRuntime();
          if (cancelled) return;
          commitState(next);
          if (shouldStartPreferredTunnel(next)) {
            next = await desktopApi.startRegularTunnel();
            if (!cancelled) commitState(next);
          }
        } catch (value) {
          if (!cancelled) setError(normalizeDesktopError(value));
        } finally {
          if (!cancelled) setRefreshing(false);
        }
      } catch (value) {
        if (!cancelled) setError(normalizeDesktopError(value));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [commitState, startupAttempt]);

  useEffect(() => {
    if (!hasLoadedState) return;

    let cancelled = false;
    let timeoutId: number | undefined;
    const interval = hasCurrentOperation || refreshing
      ? ACTIVE_OPERATION_OBSERVATION_INTERVAL_MS
      : REGULAR_TUNNEL_OBSERVATION_INTERVAL_MS;

    const scheduleObservation = () => {
      timeoutId = window.setTimeout(() => {
        void (async () => {
          const observedVersion = stateVersionRef.current;
          try {
            const next = await desktopApi.getState();
            if (!cancelled && stateVersionRef.current === observedVersion) {
              commitState(next);
            }
          } catch {
            // Observation is best-effort. A transient invoke failure must not
            // create an error storm or a second concurrent observer.
          } finally {
            if (!cancelled) scheduleObservation();
          }
        })();
      }, interval);
    };

    scheduleObservation();
    return () => {
      cancelled = true;
      if (timeoutId !== undefined) window.clearTimeout(timeoutId);
    };
  }, [commitState, hasCurrentOperation, hasLoadedState, hasRegularTunnel, refreshing]);

  useEffect(() => {
    if (navigation === "activity") {
      void desktopApi.activity().then(setActivity).catch(() => undefined);
    }
  }, [navigation, state?.activity_sequence]);

  const runStateOperation = async (operation: () => Promise<DesktopState>) => {
    setError(null);
    try {
      commitState(await operation());
    } catch (value) {
      setError(normalizeDesktopError(value));
    }
  };

  const refresh = async () => {
    setRefreshing(true);
    try {
      await runStateOperation(desktopApi.refresh);
    } finally {
      setRefreshing(false);
    }
  };

  const resumeRuntime = async () => {
    setRefreshing(true);
    setError(null);
    try {
      let next = await desktopApi.resumeSavedRuntime();
      commitState(next);
      if (shouldStartPreferredTunnel(next)) {
        next = await desktopApi.startRegularTunnel();
        commitState(next);
      }
    } catch (value) {
      setError(normalizeDesktopError(value));
    } finally {
      setRefreshing(false);
    }
  };

  const cancelCurrentOperation = async () => {
    const observed = state?.current_operation;
    if (!observed || !observed.cancellable || observed.phase === "cancelling") return;
    setCancelSubmittingId(observed.id);
    setError(null);
    try {
      commitState(await desktopApi.cancelOperation(observed.id));
    } catch (value) {
      setError(normalizeDesktopError(value));
    } finally {
      setCancelSubmittingId((current) => current === observed.id ? null : current);
    }
  };

  if (!state) {
    return (
      <main className="splash">
        <div className="brand-mark" aria-hidden="true">W</div>
        {error ? (
          <section className="startup-error" aria-label="WebCodex">
            <AppError error={error} />
            <button
              className="primary-button"
              type="button"
              onClick={() => {
                setError(null);
                setStartupAttempt((attempt) => attempt + 1);
              }}
              data-webcodex-action="retry-desktop-startup"
            >
              {t("common.retry")}
            </button>
          </section>
        ) : (
          <span role="status">{t("app.loading")}</span>
        )}
      </main>
    );
  }

  const needsSetup = !state.topology || showSetup;

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand"><div className="brand-mark" aria-hidden="true">W</div><div><strong>WebCodex</strong><span>Desktop</span></div></div>
        <nav aria-label={t("nav.main")}>
          {(["home", "projects", "connection", "activity", "settings"] as Navigation[]).map((item) => (
            <button
              key={item}
              className={navigation === item ? "active" : ""}
              onClick={() => setNavigation(item)}
              aria-current={navigation === item ? "page" : undefined}
              data-webcodex-action={`navigate-${item}`}
            >
              <span className={`nav-icon nav-${item}`} aria-hidden="true" />
              {t(`nav.${item}`)}
            </button>
          ))}
        </nav>
        <div className="sidebar-locale">
          <label htmlFor="desktop-sidebar-locale">{t("locale.label")}</label>
          <select
            id="desktop-sidebar-locale"
            aria-label={t("locale.label")}
            value={locale}
            onChange={(event) => setLocale(event.target.value as typeof locale)}
            data-webcodex-control="locale"
          >
            <option value="zh-CN">{t("locale.zh")}</option>
            <option value="en-US">{t("locale.en")}</option>
          </select>
        </div>
        <div className="sidebar-status">
          <i className={`status-dot ${state.readiness.runtime_ready ? "ready" : "unknown"}`} aria-hidden="true" />
          <div><strong>{state.readiness.runtime_ready ? t("sidebar.runtimeReady") : state.topology ? t("common.stopped") : t("sidebar.needsSetup")}</strong><span>{state.readiness.ready_for_chatgpt ? t("sidebar.chatgptReady") : t("sidebar.connectionIncomplete")}</span></div>
        </div>
      </aside>

      <main className="main-content" ref={mainRef} tabIndex={-1}>
        {navigation === "home" && showSetup && state.topology && (
          <button className="back-button" onClick={() => setShowSetup(false)}>
            <span aria-hidden="true">← </span>{t("home.backToOverview")}
          </button>
        )}
        {state.current_operation && (
          <section
            className={`operation-status ${state.current_operation.phase}`}
            role="status"
            aria-live="polite"
            aria-label={t("operation.statusLabel")}
            data-webcodex-operation={state.current_operation.kind}
          >
            <div>
              <span className="section-kicker">
                {state.current_operation.phase === "cancelling"
                  ? t("operation.cancelling")
                  : t("operation.running")}
              </span>
              <strong>{operationLabel(state.current_operation.kind, t)}</strong>
              <span>{t("operation.cancelNote")}</span>
            </div>
            {state.current_operation.cancellable && (
              <button
                className="secondary-button"
                type="button"
                disabled={
                  state.current_operation.phase === "cancelling" ||
                  cancelSubmittingId === state.current_operation.id
                }
                onClick={() => void cancelCurrentOperation()}
                data-webcodex-action="cancel-desktop-operation"
              >
                {state.current_operation.phase === "cancelling"
                  ? t("operation.cancelling")
                  : t("operation.cancel")}
              </button>
            )}
          </section>
        )}
        {error && <AppError error={error} />}
        {navigation === "home" && (needsSetup ? (
          <FirstRun
            state={state}
            onState={commitState}
            chooseModeFirst={showSetup}
            onComplete={() => setShowSetup(false)}
          />
        ) : (
          <Dashboard
            state={state}
            refreshing={refreshing}
            onRefresh={() => void refresh()}
            onResumeRuntime={() => void resumeRuntime()}
            onConnectChatGpt={() => void runStateOperation(desktopApi.startRegularTunnel)}
            onChangeSetup={openSetup}
            onNavigate={setNavigation}
            onStopQuickShare={() => void runStateOperation(desktopApi.stopQuickShare)}
            onStopRuntime={() => void runStateOperation(desktopApi.stopLocalRuntime)}
          />
        ))}
        {navigation === "projects" && (
          <ProjectsPanel state={state} onConfigure={openSetup} />
        )}
        {navigation === "connection" && <ConnectionPanel state={state} onState={commitState} />}
        {navigation === "activity" && <ActivityPanel activity={activity} />}
        {navigation === "settings" && <SettingsPanel state={state} onState={commitState} />}
      </main>
    </div>
  );
}

function shouldStartPreferredTunnel(state: DesktopState) {
  return state.preferred_connection === "open_ai_tunnel" &&
    state.topology?.experience === "full" &&
    state.topology.server.kind === "local" &&
    state.readiness.runtime_ready &&
    state.openai_tunnel_configured &&
    !state.regular_tunnel;
}

function operationLabel(
  kind: DesktopOperationKind,
  t: ReturnType<typeof useLocale>["t"],
) {
  switch (kind) {
    case "local_setup": return t("operation.localSetup");
    case "remote_setup": return t("operation.remoteSetup");
    case "quick_share_start": return t("operation.quickShareStart");
    case "quick_share_stop": return t("operation.quickShareStop");
    case "regular_tunnel_start": return t("operation.regularTunnelStart");
    case "regular_tunnel_stop": return t("operation.regularTunnelStop");
    case "local_runtime_stop": return t("operation.localRuntimeStop");
    case "runtime_refresh": return t("operation.runtimeRefresh");
    case "runtime_resume": return t("operation.runtimeResume");
    case "tunnel_proxy_update": return t("operation.tunnelProxyUpdate");
  }
}

function AppError({ error }: { error: DesktopError }) {
  const { t } = useLocale();
  const presentation = desktopErrorPresentation(error, t);
  return (
    <div className="error-card app-error" role="alert">
      <strong>{presentation.title}</strong>
      <span>{presentation.action}</span>
      <details>
        <summary>{t("common.details")}</summary>
        <code>{error.code}</code>
        <p>{error.message}</p>
      </details>
    </div>
  );
}
