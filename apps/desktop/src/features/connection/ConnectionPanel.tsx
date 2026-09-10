import { useEffect, useState } from "react";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { desktopApi } from "../../lib/desktop-api";
import type { DesktopError, DesktopState } from "../../models/topology";
import { useLocale } from "../../i18n/locale";
import {
  desktopErrorPresentation,
  normalizeDesktopError,
  runtimeLabel,
} from "../../i18n/presentation";
import { TunnelConfigDiagnostics } from "./TunnelConfigDiagnostics";

type RegularProvider = "local" | "openai";

export function ConnectionPanel({
  state,
  onState,
}: {
  state: DesktopState;
  onState: (state: DesktopState) => void;
}) {
  const { t } = useLocale();
  const [provider, setProvider] = useState<RegularProvider>(
    state.regular_tunnel || state.preferred_connection === "open_ai_tunnel" ? "openai" : "local",
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<DesktopError | null>(null);
  const [copyStatus, setCopyStatus] = useState<"idle" | "copied" | "failed">("idle");
  const tunnelId = state.openai_tunnel_config.effective_tunnel_id ?? state.openai_tunnel_config.saved_tunnel_id;
  useEffect(() => {
    if (copyStatus !== "copied") return;
    const timer = window.setTimeout(() => setCopyStatus("idle"), 2500);
    return () => window.clearTimeout(timer);
  }, [copyStatus]);
  useEffect(() => setCopyStatus("idle"), [tunnelId]);
  const copyTunnelId = async () => {
    if (!tunnelId) return;
    try { await writeText(tunnelId); setCopyStatus("copied"); }
    catch { setCopyStatus("failed"); }
  };
  const topology = state.topology;
  const mutationBusy = busy || Boolean(state.current_operation);

  const run = async (operation: () => Promise<DesktopState>) => {
    if (mutationBusy) return;
    setBusy(true);
    setError(null);
    try {
      onState(await operation());
    } catch (value) {
      setError(normalizeDesktopError(value));
    } finally {
      setBusy(false);
    }
  };

  const chooseProvider = (value: RegularProvider) => {
    setProvider(value);
  };

  if (topology?.server.kind === "remote") {
    return (
      <section className="page-section" aria-labelledby="connection-title" data-webcodex-page="connection">
        <PageHeading />
        <article className="detail-card" aria-labelledby="remote-server-title">
          <h2 id="remote-server-title">{t("connection.remoteServer")}</h2>
          <strong>{topology.server.url}</strong>
          <dl className="detail-list">
            <div><dt>Runner</dt><dd>{t("connection.runnerThisComputer")}</dd></div>
            <div><dt>{t("connection.methods")}</dt><dd>{t("connection.externalManagedRemote")}</dd></div>
          </dl>
        </article>
      </section>
    );
  }

  if (topology?.experience === "quick_share") {
    return (
      <section className="page-section" aria-labelledby="connection-title" data-webcodex-page="connection">
        <PageHeading />
        <article className="detail-card">
          <span className="section-kicker">Quick Share</span>
          <strong>{currentConnection(state, t)}</strong>
          <p>{t("connection.quickShareManaged")}</p>
        </article>
      </section>
    );
  }

  const tunnelEstablished = state.regular_tunnel?.status === "ready";
  const tunnelLocallyReady = tunnelEstablished && Boolean(state.regular_tunnel?.ready_for_chatgpt);
  const tunnelError = state.regular_tunnel?.status === "error";
  const chatgptObserved = state.readiness.runtime_ready &&
    Boolean(state.chatgpt_activity?.observed) &&
    !tunnelError;
  const canStart = state.readiness.runtime_ready && state.openai_tunnel_configured && provider === "openai";

  return (
    <section
      className="page-section"
      aria-labelledby="connection-title"
      aria-busy={mutationBusy}
      data-webcodex-page="connection"
    >
      <PageHeading />

      <article className="connection-current detail-card" aria-labelledby="connection-current-title">
        <h2 id="connection-current-title" className="section-title">{t("connection.current")}</h2>
        <div className="status-value">
          <i className={`status-dot ${chatgptObserved ? "ready" : tunnelLocallyReady ? "ready" : tunnelError ? "error" : state.regular_tunnel ? "pending" : "unknown"}`} aria-hidden="true" />
          <strong>{!state.readiness.runtime_ready ? runtimeLabel(state, t) : chatgptObserved ? t("connection.observed") : tunnelLocallyReady ? t("connection.tunnelReady") : currentConnection(state, t)}</strong>
        </div>
        <p>{!state.readiness.runtime_ready ? t("workspace.afterStart") : chatgptObserved ? t("connection.observedDescription") : tunnelLocallyReady ? t("connection.waitingForChatGpt") : tunnelEstablished ? t("connection.tunnelHandoffNeedsAction") : t("connection.notVerified")}</p>
      </article>

      {error && <LocalizedError error={error} />}

      {state.regular_tunnel ? (
        <article className="handoff-card" aria-label="OpenAI Secure Tunnel">
          <div>
            <span className="section-kicker">OpenAI Secure Tunnel</span>
            <span>{tunnelError ? t("workspace.stopToRetry") : !tunnelEstablished ? t("connection.tunnelStarting") : null}</span>
          </div>
          <button
            className="danger-button"
            disabled={mutationBusy}
            onClick={() => void run(desktopApi.stopRegularTunnel)}
            data-webcodex-action="stop-regular-tunnel"
          >
            {mutationBusy ? t("common.checking") : t("connection.stopTunnel")}
          </button>
        </article>
      ) : (
        <div
          className="connection-form"
        >
          <fieldset className="provider-row provider-fieldset" role="radiogroup" aria-labelledby="regular-provider-legend">
            <legend id="regular-provider-legend">{t("connection.methods")}</legend>
            <ProviderOption
              id="regular-provider-local"
              value="local"
              checked={provider === "local"}
              onChange={chooseProvider}
              title={t("connection.noDesktopTunnel")}
              description={t("connection.localDescription")}
              disabled={mutationBusy}
            />
            <ProviderOption
              id="regular-provider-openai"
              value="openai"
              checked={provider === "openai"}
              onChange={chooseProvider}
              title="OpenAI Secure Tunnel"
              description={state.openai_tunnel_configured ? t("connection.openaiDescription") : t("connection.openaiNotConfigured")}
              disabled={mutationBusy || !state.openai_tunnel_configured}
            />
          </fieldset>

          {!state.readiness.runtime_ready && <p className="inline-note">{t("connection.runtimeRequired")}</p>}

          {provider === "openai" && (
            <button className="primary-button" disabled={mutationBusy || !canStart} onClick={() => void run(desktopApi.startRegularTunnel)} data-webcodex-action="start-regular-tunnel">
              {mutationBusy ? t("connection.tunnelStarting") : t("home.connectChatGpt")}
            </button>
          )}
        </div>
      )}
      {tunnelId && (
        <article className="detail-card tunnel-copy">
          <label htmlFor="active-tunnel-id">Tunnel ID</label>
          <input id="active-tunnel-id" readOnly value={tunnelId} onFocus={(event) => event.target.select()} />
          <button className="secondary-button" onClick={() => void copyTunnelId()}>{t("connection.copyTunnelId")}</button>
          <span role="status">{copyStatus === "copied" ? t("connection.clipboardReady") : copyStatus === "failed" ? t("connection.copyFailed") : ""}</span>
        </article>
      )}
      <article className="connection-instructions detail-card">
        <h2>{t("workspace.handoffTitle")}</h2>
        <ol>
          <li>{t("workspace.handoffOne")}</li>
          <li>{t("workspace.handoffTwo")}</li>
          <li>{t("workspace.verifyHint")}</li>
        </ol>
      </article>
      <details className="setup-tunnel-details" open={!state.openai_tunnel_configured}>
        <summary>{t("workspace.optionalTunnel")}</summary>
        <p>{t("connection.description")}</p>
        <TunnelConfigDiagnostics state={state} onState={onState} />
      </details>
    </section>
  );
}

function PageHeading() {
  const { t } = useLocale();
  return (
    <>
      <div className="eyebrow">{t("connection.eyebrow")}</div>
      <h1 id="connection-title">{t("connection.title")}</h1>
    </>
  );
}

function ProviderOption({
  id,
  value,
  checked,
  onChange,
  title,
  description,
  disabled,
}: {
  id: string;
  value: RegularProvider;
  checked: boolean;
  onChange: (value: RegularProvider) => void;
  title: string;
  description: string;
  disabled: boolean;
}) {
  const descriptionId = `${id}-description`;
  return (
    <div className={`provider-option ${checked ? "selected" : ""}`}>
      <input
        id={id}
        type="radio"
        name="regular-connection-provider"
        value={value}
        checked={checked}
        onChange={() => onChange(value)}
        aria-describedby={descriptionId}
        disabled={disabled}
      />
      <label htmlFor={id}>
        <strong>{title}</strong>
        <span id={descriptionId}>{description}</span>
      </label>
    </div>
  );
}

function LocalizedError({ error }: { error: DesktopError }) {
  const { t } = useLocale();
  const presentation = desktopErrorPresentation(error, t);
  return (
    <div className="error-card" role="alert">
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

function currentConnection(state: DesktopState, t: ReturnType<typeof useLocale>["t"]) {
  if (state.regular_tunnel) return "OpenAI Secure Tunnel";
  const exposure = state.topology?.exposure;
  if (!exposure || exposure.kind === "none") return t("connection.noDesktopTunnel");
  if (exposure.kind === "existing_https") return `Existing HTTPS · ${exposure.url}`;
  if (exposure.kind === "cloudflare") return "Cloudflare Quick Share";
  return "OpenAI Secure Tunnel";
}

