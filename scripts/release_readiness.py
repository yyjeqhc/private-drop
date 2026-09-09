#!/usr/bin/env python3
"""Dispatch and observe one exact-source WebCodex release-readiness workflow run."""

from __future__ import annotations

import json
import os
import re
import secrets
import tempfile
import time
import urllib.error
from pathlib import Path

if __package__:
    from . import collect_release_bundle as collector
else:
    import sys

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import collect_release_bundle as collector

READINESS_WORKFLOW_FILE = "release-readiness.yml"
READINESS_WORKFLOW_PATH = f".github/workflows/{READINESS_WORKFLOW_FILE}"
CI_WORKFLOW_FILE = "ci.yml"
CI_WORKFLOW_PATH = f".github/workflows/{CI_WORKFLOW_FILE}"
LEGACY_STATE_SCHEMA_VERSION = 1
STATE_SCHEMA_VERSION = 2
REQUEST_ID_RE = re.compile(r"^rr_[0-9a-f]{24}$")
MAX_STATE_BYTES = 64 * 1024
MAX_RUN_LIST = 100


class ReadinessError(RuntimeError):
    pass


class DispatchRejected(ReadinessError):
    pass


class DispatchOutcomeUnknown(ReadinessError):
    pass


def _run_name(request_id: str, source_sha: str) -> str:
    return f"Release readiness {request_id} {source_sha}"


def _validate_request_id(value: str) -> str:
    request_id = value.strip()
    if not REQUEST_ID_RE.fullmatch(request_id):
        raise ReadinessError("invalid release-readiness request id")
    return request_id


def _validate_state_path_for_create(path: Path) -> Path:
    result = path.absolute()
    if result.exists() or result.is_symlink():
        raise ReadinessError(f"readiness state file already exists: {result}")
    result.parent.mkdir(parents=True, exist_ok=True)
    return result


def _write_state(path: Path, state: dict) -> None:
    parent = path.parent
    parent.mkdir(parents=True, exist_ok=True)
    payload = (json.dumps(state, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(payload) > MAX_STATE_BYTES:
        raise ReadinessError("readiness state exceeds its size bound")
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=parent)
    temp_path = Path(temp_name)
    try:
        if hasattr(os, "fchmod"):
            os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        if path.is_symlink():
            raise ReadinessError(f"refusing to replace readiness state symlink: {path}")
        os.replace(temp_path, path)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise


def _load_state(path: Path) -> dict:
    state_path = path.absolute()
    if state_path.is_symlink() or not state_path.is_file():
        raise ReadinessError(f"readiness state file is missing or unsafe: {state_path}")
    try:
        size = state_path.stat().st_size
        if size <= 0 or size > MAX_STATE_BYTES:
            raise ReadinessError("readiness state file is outside its size bound")
        value = json.loads(state_path.read_text(encoding="utf-8"))
    except ReadinessError:
        raise
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ReadinessError("could not read readiness state file") from exc
    if not isinstance(value, dict):
        raise ReadinessError("readiness state must be a JSON object")
    legacy_required = {
        "schema_version",
        "kind",
        "repo",
        "source_sha",
        "workflow_file",
        "workflow_path",
        "request_id",
        "run_name",
        "dispatch_state",
        "created_at",
        "run_id",
        "run_head_sha",
        "source_matches",
        "run_url",
        "run_status",
        "run_conclusion",
        "last_observed_at",
    }
    ci_proof_fields = {
        "ci_run_id",
        "ci_run_attempt",
        "ci_run_url",
        "ci_run_head_sha",
        "ci_run_conclusion",
    }
    schema_version = value.get("schema_version")
    if schema_version == LEGACY_STATE_SCHEMA_VERSION:
        required = legacy_required
    elif schema_version == STATE_SCHEMA_VERSION:
        required = legacy_required | ci_proof_fields
    else:
        raise ReadinessError("unsupported readiness state schema")
    if set(value) != required:
        raise ReadinessError("readiness state fields do not match the supported schema")
    if value.get("kind") != "release-readiness":
        raise ReadinessError("unsupported readiness state kind")
    source_sha = collector.normalize_source_sha(str(value.get("source_sha", "")))
    request_id = _validate_request_id(str(value.get("request_id", "")))
    if value.get("workflow_file") != READINESS_WORKFLOW_FILE or value.get("workflow_path") != READINESS_WORKFLOW_PATH:
        raise ReadinessError("readiness state references an unexpected workflow")
    if value.get("run_name") != _run_name(request_id, source_sha):
        raise ReadinessError("readiness state run name is inconsistent")
    if not isinstance(value.get("repo"), str):
        raise ReadinessError("readiness state repository is invalid")
    if not isinstance(value.get("created_at"), int) or value["created_at"] <= 0:
        raise ReadinessError("readiness state created_at is invalid")
    if schema_version == STATE_SCHEMA_VERSION:
        ci_run_id = value.get("ci_run_id")
        ci_run_attempt = value.get("ci_run_attempt")
        ci_run_url = value.get("ci_run_url")
        ci_run_head_sha = value.get("ci_run_head_sha")
        ci_run_conclusion = value.get("ci_run_conclusion")
        if not isinstance(ci_run_id, int) or ci_run_id <= 0:
            raise ReadinessError("readiness state CI run id is invalid")
        if not isinstance(ci_run_attempt, int) or ci_run_attempt <= 0:
            raise ReadinessError("readiness state CI run attempt is invalid")
        if not isinstance(ci_run_url, str) or not ci_run_url.startswith("https://github.com/"):
            raise ReadinessError("readiness state CI run URL is invalid")
        try:
            normalized_ci_source = collector.normalize_source_sha(str(ci_run_head_sha))
        except collector.CollectionError as exc:
            raise ReadinessError("readiness state CI run head SHA is invalid") from exc
        if normalized_ci_source != source_sha:
            raise ReadinessError("readiness state CI proof does not match release source")
        if ci_run_conclusion != "success":
            raise ReadinessError("readiness state CI proof is not successful")
    run_id = value.get("run_id")
    if run_id is not None and (not isinstance(run_id, int) or run_id <= 0):
        raise ReadinessError("readiness state run id is invalid")
    run_head_sha = value.get("run_head_sha")
    if run_head_sha is not None:
        try:
            collector.normalize_source_sha(str(run_head_sha))
        except collector.CollectionError as exc:
            raise ReadinessError("readiness state run head SHA is invalid") from exc
    source_matches = value.get("source_matches")
    if source_matches is not None and not isinstance(source_matches, bool):
        raise ReadinessError("readiness state source_matches is invalid")
    run_url = value.get("run_url")
    if run_url is not None and not isinstance(run_url, str):
        raise ReadinessError("readiness state run URL is invalid")
    run_status = value.get("run_status")
    if run_status is not None and not isinstance(run_status, str):
        raise ReadinessError("readiness state run status is invalid")
    run_conclusion = value.get("run_conclusion")
    if run_conclusion is not None and not isinstance(run_conclusion, str):
        raise ReadinessError("readiness state run conclusion is invalid")
    last_observed_at = value.get("last_observed_at")
    if last_observed_at is not None and (not isinstance(last_observed_at, int) or last_observed_at <= 0):
        raise ReadinessError("readiness state last_observed_at is invalid")
    return value


def _main_sha(client: collector.GitHubClient) -> str:
    payload = client.fetch_json("/branches/main")
    commit = payload.get("commit")
    if not isinstance(commit, dict):
        raise ReadinessError("GitHub main branch response is malformed")
    try:
        return collector.normalize_source_sha(str(commit.get("sha", "")))
    except collector.CollectionError as exc:
        raise ReadinessError("GitHub main branch SHA is invalid") from exc


def select_successful_main_ci_run(payload: dict, source_sha: str) -> dict:
    source = collector.normalize_source_sha(source_sha)
    runs = payload.get("workflow_runs")
    if not isinstance(runs, list):
        raise ReadinessError("GitHub main CI run listing is malformed")
    if len(runs) > MAX_RUN_LIST:
        raise ReadinessError("GitHub main CI run listing exceeds its bound")
    matches = []
    for run in runs:
        if not isinstance(run, dict):
            continue
        if (
            run.get("path") != CI_WORKFLOW_PATH
            or run.get("event") != "push"
            or run.get("head_branch") != "main"
            or run.get("head_sha") != source
        ):
            continue
        matches.append(run)
    if len(matches) != 1:
        raise ReadinessError(f"expected exactly one exact-source main CI run, found {len(matches)}")
    run = matches[0]
    run_id = run.get("id")
    run_attempt = run.get("run_attempt")
    run_url = run.get("html_url")
    if not isinstance(run_id, int) or run_id <= 0:
        raise ReadinessError("GitHub main CI run id is invalid")
    if not isinstance(run_attempt, int) or run_attempt <= 0:
        raise ReadinessError("GitHub main CI run attempt is invalid")
    if not isinstance(run_url, str) or not run_url.startswith("https://github.com/"):
        raise ReadinessError("GitHub main CI run URL is invalid")
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        raise ReadinessError("exact-source main CI has not completed successfully")
    return run


def _successful_main_ci_run(client: collector.GitHubClient, source_sha: str) -> dict:
    source = collector.normalize_source_sha(source_sha)
    payload = client.fetch_json(
        f"/actions/workflows/{CI_WORKFLOW_FILE}/runs?event=push&branch=main&head_sha={source}&per_page={MAX_RUN_LIST}"
    )
    return select_successful_main_ci_run(payload, source)


def _post_dispatch(
    client: collector.GitHubClient,
    source_sha: str,
    request_id: str,
    ci_run_id: int,
    ci_run_attempt: int,
) -> None:
    url = client.api_url(f"/actions/workflows/{READINESS_WORKFLOW_FILE}/dispatches")
    data = json.dumps(
        {
            "ref": "main",
            "inputs": {
                "source_sha": source_sha,
                "request_id": request_id,
                "ci_run_id": str(ci_run_id),
                "ci_run_attempt": str(ci_run_attempt),
            },
        },
        separators=(",", ":"),
    ).encode("utf-8")
    request = client._request(url)
    request.data = data
    request.method = "POST"
    request.add_header("Content-Type", "application/json")
    try:
        with client.opener.open(request, timeout=client.timeout) as response:
            status = getattr(response, "status", response.getcode())
            body = response.read(1)
    except urllib.error.HTTPError as exc:
        if 400 <= exc.code < 500:
            raise DispatchRejected(f"GitHub rejected release-readiness dispatch with HTTP {exc.code}") from exc
        raise DispatchOutcomeUnknown(
            f"GitHub release-readiness dispatch outcome is unknown after HTTP {exc.code}; do not redispatch"
        ) from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise DispatchOutcomeUnknown(
            "GitHub release-readiness dispatch outcome is unknown after transport failure; do not redispatch"
        ) from exc
    if status != 204 or body:
        raise DispatchOutcomeUnknown(
            f"GitHub release-readiness dispatch returned unexpected HTTP {status}; do not redispatch"
        )


def _run_identity_matches(run: dict, state: dict) -> bool:
    return (
        run.get("path") == READINESS_WORKFLOW_PATH
        and run.get("event") == "workflow_dispatch"
        and run.get("head_branch") == "main"
        and run.get("display_title") == state["run_name"]
    )


def select_readiness_run(payload: dict, state: dict) -> dict | None:
    runs = payload.get("workflow_runs")
    if not isinstance(runs, list):
        raise ReadinessError("GitHub readiness run listing is malformed")
    if len(runs) > MAX_RUN_LIST:
        raise ReadinessError("GitHub readiness run listing exceeds its bound")
    matches = [run for run in runs if isinstance(run, dict) and _run_identity_matches(run, state)]
    if len(matches) > 1:
        raise ReadinessError("multiple GitHub readiness runs match one request id")
    if not matches:
        return None
    run = matches[0]
    run_id = run.get("id")
    if not isinstance(run_id, int) or run_id <= 0:
        raise ReadinessError("GitHub readiness run id is invalid")
    if not isinstance(run.get("html_url"), str):
        raise ReadinessError("GitHub readiness run URL is invalid")
    return run


def _list_runs(client: collector.GitHubClient) -> dict:
    return client.fetch_json(
        f"/actions/workflows/{READINESS_WORKFLOW_FILE}/runs?event=workflow_dispatch&branch=main&per_page={MAX_RUN_LIST}"
    )


def _apply_run_snapshot(state: dict, run: dict) -> None:
    if not _run_identity_matches(run, state):
        raise ReadinessError("GitHub readiness run no longer matches durable request identity")
    run_id = run.get("id")
    if not isinstance(run_id, int) or run_id <= 0:
        raise ReadinessError("GitHub readiness run id is invalid")
    bound_run_id = state.get("run_id")
    if bound_run_id is not None and bound_run_id != run_id:
        raise ReadinessError("GitHub readiness run id changed after durable binding")
    try:
        run_head_sha = collector.normalize_source_sha(str(run.get("head_sha", "")))
    except collector.CollectionError as exc:
        raise ReadinessError("GitHub readiness run head SHA is invalid") from exc
    status = run.get("status")
    conclusion = run.get("conclusion")
    if not isinstance(status, str):
        raise ReadinessError("GitHub readiness run status is invalid")
    if conclusion is not None and not isinstance(conclusion, str):
        raise ReadinessError("GitHub readiness run conclusion is invalid")
    run_url = run.get("html_url")
    if not isinstance(run_url, str) or not run_url.startswith("https://github.com/"):
        raise ReadinessError("GitHub readiness run URL is invalid")
    state["run_id"] = run_id
    state["run_head_sha"] = run_head_sha
    state["source_matches"] = run_head_sha == state["source_sha"]
    state["run_url"] = run_url
    state["run_status"] = status
    state["run_conclusion"] = conclusion
    state["last_observed_at"] = int(time.time())
    state["dispatch_state"] = "completed" if status == "completed" else "resolved"


def _recover_run(client: collector.GitHubClient, state: dict) -> dict | None:
    run = select_readiness_run(_list_runs(client), state)
    if run is not None:
        _apply_run_snapshot(state, run)
    return run


def _get_bound_run(client: collector.GitHubClient, state: dict) -> dict | None:
    run_id = state.get("run_id")
    if run_id is None:
        return _recover_run(client, state)
    run = client.fetch_json(f"/actions/runs/{run_id}")
    _apply_run_snapshot(state, run)
    return run


def start_readiness(
    *,
    repo: str,
    source_sha: str,
    state_file: Path,
    timeout: float,
    resolve_secs: int,
) -> tuple[dict, int]:
    source = collector.normalize_source_sha(source_sha)
    if resolve_secs < 0 or resolve_secs > 120:
        raise ReadinessError("resolve_secs must be within 0..120")
    state_path = _validate_state_path_for_create(state_file)
    client = collector.GitHubClient(repo, collector.resolve_github_token(), timeout)
    main_sha = _main_sha(client)
    if main_sha != source:
        raise ReadinessError(f"main source fence failed: expected={source} current={main_sha}")

    ci_run = _successful_main_ci_run(client, source)
    ci_run_id = ci_run["id"]
    ci_run_attempt = ci_run["run_attempt"]
    ci_run_url = ci_run["html_url"]

    request_id = f"rr_{secrets.token_hex(12)}"
    now = int(time.time())
    state = {
        "schema_version": STATE_SCHEMA_VERSION,
        "kind": "release-readiness",
        "repo": repo,
        "source_sha": source,
        "workflow_file": READINESS_WORKFLOW_FILE,
        "workflow_path": READINESS_WORKFLOW_PATH,
        "request_id": request_id,
        "run_name": _run_name(request_id, source),
        "dispatch_state": "prepared",
        "created_at": now,
        "ci_run_id": ci_run_id,
        "ci_run_attempt": ci_run_attempt,
        "ci_run_url": ci_run_url,
        "ci_run_head_sha": source,
        "ci_run_conclusion": "success",
        "run_id": None,
        "run_head_sha": None,
        "source_matches": None,
        "run_url": None,
        "run_status": None,
        "run_conclusion": None,
        "last_observed_at": None,
    }
    _write_state(state_path, state)

    try:
        _post_dispatch(client, source, request_id, ci_run_id, ci_run_attempt)
    except DispatchRejected:
        state["dispatch_state"] = "rejected"
        _write_state(state_path, state)
        raise
    except DispatchOutcomeUnknown:
        state["dispatch_state"] = "unknown"
        _write_state(state_path, state)
        raise

    state["dispatch_state"] = "dispatched"
    _write_state(state_path, state)
    deadline = time.monotonic() + resolve_secs
    while True:
        run = _recover_run(client, state)
        if run is not None:
            _write_state(state_path, state)
            return state, 0 if state["source_matches"] is True else 1
        if time.monotonic() >= deadline:
            state["dispatch_state"] = "dispatched_unresolved"
            state["last_observed_at"] = int(time.time())
            _write_state(state_path, state)
            return state, 2
        time.sleep(2)


def status_readiness(*, state_file: Path, timeout: float, wait_secs: int) -> tuple[dict, int]:
    if wait_secs < 0 or wait_secs > 3600:
        raise ReadinessError("wait_secs must be within 0..3600")
    state_path = state_file.absolute()
    state = _load_state(state_path)
    if state["dispatch_state"] == "rejected":
        raise ReadinessError("release-readiness dispatch was rejected; create a new state only after fixing the cause")
    client = collector.GitHubClient(state["repo"], collector.resolve_github_token(), timeout)
    deadline = time.monotonic() + wait_secs
    while True:
        run = _get_bound_run(client, state)
        _write_state(state_path, state)
        if run is not None and state["source_matches"] is False:
            return state, 1
        if run is not None and state["run_status"] == "completed":
            return state, 0 if state["run_conclusion"] == "success" else 1
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return state, 2
        time.sleep(min(10, max(0.1, remaining)))
