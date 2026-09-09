#!/usr/bin/env python3
"""Bounded release publication-control helpers for WebCodex."""

from __future__ import annotations

import hashlib
import json
import os
import re
import secrets
import subprocess
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

if __package__:
    from . import collect_release_bundle as collector
else:
    import sys

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import collect_release_bundle as collector

PACKAGE = "@yyjeqhc/webcodex"
NPM_REGISTRY = "https://registry.npmjs.org/"
BUILD_WORKFLOW_FILE = "release-build.yml"
BUILD_WORKFLOW_PATH = f".github/workflows/{BUILD_WORKFLOW_FILE}"
BUILD_STATE_SCHEMA_VERSION = 1
BUILD_REQUEST_RE = re.compile(r"^rb_[0-9a-f]{24}$")
MAX_STATE_BYTES = 64 * 1024
MAX_RUN_LIST = 100
MAX_PUBLIC_JSON_BYTES = 2 * 1024 * 1024
MAX_RELEASE_BUILD_BYTES = 256 * 1024
MAX_STAGE_BINARY_BYTES = collector.MAX_MEMBER_BYTES
MAX_RECLAIM_BUILD_RUNS_PER_PAGE = 100
MAX_RECLAIM_BUILD_PAGES = 10
MAX_DRAFT_RELEASES_PER_PAGE = 100
MAX_DRAFT_RELEASE_PAGES = 10


class PublicationError(RuntimeError):
    pass


class DispatchRejected(PublicationError):
    pass


class DispatchOutcomeUnknown(PublicationError):
    pass


def normalize_version(value: str) -> str:
    version = value.strip().removeprefix("v")
    if not collector.VERSION_RE.fullmatch(version):
        raise PublicationError(f"invalid release version: {value!r}")
    return version


def _run_capture(argv: list[str], *, cwd: Path | None = None, timeout: float = 30.0) -> str:
    try:
        result = subprocess.run(
            argv,
            cwd=cwd,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise PublicationError(f"could not execute {argv[0]}") from exc
    if result.returncode != 0:
        raise PublicationError(f"{argv[0]} command failed with exit code {result.returncode}")
    return result.stdout.strip()


def _run_checked(argv: list[str], *, cwd: Path | None = None, timeout: float = 600.0) -> None:
    try:
        result = subprocess.run(argv, cwd=cwd, check=False, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as exc:
        raise PublicationError(f"could not execute {argv[0]}") from exc
    if result.returncode != 0:
        raise PublicationError(f"{argv[0]} command failed with exit code {result.returncode}")


def _git(root: Path, *args: str) -> str:
    return _run_capture(["git", *args], cwd=root, timeout=30.0)


def _require_exact_clean_root(root: Path, source_sha: str) -> None:
    if not root.is_dir():
        raise PublicationError(f"release source root is missing: {root}")
    head = collector.normalize_source_sha(_git(root, "rev-parse", "HEAD"))
    if head != source_sha:
        raise PublicationError(f"release source HEAD mismatch: expected={source_sha} actual={head}")
    if _git(root, "status", "--porcelain", "--untracked-files=all"):
        raise PublicationError("release source worktree is not clean")


def _package_versions(root: Path) -> dict[str, str]:
    try:
        cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
        cargo_version = cargo["workspace"]["package"]["version"]
        npm = json.loads((root / "npm/webcodex/package.json").read_text(encoding="utf-8"))
        npm_version = npm["version"]
        desktop_package = json.loads((root / "apps/desktop/package.json").read_text(encoding="utf-8"))
        desktop_package_version = desktop_package["version"]
        desktop_cargo = tomllib.loads((root / "apps/desktop/src-tauri/Cargo.toml").read_text(encoding="utf-8"))
        desktop_cargo_version = desktop_cargo["package"]["version"]
        desktop_tauri = json.loads((root / "apps/desktop/src-tauri/tauri.conf.json").read_text(encoding="utf-8"))
        desktop_tauri_version = desktop_tauri["version"]
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError, json.JSONDecodeError, KeyError, TypeError) as exc:
        raise PublicationError("could not read canonical release versions") from exc
    versions = {
        "cargo": cargo_version,
        "npm": npm_version,
        "desktop_package": desktop_package_version,
        "desktop_cargo": desktop_cargo_version,
        "desktop_tauri": desktop_tauri_version,
    }
    if any(not isinstance(value, str) for value in versions.values()):
        raise PublicationError("canonical release versions are invalid")
    return versions


def _require_package_versions(root: Path, requested: str, *, context: str) -> dict[str, str]:
    versions = _package_versions(root)
    mismatches = {name: value for name, value in versions.items() if value != requested}
    if mismatches:
        detail = " ".join(f"{name}={value}" for name, value in versions.items())
        raise PublicationError(f"{context} version mismatch: requested={requested} {detail}")
    return versions


def _github_optional_json(client: collector.GitHubClient, suffix: str) -> dict | None:
    url = client.api_url(suffix)
    try:
        with client.opener.open(client._request(url), timeout=client.timeout) as response:
            raw = response.read(collector.MAX_JSON_BYTES + 1)
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        raise PublicationError(f"GitHub API request failed with HTTP {exc.code}: {url}") from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise PublicationError(f"GitHub API request failed: {url}") from exc
    if len(raw) > collector.MAX_JSON_BYTES:
        raise PublicationError("GitHub JSON response exceeds its bound")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PublicationError("GitHub API returned invalid JSON") from exc
    if not isinstance(value, dict):
        raise PublicationError("GitHub API returned a non-object")
    return value


def _github_json_array(client: collector.GitHubClient, suffix: str) -> list[dict]:
    url = client.api_url(suffix)
    try:
        with client.opener.open(client._request(url), timeout=client.timeout) as response:
            raw = response.read(collector.MAX_JSON_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise PublicationError(f"GitHub API request failed with HTTP {exc.code}: {url}") from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise PublicationError(f"GitHub API request failed: {url}") from exc
    if len(raw) > collector.MAX_JSON_BYTES:
        raise PublicationError("GitHub JSON response exceeds its bound")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PublicationError("GitHub API returned invalid JSON") from exc
    if not isinstance(value, list) or len(value) > MAX_DRAFT_RELEASES_PER_PAGE:
        raise PublicationError("GitHub API returned a malformed release listing")
    if any(not isinstance(item, dict) for item in value):
        raise PublicationError("GitHub API returned a malformed release entry")
    return value


def _find_authenticated_release_by_tag(client: collector.GitHubClient, tag: str) -> dict:
    matches: list[dict] = []
    reached_end = False
    for page in range(1, MAX_DRAFT_RELEASE_PAGES + 1):
        query = urllib.parse.urlencode({"per_page": MAX_DRAFT_RELEASES_PER_PAGE, "page": page})
        releases = _github_json_array(client, f"/releases?{query}")
        matches.extend(release for release in releases if release.get("tag_name") == tag)
        if len(matches) > 1:
            raise PublicationError(f"GitHub release listing contains duplicate tag: {tag}")
        if len(releases) < MAX_DRAFT_RELEASES_PER_PAGE:
            reached_end = True
            break
    if not reached_end:
        raise PublicationError("GitHub release listing exceeds the bounded draft lookup")
    if len(matches) != 1:
        raise PublicationError(f"GitHub draft Release was not found for tag: {tag}")
    return matches[0]


def _fetch_public_json_optional(url: str, timeout: float) -> dict | None:
    request = urllib.request.Request(url, headers={"User-Agent": collector.USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read(MAX_PUBLIC_JSON_BYTES + 1)
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        raise PublicationError(f"public metadata request failed with HTTP {exc.code}") from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise PublicationError("public metadata request failed") from exc
    if len(raw) > MAX_PUBLIC_JSON_BYTES:
        raise PublicationError("public metadata response exceeds its bound")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PublicationError("public metadata response is invalid JSON") from exc
    if not isinstance(value, dict):
        raise PublicationError("public metadata response is not an object")
    return value


def _github_main_sha(client: collector.GitHubClient) -> str:
    payload = client.fetch_json("/branches/main")
    commit = payload.get("commit")
    if not isinstance(commit, dict):
        raise PublicationError("GitHub main branch response is malformed")
    try:
        return collector.normalize_source_sha(str(commit.get("sha", "")))
    except collector.CollectionError as exc:
        raise PublicationError("GitHub main branch SHA is invalid") from exc


def _require_origin_repo(root: Path, repo: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise PublicationError(f"invalid GitHub repository: {repo!r}")
    origin = _git(root, "remote", "get-url", "origin")
    accepted = {
        f"https://github.com/{repo}",
        f"https://github.com/{repo}.git",
        f"git@github.com:{repo}",
        f"git@github.com:{repo}.git",
        f"ssh://git@github.com/{repo}",
        f"ssh://git@github.com/{repo}.git",
    }
    if origin not in accepted:
        raise PublicationError(f"origin repository mismatch: expected={repo} actual={origin}")
    return origin


def _remote_main_source(root: Path) -> str:
    output = _run_capture(["git", "ls-remote", "origin", "refs/heads/main"], cwd=root, timeout=30.0)
    lines = output.splitlines()
    if len(lines) != 1:
        raise PublicationError("could not resolve exactly one remote main ref")
    fields = lines[0].split("\t", 1)
    if len(fields) != 2 or fields[1] != "refs/heads/main":
        raise PublicationError("git ls-remote returned malformed main ref output")
    try:
        return collector.normalize_source_sha(fields[0])
    except collector.CollectionError as exc:
        raise PublicationError("remote main SHA is invalid") from exc


def _remote_annotated_tag_identity(root: Path, tag: str) -> tuple[str, str] | None:
    output = _run_capture(
        ["git", "ls-remote", "--tags", "origin", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"],
        cwd=root,
        timeout=30.0,
    )
    if not output:
        return None
    refs: dict[str, str] = {}
    for line in output.splitlines():
        fields = line.split("\t", 1)
        if len(fields) != 2:
            raise PublicationError("git ls-remote returned malformed tag output")
        sha, ref = fields
        try:
            refs[ref] = collector.normalize_source_sha(sha)
        except collector.CollectionError as exc:
            raise PublicationError("git ls-remote returned an invalid tag object SHA") from exc
    tag_ref = f"refs/tags/{tag}"
    peeled_ref = f"{tag_ref}^{{}}"
    tag_object = refs.get(tag_ref)
    source = refs.get(peeled_ref)
    if tag_object is None or source is None or set(refs) != {tag_ref, peeled_ref}:
        raise PublicationError(f"remote tag is missing or not an annotated commit tag: {tag}")
    return tag_object, source


def _reclaim_release_check(
    *, repo: str, tag: str, timeout: float, allow_public_release_check: bool
) -> tuple[dict | None, str]:
    encoded_tag = urllib.parse.quote(tag, safe="")
    try:
        token = collector.resolve_github_token()
    except collector.CollectionError:
        if not allow_public_release_check:
            raise PublicationError(
                "authenticated GitHub release check is unavailable; authenticate GitHub or explicitly use "
                "--allow-public-release-check only after separately confirming no draft release exists"
            )
        payload = _fetch_public_json_optional(
            f"https://api.github.com/repos/{repo}/releases/tags/{encoded_tag}", timeout
        )
        return payload, "public_only"
    client = collector.GitHubClient(repo, token, timeout)
    return _github_optional_json(client, f"/releases/tags/{encoded_tag}"), "authenticated"


def _reclaim_build_history(*, repo: str, tag: str, timeout: float) -> list[dict]:
    matches: list[dict] = []
    prefix = f"Release build {tag} "
    for page in range(1, MAX_RECLAIM_BUILD_PAGES + 1):
        query = urllib.parse.urlencode(
            {
                "event": "workflow_dispatch",
                "branch": "main",
                "per_page": MAX_RECLAIM_BUILD_RUNS_PER_PAGE,
                "page": page,
            }
        )
        payload = _fetch_public_json_optional(
            f"https://api.github.com/repos/{repo}/actions/workflows/{BUILD_WORKFLOW_FILE}/runs?{query}",
            timeout,
        )
        if payload is None:
            raise PublicationError("GitHub release-build history is unavailable")
        runs = payload.get("workflow_runs")
        total = payload.get("total_count")
        if not isinstance(runs, list) or not isinstance(total, int) or total < 0:
            raise PublicationError("GitHub release-build history is malformed")
        for run in runs:
            if not isinstance(run, dict) or not str(run.get("display_title", "")).startswith(prefix):
                continue
            run_id = run.get("id")
            status = run.get("status")
            conclusion = run.get("conclusion")
            source = run.get("head_sha")
            if not isinstance(run_id, int) or run_id <= 0 or not isinstance(status, str):
                raise PublicationError("matching release-build history is malformed")
            if conclusion is not None and not isinstance(conclusion, str):
                raise PublicationError("matching release-build conclusion is malformed")
            try:
                normalized_source = collector.normalize_source_sha(str(source))
            except collector.CollectionError as exc:
                raise PublicationError("matching release-build source SHA is invalid") from exc
            matches.append(
                {
                    "run_id": run_id,
                    "status": status,
                    "conclusion": conclusion,
                    "source_sha": normalized_source,
                }
            )
        if page * MAX_RECLAIM_BUILD_RUNS_PER_PAGE >= total:
            return matches
    raise PublicationError("GitHub release-build history exceeds the bounded reclaim scan")


def reclaim_prepublication_tag(
    *,
    repo: str,
    version: str,
    root: Path,
    confirm: str,
    timeout: float,
    allow_public_release_check: bool,
) -> dict:
    release_version = normalize_version(version)
    tag = f"v{release_version}"
    if confirm != tag:
        raise PublicationError(f"reclaim confirmation must exactly equal {tag}")
    source_root = root.absolute()
    if not source_root.is_dir():
        raise PublicationError(f"release source root is missing: {source_root}")
    if _git(source_root, "status", "--porcelain", "--untracked-files=all"):
        raise PublicationError("release recovery worktree is not clean")
    _require_origin_repo(source_root, repo)
    try:
        head = collector.normalize_source_sha(_git(source_root, "rev-parse", "HEAD"))
    except collector.CollectionError as exc:
        raise PublicationError("release recovery HEAD is invalid") from exc
    remote_main = _remote_main_source(source_root)
    if head != remote_main:
        raise PublicationError(f"release recovery source is not exact remote main: head={head} main={remote_main}")
    _require_package_versions(source_root, release_version, context="release recovery")

    remote_identity = _remote_annotated_tag_identity(source_root, tag)
    if remote_identity is None:
        raise PublicationError(f"remote tag does not exist: {tag}")
    tag_object_sha, old_source_sha = remote_identity
    local_tag = _git(source_root, "tag", "--list", tag)
    if local_tag:
        try:
            local_source = collector.normalize_source_sha(_git(source_root, "rev-list", "-n", "1", tag))
        except collector.CollectionError as exc:
            raise PublicationError("local reclaim tag source is invalid") from exc
        if local_source != old_source_sha:
            raise PublicationError(
                f"local/remote tag source mismatch: local={local_source} remote={old_source_sha}"
            )

    release, release_check = _reclaim_release_check(
        repo=repo,
        tag=tag,
        timeout=timeout,
        allow_public_release_check=allow_public_release_check,
    )
    if release is not None:
        raise PublicationError(f"GitHub Release exists; refusing to reclaim {tag}")

    encoded_package = urllib.parse.quote(PACKAGE, safe="")
    npm_metadata = _fetch_public_json_optional(
        f"{NPM_REGISTRY}{encoded_package}/{urllib.parse.quote(release_version, safe='')}", timeout
    )
    if npm_metadata is not None:
        raise PublicationError(f"npm package version exists; refusing to reclaim {PACKAGE}@{release_version}")

    history = _reclaim_build_history(repo=repo, tag=tag, timeout=timeout)
    active = [run for run in history if run["status"] != "completed"]
    successful = [run for run in history if run["status"] == "completed" and run["conclusion"] == "success"]
    if active:
        raise PublicationError(f"release-build run is still active for {tag}; refusing reclaim")
    if successful:
        raise PublicationError(f"successful authoritative release-build exists for {tag}; refusing reclaim")

    lease = f"--force-with-lease=refs/tags/{tag}:{tag_object_sha}"
    try:
        _run_checked(
            ["git", "push", lease, "origin", f":refs/tags/{tag}"],
            cwd=source_root,
            timeout=120.0,
        )
    except PublicationError as exc:
        if _remote_annotated_tag_identity(source_root, tag) is not None:
            raise PublicationError(f"remote tag deletion failed for {tag}") from exc
    if _remote_annotated_tag_identity(source_root, tag) is not None:
        raise PublicationError(f"remote tag deletion could not be verified for {tag}")
    if local_tag:
        _run_checked(["git", "tag", "-d", tag], cwd=source_root, timeout=30.0)

    return {
        "version": release_version,
        "tag": tag,
        "old_tag_object_sha": tag_object_sha,
        "old_source_sha": old_source_sha,
        "github_release_absent": True,
        "github_release_check": release_check,
        "npm_version_absent": True,
        "matching_release_builds": history,
        "remote_tag_deleted": True,
        "local_tag_deleted": bool(local_tag),
    }


def preflight_release(
    *,
    repo: str,
    version: str,
    source_sha: str,
    root: Path,
    timeout: float,
) -> dict:
    release_version = normalize_version(version)
    source = collector.normalize_source_sha(source_sha)
    source_root = root.absolute()
    _require_exact_clean_root(source_root, source)
    _require_package_versions(source_root, release_version, context="release")

    client = collector.GitHubClient(repo, collector.resolve_github_token(), timeout)
    main_sha = _github_main_sha(client)
    if main_sha != source:
        raise PublicationError(f"GitHub main source fence failed: expected={source} current={main_sha}")
    tag = f"v{release_version}"
    if _git(source_root, "tag", "--list", tag):
        raise PublicationError(f"local Git tag already exists: {tag}")
    encoded_tag = urllib.parse.quote(tag, safe="")
    if _github_optional_json(client, f"/git/ref/tags/{encoded_tag}") is not None:
        raise PublicationError(f"Git tag already exists: {tag}")
    if _github_optional_json(client, f"/releases/tags/{encoded_tag}") is not None:
        raise PublicationError(f"GitHub Release already exists: {tag}")

    encoded_package = urllib.parse.quote(PACKAGE, safe="")
    npm_metadata = _fetch_public_json_optional(
        f"{NPM_REGISTRY}{encoded_package}/{urllib.parse.quote(release_version, safe='')}",
        timeout,
    )
    if npm_metadata is not None:
        raise PublicationError(f"npm package version already exists: {PACKAGE}@{release_version}")

    github_user = client.fetch_authenticated_user().get("login")
    if not isinstance(github_user, str) or not github_user:
        raise PublicationError("GitHub publication identity is unavailable")
    npm_user = _run_capture(
        ["npm", "whoami", "--registry", NPM_REGISTRY], timeout=min(timeout, 30.0)
    )
    if not npm_user or any(ch.isspace() for ch in npm_user):
        raise PublicationError("npm publication identity is unavailable")

    return {
        "version": release_version,
        "tag": tag,
        "source_sha": source,
        "github_main_sha": main_sha,
        "github_user": github_user,
        "npm_user": npm_user,
        "tag_available": True,
        "github_release_available": True,
        "npm_version_available": True,
    }


def _build_run_name(tag: str, request_id: str) -> str:
    return f"Release build {tag} {request_id}"


def _validate_build_request_id(value: str) -> str:
    request_id = value.strip()
    if not BUILD_REQUEST_RE.fullmatch(request_id):
        raise PublicationError("invalid release-build request id")
    return request_id


def _validate_state_path_for_create(path: Path) -> Path:
    result = path.absolute()
    if result.exists() or result.is_symlink():
        raise PublicationError(f"release-build state file already exists: {result}")
    result.parent.mkdir(parents=True, exist_ok=True)
    return result


def _write_state(path: Path, state: dict) -> None:
    payload = (json.dumps(state, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(payload) > MAX_STATE_BYTES:
        raise PublicationError("release-build state exceeds its size bound")
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temp_path = Path(temp_name)
    try:
        if hasattr(os, "fchmod"):
            os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        if path.is_symlink():
            raise PublicationError(f"refusing to replace release-build state symlink: {path}")
        os.replace(temp_path, path)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise


def _load_state(path: Path) -> dict:
    state_path = path.absolute()
    if state_path.is_symlink() or not state_path.is_file():
        raise PublicationError(f"release-build state file is missing or unsafe: {state_path}")
    try:
        size = state_path.stat().st_size
        if size <= 0 or size > MAX_STATE_BYTES:
            raise PublicationError("release-build state file is outside its size bound")
        value = json.loads(state_path.read_text(encoding="utf-8"))
    except PublicationError:
        raise
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PublicationError("could not read release-build state file") from exc
    if not isinstance(value, dict):
        raise PublicationError("release-build state must be a JSON object")
    required = {
        "schema_version",
        "kind",
        "repo",
        "tag",
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
    if set(value) != required:
        raise PublicationError("release-build state fields do not match the supported schema")
    if value.get("schema_version") != BUILD_STATE_SCHEMA_VERSION or value.get("kind") != "release-build":
        raise PublicationError("unsupported release-build state schema")
    tag = collector.validate_expected_tag(str(value.get("tag", "")))
    source = collector.normalize_source_sha(str(value.get("source_sha", "")))
    request_id = _validate_build_request_id(str(value.get("request_id", "")))
    if value.get("workflow_file") != BUILD_WORKFLOW_FILE or value.get("workflow_path") != BUILD_WORKFLOW_PATH:
        raise PublicationError("release-build state references an unexpected workflow")
    if value.get("run_name") != _build_run_name(tag, request_id):
        raise PublicationError("release-build state run name is inconsistent")
    if not isinstance(value.get("repo"), str):
        raise PublicationError("release-build state repository is invalid")
    if not isinstance(value.get("created_at"), int) or value["created_at"] <= 0:
        raise PublicationError("release-build state created_at is invalid")
    run_id = value.get("run_id")
    if run_id is not None and (not isinstance(run_id, int) or run_id <= 0):
        raise PublicationError("release-build state run id is invalid")
    run_head_sha = value.get("run_head_sha")
    if run_head_sha is not None:
        collector.normalize_source_sha(str(run_head_sha))
    if value.get("source_matches") is not None and not isinstance(value.get("source_matches"), bool):
        raise PublicationError("release-build state source_matches is invalid")
    for field in ("run_url", "run_status", "run_conclusion"):
        if value.get(field) is not None and not isinstance(value.get(field), str):
            raise PublicationError(f"release-build state {field} is invalid")
    last_observed_at = value.get("last_observed_at")
    if last_observed_at is not None and (not isinstance(last_observed_at, int) or last_observed_at <= 0):
        raise PublicationError("release-build state last_observed_at is invalid")
    return value


def _remote_annotated_tag_source(client: collector.GitHubClient, tag: str) -> str:
    encoded = urllib.parse.quote(tag, safe="")
    ref = client.fetch_json(f"/git/ref/tags/{encoded}")
    obj = ref.get("object")
    if not isinstance(obj, dict) or obj.get("type") != "tag" or not isinstance(obj.get("sha"), str):
        raise PublicationError(f"remote tag is missing or not annotated: {tag}")
    tag_object = client.fetch_json(f"/git/tags/{obj['sha']}")
    target = tag_object.get("object")
    if not isinstance(target, dict) or target.get("type") != "commit":
        raise PublicationError(f"annotated tag does not point directly to a commit: {tag}")
    try:
        return collector.normalize_source_sha(str(target.get("sha", "")))
    except collector.CollectionError as exc:
        raise PublicationError("annotated tag target SHA is invalid") from exc


def _post_build_dispatch(client: collector.GitHubClient, tag: str, request_id: str) -> None:
    url = client.api_url(f"/actions/workflows/{BUILD_WORKFLOW_FILE}/dispatches")
    data = json.dumps(
        {"ref": "main", "inputs": {"tag": tag, "request_id": request_id}},
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
            raise DispatchRejected(f"GitHub rejected release-build dispatch with HTTP {exc.code}") from exc
        raise DispatchOutcomeUnknown(
            f"GitHub release-build dispatch outcome is unknown after HTTP {exc.code}; do not redispatch"
        ) from exc
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise DispatchOutcomeUnknown(
            "GitHub release-build dispatch outcome is unknown after transport failure; do not redispatch"
        ) from exc
    if status != 204 or body:
        raise DispatchOutcomeUnknown(
            f"GitHub release-build dispatch returned unexpected HTTP {status}; do not redispatch"
        )


def _build_run_identity_matches(run: dict, state: dict) -> bool:
    return (
        run.get("path") == BUILD_WORKFLOW_PATH
        and run.get("event") == "workflow_dispatch"
        and run.get("head_branch") == "main"
        and run.get("display_title") == state["run_name"]
    )


def select_build_run(payload: dict, state: dict) -> dict | None:
    runs = payload.get("workflow_runs")
    if not isinstance(runs, list):
        raise PublicationError("GitHub release-build run listing is malformed")
    if len(runs) > MAX_RUN_LIST:
        raise PublicationError("GitHub release-build run listing exceeds its bound")
    matches = [run for run in runs if isinstance(run, dict) and _build_run_identity_matches(run, state)]
    if len(matches) > 1:
        raise PublicationError("multiple GitHub release-build runs match one request id")
    return matches[0] if matches else None


def _apply_build_run_snapshot(state: dict, run: dict) -> None:
    if not _build_run_identity_matches(run, state):
        raise PublicationError("GitHub release-build run no longer matches durable request identity")
    run_id = run.get("id")
    if not isinstance(run_id, int) or run_id <= 0:
        raise PublicationError("GitHub release-build run id is invalid")
    if state.get("run_id") is not None and state["run_id"] != run_id:
        raise PublicationError("GitHub release-build run id changed after durable binding")
    try:
        run_head_sha = collector.normalize_source_sha(str(run.get("head_sha", "")))
    except collector.CollectionError as exc:
        raise PublicationError("GitHub release-build run head SHA is invalid") from exc
    status = run.get("status")
    conclusion = run.get("conclusion")
    run_url = run.get("html_url")
    if not isinstance(status, str):
        raise PublicationError("GitHub release-build run status is invalid")
    if conclusion is not None and not isinstance(conclusion, str):
        raise PublicationError("GitHub release-build run conclusion is invalid")
    if not isinstance(run_url, str) or not run_url.startswith("https://github.com/"):
        raise PublicationError("GitHub release-build run URL is invalid")
    state["run_id"] = run_id
    state["run_head_sha"] = run_head_sha
    state["source_matches"] = run_head_sha == state["source_sha"]
    state["run_url"] = run_url
    state["run_status"] = status
    state["run_conclusion"] = conclusion
    state["last_observed_at"] = int(time.time())
    state["dispatch_state"] = "completed" if status == "completed" else "resolved"


def _list_build_runs(client: collector.GitHubClient) -> dict:
    return client.fetch_json(
        f"/actions/workflows/{BUILD_WORKFLOW_FILE}/runs?event=workflow_dispatch&branch=main&per_page={MAX_RUN_LIST}"
    )


def _recover_build_run(client: collector.GitHubClient, state: dict) -> dict | None:
    run = select_build_run(_list_build_runs(client), state)
    if run is not None:
        _apply_build_run_snapshot(state, run)
    return run


def _get_bound_build_run(client: collector.GitHubClient, state: dict) -> dict | None:
    run_id = state.get("run_id")
    if run_id is None:
        return _recover_build_run(client, state)
    run = client.fetch_json(f"/actions/runs/{run_id}")
    _apply_build_run_snapshot(state, run)
    return run


def start_build(
    *,
    repo: str,
    source_sha: str,
    tag: str,
    state_file: Path,
    timeout: float,
    resolve_secs: int,
) -> tuple[dict, int]:
    source = collector.normalize_source_sha(source_sha)
    release_tag = collector.validate_expected_tag(tag)
    if resolve_secs < 0 or resolve_secs > 120:
        raise PublicationError("resolve_secs must be within 0..120")
    state_path = _validate_state_path_for_create(state_file)
    client = collector.GitHubClient(repo, collector.resolve_github_token(), timeout)
    main_sha = _github_main_sha(client)
    if main_sha != source:
        raise PublicationError(f"main source fence failed: expected={source} current={main_sha}")
    tag_source = _remote_annotated_tag_source(client, release_tag)
    if tag_source != source:
        raise PublicationError(f"remote tag source mismatch: expected={source} actual={tag_source}")

    request_id = f"rb_{secrets.token_hex(12)}"
    state = {
        "schema_version": BUILD_STATE_SCHEMA_VERSION,
        "kind": "release-build",
        "repo": repo,
        "tag": release_tag,
        "source_sha": source,
        "workflow_file": BUILD_WORKFLOW_FILE,
        "workflow_path": BUILD_WORKFLOW_PATH,
        "request_id": request_id,
        "run_name": _build_run_name(release_tag, request_id),
        "dispatch_state": "prepared",
        "created_at": int(time.time()),
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
        _post_build_dispatch(client, release_tag, request_id)
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
        run = _recover_build_run(client, state)
        if run is not None:
            _write_state(state_path, state)
            return state, 0 if state["source_matches"] is True else 1
        if time.monotonic() >= deadline:
            state["dispatch_state"] = "dispatched_unresolved"
            state["last_observed_at"] = int(time.time())
            _write_state(state_path, state)
            return state, 2
        time.sleep(2)


def status_build(*, state_file: Path, timeout: float, wait_secs: int) -> tuple[dict, int]:
    if wait_secs < 0 or wait_secs > 7200:
        raise PublicationError("wait_secs must be within 0..7200")
    state_path = state_file.absolute()
    state = _load_state(state_path)
    if state["dispatch_state"] == "rejected":
        raise PublicationError("release-build dispatch was rejected; create a new state only after fixing the cause")
    client = collector.GitHubClient(state["repo"], collector.resolve_github_token(), timeout)
    deadline = time.monotonic() + wait_secs
    while True:
        run = _get_bound_build_run(client, state)
        _write_state(state_path, state)
        if run is not None and state["source_matches"] is False:
            return state, 1
        if run is not None and state["run_status"] == "completed":
            return state, 0 if state["run_conclusion"] == "success" else 1
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return state, 2
        time.sleep(min(10, max(0.1, remaining)))


def _read_release_build(bundle_dir: Path) -> dict:
    path = bundle_dir / "release-build.json"
    try:
        raw = path.read_bytes()
    except OSError as exc:
        raise PublicationError("release-build.json is missing from bundle") from exc
    if not raw or len(raw) > MAX_RELEASE_BUILD_BYTES:
        raise PublicationError("release-build.json is outside its size bound")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PublicationError("release-build.json is invalid JSON") from exc
    if not isinstance(value, dict):
        raise PublicationError("release-build.json is not an object")
    return value


def verify_bundle(bundle_dir: Path, repo: str) -> dict:
    root = bundle_dir.absolute()
    metadata = _read_release_build(root)
    try:
        tag = collector.validate_expected_tag(str(metadata.get("tag", "")))
        source = collector.normalize_source_sha(str(metadata.get("source_sha", "")))
    except collector.CollectionError as exc:
        raise PublicationError("release-build.json has invalid tag/source") from exc
    run_id = metadata.get("workflow_run_id")
    archive_stem = metadata.get("archive_stem")
    if not isinstance(run_id, int) or run_id <= 0 or not isinstance(archive_stem, str):
        raise PublicationError("release-build.json has invalid workflow_run_id/archive_stem")
    try:
        return collector.verify_bundle_directory(
            root,
            repo=repo,
            run_id=run_id,
            expected_source_sha=source,
            expected_tag=tag,
            artifact_name=f"{archive_stem}-bundle",
        )
    except collector.CollectionError as exc:
        raise PublicationError(str(exc)) from exc


def _extract_linux_x64_binaries(bundle_dir: Path, summary: dict, destination: Path) -> None:
    archive = bundle_dir / f"{summary['archive_stem']}-linux-x64.tar.gz"
    expected = set(collector.BINARIES)
    seen: set[str] = set()
    try:
        with tarfile.open(archive, "r:gz") as handle:
            members = handle.getmembers()
            if len(members) != len(expected):
                raise PublicationError("linux-x64 release archive has unexpected member count")
            destination.mkdir(mode=0o700)
            for member in members:
                name = member.name.removeprefix("./")
                if name not in expected or name in seen or not member.isfile():
                    raise PublicationError("linux-x64 release archive has unexpected member")
                if member.size <= 0 or member.size > MAX_STAGE_BINARY_BYTES:
                    raise PublicationError("linux-x64 release binary is outside its size bound")
                source = handle.extractfile(member)
                if source is None:
                    raise PublicationError("linux-x64 release binary could not be read")
                target = destination / name
                with target.open("xb") as output:
                    remaining = member.size
                    while remaining:
                        chunk = source.read(min(1024 * 1024, remaining))
                        if not chunk:
                            raise PublicationError("linux-x64 release binary was truncated")
                        output.write(chunk)
                        remaining -= len(chunk)
                    if source.read(1):
                        raise PublicationError("linux-x64 release binary exceeded declared size")
                target.chmod(0o755)
                seen.add(name)
    except PublicationError:
        raise
    except (tarfile.TarError, OSError) as exc:
        raise PublicationError("could not extract linux-x64 release binaries") from exc
    if seen != expected:
        raise PublicationError("linux-x64 release archive is missing binaries")


def stage_npm(
    *,
    repo: str,
    bundle_dir: Path,
    source_root: Path,
    output_dir: Path,
) -> dict:
    bundle = bundle_dir.absolute()
    source = source_root.absolute()
    destination = output_dir.absolute()
    summary = verify_bundle(bundle, repo)
    if summary.get("build_kind") != "release":
        raise PublicationError("npm staging requires a real release bundle")
    _require_exact_clean_root(source, str(summary["source_sha"]))
    tag_commit = _git(source, "rev-parse", f"{summary['tag']}^{{commit}}")
    if tag_commit != summary["source_sha"]:
        raise PublicationError("source worktree tag does not match bundle source")
    if destination.exists() or destination.is_symlink():
        raise PublicationError(f"npm staging output already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)

    _run_checked(
        [
            "bash",
            str(source / "scripts/stage_npm_release.sh"),
            "--manifest",
            str(bundle / "manifest.json"),
            "--output-dir",
            str(destination),
        ],
        cwd=source,
    )
    with tempfile.TemporaryDirectory(prefix="webcodex-release-binaries-") as temp:
        binaries = Path(temp) / "bin"
        _extract_linux_x64_binaries(bundle, summary, binaries)
        _run_checked(
            [
                "bash",
                str(source / "scripts/npm_package_smoke.sh"),
                "--package-dir",
                str(destination / "npm-package"),
                "--binary-dir",
                str(binaries),
            ],
            cwd=source,
            timeout=900.0,
        )
    return {
        **summary,
        "source_root": str(source),
        "stage_dir": str(destination / "npm-package"),
        "npm_smoke": "passed",
    }


def _github_asset_digest(asset: dict) -> str:
    value = asset.get("digest")
    if not isinstance(value, str) or not value.startswith("sha256:"):
        raise PublicationError(f"GitHub asset is missing a SHA-256 digest: {asset.get('name')!r}")
    digest = value.removeprefix("sha256:")
    if not collector.SHA256_RE.fullmatch(digest):
        raise PublicationError(f"GitHub asset digest is invalid: {asset.get('name')!r}")
    return digest


def verify_draft_assets(*, repo: str, bundle_dir: Path, timeout: float) -> dict:
    bundle = bundle_dir.absolute()
    summary = verify_bundle(bundle, repo)
    if summary.get("build_kind") != "release":
        raise PublicationError("draft verification requires a real release bundle")
    tag = str(summary["tag"])
    client = collector.GitHubClient(repo, collector.resolve_github_token(), timeout)
    release = _find_authenticated_release_by_tag(client, tag)
    if release.get("tag_name") != tag or release.get("draft") is not True or release.get("prerelease") is not False:
        raise PublicationError("GitHub Release must be the expected draft, non-prerelease release")
    release_id = release.get("id")
    release_url = release.get("html_url")
    if not isinstance(release_id, int) or release_id <= 0:
        raise PublicationError("GitHub draft release id is invalid")
    if not isinstance(release_url, str) or not release_url.startswith("https://github.com/"):
        raise PublicationError("GitHub draft release URL is invalid")
    assets = release.get("assets")
    if not isinstance(assets, list) or len(assets) > 16:
        raise PublicationError("GitHub draft asset listing is malformed or too large")

    expected_files = {"SHA256SUMS"}
    expected_files.update(f"{summary['archive_stem']}-{platform}.tar.gz" for platform in collector.PLATFORMS)
    desktop_artifacts = summary.get("desktop_artifacts")
    if not isinstance(desktop_artifacts, dict) or set(desktop_artifacts) != set(collector.DESKTOP_PLATFORMS):
        raise PublicationError("retained bundle Desktop artifact summary is invalid")
    for platform in collector.DESKTOP_PLATFORMS:
        item = desktop_artifacts.get(platform)
        if not isinstance(item, dict) or not isinstance(item.get("filename"), str):
            raise PublicationError(f"retained bundle Desktop artifact is invalid: {platform}")
        expected_files.add(item["filename"])
    by_name: dict[str, dict] = {}
    for asset in assets:
        if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
            raise PublicationError("GitHub draft asset entry is malformed")
        name = asset["name"]
        if name in by_name:
            raise PublicationError(f"GitHub draft contains duplicate asset: {name}")
        by_name[name] = asset
    if set(by_name) != expected_files:
        raise PublicationError(
            f"GitHub draft asset set mismatch: expected={sorted(expected_files)} actual={sorted(by_name)}"
        )

    verified: dict[str, str] = {}
    for name in sorted(expected_files):
        local = bundle / name
        try:
            size = local.stat().st_size
        except OSError as exc:
            raise PublicationError(f"retained release asset is missing: {name}") from exc
        asset = by_name[name]
        if asset.get("state") != "uploaded" or asset.get("size") != size:
            raise PublicationError(f"GitHub draft asset size/state mismatch: {name}")
        local_digest = collector.sha256_file(local)
        if _github_asset_digest(asset) != local_digest:
            raise PublicationError(f"GitHub draft asset SHA-256 mismatch: {name}")
        verified[name] = local_digest

    return {
        "tag": tag,
        "version": summary["version"],
        "source_sha": summary["source_sha"],
        "workflow_run_id": summary["workflow_run_id"],
        "release_id": release_id,
        "release_url": release_url,
        "draft": True,
        "assets": verified,
        "verification": "github_asset_digest_matches_retained_bytes",
    }
