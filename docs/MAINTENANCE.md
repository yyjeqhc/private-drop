# Repository Maintenance

This page defines durable repository-maintenance policy for maintainers and contributors. It is not a roadmap, changelog, release ledger, or branch-local TODO list.

## Sources of truth

- [`AGENTS.md`](../AGENTS.md) contains the executable repository-work rules for coding agents and maintainers.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) and [`agent/architecture-decisions.md`](agent/architecture-decisions.md) contain durable product structure and design rationale.
- [`TESTING.md`](TESTING.md) defines test lanes and validation principles.
- [`SECURITY.md`](../SECURITY.md) defines security boundaries and how to report sensitive vulnerabilities without exposing private details.
- [`RELEASE_CHECKLIST.md`](RELEASE_CHECKLIST.md) and [`agent/release-process.md`](agent/release-process.md) define release readiness and publication.
- [GitHub Issues](https://github.com/yyjeqhc/webcodex/issues) is the canonical queue for non-sensitive, safe-to-disclose actionable cross-branch maintenance work, technical debt, and follow-ups. Use the existing `priority:*` and `area:*` labels when they improve triage, but keep the issue body as the work contract.

If a non-sensitive actionable item will not be completed in the current change, open or update a GitHub Issue instead of adding a competing debt/backlog Markdown file on a feature branch. Sensitive security findings must follow [`SECURITY.md`](../SECURITY.md) and must not be copied into a public Issue until the details are safe to disclose. Repository docs may explain durable rationale and future direction, but should not mirror Issue state.

## Branches, pull requests, and validation

- Refresh `origin/main` before starting non-trivial work and prefer an isolated branch or worktree for one coherent change.
- Link the pull request to the relevant Issue. Use a closing keyword only when the merged pull request fully satisfies that Issue's acceptance criteria.
- Follow [`AGENTS.md`](../AGENTS.md) for focused validation. Documentation-only changes normally require `python3 scripts/check_markdown_links.py`, not Cargo compilation or broad test suites.
- Follow [`TESTING.md`](TESTING.md) for the current pull-request and `main` CI mapping. Keep workflow trigger details there instead of duplicating them in maintenance policy.

## Dependency maintenance

Review direct Rust and npm dependency freshness and relevant security advisories at least monthly or before release preparation, whichever comes first. Security-relevant updates with meaningful impact should be handled promptly rather than waiting for that cadence.

Record safe-to-disclose actionable dependency work as GitHub Issues and keep dependency-only changes narrow where practical. Sensitive security findings follow [`SECURITY.md`](../SECURITY.md) and stay out of public Issues until disclosure is appropriate. Do not mix unrelated upgrades into feature work, and do not treat dependency review as release authorization; release publication remains governed by the release checklist.

## Documentation retention

Keep documentation that helps a user operate the supported product or helps a
contributor maintain it correctly. Judge content by its continuing purpose and
accuracy, not its age or whether it was written for a coding agent.

| Content | Treatment |
| --- | --- |
| Setup, usage, configuration, API contracts, troubleshooting, security, contribution, testing, and release procedures | Keep accurate, discoverable instructions for supported behavior. |
| Architecture decisions, constraints, alternatives, and reasons for rejected approaches | Keep the rationale needed to avoid repeating mistakes; link to the authoritative contract instead of duplicating it. |
| Known limitations and future direction | Distinguish implemented behavior from proposals; track actionable work in Issues without mirroring their status in docs. |
| Compatibility and migration notes | Keep while supported versions, persisted data, or upgrade/recovery paths still depend on them. Age alone is not grounds for removal. |
| Development diaries, completed phase plans, self-review reports, and one-off validation transcripts | Remove redundant progress/status narration after preserving useful technical content. Distinguish these records from instructional walkthroughs and case studies. PRs, CI artifacts, and Git history hold change-specific review evidence. |
| Release evidence and security incident records | Preserve required provenance, immutable artifacts, and disclosure records under the release and security policies; do not treat them as disposable development logs. |
| Hands-on tutorials, case studies, and screenshots | Keep useful steps, intermediate observations, failures, recovery paths, and illustrations, not just conclusions. Label the tested version/date and distinguish case-specific values from setup requirements. Improve organization and captions; an older UI or a single-machine origin alone does not make the material disposable. |

When cleaning a page, check suspected stale claims against the relevant code or
tests. Do not turn an old successful experiment into a claim of current support.
If support cannot be established, retain a clearly scoped limitation or proposal.
Prefer one canonical explanation with links; avoid creating an archive directory
that merely relocates redundant logs. Update existing translations, navigation,
and local links when moving or removing content.

## Bilingual documentation

For user-facing documentation that already has a tracked `*.zh-CN.md` peer, the English file is the canonical source for normative wording.

- Changes to commands, configuration, installation, deployment, security, or user-visible runtime contracts should update the English and Chinese peers in the same pull request.
- Purely editorial prose may defer translation only when the pull request records the deferral and links a durable GitHub Issue before merge; do not leave silent contract drift.
- Internal or agent-facing documents without an existing translated peer do not need a new translation solely for symmetry.

## Lightweight maintenance sweep

Once a month or during release preparation, do a short maintenance pass over open safe-to-disclose maintenance/security Issues, direct dependency/security status, sensitive findings handled under [`SECURITY.md`](../SECURITY.md), and evergreen docs for obsolete phase-specific acceptance text. Record only safe-to-disclose actionable findings as Issues; do not create a second roadmap or maintenance ledger.
