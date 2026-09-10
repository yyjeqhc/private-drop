import { useState } from "react";
import type { ActivityEntry } from "../../models/topology";
import { useLocale } from "../../i18n/locale";
import { activityMessage, activitySource } from "../../i18n/presentation";

export function ActivityPanel({ activity }: { activity: ActivityEntry[] }) {
  const { t, formatTime } = useLocale();
  const [query, setQuery] = useState("");
  const [problemsOnly, setProblemsOnly] = useState(false);
  const [showProcesses, setShowProcesses] = useState(false);
  const visible = [...activity].reverse().filter((entry) => {
    if (!showProcesses && entry.level === "info" && entry.event_kind.startsWith("process_")) return false;
    if (problemsOnly && entry.level !== "error" && entry.level !== "warning") return false;
    return `${activityMessage(entry, t)} ${activitySource(entry.source, t)}`
      .toLocaleLowerCase().includes(query.trim().toLocaleLowerCase());
  });
  return (
    <section className="page-section" aria-labelledby="activity-title" data-webcodex-page="activity">
      <div className="eyebrow">{t("activity.eyebrow")}</div>
      <h1 id="activity-title">{t("activity.title")}</h1>
      <p className="lede">{t("activity.description")}</p>
      <div className="activity-toolbar">
        <div className="field-group">
          <label htmlFor="activity-search">{t("activity.search")}</label>
          <input id="activity-search" type="search" value={query} onChange={(event) => setQuery(event.target.value)} />
        </div>
        <label className="activity-filter">
          <input type="checkbox" checked={problemsOnly} onChange={(event) => setProblemsOnly(event.target.checked)} />
          {t("activity.problemsOnly")}
        </label>
        <label className="activity-filter">
          <input type="checkbox" checked={showProcesses} onChange={(event) => setShowProcesses(event.target.checked)} />
          {t("activity.showProcesses")}
        </label>
        <span role="status">{t("activity.count", { count: visible.length })}</span>
      </div>
      <div className="activity-list">
        {activity.length === 0 && <div className="empty-state">{t("activity.empty")}</div>}
        {activity.length > 0 && visible.length === 0 && <div className="empty-state">{t("activity.noMatches")}</div>}
        {visible.map((entry) => (
          <article className="activity-row" key={entry.sequence}>
            <i className={`status-dot ${entry.level === "error" ? "error" : entry.level === "warning" ? "pending" : "unknown"}`} aria-hidden="true" />
            <div>
              <strong>{activityMessage(entry, t)}</strong>
              <span>{activitySource(entry.source, t)} · {formatTime(entry.timestamp_ms)}</span>
            </div>
          </article>
        ))}
      </div>
    </section>
  );
}

