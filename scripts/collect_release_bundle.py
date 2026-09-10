#!/usr/bin/env python3
"""Collect and verify one assembled WebCodex release bundle from GitHub Actions."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path
from typing import BinaryIO

DEFAULT_REPO = "yyjeqhc/webcodex"
PLATFORMS = ("linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "win32-x64", "win32-arm64")
BINARIES = ("webcodex", "webcodex-server", "webcodex-runner")
DESKTOP_PLATFORMS = ("darwin-x64", "darwin-arm64", "win32-x64", "win32-arm64")
RELEASE_WORKFLOW_PATH = ".github/workflows/release-build.yml"
API_VERSION = "2022-11-28"
USER_AGENT = "webcodex-release-bundle-collector/1"
MAX_JSON_BYTES = 2 * 1024 * 1024
MAX_ARTIFACT_COUNT = 16
MAX_ARTIFACT_ZIP_BYTES = 384 * 1024 * 1024
MAX_ZIP_MEMBERS = 16
MAX_UNCOMPRESSED_BYTES = 384 * 1024 * 1024
MAX_MEMBER_BYTES = 96 * 1024 * 1024
MAX_REPORT_BYTES = 2 * 1024 * 1024
MAX_RELEASE_BUILD_BYTES = 256 * 1024
MAX_MANIFEST_BYTES = 256 * 1024
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SOURCE_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
RELEASE_TAG_RE = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
VERIFY_TAG_RE = re.compile(r"^release-build-test-[0-9A-Za-z][0-9A-Za-z._-]*$")
SAFE_NAME_RE = re.compile(r"^[0-9A-Za-z][0-9A-Za-z._+-]*$")


class CollectionError(RuntimeError):
    pass


def normalize_source_sha(value: str) -> str:
    source = value.strip().lower()
    if not SOURCE_SHA_RE.fullmatch(source):
        raise CollectionError(f"invalid expected source SHA: {value!r}")
    return source


def validate_expected_tag(value: str) -> str:
    tag = value.strip()
    if RELEASE_TAG_RE.fullmatch(tag):
        return tag
    if len(tag) <= 80 and VERIFY_TAG_RE.fullmatch(tag):
        return tag
    raise CollectionError(f"invalid release or verification tag: {value!r}")


def desktop_artifact_filename(
    version: str,
    platform: str,
    *,
    build_kind: str,
    tag: str,
    source_sha: str,
) -> str:
    if platform not in DESKTOP_PLATFORMS:
        raise CollectionError(f"unsupported Desktop platform: {platform!r}")
    suffix = "-setup.exe" if platform.startswith("win32-") else ".dmg"
    if build_kind == "release":
        return f"webcodex-desktop-v{version}-{platform}{suffix}"
    if build_kind == "verification":
        return f"webcodex-desktop-{tag}-{source_sha[:12]}-v{version}-{platform}{suffix}"
    raise CollectionError(f"unsupported release build kind: {build_kind!r}")


def resolve_github_token() -> str:
    for name in ("GH_TOKEN", "GITHUB_TOKEN"):
        token = os.environ.get(name, "").strip()
        if token:
            if any(ch.isspace() for ch in token):
                raise CollectionError(f"{name} contains invalid whitespace")
            return token
    if shutil.which("gh") is None:
        raise CollectionError("GitHub authentication required: set GH_TOKEN/GITHUB_TOKEN or log in with gh")
    try:
        result = subprocess.run(
            ["gh", "auth", "token", "--hostname", "github.com"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise CollectionError("could not obtain the current gh authentication token") from exc
    token = result.stdout.strip() if result.returncode == 0 else ""
    if not token:
        raise CollectionError("GitHub authentication required: set GH_TOKEN/GITHUB_TOKEN or run gh auth login")
    if any(ch.isspace() for ch in token):
        raise CollectionError("gh auth token returned invalid whitespace")
    return token


class _SafeRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Follow GitHub's signed artifact redirect without forwarding API credentials cross-host."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[override]
        old = urllib.parse.urlsplit(req.full_url)
        new = urllib.parse.urlsplit(newurl)
        if new.scheme != "https":
            raise CollectionError(f"refusing non-HTTPS GitHub redirect: {newurl}")
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if redirected is not None and old.netloc.lower() != new.netloc.lower():
            for header in ("Authorization", "Accept", "X-GitHub-Api-Version"):
                redirected.remove_header(header)
        return redirected


class GitHubClient:
    def __init__(self, repo: str, token: str, timeout: float) -> None:
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
            raise CollectionError(f"invalid GitHub repository: {repo!r}")
        if timeout <= 0 or timeout > 600:
            raise CollectionError("timeout must be within 0 < timeout <= 600 seconds")
        self.repo = repo
        self.token = token
        self.timeout = timeout
        self.opener = urllib.request.build_opener(_SafeRedirectHandler())

    def api_url(self, suffix: str) -> str:
        return f"https://api.github.com/repos/{self.repo}{suffix}"

    def _request(self, url: str) -> urllib.request.Request:
        return urllib.request.Request(
            url,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": API_VERSION,
                "User-Agent": USER_AGENT,
            },
        )

    def _fetch_json_url(self, url: str) -> dict:
        try:
            with self.opener.open(self._request(url), timeout=self.timeout) as response:
                length = _content_length(response.headers)
                if length is not None and length > MAX_JSON_BYTES:
                    raise CollectionError(f"GitHub JSON response exceeds {MAX_JSON_BYTES} bytes")
                raw = response.read(MAX_JSON_BYTES + 1)
        except CollectionError:
            raise
        except urllib.error.HTTPError as exc:
            raise CollectionError(f"GitHub API request failed with HTTP {exc.code}: {url}") from exc
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            raise CollectionError(f"GitHub API request failed: {url}: {exc}") from exc
        if len(raw) > MAX_JSON_BYTES:
            raise CollectionError(f"GitHub JSON response exceeds {MAX_JSON_BYTES} bytes")
        try:
            value = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise CollectionError(f"GitHub API returned invalid JSON: {url}") from exc
        if not isinstance(value, dict):
            raise CollectionError(f"GitHub API returned a non-object: {url}")
        return value

    def fetch_json(self, suffix: str) -> dict:
        return self._fetch_json_url(self.api_url(suffix))

    def fetch_authenticated_user(self) -> dict:
        return self._fetch_json_url("https://api.github.com/user")

    def download_artifact_zip(
        self,
        artifact_id: int,
        destination: Path,
        expected_size: int,
        expected_digest: str,
    ) -> tuple[int, str]:
        if artifact_id <= 0:
            raise CollectionError("invalid artifact id")
        if expected_size <= 0 or expected_size > MAX_ARTIFACT_ZIP_BYTES:
            raise CollectionError(f"artifact zip size is outside the allowed bound: {expected_size}")
        url = self.api_url(f"/actions/artifacts/{artifact_id}/zip")
        digest = hashlib.sha256()
        written = 0
        progress_step = 8 * 1024 * 1024
        next_progress = progress_step
        print(
            f"[release-collect] downloading bundle artifact {artifact_id}: "
            f"{expected_size / (1024 * 1024):.1f} MiB",
            file=os.sys.stderr,
            flush=True,
        )
        try:
            with self.opener.open(self._request(url), timeout=self.timeout) as response, destination.open("xb") as output:
                length = _content_length(response.headers)
                if length is not None and length > MAX_ARTIFACT_ZIP_BYTES:
                    raise CollectionError("artifact download exceeds the maximum zip size")
                while True:
                    chunk = response.read(1024 * 1024)
                    if not chunk:
                        break
                    written += len(chunk)
                    if written > MAX_ARTIFACT_ZIP_BYTES:
                        raise CollectionError("artifact download exceeds the maximum zip size")
                    output.write(chunk)
                    digest.update(chunk)
                    if written >= next_progress:
                        print(
                            f"[release-collect] downloaded "
                            f"{written / (1024 * 1024):.1f}/"
                            f"{expected_size / (1024 * 1024):.1f} MiB",
                            file=os.sys.stderr,
                            flush=True,
                        )
                        while next_progress <= written:
                            next_progress += progress_step
        except CollectionError:
            destination.unlink(missing_ok=True)
            raise
        except urllib.error.HTTPError as exc:
            destination.unlink(missing_ok=True)
            raise CollectionError(f"GitHub artifact download failed with HTTP {exc.code}") from exc
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            destination.unlink(missing_ok=True)
            raise CollectionError(f"GitHub artifact download failed: {exc}") from exc
        actual_digest = digest.hexdigest()
        if written != expected_size:
            destination.unlink(missing_ok=True)
            raise CollectionError(
                f"artifact zip size mismatch: metadata={expected_size} downloaded={written}"
            )
        if actual_digest != expected_digest:
            destination.unlink(missing_ok=True)
            raise CollectionError("artifact zip SHA-256 does not match GitHub artifact metadata")
        print(
            f"[release-collect] bundle artifact download verified: "
            f"{written / (1024 * 1024):.1f} MiB",
            file=os.sys.stderr,
            flush=True,
        )
        return written, actual_digest


def _content_length(headers: object) -> int | None:
    value = getattr(headers, "get", lambda _name: None)("Content-Length")
    if value is None:
        return None
    try:
        length = int(value)
    except (TypeError, ValueError) as exc:
        raise CollectionError(f"invalid Content-Length: {value!r}") from exc
    if length < 0:
        raise CollectionError(f"invalid Content-Length: {value!r}")
    return length


def validate_run(run: dict, run_id: int, expected_source_sha: str) -> None:
    if run.get("id") != run_id:
        raise CollectionError("workflow run id mismatch")
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        raise CollectionError("workflow run is not completed successfully")
    if run.get("head_sha") != expected_source_sha:
        raise CollectionError("workflow run head SHA does not match the expected release source")
    if run.get("event") != "workflow_dispatch":
        raise CollectionError("release-build run was not started by workflow_dispatch")
    if run.get("path") != RELEASE_WORKFLOW_PATH:
        raise CollectionError(f"unexpected workflow path: {run.get('path')!r}")
    if run.get("head_branch") != "main":
        raise CollectionError(f"release-build run did not use main: {run.get('head_branch')!r}")


def _artifact_digest(value: object) -> str:
    if not isinstance(value, str) or not value.startswith("sha256:"):
        raise CollectionError("bundle artifact metadata is missing a SHA-256 digest")
    digest = value.removeprefix("sha256:").lower()
    if not SHA256_RE.fullmatch(digest):
        raise CollectionError("bundle artifact metadata contains an invalid SHA-256 digest")
    return digest


def select_bundle_artifact(payload: dict, run_id: int, expected_source_sha: str) -> dict:
    artifacts = payload.get("artifacts")
    total = payload.get("total_count")
    if not isinstance(artifacts, list) or not isinstance(total, int):
        raise CollectionError("GitHub artifact listing is malformed")
    if total != len(artifacts):
        raise CollectionError("GitHub artifact listing was paginated or incomplete")
    if total <= 0 or total > MAX_ARTIFACT_COUNT:
        raise CollectionError(f"unexpected artifact count for release-build run: {total}")
    candidates = [
        item
        for item in artifacts
        if isinstance(item, dict)
        and isinstance(item.get("name"), str)
        and item["name"].endswith("-bundle")
    ]
    if len(candidates) != 1:
        raise CollectionError(f"expected exactly one assembled bundle artifact, found {len(candidates)}")
    artifact = candidates[0]
    artifact_id = artifact.get("id")
    name = artifact.get("name")
    size = artifact.get("size_in_bytes")
    workflow = artifact.get("workflow_run")
    if not isinstance(artifact_id, int) or artifact_id <= 0:
        raise CollectionError("bundle artifact id is invalid")
    if not isinstance(name, str) or not SAFE_NAME_RE.fullmatch(name) or not name.endswith("-bundle"):
        raise CollectionError("bundle artifact name is invalid")
    if artifact.get("expired") is not False:
        raise CollectionError("bundle artifact is expired")
    if not isinstance(size, int) or size <= 0 or size > MAX_ARTIFACT_ZIP_BYTES:
        raise CollectionError("bundle artifact size is invalid")
    if not isinstance(workflow, dict):
        raise CollectionError("bundle artifact workflow metadata is missing")
    if workflow.get("id") != run_id or workflow.get("head_sha") != expected_source_sha:
        raise CollectionError("bundle artifact is not bound to the expected workflow run/source")
    digest = _artifact_digest(artifact.get("digest"))
    return {
        "id": artifact_id,
        "name": name,
        "size_in_bytes": size,
        "sha256": digest,
    }


def _simple_zip_member_name(name: str) -> str:
    if not name or name in {".", ".."} or "/" in name or "\\" in name:
        raise CollectionError(f"artifact zip contains a nested or invalid entry: {name!r}")
    return name


def _copy_exact(source: BinaryIO, destination: Path, expected_size: int) -> None:
    written = 0
    with destination.open("xb") as output:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            written += len(chunk)
            if written > expected_size or written > MAX_MEMBER_BYTES:
                raise CollectionError(f"artifact zip member exceeds its declared size: {destination.name}")
            output.write(chunk)
    if written != expected_size:
        raise CollectionError(f"artifact zip member size mismatch: {destination.name}")


def safe_extract_zip(path: Path, destination: Path) -> set[str]:
    if destination.exists():
        raise CollectionError(f"temporary extraction directory already exists: {destination}")
    destination.mkdir()
    names: set[str] = set()
    total = 0
    try:
        with zipfile.ZipFile(path) as archive:
            infos = archive.infolist()
            if not infos or len(infos) > MAX_ZIP_MEMBERS:
                raise CollectionError(f"artifact zip member count is invalid: {len(infos)}")
            for info in infos:
                name = _simple_zip_member_name(info.filename)
                if info.is_dir() or name in names:
                    raise CollectionError(f"artifact zip contains a directory or duplicate entry: {name!r}")
                mode = (info.external_attr >> 16) & 0xFFFF
                kind = stat.S_IFMT(mode)
                if kind not in {0, stat.S_IFREG}:
                    raise CollectionError(f"artifact zip contains a non-regular entry: {name!r}")
                if info.file_size < 0 or info.file_size > MAX_MEMBER_BYTES:
                    raise CollectionError(f"artifact zip member is too large: {name!r}")
                total += info.file_size
                if total > MAX_UNCOMPRESSED_BYTES:
                    raise CollectionError("artifact zip uncompressed size exceeds the allowed bound")
                source = archive.open(info, "r")
                try:
                    _copy_exact(source, destination / name, info.file_size)
                finally:
                    source.close()
                names.add(name)
    except CollectionError:
        shutil.rmtree(destination, ignore_errors=True)
        raise
    except (zipfile.BadZipFile, OSError) as exc:
        shutil.rmtree(destination, ignore_errors=True)
        raise CollectionError(f"invalid artifact zip: {exc}") from exc
    return names


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as exc:
        raise CollectionError(f"cannot read assembled bundle file: {path.name}") from exc
    return digest.hexdigest()


def _read_bounded(path: Path, max_bytes: int) -> bytes:
    try:
        size = path.stat().st_size
        if size < 0 or size > max_bytes:
            raise CollectionError(f"bundle metadata file is outside its size bound: {path.name}")
        data = path.read_bytes()
    except CollectionError:
        raise
    except OSError as exc:
        raise CollectionError(f"cannot read assembled bundle metadata: {path.name}") from exc
    if len(data) != size:
        raise CollectionError(f"bundle metadata file changed while reading: {path.name}")
    return data


def _read_json(path: Path, max_bytes: int) -> dict:
    try:
        value = json.loads(_read_bounded(path, max_bytes))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise CollectionError(f"invalid JSON in assembled bundle: {path.name}") from exc
    if not isinstance(value, dict):
        raise CollectionError(f"assembled bundle JSON must be an object: {path.name}")
    return value


def _expected_archive_members(platform: str) -> set[str]:
    suffix = ".exe" if platform.startswith("win32-") else ""
    return {f"{name}{suffix}" for name in BINARIES}


def _verify_archive_members(path: Path, platform: str) -> None:
    names: set[str] = set()
    declared_total = 0
    try:
        with tarfile.open(path, "r:gz") as archive:
            count = 0
            for member in archive:
                count += 1
                if count > 8:
                    raise CollectionError(f"release archive contains too many members: {path.name}")
                if member.isdir():
                    continue
                if not member.isfile():
                    raise CollectionError(f"release archive contains a non-file member: {path.name}")
                if member.size < 0 or member.size > MAX_MEMBER_BYTES:
                    raise CollectionError(f"release archive member is outside its size bound: {path.name}")
                declared_total += member.size
                if declared_total > MAX_UNCOMPRESSED_BYTES:
                    raise CollectionError(f"release archive declared size exceeds the allowed bound: {path.name}")
                name = member.name
                while name.startswith("./"):
                    name = name[2:]
                if not name or "/" in name or "\\" in name or name in names:
                    raise CollectionError(f"release archive contains an invalid member: {path.name}")
                names.add(name)
    except CollectionError:
        raise
    except (tarfile.TarError, OSError) as exc:
        raise CollectionError(f"invalid release archive {path.name}: {exc}") from exc
    if names != _expected_archive_members(platform):
        raise CollectionError(f"unexpected archive members for {platform}: {sorted(names)}")


def _parse_sha256sums(text: str, expected_names: set[str]) -> dict[str, str]:
    values: dict[str, str] = {}
    for raw in text.splitlines():
        if not raw:
            continue
        match = re.fullmatch(r"([0-9a-f]{64})  ([^\s]+)", raw)
        if match is None:
            raise CollectionError(f"invalid SHA256SUMS line: {raw!r}")
        digest, name = match.groups()
        if name in values:
            raise CollectionError(f"duplicate SHA256SUMS entry: {name}")
        values[name] = digest
    if set(values) != expected_names:
        raise CollectionError("SHA256SUMS does not describe exactly the downloadable release artifacts")
    return values


def _validate_release_manifest(
    manifest: dict,
    repo: str,
    version: str,
    artifact_files: dict[str, str],
    artifact_hashes: dict[str, str],
) -> None:
    if set(manifest) != {"version", "binaries", "artifacts"}:
        raise CollectionError("release manifest contains unexpected top-level fields")
    if manifest.get("version") != version or manifest.get("binaries") != list(BINARIES):
        raise CollectionError("release manifest version/binary list mismatch")
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, dict) or set(artifacts) != set(PLATFORMS):
        raise CollectionError("release manifest does not contain exactly the six platforms")
    for platform in PLATFORMS:
        item = artifacts.get(platform)
        if not isinstance(item, dict) or set(item) != {"url", "sha256"}:
            raise CollectionError(f"release manifest entry is malformed: {platform}")
        filename = artifact_files[platform]
        expected_url = f"https://github.com/{repo}/releases/download/v{version}/{filename}"
        if item.get("url") != expected_url or item.get("sha256") != artifact_hashes[platform]:
            raise CollectionError(f"release manifest does not match assembled archive: {platform}")


def verify_bundle_directory(
    root: Path,
    *,
    repo: str,
    run_id: int,
    expected_source_sha: str,
    expected_tag: str,
    artifact_name: str,
) -> dict:
    if not root.is_dir():
        raise CollectionError("assembled bundle directory is missing")
    try:
        initial_children = list(root.iterdir())
    except OSError as exc:
        raise CollectionError("cannot list assembled bundle directory") from exc
    for child in initial_children:
        if child.is_symlink() or not child.is_file():
            raise CollectionError(f"assembled bundle contains a non-regular entry: {child.name}")

    release_build = _read_json(root / "release-build.json", MAX_RELEASE_BUILD_BYTES)
    required_fields = {
        "tag",
        "version",
        "source_sha",
        "built_at",
        "build_kind",
        "workflow_run_id",
        "archive_stem",
        "artifacts",
        "desktop_artifacts",
    }
    if set(release_build) != required_fields:
        raise CollectionError("release-build.json contains unexpected or missing fields")
    if release_build.get("tag") != expected_tag:
        raise CollectionError("release-build.json tag mismatch")
    if release_build.get("source_sha") != expected_source_sha:
        raise CollectionError("release-build.json source SHA mismatch")
    if release_build.get("workflow_run_id") != run_id:
        raise CollectionError("release-build.json workflow run id mismatch")
    built_at = release_build.get("built_at")
    if not isinstance(built_at, int) or built_at <= 0:
        raise CollectionError("release-build.json built_at is invalid")
    version = release_build.get("version")
    if not isinstance(version, str) or not VERSION_RE.fullmatch(version):
        raise CollectionError("release-build.json version is invalid")
    build_kind = release_build.get("build_kind")
    expected_kind = "release" if RELEASE_TAG_RE.fullmatch(expected_tag) else "verification"
    if build_kind != expected_kind:
        raise CollectionError("release-build.json build kind does not match the requested tag")
    archive_stem = release_build.get("archive_stem")
    expected_stem = (
        f"webcodex-v{version}"
        if build_kind == "release"
        else f"webcodex-{expected_tag}-{expected_source_sha[:12]}-v{version}"
    )
    if archive_stem != expected_stem or artifact_name != f"{expected_stem}-bundle":
        raise CollectionError("assembled bundle artifact name/archive stem mismatch")

    artifacts = release_build.get("artifacts")
    if not isinstance(artifacts, dict) or set(artifacts) != set(PLATFORMS):
        raise CollectionError("release-build.json must contain exactly the six platforms")
    artifact_files: dict[str, str] = {}
    artifact_hashes: dict[str, str] = {}
    for platform in PLATFORMS:
        item = artifacts.get(platform)
        if not isinstance(item, dict) or set(item) != {"filename", "sha256"}:
            raise CollectionError(f"release-build.json artifact entry is malformed: {platform}")
        filename = item.get("filename")
        digest = item.get("sha256")
        expected_filename = f"{archive_stem}-{platform}.tar.gz"
        if filename != expected_filename:
            raise CollectionError(f"release-build.json filename mismatch: {platform}")
        if not isinstance(digest, str) or not SHA256_RE.fullmatch(digest):
            raise CollectionError(f"release-build.json SHA-256 is invalid: {platform}")
        artifact_files[platform] = filename
        artifact_hashes[platform] = digest

    desktop_artifacts = release_build.get("desktop_artifacts")
    if not isinstance(desktop_artifacts, dict) or set(desktop_artifacts) != set(DESKTOP_PLATFORMS):
        raise CollectionError("release-build.json must contain exactly the supported Desktop artifacts")
    desktop_files: dict[str, str] = {}
    desktop_hashes: dict[str, str] = {}
    for platform in DESKTOP_PLATFORMS:
        item = desktop_artifacts.get(platform)
        if not isinstance(item, dict) or set(item) != {"filename", "sha256"}:
            raise CollectionError(f"release-build.json Desktop artifact entry is malformed: {platform}")
        filename = item.get("filename")
        digest = item.get("sha256")
        expected_filename = desktop_artifact_filename(
            version,
            platform,
            build_kind=build_kind,
            tag=expected_tag,
            source_sha=expected_source_sha,
        )
        if filename != expected_filename:
            raise CollectionError(f"release-build.json Desktop filename mismatch: {platform}")
        if not isinstance(digest, str) or not SHA256_RE.fullmatch(digest):
            raise CollectionError(f"release-build.json Desktop SHA-256 is invalid: {platform}")
        desktop_files[platform] = filename
        desktop_hashes[platform] = digest

    expected_files = {
        "release-build.json",
        "SHA256SUMS",
        "linux-x64-elf.txt",
        "linux-arm64-elf.txt",
        *artifact_files.values(),
        *desktop_files.values(),
    }
    if build_kind == "release":
        expected_files.add("manifest.json")
    try:
        actual_files = {child.name for child in root.iterdir()}
    except OSError as exc:
        raise CollectionError("cannot list assembled bundle directory") from exc
    if actual_files != expected_files:
        raise CollectionError(
            f"assembled bundle file set mismatch: expected={sorted(expected_files)} actual={sorted(actual_files)}"
        )

    try:
        sums_text = _read_bounded(root / "SHA256SUMS", 16 * 1024).decode("ascii")
    except UnicodeDecodeError as exc:
        raise CollectionError("SHA256SUMS is not ASCII") from exc
    sums = _parse_sha256sums(sums_text, set(artifact_files.values()) | set(desktop_files.values()))
    for platform in PLATFORMS:
        filename = artifact_files[platform]
        path = root / filename
        actual = sha256_file(path)
        if actual != artifact_hashes[platform] or sums.get(filename) != actual:
            raise CollectionError(f"assembled archive SHA-256 mismatch: {platform}")
        _verify_archive_members(path, platform)

    for platform in DESKTOP_PLATFORMS:
        filename = desktop_files[platform]
        path = root / filename
        try:
            size = path.stat().st_size
        except OSError as exc:
            raise CollectionError(f"Desktop distribution artifact is missing: {filename}") from exc
        if size <= 0 or size > MAX_MEMBER_BYTES:
            raise CollectionError(f"Desktop distribution artifact is outside its size bound: {filename}")
        actual = sha256_file(path)
        if actual != desktop_hashes[platform] or sums.get(filename) != actual:
            raise CollectionError(f"Desktop distribution artifact SHA-256 mismatch: {platform}")

    for report in ("linux-x64-elf.txt", "linux-arm64-elf.txt"):
        try:
            size = (root / report).stat().st_size
        except OSError as exc:
            raise CollectionError(f"Linux ELF evidence is missing: {report}") from exc
        if size <= 0 or size > MAX_REPORT_BYTES:
            raise CollectionError(f"Linux ELF evidence is missing or too large: {report}")

    if build_kind == "release":
        manifest = _read_json(root / "manifest.json", MAX_MANIFEST_BYTES)
        _validate_release_manifest(manifest, repo, version, artifact_files, artifact_hashes)

    return {
        "tag": expected_tag,
        "version": version,
        "source_sha": expected_source_sha,
        "built_at": built_at,
        "build_kind": build_kind,
        "workflow_run_id": run_id,
        "archive_stem": archive_stem,
        "artifacts": artifact_hashes,
        "desktop_artifacts": {
            platform: {"filename": desktop_files[platform], "sha256": desktop_hashes[platform]}
            for platform in DESKTOP_PLATFORMS
        },
    }


def collect_bundle(
    *,
    repo: str,
    run_id: int,
    expected_source_sha: str,
    expected_tag: str,
    output_dir: Path,
    timeout: float,
    token: str | None = None,
) -> dict:
    if run_id <= 0:
        raise CollectionError("run id must be positive")
    source_sha = normalize_source_sha(expected_source_sha)
    tag = validate_expected_tag(expected_tag)
    destination = output_dir.absolute()
    if destination.exists() or destination.is_symlink():
        raise CollectionError(f"output directory already exists: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)

    client = GitHubClient(repo, token or resolve_github_token(), timeout)
    run = client.fetch_json(f"/actions/runs/{run_id}")
    validate_run(run, run_id, source_sha)
    artifacts_payload = client.fetch_json(f"/actions/runs/{run_id}/artifacts?per_page=100")
    artifact = select_bundle_artifact(artifacts_payload, run_id, source_sha)

    temp_root = Path(tempfile.mkdtemp(prefix=".webcodex-release-collect-", dir=destination.parent))
    zip_path = temp_root / "bundle.zip"
    extracted = temp_root / "bundle"
    try:
        client.download_artifact_zip(
            artifact["id"],
            zip_path,
            artifact["size_in_bytes"],
            artifact["sha256"],
        )
        safe_extract_zip(zip_path, extracted)
        summary = verify_bundle_directory(
            extracted,
            repo=repo,
            run_id=run_id,
            expected_source_sha=source_sha,
            expected_tag=tag,
            artifact_name=artifact["name"],
        )
        os.replace(extracted, destination)
    finally:
        shutil.rmtree(temp_root, ignore_errors=True)

    return {
        **summary,
        "repo": repo,
        "bundle_artifact_id": artifact["id"],
        "bundle_artifact_name": artifact["name"],
        "bundle_artifact_sha256": artifact["sha256"],
        "output_dir": str(destination),
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Collect one same-run assembled WebCodex release bundle through the GitHub REST API."
    )
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--repo", default=DEFAULT_REPO)
    parser.add_argument("--timeout", type=float, default=120.0)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        summary = collect_bundle(
            repo=args.repo,
            run_id=args.run_id,
            expected_source_sha=args.source_sha,
            expected_tag=args.tag,
            output_dir=args.output_dir,
            timeout=args.timeout,
        )
    except CollectionError as exc:
        print(f"release bundle collection failed: {exc}", file=os.sys.stderr)
        return 1
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
