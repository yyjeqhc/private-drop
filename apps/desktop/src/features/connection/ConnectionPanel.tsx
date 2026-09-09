import { useState } from "react";
import { desktopApi } from "../../lib/desktop-api";
import type { DesktopError, DesktopState } from "../../models/topology";
import { useLocale } from "../../i18n/locale";
import {
  desktopErrorPresentation,
  normalizeDesktopError,
} from "../../i18n/presentation";
import { TunnelConfigDiagnostics } from "./TunnelConfigDiagnostics";

type RegularProvider = "local" | "openai" | "cloudflare";

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
  const topology = state.topology;
  const mutationBusy = busy || Boolean(state.current_operation);

  const run = async (operation: () => Promise<DesktopState>) => {
    if (state.current_operation) return;
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
    if (value === "openai" && !state.regular_tunnel && state.readiness.runtime_ready && state.openai_tunnel_configured) {
      void run(desktopApi.startRegularTunnel);
    } else if (value === "local" && state.regular_tunnel) {
      void run(desktopApi.stopRegularTunnel);
    }
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

      <TunnelConfigDiagnostics state={state} onState={onState} />

      <article className="connection-current detail-card" aria-labelledby="connection-current-title">
        <h2 id="connection-current-title" className="section-title">{t("connection.current")}</h2>
        <div className="status-value">
          <i className={`status-dot ${chatgptObserved ? "ready" : tunnelLocallyReady ? "ready" : tunnelError ? "error" : state.regular_tunnel ? "pending" : "unknown"}`} aria-hidden="true" />
          <strong>{chatgptObserved ? t("connection.observed") : tunnelLocallyReady ? t("connection.tunnelReady") : currentConnection(state, t)}</strong>
        </div>
        <p>{chatgptObserved ? t("connection.observedDescription") : tunnelLocallyReady ? t("connection.waitingForChatGpt") : tunnelEstablished ? t("connection.tunnelHandoffNeedsAction") : t("connection.notVerified")}</p>
      </article>

      {tunnelEstablished ? (
        <article className="handoff-card" aria-labelledby="regular-tunnel-ready-title">
          <div>
            <span className="section-kicker">OpenAI Secure Tunnel</span>
            <h2 id="regular-tunnel-ready-title" className="handoff-title">{tunnelLocallyReady ? t("connection.tunnelReady") : t("connection.tunnelHandoffNeedsAction")}</h2>
            <span>{state.regular_tunnel?.clipboard_state === "copied" ? t("connection.clipboardReady") : t("clipboard.unavailable")}</span>
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
            <ProviderOption
              id="regular-provider-cloudflare"
              value="cloudflare"
              checked={provider === "cloudflare"}
              onChange={chooseProvider}
              title="Cloudflare"
              description={t("connection.cloudflareQuickOnly")}
              disabled
            />
          </fieldset>

          {!state.readiness.runtime_ready && <p className="inline-note">{t("connection.runtimeRequired")}</p>}

          {error && <LocalizedError error={error} />}
          {provider === "openai" && canStart && (
            <p className="inline-note">{mutationBusy ? t("connection.tunnelStarting") : t("connection.openaiSelectHint")}</p>
          )}
        </div>
      )}
    </section>
  );
}

function PageHeading() {
  const { t } = useLocale();
  return (
    <>
      <div className="eyebrow">{t("connection.eyebrow")}</div>
      <h1 id="connection-title">{t("connection.title")}</h1>
      <p className="lede">{t("connection.description")}</p>
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

