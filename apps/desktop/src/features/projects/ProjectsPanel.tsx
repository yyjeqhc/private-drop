import type { DesktopState } from "../../models/topology";
import { useLocale } from "../../i18n/locale";
import { projectReadinessLabel } from "../../i18n/presentation";

export function ProjectsPanel({ state, onChooseProject }: { state: DesktopState; onChooseProject: () => void }) {
  const { t } = useLocale();
  return (
    <section className="page-section" aria-labelledby="projects-title" data-webcodex-page="projects">
      <div className="eyebrow">{t("project.eyebrow")}</div>
      <h1 id="projects-title">{t("project.title")}</h1>
      <p className="lede">{t("project.description")}</p>
      {state.project ? (
        <article className="detail-card" aria-labelledby="configured-project-title">
          <h2 id="configured-project-title" className="section-title">{t("project.configured")}</h2>
          <strong>{state.project.path}</strong>
          <dl className="detail-list">
            <div><dt>{t("project.status")}</dt><dd>{projectReadinessLabel(state.readiness.project, t)}</dd></div>
            <div><dt>{t("project.allowedRoot")}</dt><dd>{state.project.allowed_root}</dd></div>
            <div><dt>{t("project.git")}</dt><dd>{state.project.is_git_repository ? t("project.gitDetected") : t("project.gitNotRequired")}</dd></div>
          </dl>
          <button className="secondary-button" onClick={onChooseProject} disabled={Boolean(state.current_operation)} data-webcodex-action="change-project">
            {t("project.change")}
          </button>
        </article>
      ) : (
        <div className="empty-state">
          <span>{t("project.none")}</span>
          <button className="primary-button" onClick={onChooseProject} disabled={Boolean(state.current_operation)} data-webcodex-action="add-project">
            {t("project.add")}
          </button>
        </div>
      )}
    </section>
  );
}
