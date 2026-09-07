# Quality Guidelines

## Required working style

- Verify root/branch/HEAD/status before editing and after Git operations or observed concurrent changes.
- Preserve unrelated work. Do not reset, rebase, restore, clean, or rewrite history without explicit authorization.
- Prefer guarded edits based on freshly read source. Follow existing architecture and naming before adding abstractions.
- For cross-layer work, map the authoritative path first and complete the change end to end.

## Testing

- Start with the smallest focused check capable of catching the regression.
- Rust changes normally need formatting, the smallest affected package check when compilation can change, and focused tests.
- Full workspace/all-target/ignored/real-process/E2E suites are not defaults; run them for boundaries that actually need them.
- Use the `dogfood` Cargo profile for optimized development builds; reserve `release` for publication artifacts.
- Async readiness tests use bounded `wait_*` semantics with one absolute deadline; do not stabilize tests by arbitrary sleep inflation.

## Review checklist

- Correctness: does the authoritative state/contract live in one place and all real consumers use it?
- Security: are scopes, ownership, project roots, credential audiences, and destructive targets unchanged or explicitly modeled?
- Recovery: can an uncertain result be re-observed safely without duplicate effects?
- Privacy: are traces/results free of secrets and raw host identifiers?
- Boundedness: are waits, output, queues, retention, and retries bounded?
- Hygiene: final diff/status/conflicts/active Jobs checked before commit or handoff.

## Delivery

Push, PR, deploy, restart, tag, release, or other external publication only when the user explicitly authorizes the named destination/action. Development/dogfood deployment is distinct from a release.
