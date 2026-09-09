import { useEffect, useState } from "react";
import { desktopApi } from "../../lib/desktop-api";
import type { DesktopError, DesktopState, TunnelProxyMode } from "../../models/topology";
import { useLocale } from "../../i18n/locale";
import { desktopErrorPresentation, normalizeDesktopError } from "../../i18n/presentation";
import { TunnelConfigDiagnostics } from "../connection/TunnelConfigDiagnostics";
import { PowerShellInstallGuidance } from "./PowerShellInstallGuidance";

export function SettingsPanel({
  state,
  onState,
}: {
  state: DesktopState;
  onState: (state: DesktopState) => void;
}) {
  const { locale, setLocale, t } = useLocale();
  const [proxyMode, setProxyMode] = useState<TunnelProxyMode>(state.tunnel_proxy.mode);
  const [customProxy, setCustomProxy] = useState(state.tunnel_proxy.custom_url ?? "");
  const [savingProxy, setSavingProxy] = useState(false);
  const [proxyError, setProxyError] = useState<DesktopError | null>(null);
  const [launchAtLogin, setLaunchAtLogin] = useState<boolean | null>(null);
  const [savingLaunchAtLogin, setSavingLaunchAtLogin] = useState(false);
  const [launchAtLoginError, setLaunchAtLoginError] = useState<DesktopError | null>(null);
  const operationBusy = Boolean(state.current_operation);

  useEffect(() => {
    let cancelled = false;
    void desktopApi.getLaunchAtLogin().then((enabled) => {
      if (!cancelled) setLaunchAtLogin(enabled);
    }).catch((value) => {
      if (!cancelled) setLaunchAtLoginError(normalizeDesktopError(value));
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const saveProxy = async () => {
    if (operationBusy) return;
    setSavingProxy(true);
    setProxyError(null);
    try {
      onState(await desktopApi.updateTunnelProxy(proxyMode, customProxy));
    } catch (value) {
      setProxyError(normalizeDesktopError(value));
    } finally {
      setSavingProxy(false);
    }
  };

  const updateLaunchAtLogin = async (enabled: boolean) => {
    if (savingLaunchAtLogin) return;
    setSavingLaunchAtLogin(true);
    setLaunchAtLoginError(null);
    try {
      setLaunchAtLogin(await desktopApi.setLaunchAtLogin(enabled));
    } catch (value) {
      setLaunchAtLoginError(normalizeDesktopError(value));
    } finally {
      setSavingLaunchAtLogin(false);
    }
  };
  return (
    <section className="page-section" aria-labelledby="settings-title" data-webcodex-page="settings">
      <div className="eyebrow">{t("settings.eyebrow")}</div>
      <h1 id="settings-title">{t("settings.title")}</h1>
      <p className="lede">{t("settings.description")}</p>

      <section className="settings-section" aria-labelledby="settings-interface-title">
        <h2 id="settings-interface-title">{t("settings.interface")}</h2>
        <div className="detail-card setting-row">
          <label htmlFor="desktop-settings-locale">{t("locale.label")}</label>
          <select
            id="desktop-settings-locale"
            value={locale}
            onChange={(event) => setLocale(event.target.value as typeof locale)}
            data-webcodex-control="locale"
          >
            <option value="zh-CN">{t("locale.zh")}</option>
            <option value="en-US">{t("locale.en")}</option>
          </select>
        </div>
      </section>

      <section className="settings-section" aria-labelledby="settings-background-title">
        <h2 id="settings-background-title">{t("settings.backgroundStartup")}</h2>
        <article className="detail-card setting-row">
          <div className="field-group">
            <label htmlFor="desktop-launch-at-login">{t("settings.launchAtLogin")}</label>
            <span className="field-help">{t("settings.launchAtLoginHelp")}</span>
          </div>
          <input
            id="desktop-launch-at-login"
            type="checkbox"
            checked={launchAtLogin ?? false}
            onChange={(event) => void updateLaunchAtLogin(event.target.checked)}
            disabled={launchAtLogin === null || savingLaunchAtLogin}
            data-webcodex-control="launch-at-login"
          />
        </article>
        {launchAtLoginError && <SettingsError error={launchAtLoginError} />}
      </section>

      <section className="settings-section" aria-labelledby="settings-tunnel-title">
        <h2 id="settings-tunnel-title">{t("settings.tunnel")}</h2>
        <TunnelConfigDiagnostics state={state} onState={onState} />
        <article className="detail-card tunnel-proxy-settings">
          <div className="field-group">
            <label htmlFor="desktop-tunnel-proxy-mode">{t("settings.tunnelProxy")}</label>
            <select
              id="desktop-tunnel-proxy-mode"
              value={proxyMode}
              onChange={(event) => setProxyMode(event.target.value as TunnelProxyMode)}
              disabled={savingProxy || operationBusy}
              data-webcodex-control="tunnel-proxy-mode"
            >
              <option value="auto">{t("settings.tunnelProxyAuto")}</option>
              <option value="direct">{t("settings.tunnelProxyDirect")}</option>
              <option value="custom">{t("settings.tunnelProxyCustom")}</option>
            </select>
            {proxyMode === "auto" && <span className="field-help">{t("settings.tunnelProxyAutoHelp")}</span>}
          </div>
          {proxyMode === "custom" && (
            <div className="field-group">
              <label htmlFor="desktop-tunnel-proxy-url">{t("settings.tunnelProxyCustomUrl")}</label>
              <input
                id="desktop-tunnel-proxy-url"
                value={customProxy}
                onChange={(event) => setCustomProxy(event.target.value)}
                placeholder="http://127.0.0.1:7890"
                disabled={savingProxy || operationBusy}
                spellCheck={false}
                data-webcodex-control="tunnel-proxy-url"
              />
              <span className="field-help">{t("settings.tunnelProxyCustomHelp")}</span>
            </div>
          )}
          <dl className="detail-list tunnel-proxy-status">
            <div>
              <dt>{t("settings.tunnelProxyEffective")}</dt>
              <dd>{state.tunnel_proxy.effective_url ?? t("settings.tunnelProxyDirectValue")}</dd>
            </div>
            {state.tunnel_proxy.detected_url && (
              <div>
                <dt>{t("settings.tunnelProxyDetected")}</dt>
                <dd>{state.tunnel_proxy.detected_url}</dd>
              </div>
            )}
          </dl>
          <button
            type="button"
            className="secondary-button tunnel-proxy-save"
            onClick={() => void saveProxy()}
            disabled={savingProxy || operationBusy || (proxyMode === "custom" && !customProxy.trim())}
            data-webcodex-action="save-tunnel-proxy"
          >
            {savingProxy ? t("common.checking") : t("settings.saveTunnelProxy")}
          </button>
          {proxyError && <SettingsError error={proxyError} />}
        </article>
      </section>

      <section className="settings-section" aria-labelledby="settings-diagnostics-title">
        <h2 id="settings-diagnostics-title">{t("settings.diagnostics")}</h2>
        <PowerShellInstallGuidance state={state} onState={onState} />
        <article className="detail-card">
        {state.binaries ? (
          <dl className="detail-list">
            <div><dt>{t("settings.version")}</dt><dd>{state.binaries.version}</dd></div>
            <div><dt>{t("settings.sourceRevision")}</dt><dd>{state.binaries.git_commit}</dd></div>
            <div><dt>{t("settings.binaryDirectory")}</dt><dd>{state.binaries.directory}</dd></div>
            <div><dt>{t("settings.binaryResolution")}</dt><dd>{state.binaries.source}</dd></div>
          </dl>
        ) : (
          <p>{t("settings.binariesPending")}</p>
        )}
        </article>
      </section>

      <section className="settings-section" aria-labelledby="settings-advanced-title">
        <h2 id="settings-advanced-title">{t("settings.advanced")}</h2>
        <article className="detail-card">
          <dl className="detail-list">
            <div><dt>{t("settings.runtimeProjectId")}</dt><dd>{state.project?.runtime_project_id ?? t("settings.notEstablished")}</dd></div>
          </dl>
        </article>
      </section>
    </section>
  );
}

function SettingsError({ error }: { error: DesktopError }) {
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

