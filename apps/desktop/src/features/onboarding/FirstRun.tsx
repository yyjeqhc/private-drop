import { useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { desktopApi, type QuickShareProvider } from "../../lib/desktop-api";
import { useLocale } from "../../i18n/locale";
import { TunnelConfigDiagnostics } from "../connection/TunnelConfigDiagnostics";
import { PowerShellInstallGuidance } from "../settings/PowerShellInstallGuidance";
import {
  desktopErrorPresentation,
  normalizeDesktopError,
} from "../../i18n/presentation";
import type {
  DesktopError,
  DesktopState,
  ProjectSelection,
} from "../../models/topology";

type SetupMode = "local" | "remote" | "share";

interface FirstRunProps {
  state: DesktopState;
  onState: (state: DesktopState) => void;
  chooseModeFirst?: boolean;
  onComplete?: () => void;
}

export function FirstRun({ state, onState, chooseModeFirst = false, onComplete }: FirstRunProps) {
  const { t } = useLocale();
  const initialMode = useMemo<SetupMode | null>(() => {
    if (chooseModeFirst) return null;
    if (state.topology?.experience === "quick_share") return "share";
    if (state.topology?.server.kind === "local") return "local";
    if (state.topology?.server.kind === "remote") return "remote";
    return null;
  }, [chooseModeFirst, state.topology]);
  const [mode, setMode] = useState<SetupMode | null>(initialMode);
  const [project, setProject] = useState<ProjectSelection | null>(
    state.project ?? null,
  );
  const [serverUrl, setServerUrl] = useState(
    state.topology?.server.kind === "remote" ? state.topology.server.url : "",
  );
  const [pairingCode, setPairingCode] = useState("");
  const [remoteEnrollmentNeedsRefresh, setRemoteEnrollmentNeedsRefresh] = useState(false);
  const [provider, setProvider] = useState<QuickShareProvider>("cloudflare");
  const [connectAfterSetup, setConnectAfterSetup] = useState(state.openai_tunnel_configured);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<DesktopError | null>(null);
  const mutationBusy = busy || Boolean(state.current_operation);
  const canReuseRemoteEnrollment = Boolean(
    mode === "remote" &&
      !remoteEnrollmentNeedsRefresh &&
      state.project?.runtime_project_id &&
      state.topology?.experience === "full" &&
      state.topology.server.kind === "remote" &&
      sameServerOrigin(serverUrl, state.topology.server.url),
  );

  const chooseProject = async () => {
    setError(null);
    try {
      const selection = await open({
        directory: true,
        multiple: false,
        title: t("setup.chooseProject"),
      });
      if (typeof selection !== "string") return;
      setProject(await desktopApi.inspectProject(selection));
    } catch (value) {
      setError(normalizeDesktopError(value));
    }
  };

  const run = async () => {
    if (!mode || !project || mutationBusy) return;
    setBusy(true);
    setError(null);
    try {
      if (mode === "local") {
        let next = await desktopApi.configureLocal(project.path);
        onState(next);
        if (connectAfterSetup && next.openai_tunnel_configured && next.readiness.runtime_ready) {
          next = await desktopApi.startRegularTunnel();
          onState(next);
        }
        onComplete?.();
      } else if (mode === "remote") {
        if (!project) return;
        const oneTimeCode = pairingCode;
        setPairingCode("");
        const next = await desktopApi.configureRemote(
          serverUrl,
          oneTimeCode,
          project.path,
        );
        setRemoteEnrollmentNeedsRefresh(false);
        onState(next);
        onComplete?.();
      } else {
        if (!project) return;
        onState(await desktopApi.startQuickShare(project.path, provider));
        onComplete?.();
      }
    } catch (value) {
      const normalized = normalizeDesktopError(value);
      if (mode === "remote" && normalized.code === "pairing_code_invalid") {
        // The optimistic reuse hint is based on saved Desktop state. If the
        // backend proves that connection identity is no longer reusable, expose
        // the one-shot recovery field instead of trapping the user behind the
        // stale reuse hint.
        setRemoteEnrollmentNeedsRefresh(true);
      }
      setError(normalized);
    } finally {
      setBusy(false);
    }
  };

  if (!mode) {
    return (
      <section className="first-run" aria-labelledby="first-run-title" data-webcodex-page="first-run">
        <div className="eyebrow">{t("first.welcome")}</div>
        <h1 id="first-run-title">{t("first.title")}</h1>
        <p className="lede">{t("first.description")}</p>
        <ol className="setup-overview" aria-label={t("workspace.progress")}>
          <li><span>01</span>{t("workspace.prepare")}</li>
          <li><span>02</span>{t("workspace.connect")}</li>
          <li><span>03</span>{t("workspace.verify")}</li>
        </ol>
        <div className="entry-grid">
          <button className="entry-card recommended" onClick={() => setMode("local")} data-webcodex-action="choose-local-setup">
            <span className="entry-badge">{t("first.recommended")}</span>
            <strong>{t("first.localTitle")}</strong>
            <span>{t("first.localDescription")}</span>
          </button>
          <button className="entry-card" onClick={() => setMode("remote")} data-webcodex-action="choose-remote-setup">
            <strong>{t("first.remoteTitle")}</strong>
            <span>{t("first.remoteDescription")}</span>
          </button>
          <button className="entry-card" onClick={() => setMode("share")} data-webcodex-action="choose-quick-share-setup">
            <strong>{t("first.shareTitle")}</strong>
            <span>{t("first.shareDescription")}</span>
          </button>
        </div>
      </section>
    );
  }

  const presentation = error ? desktopErrorPresentation(error, t) : null;
  const serverInvalid = error?.code === "server_url_invalid" || error?.code === "server_unreachable";
  const pairingInvalid = error?.code === "pairing_code_invalid";

  return (
    <form
      className="setup-shell"
      aria-labelledby="setup-title"
      aria-busy={mutationBusy}
      data-webcodex-page="setup"
      onSubmit={(event) => {
        event.preventDefault();
        void run();
      }}
    >
      <button type="button" className="back-button" onClick={() => setMode(null)} data-webcodex-action="show-setup-options">
        {t("setup.back")}
      </button>
      <div className="eyebrow">{modeLabel(mode, t)}</div>
      <h1 id="setup-title">{setupTitle(mode, t)}</h1>
      <p className="lede">{setupDescription(mode, t)}</p>
      <div className="project-picker-card">
        <div>
          <span className="section-kicker">{t("setup.project")}</span>
          <strong>{project ? project.path : t("setup.chooseProject")}</strong>
          {mode === "local" && !project && (
            <span className="project-meta">{t("setup.projectRequired")}</span>
          )}
          {project && (
            <span className="project-meta">
              {t("setup.allowedRoot", {
                root: project.allowed_root,
                kind: project.is_git_repository ? t("setup.gitRepository") : t("setup.folder"),
              })}
            </span>
          )}
        </div>
        <button type="button" className="secondary-button" onClick={chooseProject} disabled={mutationBusy} data-webcodex-action="choose-project">
          {project ? t("setup.changeFolder") : t("setup.chooseFolder")}
        </button>
      </div>

      <PowerShellInstallGuidance state={state} onState={onState} />

      {mode === "local" && (
        <details className="setup-tunnel-details">
          <summary>{t("workspace.optionalTunnel")}</summary>
          <TunnelConfigDiagnostics state={state} onState={onState} />
        </details>
      )}

      {mode === "remote" && (
        <div className="form-card">
          <div className="field-group">
            <label htmlFor="setup-server-url">{t("setup.serverUrl")}</label>
            <input
              id="setup-server-url"
              type="url"
              value={serverUrl}
              onChange={(event) => setServerUrl(event.target.value)}
              placeholder="https://webcodex.example.com"
              disabled={mutationBusy}
              aria-describedby="setup-server-url-help"
              aria-invalid={serverInvalid || undefined}
              aria-errormessage={serverInvalid ? "setup-error" : undefined}
            />
            <span className="field-help" id="setup-server-url-help">{t("setup.serverUrlHelp")}</span>
          </div>
          {canReuseRemoteEnrollment ? (
            <div className="enrollment-note">
              <span className="section-kicker">{t("setup.enrollment")}</span>
              <strong>{t("setup.reuseEnrollment")}</strong>
              <span>{t("setup.reuseEnrollmentHelp")}</span>
            </div>
          ) : (
            <div className="field-group">
              <label htmlFor="setup-pairing-code">{t("setup.pairingCode")}</label>
              <input
                id="setup-pairing-code"
                type="password"
                value={pairingCode}
                onChange={(event) => setPairingCode(event.target.value)}
                placeholder="wc_pair_…"
                autoComplete="off"
                spellCheck={false}
                disabled={mutationBusy}
                aria-describedby="setup-pairing-code-help"
                aria-invalid={pairingInvalid || undefined}
                aria-errormessage={pairingInvalid ? "setup-error" : undefined}
              />
              <span className="field-help" id="setup-pairing-code-help">{t("setup.pairingCodeHelp")}</span>
            </div>
          )}
        </div>
      )}

      {mode === "share" && (
        <fieldset className="provider-row provider-fieldset" role="radiogroup" aria-labelledby="quick-share-provider-legend">
          <legend id="quick-share-provider-legend">{t("setup.providerLegend")}</legend>
          {(["cloudflare", "openai", "none"] as QuickShareProvider[]).map((value) => (
            <div
              className={`provider-option ${provider === value ? "selected" : ""}`}
              key={value}
            >
              <input
                id={`quick-share-provider-${value}`}
                type="radio"
                name="quick-share-provider"
                value={value}
                checked={provider === value}
                onChange={() => setProvider(value)}
                disabled={mutationBusy}
                aria-describedby={`quick-share-provider-${value}-description`}
                data-webcodex-control={`quick-share-provider-${value}`}
              />
              <label htmlFor={`quick-share-provider-${value}`}>
                <strong>{providerLabel(value, t)}</strong>
                <span id={`quick-share-provider-${value}-description`}>{providerDescription(value, t)}</span>
              </label>
            </div>
          ))}
        </fieldset>
      )}

      {mode === "local" && state.openai_tunnel_configured && (
        <label className="setup-choice-card" htmlFor="setup-connect-chatgpt">
          <input
            id="setup-connect-chatgpt"
            type="checkbox"
            checked={connectAfterSetup}
            onChange={(event) => setConnectAfterSetup(event.target.checked)}
            disabled={mutationBusy}
          />
          <span>
            <strong>{t("setup.connectChatGptAfterSetup")}</strong>
            <small>{t("setup.connectChatGptAfterSetupHelp")}</small>
          </span>
        </label>
      )}

      {mode === "remote" && (
        <details className="advanced-enrollment">
          <summary>{t("setup.advancedEnrollment")}</summary>
          <p>{t("setup.advancedEnrollmentHelp")}</p>
        </details>
      )}

      {error && (
        <div className="error-card" role="alert" id="setup-error">
          <strong>{presentation?.title}</strong>
          <span>{presentation?.action}</span>
          <details>
            <summary>{t("common.details")}</summary>
            <code>{error.code}</code>
            <p>{error.message}</p>
          </details>
          {error.code === "project_not_loaded" && (
            <div className="setup-recovery-actions">
              <button
                type="button"
                className="secondary-button"
                onClick={() => void run()}
                disabled={mutationBusy}
                data-webcodex-action="activate-project"
              >
                {t("setup.reloadProject")}
              </button>
              <span>{t("setup.reloadProjectHelp")}</span>
            </div>
          )}
        </div>
      )}

      <div className="setup-actions">
        <button
          type="submit"
          className="primary-button"
          disabled={
            mutationBusy ||
            !project ||
            (mode === "remote" &&
              (!serverUrl.trim() || (!canReuseRemoteEnrollment && !pairingCode.trim())))
          }
          data-webcodex-action={mode === "local" ? "configure-local" : mode === "remote" ? "configure-remote" : "start-quick-share"}
        >
          {mutationBusy ? t("common.checking") : actionLabel(mode, canReuseRemoteEnrollment, t)}
        </button>
        <span className="action-help">
          {mutationBusy ? t("setup.verifying") : t("setup.noTerminal")}
        </span>
      </div>
    </form>
  );
}

type Translate = ReturnType<typeof useLocale>["t"];

function modeLabel(mode: SetupMode, t: Translate) {
  return mode === "local" ? t("setup.localLabel") : mode === "remote" ? t("setup.remoteLabel") : t("setup.shareLabel");
}

function setupTitle(mode: SetupMode, t: Translate) {
  return mode === "local"
    ? t("setup.localTitle")
    : mode === "remote"
      ? t("setup.remoteTitle")
      : t("setup.shareTitle");
}

function setupDescription(mode: SetupMode, t: Translate) {
  if (mode === "local") return t("setup.localDescription");
  if (mode === "remote") return t("setup.remoteDescription");
  return t("setup.shareDescription");
}

function actionLabel(mode: SetupMode, canReuseRemoteEnrollment: boolean, t: Translate) {
  if (mode === "local") return t("setup.setUp");
  if (mode === "remote") return canReuseRemoteEnrollment ? t("setup.reconnect") : t("setup.connect");
  return t("setup.startShare");
}

function sameServerOrigin(left: string, right: string) {
  return left.trim().replace(/\/+$/, "").toLowerCase() === right.trim().replace(/\/+$/, "").toLowerCase();
}

function providerLabel(provider: QuickShareProvider, t: Translate) {
  if (provider === "cloudflare") return "Cloudflare";
  if (provider === "openai") return "OpenAI Secure Tunnel";
  return t("common.noChatGpt");
}

function providerDescription(provider: QuickShareProvider, t: Translate) {
  if (provider === "cloudflare") return t("provider.cloudflareDescription");
  if (provider === "openai") return t("provider.openaiDescription");
  return t("provider.localDescription");
}
