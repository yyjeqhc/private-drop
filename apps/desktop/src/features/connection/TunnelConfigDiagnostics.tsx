import { useEffect, useId, useState } from "react";
import { useLocale } from "../../i18n/locale";
import { desktopErrorPresentation, normalizeDesktopError } from "../../i18n/presentation";
import { desktopApi } from "../../lib/desktop-api";
import type { DesktopError, DesktopState } from "../../models/topology";

export function TunnelConfigDiagnostics({
  state,
  onState,
}: {
  state: DesktopState;
  onState: (state: DesktopState) => void;
}) {
  const { t } = useLocale();
  const [rechecking, setRechecking] = useState(false);
  const [error, setError] = useState<DesktopError | null>(null);
  const config = state.openai_tunnel_config;
  const inputId = useId();
  const [tunnelId, setTunnelId] = useState(config.saved_tunnel_id ?? "");
  const [apiKey, setApiKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const busy = saving || rechecking || Boolean(state.current_operation);
  useEffect(() => { setTunnelId(config.saved_tunnel_id ?? ""); }, [config.saved_tunnel_id]);
  const save = async (useEnvironment = false) => {
    if (busy) return;
    const request = useEnvironment
      ? { action: "use_environment" as const }
      : { action: "save" as const, tunnelId, apiKey: apiKey || null };
    setApiKey("");
    setSaving(true);
    setSaved(false);
    setError(null);
    try {
      onState(await desktopApi.updateTunnelConfig(request));
      setSaved(true);
    } catch (value) {
      setError(normalizeDesktopError(value));
    } finally { setSaving(false); }
  };
  const configured = config.tunnel_id_present && config.api_key_present;

  const recheck = async () => {
    if (rechecking) return;
    setRechecking(true);
    setError(null);
    try {
      onState(await desktopApi.getState());
    } catch (value) {
      setError(normalizeDesktopError(value));
    } finally {
      setRechecking(false);
    }
  };

  return (
    <article onKeyDown={(event) => { if (event.key === "Enter" && event.target instanceof HTMLInputElement) event.preventDefault(); }} className="detail-card tunnel-config-diagnostics" data-webcodex-component="tunnel-config-diagnostics">
      <span className="section-kicker">{t("tunnelConfig.title")}</span>
      <p className="tunnel-config-guidance">{t("tunnelConfig.fileFirst")}</p>
      <div className="field-group">
        <label htmlFor={`${inputId}-id`}>{t("tunnelConfig.tunnelId")}</label>
        <input id={`${inputId}-id`} value={tunnelId} onChange={(event) => { setTunnelId(event.target.value); setSaved(false); }} placeholder="tunnel_…" maxLength={256} autoComplete="off" spellCheck={false} disabled={busy} />
      </div>
      <div className="field-group">
        <label htmlFor={`${inputId}-key`}>{t("tunnelConfig.apiKey")}</label>
        <input id={`${inputId}-key`} type="password" value={apiKey} onChange={(event) => { setApiKey(event.target.value); setSaved(false); }} placeholder={config.source === "file" ? t("tunnelConfig.keepKey") : t("tunnelConfig.enterKey")} maxLength={8192} autoComplete="new-password" spellCheck={false} disabled={busy} aria-describedby={`${inputId}-key-help`} />
        <span className="field-help" id={`${inputId}-key-help`}>{t("tunnelConfig.storage")}</span>
      </div>
      <div className="tunnel-config-actions">
        <button type="button" className="primary-button" onClick={() => void save()} disabled={busy || !tunnelId.trim() || (config.source !== "file" && !apiKey.trim())}>{saving ? t("common.checking") : t("tunnelConfig.save")}</button>
        {config.source !== "environment" && <button type="button" className="secondary-button" disabled={busy} onClick={() => void save(true)}>{t("tunnelConfig.useEnvironment")}</button>}
      </div>
      {saved && <p className="tunnel-config-guidance" role="status">{t("tunnelConfig.saved")}</p>}
      <p className="tunnel-config-guidance">{t(config.source === "file" ? "tunnelConfig.sourceFile" : config.source === "invalid" ? "tunnelConfig.sourceInvalid" : "tunnelConfig.sourceEnvironment")}</p>
      <dl className="detail-list">
        <div>
          <dt>{t("tunnelConfig.tunnelId")}</dt>
          <dd>{config.tunnel_id_present ? t("tunnelConfig.detected") : t("tunnelConfig.missing")}</dd>
        </div>
        <div>
          <dt>{t("tunnelConfig.apiKey")}</dt>
          <dd>{config.api_key_present ? t("tunnelConfig.detected") : t("tunnelConfig.missing")}</dd>
        </div>
      </dl>
      <p className="tunnel-config-guidance">{t("tunnelConfig.processScope")}</p>
      {!configured && config.source === "environment" && <p className="tunnel-config-guidance">{t("tunnelConfig.restartRequired")}</p>}
      {!configured && config.source === "environment" && isMacOs() && <p className="tunnel-config-guidance">{t("tunnelConfig.macosShell")}</p>}
      {error && <p className="tunnel-config-guidance" role="alert">{desktopErrorPresentation(error, t).action}</p>}
      <div className="tunnel-config-actions">
        <button
          type="button"
          className="secondary-button"
          onClick={() => void recheck()}
          disabled={busy}
          data-webcodex-action="recheck-tunnel-config"
        >
          {rechecking ? t("tunnelConfig.rechecking") : t("tunnelConfig.recheck")}
        </button>
      </div>
    </article>
  );
}

function isMacOs() {
  return typeof navigator !== "undefined" && /Macintosh|Mac OS X/i.test(navigator.userAgent);
}
