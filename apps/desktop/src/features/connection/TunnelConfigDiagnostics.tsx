import { useState } from "react";
import { useLocale } from "../../i18n/locale";
import { normalizeDesktopError } from "../../i18n/presentation";
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
    <article className="detail-card tunnel-config-diagnostics" data-webcodex-component="tunnel-config-diagnostics">
      <span className="section-kicker">{t("tunnelConfig.title")}</span>
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
      {!configured && <p className="tunnel-config-guidance">{t("tunnelConfig.restartRequired")}</p>}
      {!configured && isMacOs() && <p className="tunnel-config-guidance">{t("tunnelConfig.macosShell")}</p>}
      {error && <p className="tunnel-config-guidance" role="alert">{error.message}</p>}
      <div className="tunnel-config-actions">
        <button
          type="button"
          className="secondary-button"
          onClick={() => void recheck()}
          disabled={rechecking || Boolean(state.current_operation)}
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
