# Backend Development Guidelines

These notes summarize the repository-specific conventions that are most useful to Trellis tasks. `AGENTS.md` remains the primary repository-wide source of truth; read deeper domain guidance from `docs/agent/` when a task touches those boundaries.

## Pre-development checklist

1. Verify repository root, branch, HEAD, worktree status, and relevant concurrent work.
2. Read `AGENTS.md` and any linked domain guidance for the area being changed.
3. Identify the authoritative layer before editing; do not duplicate a contract in adapters or UI surfaces.
4. Prefer the smallest focused validation that can prove the changed behavior.

## Guidelines index

| Guide | Focus |
|---|---|
| [Directory Structure](./directory-structure.md) | Workspace and ownership boundaries |
| [Database Guidelines](./database-guidelines.md) | SQLite durability and state ownership |
| [Error Handling](./error-handling.md) | Structured failures, authority, and uncertainty |
| [Quality Guidelines](./quality-guidelines.md) | Editing, tests, review, and delivery |
| [Logging Guidelines](./logging-guidelines.md) | `tracing` and diagnostic privacy |
| [Tool Discovery Guidelines](./tool-discovery-guidelines.md) | Protocol/surface-aware discovery and `ModelHidden` extension boundaries |

## Quality check

- The diff stays within the requested scope and preserves unrelated work.
- New cross-layer behavior is wired through all authoritative enums/registries/schemas/projections that actually own it.
- Credentials, raw host/session identifiers, sensitive payloads, and private paths are not exposed by diagnostics or model-facing results.
- Relevant focused tests pass; broad or real-process suites are used only when the changed boundary warrants them.
- Final status/diff/hygiene and active Jobs are reviewed before handoff.
