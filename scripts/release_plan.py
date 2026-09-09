#!/usr/bin/env python3
"""Crash-safe high-level orchestration for reversible WebCodex release steps.

The plan deliberately stops at irreversible human-authorization boundaries. It
never creates/pushes a tag, creates/publishes a GitHub Release, publishes npm, or
deploys services. Those actions remain explicit operator decisions; `resume`
reconciles their externally visible result before advancing.
"""

from __future__ import annotations

import json
import os
import stat
import tempfile
import time
from pathlib import Path

if __package__:
    from . import collect_release_bundle as collector
    from . import release_publication as publication
    from . import release_readiness as readiness
else:
    import sys

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import collect_release_bundle as collector
    import release_publication as publication
    import release_readiness as readiness


STATE_SCHEMA_VERSION = 1
MAX_STATE_BYTES = 64 * 1024
KIND = "release-plan"

PHASE_PREFLIGHT = "preflight_passed"
PHASE_READINESS = "readiness_running"
PHASE_AWAIT_TAG = "awaiting_tag_authorization"
PHASE_BUILD = "build_running"
PHASE_BUILD_PASSED = "build_passed"
PHASE_BUNDLE = "bundle_collected"
PHASE_NPM_STAGED = "npm_staged"
PHASE_AWAIT_DRAFT = "awaiting_draft_authorization"
PHASE_DRAFT_VERIFIED = "draft_verified"
PHASE_AWAIT_PUBLICATION = "awaiting_publication_authorization"

PHASES = {
    PHASE_PREFLIGHT,
    PHASE_READINESS,
    PHASE_AWAIT_TAG,
    PHASE_BUILD,
    PHASE_BUILD_PASSED,
    PHASE_BUNDLE,
    PHASE_NPM_STAGED,
    PHASE_AWAIT_DRAFT,
    PHASE_DRAFT_VERIFIED,
    PHASE_AWAIT_PUBLICATION,
}


class ReleasePlanError(RuntimeError):
    pass


def _validate_new_state_path(path: Path) -> Path:
    target = path.absolute()
    if target.exists() or target.is_symlink():
        raise ReleasePlanError(f"release plan state already exists: {target}")
    target.parent.mkdir(parents=True, exist_ok=True)
    if target.parent.is_symlink():
        raise ReleasePlanError("release plan state parent may not be a symlink")
    return target


def _write_state(path: Path, state: dict) -> None:
    raw = (json.dumps(state, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(raw) > MAX_STATE_BYTES:
        raise ReleasePlanError("release plan state exceeds its size bound")
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temp = Path(temp_name)
    try:
        if hasattr(os, "fchmod"):
            os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(raw)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp, path)
        os.chmod(path, 0o600)
    finally:
        try:
            temp.unlink(missing_ok=True)
        except OSError:
            pass


def _load_state(path: Path) -> dict:
    target = path.absolute()
    if target.is_symlink():
        raise ReleasePlanError("release plan state may not be a symlink")
    try:
        info = target.stat()
    except OSError as exc:
        raise ReleasePlanError(f"could not stat release plan state: {target}") from exc
    if not stat.S_ISREG(info.st_mode):
        raise ReleasePlanError("release plan state must be a regular file")
    if info.st_size <= 0 or info.st_size > MAX_STATE_BYTES:
        raise ReleasePlanError("release plan state is outside its size bound")
    try:
        value = json.loads(target.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ReleasePlanError("release plan state is invalid JSON") from exc
    if not isinstance(value, dict):
        raise ReleasePlanError("release plan state must be an object")
    required = {
        "schema_version",
        "kind",
        "repo",
        "version",
        "tag",
        "source_sha",
        "root",
        "work_dir",
        "readiness_state_file",
        "build_state_file",
        "bundle_dir",
        "stage_dir",
        "phase",
        "created_at",
        "updated_at",
        "readiness_run_id",
        "build_run_id",
        "last_action",
    }
    if set(value) != required:
        raise ReleasePlanError("release plan state fields do not match the supported schema")
    if value.get("schema_version") != STATE_SCHEMA_VERSION or value.get("kind") != KIND:
        raise ReleasePlanError("unsupported release plan state schema")
    source = collector.normalize_source_sha(str(value.get("source_sha", "")))
    version = publication.normalize_version(str(value.get("version", "")))
    tag = collector.validate_expected_tag(str(value.get("tag", "")))
    if tag != f"v{version}":
        raise ReleasePlanError("release plan tag/version mismatch")
    if value.get("phase") not in PHASES:
        raise ReleasePlanError("release plan phase is invalid")
    if not isinstance(value.get("repo"), str) or not value["repo"]:
        raise ReleasePlanError("release plan repository is invalid")
    for field in ("root", "work_dir", "readiness_state_file", "build_state_file", "bundle_dir", "stage_dir"):
        field_value = value.get(field)
        if not isinstance(field_value, str) or not Path(field_value).is_absolute():
            raise ReleasePlanError(f"release plan {field} must be an absolute path")
    for field in ("created_at", "updated_at"):
        if not isinstance(value.get(field), int) or value[field] <= 0:
            raise ReleasePlanError(f"release plan {field} is invalid")
    for field in ("readiness_run_id", "build_run_id"):
        field_value = value.get(field)
        if field_value is not None and (not isinstance(field_value, int) or field_value <= 0):
            raise ReleasePlanError(f"release plan {field} is invalid")
    if not isinstance(value.get("last_action"), str):
        raise ReleasePlanError("release plan last_action is invalid")
    value["source_sha"] = source
    value["version"] = version
    return value


def _update(path: Path, state: dict, *, phase: str | None = None, action: str) -> None:
    if phase is not None:
        state["phase"] = phase
    state["last_action"] = action
    state["updated_at"] = int(time.time())
    _write_state(path, state)


def _summary(
    state: dict,
    *,
    status: str,
    state_file: Path | None = None,
    next_action: str | None = None,
) -> dict:
    result = {
        "status": status,
        "phase": state["phase"],
        "version": state["version"],
        "tag": state["tag"],
        "source_sha": state["source_sha"],
        "state_file": str(state_file) if state_file is not None else None,
        "readiness_run_id": state.get("readiness_run_id"),
        "build_run_id": state.get("build_run_id"),
        "bundle_dir": state["bundle_dir"],
        "stage_dir": str(Path(state["stage_dir"]) / "npm-package"),
        "last_action": state["last_action"],
    }
    if next_action is not None:
        result["next_action"] = next_action
    return result


def init_plan(
    *,
    repo: str,
    version: str,
    source_sha: str,
    root: Path,
    state_file: Path,
    work_dir: Path,
    timeout: float,
) -> dict:
    state_path = _validate_new_state_path(state_file)
    source = collector.normalize_source_sha(source_sha)
    release_version = publication.normalize_version(version)
    source_root = root.absolute()
    workspace = work_dir.absolute()
    if workspace.exists() and not workspace.is_dir():
        raise ReleasePlanError(f"release plan work path is not a directory: {workspace}")
    if workspace.exists() and any(workspace.iterdir()):
        raise ReleasePlanError(f"release plan work directory must be empty: {workspace}")
    workspace.mkdir(parents=True, exist_ok=True)
    preflight = publication.preflight_release(
        repo=repo,
        version=release_version,
        source_sha=source,
        root=source_root,
        timeout=timeout,
    )
    now = int(time.time())
    state = {
        "schema_version": STATE_SCHEMA_VERSION,
        "kind": KIND,
        "repo": repo,
        "version": release_version,
        "tag": f"v{release_version}",
        "source_sha": source,
        "root": str(source_root),
        "work_dir": str(workspace),
        "readiness_state_file": str(workspace / "readiness.json"),
        "build_state_file": str(workspace / "build.json"),
        "bundle_dir": str(workspace / "bundle"),
        "stage_dir": str(workspace / "npm-stage"),
        "phase": PHASE_PREFLIGHT,
        "created_at": now,
        "updated_at": now,
        "readiness_run_id": None,
        "build_run_id": None,
        "last_action": "preflight_passed",
    }
    _write_state(state_path, state)
    result = _summary(state, status="ready", state_file=state_path)
    result["preflight"] = preflight
    return result


def _tag_source(state: dict) -> str | None:
    identity = publication._remote_annotated_tag_identity(Path(state["root"]), state["tag"])
    if identity is None:
        return None
    _tag_object, source = identity
    return source


def _existing_bundle_is_valid(state: dict) -> bool:
    bundle = Path(state["bundle_dir"])
    if not bundle.is_dir():
        return False
    summary = publication.verify_bundle(bundle, state["repo"])
    return summary.get("source_sha") == state["source_sha"] and summary.get("tag") == state["tag"]


def resume_plan(*, state_file: Path, timeout: float, wait_secs: int) -> tuple[dict, int]:
    if wait_secs < 0 or wait_secs > 7200:
        raise ReleasePlanError("wait_secs must be within 0..7200")
    state_path = state_file.absolute()
    state = _load_state(state_path)

    # Advance through locally safe/recoverable phases until external work is
    # still running or an explicit human authorization boundary is reached.
    while True:
        phase = state["phase"]
        if phase == PHASE_PREFLIGHT:
            readiness_state = Path(state["readiness_state_file"])
            if readiness_state.exists() or readiness_state.is_symlink():
                # The low-level operator writes its durable state before POSTing.
                # An uncertain dispatch can therefore outlive the high-level
                # phase transition; recover that exact state instead of issuing
                # a second dispatch.
                _update(
                    state_path,
                    state,
                    phase=PHASE_READINESS,
                    action="readiness_state_reconciled",
                )
                continue
            summary, _ = readiness.start_readiness(
                repo=state["repo"],
                source_sha=state["source_sha"],
                state_file=readiness_state,
                timeout=timeout,
                resolve_secs=min(wait_secs, 120),
            )
            state["readiness_run_id"] = summary.get("run_id")
            _update(state_path, state, phase=PHASE_READINESS, action="readiness_dispatched")
            continue

        if phase == PHASE_READINESS:
            summary, exit_code = readiness.status_readiness(
                state_file=Path(state["readiness_state_file"]),
                timeout=timeout,
                wait_secs=wait_secs,
            )
            state["readiness_run_id"] = summary.get("run_id")
            if exit_code == 2:
                _update(state_path, state, action="readiness_waiting")
                return _summary(state, status="waiting", state_file=state_path, next_action="resume the same release plan"), 2
            if exit_code != 0:
                _update(state_path, state, action="readiness_failed")
                return _summary(state, status="failed", state_file=state_path, next_action="diagnose the bound readiness run; do not redispatch"), 1
            _update(state_path, state, phase=PHASE_AWAIT_TAG, action="readiness_passed")
            continue

        if phase == PHASE_AWAIT_TAG:
            source = _tag_source(state)
            if source is None:
                _update(state_path, state, action="awaiting_tag_authorization")
                return _summary(
                    state,
                    status="needs_authorization",
                    state_file=state_path,
                    next_action=f"after explicit approval, create and push annotated {state['tag']} at {state['source_sha']}; then resume",
                ), 3
            if source != state["source_sha"]:
                raise ReleasePlanError(
                    f"remote tag source mismatch: expected={state['source_sha']} actual={source}"
                )
            _update(state_path, state, action="tag_reconciled")
            build_state = Path(state["build_state_file"])
            if build_state.exists() or build_state.is_symlink():
                # Same recovery rule as readiness: a durable nested build state
                # proves which request must be observed after an uncertain POST.
                _update(
                    state_path,
                    state,
                    phase=PHASE_BUILD,
                    action="build_state_reconciled",
                )
                continue
            summary, _ = publication.start_build(
                repo=state["repo"],
                source_sha=state["source_sha"],
                tag=state["tag"],
                state_file=build_state,
                timeout=timeout,
                resolve_secs=min(wait_secs, 120),
            )
            state["build_run_id"] = summary.get("run_id")
            _update(state_path, state, phase=PHASE_BUILD, action="build_dispatched")
            continue

        if phase == PHASE_BUILD:
            summary, exit_code = publication.status_build(
                state_file=Path(state["build_state_file"]),
                timeout=timeout,
                wait_secs=wait_secs,
            )
            state["build_run_id"] = summary.get("run_id")
            if exit_code == 2:
                _update(state_path, state, action="build_waiting")
                return _summary(state, status="waiting", state_file=state_path, next_action="resume the same release plan"), 2
            if exit_code != 0:
                _update(state_path, state, action="build_failed")
                return _summary(state, status="failed", state_file=state_path, next_action="diagnose the bound release-build run; do not redispatch"), 1
            if state["build_run_id"] is None:
                raise ReleasePlanError("successful release-build state is missing its run id")
            _update(state_path, state, phase=PHASE_BUILD_PASSED, action="build_passed")
            continue

        if phase == PHASE_BUILD_PASSED:
            bundle = Path(state["bundle_dir"])
            if bundle.exists() or bundle.is_symlink():
                if not _existing_bundle_is_valid(state):
                    raise ReleasePlanError("existing bundle directory cannot be reconciled to this release plan")
            else:
                collector.collect_bundle(
                    repo=state["repo"],
                    run_id=state["build_run_id"],
                    expected_source_sha=state["source_sha"],
                    expected_tag=state["tag"],
                    output_dir=bundle,
                    timeout=max(timeout, 120.0),
                )
            _update(state_path, state, phase=PHASE_BUNDLE, action="bundle_collected")
            continue

        if phase == PHASE_BUNDLE:
            stage = Path(state["stage_dir"])
            if stage.exists() or stage.is_symlink():
                _update(state_path, state, action="stage_requires_reconciliation")
                return _summary(
                    state,
                    status="needs_reconciliation",
                    state_file=state_path,
                    next_action="the npm stage path already exists but success was not recorded; inspect it before deleting or retrying",
                ), 4
            publication.stage_npm(
                repo=state["repo"],
                bundle_dir=Path(state["bundle_dir"]),
                source_root=Path(state["root"]),
                output_dir=stage,
            )
            _update(state_path, state, phase=PHASE_NPM_STAGED, action="npm_staged")
            continue

        if phase == PHASE_NPM_STAGED:
            _update(state_path, state, phase=PHASE_AWAIT_DRAFT, action="awaiting_draft_authorization")
            continue

        if phase == PHASE_AWAIT_DRAFT:
            try:
                publication.verify_draft_assets(
                    repo=state["repo"],
                    bundle_dir=Path(state["bundle_dir"]),
                    timeout=timeout,
                )
            except publication.PublicationError as exc:
                if "draft Release was not found" in str(exc):
                    _update(state_path, state, action="awaiting_draft_authorization")
                    return _summary(
                        state,
                        status="needs_authorization",
                        state_file=state_path,
                        next_action="after explicit approval, create the draft GitHub Release from the retained bundle; then resume",
                    ), 3
                raise
            _update(state_path, state, phase=PHASE_DRAFT_VERIFIED, action="draft_verified")
            continue

        if phase == PHASE_DRAFT_VERIFIED:
            _update(state_path, state, phase=PHASE_AWAIT_PUBLICATION, action="awaiting_publication_authorization")
            continue

        if phase == PHASE_AWAIT_PUBLICATION:
            return _summary(
                state,
                status="needs_authorization",
                state_file=state_path,
                next_action="publish the verified GitHub draft and staged npm package only after explicit approval; public verification remains a separate final gate",
            ), 3

        raise AssertionError(f"unhandled release plan phase: {phase}")


def _status_projection(state: dict) -> tuple[str, str | None]:
    last_action = state["last_action"]
    phase = state["phase"]
    if last_action in {"readiness_failed", "build_failed"}:
        target = "readiness" if last_action == "readiness_failed" else "release-build"
        return "failed", f"diagnose the bound {target} run; do not redispatch"
    if last_action == "stage_requires_reconciliation":
        return (
            "needs_reconciliation",
            "the npm stage path exists but success was not durably recorded; inspect it before deleting or retrying",
        )
    if phase == PHASE_AWAIT_TAG:
        return (
            "needs_authorization",
            f"after explicit approval, create and push annotated {state['tag']} at {state['source_sha']}; then resume",
        )
    if phase == PHASE_AWAIT_DRAFT:
        return (
            "needs_authorization",
            "after explicit approval, create the draft GitHub Release from the retained bundle; then resume",
        )
    if phase == PHASE_AWAIT_PUBLICATION:
        return (
            "needs_authorization",
            "publish the verified GitHub draft and staged npm package only after explicit approval; public verification remains a separate final gate",
        )
    if phase in {PHASE_READINESS, PHASE_BUILD}:
        return "waiting", "resume the same release plan"
    return "ready", None


def status_plan(*, state_file: Path) -> dict:
    state_path = state_file.absolute()
    state = _load_state(state_path)
    status, next_action = _status_projection(state)
    return _summary(state, status=status, state_file=state_path, next_action=next_action)
