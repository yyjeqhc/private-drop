import { useState } from "react";
import { useLocale } from "../../i18n/locale";
import { desktopErrorPresentation, normalizeDesktopError } from "../../i18n/presentation";
import { desktopApi } from "../../lib/desktop-api";
import type { DesktopError, DesktopState } from "../../models/topology";

const WINGET_INSTALL_COMMAND = "winget install --id Microsoft.PowerShell --source winget";

export function PowerShellInstallGuidance({
  state,
  onState,
}: {
  state: DesktopState;
  onState: (state: DesktopState) => void;
}) {
  const { t } = useLocale();
  const [busy, setBusy] = useState<"guide" | "recheck" | null>(null);
  const [error, setError] = useState<DesktopError | null>(null);
  const runtime = state.powershell_runtime;

  const openGuide = async () => {
    if (busy) return;
    setBusy("guide");
    setError(null);
    try {
      await desktopApi.openPowerShellInstallGuide();
    } catch (value) {
      setError(normalizeDesktopError(value));
    } finally {
      setBusy(null);
    }
  };

  const recheck = async () => {
    if (busy) return;
    setBusy("recheck");
    setError(null);
    try {
      onState(await desktopApi.getState());
    } catch (value) {
      setError(normalizeDesktopError(value));
    } finally {
      setBusy(null);
    }
  };

  if (!runtime || runtime.pwsh_available) return null;

  const presentation = error ? desktopErrorPresentation(error, t) : null;

  return (
    <article className="detail-card tunnel-config-diagnostics" role="status" data-webcodex-diagnostic="powershell-7">
      <span className="section-kicker">{t("powershell7.kicker")}</span>
      <strong>{t("powershell7.title")}</strong>
      <p>
        {runtime.windows_powershell_available
          ? t("powershell7.fallbackAvailable")
          : t("powershell7.fallbackUnavailable")}
      </p>
      <p className="tunnel-config-guidance">
        {t("powershell7.installCommand")} <code>{WINGET_INSTALL_COMMAND}</code>
      </p>
      <p className="tunnel-config-guidance">{t("powershell7.restartHint")}</p>
      <div className="tunnel-config-actions">
        <button
          type="button"
          className="secondary-button"
          onClick={() => void openGuide()}
          disabled={busy !== null}
          data-webcodex-action="open-powershell-install-guide"
        >
          {t("powershell7.openGuide")}
        </button>
        <button
          type="button"
          className="secondary-button"
          onClick={() => void recheck()}
          disabled={busy !== null}
          data-webcodex-action="recheck-powershell-7"
        >
          {busy === "recheck" ? t("powershell7.rechecking") : t("powershell7.recheck")}
        </button>
      </div>
      {error && presentation && (
        <div className="error-card" role="alert">
          <strong>{presentation.title}</strong>
          <span>{presentation.action}</span>
          <details>
            <summary>{t("common.details")}</summary>
            <code>{error.code}</code>
            <p>{error.message}</p>
          </details>
        </div>
      )}
    </article>
  );
}
