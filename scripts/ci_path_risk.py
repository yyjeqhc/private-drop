#!/usr/bin/env python3
"""Deterministic CI native-risk classification for a Git base...head range."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import PurePosixPath


MAX_CHANGED_PATHS = 2048
MAX_PATH_BYTES = 4096
MAX_NAME_STATUS_BYTES = 1024 * 1024
MAX_PLATFORM_CONTEXT_BYTES = 2 * 1024 * 1024
MAX_GIT_PATH_BATCH_BYTES = 64 * 1024
MAX_GIT_STDERR_BYTES = 64 * 1024

SHA_RE = re.compile(r"^[0-9a-fA-F]{40}$")
PLATFORM_CONTEXT_GREP_RE = r"windows|macos|aarch64|target_os|target_family|target_arch"
WINDOWS_CFG_RE = re.compile(
    r"(?:\bcfg!?\s*\([^\n)]*\bwindows\b|\btarget_(?:os|family)\s*=\s*\"windows\")"
)
MACOS_CFG_RE = re.compile(
    r"(?:\bcfg!?\s*\([^\n)]*\bmacos\b|\btarget_os\s*=\s*\"macos\")"
)
AARCH64_CFG_RE = re.compile(r"\btarget_arch\s*=\s*\"aarch64\"")


class GitDiffError(RuntimeError):
    pass


class DiffLimitExceeded(RuntimeError):
    pass


@dataclass(frozen=True)
class Change:
    status: str
    path: str


@dataclass
class Risk:
    needs_windows_core: bool = False
    needs_windows_runner: bool = False
    needs_windows_package: bool = False
    needs_windows_desktop: bool = False
    needs_windows_arm64: bool = False
    needs_macos: bool = False
    needs_macos_desktop: bool = False
    needs_linux_arm64: bool = False
    needs_full_native: bool = False
    categories: set[str] = field(default_factory=set)
    changed_count: int = 0

    @classmethod
    def full(cls, category: str, *, changed_count: int = 0) -> "Risk":
        risk = cls(needs_full_native=True, changed_count=changed_count)
        risk.categories.add(category)
        return risk.finalize()

    def finalize(self) -> "Risk":
        if self.needs_full_native:
            self.needs_windows_core = True
            self.needs_windows_runner = True
            self.needs_windows_package = True
            self.needs_windows_desktop = True
            self.needs_windows_arm64 = True
            self.needs_macos = True
            self.needs_macos_desktop = True
            self.needs_linux_arm64 = True
        return self

    def outputs(self) -> dict[str, str]:
        self.finalize()
        needs_windows = any(
            (
                self.needs_windows_core,
                self.needs_windows_runner,
                self.needs_windows_package,
                self.needs_windows_desktop,
                self.needs_windows_arm64,
            )
        )
        needs_desktop_package = self.needs_windows_desktop or self.needs_macos_desktop
        categories = ",".join(sorted(self.categories)) or "no-changes"
        reason_prefix = "full-native" if self.needs_full_native else "path-risk"
        return {
            "needs_windows": _bool(needs_windows),
            "needs_windows_core": _bool(self.needs_windows_core),
            "needs_windows_runner": _bool(self.needs_windows_runner),
            "needs_windows_package": _bool(self.needs_windows_package),
            "needs_windows_desktop": _bool(self.needs_windows_desktop),
            "needs_windows_arm64": _bool(self.needs_windows_arm64),
            "needs_macos": _bool(self.needs_macos),
            "needs_macos_desktop": _bool(self.needs_macos_desktop),
            "needs_linux_arm64": _bool(self.needs_linux_arm64),
            "needs_desktop_package": _bool(needs_desktop_package),
            "needs_full_native": _bool(self.needs_full_native),
            "categories": categories,
            "reason": f"{reason_prefix}:{categories}",
            "changed_count": str(self.changed_count),
        }


def _bool(value: bool) -> str:
    return "true" if value else "false"


def _mark_windows_core(risk: Risk, category: str) -> None:
    risk.needs_windows_core = True
    risk.categories.add(category)


def _mark_windows_runner(risk: Risk, category: str) -> None:
    risk.needs_windows_runner = True
    risk.categories.add(category)


def _mark_windows_package(risk: Risk, category: str) -> None:
    risk.needs_windows_package = True
    risk.categories.add(category)


def _mark_windows_desktop(risk: Risk, category: str) -> None:
    risk.needs_windows_desktop = True
    risk.categories.add(category)


def _mark_macos(risk: Risk, category: str, *, desktop: bool = False) -> None:
    risk.needs_macos = True
    if desktop:
        risk.needs_macos_desktop = True
    risk.categories.add(category)


def _mark_platform_context_bounded(risk: Risk) -> None:
    """Fail closed on platform semantics without pulling in unrelated packaging."""
    category = "platform-context-bounded"
    _mark_windows_core(risk, category)
    _mark_macos(risk, category)
    risk.needs_linux_arm64 = True
    risk.needs_windows_arm64 = True


def _is_docs_or_text(path: str) -> bool:
    return path.startswith("docs/") or path in {"README.md", "CHANGELOG.md", "LICENSE"}


def _is_frontend_only(path: str) -> bool:
    return path.startswith("frontend/") or path.startswith("apps/desktop/src/") or path in {
        "apps/desktop/index.html",
        "apps/desktop/tsconfig.json",
        "apps/desktop/vite.config.ts",
    }


def _release_tooling(path: str) -> bool:
    if path.startswith(".github/workflows/release-"):
        return True
    if not path.startswith("scripts/"):
        return False
    name = PurePosixPath(path).name
    return (
        name.startswith("release_")
        or name == "release_check.sh"
        or name.startswith("package_release_artifact.")
        or name
        in {
            "collect_release_bundle.py",
            "prepare_release_metadata.py",
            "stage_npm_release.sh",
            "verify_public_release.py",
            "macos_sign_local_runner.sh",
        }
    )


def _classify_path(risk: Risk, path: str) -> None:
    lower = path.lower()
    name = PurePosixPath(lower).name
    tokens = {token for token in re.split(r"[/_.-]+", lower) if token}

    if _is_docs_or_text(path):
        risk.categories.add("docs")
        return
    if _is_frontend_only(path):
        risk.categories.add("frontend")
        return

    if path.startswith(".github/workflows/") or path == "scripts/ci_path_risk.py":
        risk.needs_full_native = True
        risk.categories.add("ci-policy")
        return
    if _release_tooling(path) or (path.startswith("scripts/") and "sign" in name):
        risk.needs_full_native = True
        risk.categories.add("release-signing")
        return
    if path in {"Cargo.toml", "Cargo.lock"} or path.startswith(".cargo/"):
        risk.needs_full_native = True
        risk.categories.add("workspace-dependencies")
        return
    if name == "build.rs":
        risk.needs_full_native = True
        risk.categories.add("native-build-script")
        return

    if path.startswith("apps/desktop/src-tauri/"):
        _mark_windows_desktop(risk, "desktop-native")
        _mark_macos(risk, "desktop-native", desktop=True)
        return
    if path in {"apps/desktop/package.json", "apps/desktop/package-lock.json"}:
        _mark_windows_desktop(risk, "desktop-package")
        _mark_macos(risk, "desktop-package", desktop=True)
        return

    # The npm installer/wrapper contains real Windows-specific process/path
    # behavior. Linux tooling exercises its portable contract, but production
    # package changes also need the native Windows package lane that runs the
    # artifact-to-install smoke.
    if path.startswith("npm/webcodex/"):
        _mark_windows_package(risk, "npm-package")
        return

    if path in {
        "scripts/prepare_desktop_bundle.ps1",
        "scripts/desktop_install_windows_smoke.ps1",
    }:
        _mark_windows_desktop(risk, "windows-desktop-package")
        if path == "scripts/desktop_install_windows_smoke.ps1":
            _mark_windows_package(risk, "windows-package")
        return
    if path == "scripts/npm_install_windows_smoke.ps1":
        _mark_windows_package(risk, "windows-package")
        return
    if path in {
        "scripts/prepare_desktop_bundle_macos.py",
        "scripts/desktop_install_macos_smoke.sh",
    }:
        _mark_macos(risk, "macos-desktop-package", desktop=True)
        return

    signing_tokens = {"sign", "signing", "codesign", "notarize", "notarization"}
    windows_package_tokens = {"windows", "win32", "win64", "msi", "nsis", "wix", "wxs"}
    macos_package_tokens = {"macos", "darwin", "osx", "dmg"}
    packaging_tokens = {"installer", "packaging", "bundle", "bundles", "msi", "nsis", "wix", "wxs", "dmg"}
    suffix = PurePosixPath(lower).suffix
    if signing_tokens & tokens:
        risk.needs_full_native = True
        risk.categories.add("release-signing")
        return
    if suffix in {".msi", ".wxs", ".wixproj"} or (
        packaging_tokens & tokens and windows_package_tokens & tokens
    ):
        _mark_windows_package(risk, "windows-package")
        _mark_windows_desktop(risk, "windows-desktop-package")
        return
    if suffix in {".dmg", ".pkg"} or (
        packaging_tokens & tokens and macos_package_tokens & tokens
    ):
        _mark_macos(risk, "macos-desktop-package", desktop=True)
        return
    if packaging_tokens & tokens:
        risk.needs_full_native = True
        risk.categories.add("release-packaging")
        return

    if path.startswith("crates/webcodex-process/"):
        _mark_windows_core(risk, "native-process")
        _mark_macos(risk, "native-process")
        return
    if path.startswith("crates/webcodex-persistent-shell/"):
        _mark_windows_core(risk, "persistent-shell")
        _mark_macos(risk, "persistent-shell")
        return
    if path.startswith("crates/webcodex-computer/"):
        _mark_windows_runner(risk, "computer-runtime")
        _mark_macos(risk, "computer-runtime")
        return
    if path.startswith("crates/webcodex-runner/"):
        runner_native_tokens = (
            "plugin",
            "shell",
            "process",
            "transport",
            "ssh",
            "computer",
            "shutdown",
            "detached_job",
            "supervisor",
            "coding_agent",
            "external_tools",
            "mcp_gateway",
            "projects",
            "validation",
        )
        if any(token in lower for token in runner_native_tokens):
            _mark_windows_runner(risk, "runner-native")
            _mark_macos(risk, "runner-native")
            return

    # Platform-specific source outside the known ownership crates is still native
    # risk. Rust platform files gate both major desktop OSes so counterpart cfg
    # drift is caught rather than only compiling the named platform.
    if path.endswith(".rs") and ({"windows", "macos", "darwin"} & tokens):
        _mark_windows_core(risk, "platform-source")
        _mark_macos(risk, "platform-source")
        return
    if path.startswith("scripts/") and "windows" in tokens:
        _mark_windows_runner(risk, "windows-runner-script")
        return
    if path.startswith("scripts/") and ({"macos", "darwin"} & tokens):
        _mark_macos(risk, "macos-script")
        return
    if {"arm64", "aarch64"} & tokens:
        risk.needs_linux_arm64 = True
        risk.categories.add("arm64-target")
        return

    risk.categories.add("normal-linux")


def classify_changes(changes: list[Change], platform_diff: str = "") -> Risk:
    risk = Risk(changed_count=len(changes))
    for change in changes:
        _validate_path(change.path)
        _classify_path(risk, change.path)

    # `platform_diff` contains only platform-marker lines observed from the base and
    # head versions of changed Rust/Cargo files. Looking at both revisions keeps a
    # body-only edit inside an existing `#[cfg(windows)]` block or target-specific
    # Cargo section visible even when the marker line itself did not change.
    if WINDOWS_CFG_RE.search(platform_diff) or MACOS_CFG_RE.search(platform_diff):
        _mark_windows_core(risk, "platform-cfg")
        _mark_macos(risk, "platform-cfg")
    if AARCH64_CFG_RE.search(platform_diff):
        risk.needs_linux_arm64 = True
        risk.needs_windows_arm64 = True
        _mark_macos(risk, "aarch64-cfg")
        risk.categories.add("aarch64-cfg")

    return risk.finalize()


def invocation_override_reason(
    event_name: str, *, external_contributor: bool, run_ci: bool
) -> str | None:
    if event_name == "push":
        return "override-push-main"
    if external_contributor:
        return "override-external-contributor"
    if run_ci:
        return "override-run-ci"
    return None


def forced_risk_for_invocation(
    event_name: str, *, external_contributor: bool, run_ci: bool
) -> Risk | None:
    reason = invocation_override_reason(
        event_name, external_contributor=external_contributor, run_ci=run_ci
    )
    return Risk.full(reason) if reason else None


def _validate_path(path: str) -> None:
    encoded = path.encode("utf-8")
    if not path or len(encoded) > MAX_PATH_BYTES:
        raise DiffLimitExceeded("changed path exceeds classifier bounds")
    pure = PurePosixPath(path)
    if pure.is_absolute() or ".." in pure.parts or "\x00" in path:
        raise DiffLimitExceeded("changed path is not a bounded repository-relative path")


def _run_git_bounded(
    args: list[str],
    *,
    max_stdout_bytes: int,
    accepted_return_codes: tuple[int, ...] = (0,),
) -> bytes:
    with subprocess.Popen(
        ["git", *args],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    ) as process:
        assert process.stdout is not None
        stdout = process.stdout.read(max_stdout_bytes + 1)
        if len(stdout) > max_stdout_bytes:
            process.kill()
            process.wait()
            raise DiffLimitExceeded("git output exceeds classifier bounds")
        return_code = process.wait()
        if return_code not in accepted_return_codes:
            message = stdout[:MAX_GIT_STDERR_BYTES].decode("utf-8", errors="replace").strip()
            raise GitDiffError(message or f"git exited with status {return_code}")
        return stdout


def _git_changes(base: str, head: str) -> list[Change]:
    raw = _run_git_bounded(
        [
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            f"{base}...{head}",
        ],
        max_stdout_bytes=MAX_NAME_STATUS_BYTES,
    )
    fields = raw.split(b"\x00")
    if fields and fields[-1] == b"":
        fields.pop()
    if len(fields) % 2 != 0:
        raise DiffLimitExceeded("git returned malformed zero-delimited name-status data")
    changes: list[Change] = []
    for status_raw, path_raw in zip(fields[0::2], fields[1::2]):
        try:
            status = status_raw.decode("ascii", errors="strict")
            path = path_raw.decode("utf-8", errors="strict")
        except UnicodeDecodeError as exc:
            raise DiffLimitExceeded("git returned an unsupported changed path") from exc
        if not status:
            raise DiffLimitExceeded("git returned an empty change status")
        _validate_path(path)
        changes.append(Change(status=status[:1], path=path))
        if len(changes) > MAX_CHANGED_PATHS:
            raise DiffLimitExceeded("changed path count exceeds classifier bounds")
    return changes


def _platform_context_paths(changes: list[Change], *, excluded_status: str) -> list[str]:
    return sorted(
        {
            change.path
            for change in changes
            if change.status != excluded_status
            and (change.path.endswith(".rs") or PurePosixPath(change.path).name == "Cargo.toml")
            and not _is_docs_or_text(change.path)
            and not _is_frontend_only(change.path)
        }
    )


def _path_batches(paths: list[str]) -> list[list[str]]:
    batches: list[list[str]] = []
    batch: list[str] = []
    batch_bytes = 0
    for path in paths:
        path_bytes = len(path.encode("utf-8")) + 1
        if batch and batch_bytes + path_bytes > MAX_GIT_PATH_BATCH_BYTES:
            batches.append(batch)
            batch = []
            batch_bytes = 0
        batch.append(path)
        batch_bytes += path_bytes
    if batch:
        batches.append(batch)
    return batches


def _git_platform_context(base: str, head: str, changes: list[Change]) -> str:
    chunks: list[bytes] = []
    remaining = MAX_PLATFORM_CONTEXT_BYTES
    for revision, excluded_status in ((base, "A"), (head, "D")):
        paths = _platform_context_paths(changes, excluded_status=excluded_status)
        for batch in _path_batches(paths):
            raw = _run_git_bounded(
                [
                    "grep",
                    "-h",
                    "-I",
                    "-E",
                    "-e",
                    PLATFORM_CONTEXT_GREP_RE,
                    revision,
                    "--",
                    *batch,
                ],
                max_stdout_bytes=remaining,
                # git grep uses 1 for a valid search with no matches.
                accepted_return_codes=(0, 1),
            )
            chunks.append(raw)
            remaining -= len(raw)
    return b"\n".join(chunks).decode("utf-8", errors="replace")


def classify_git_range(base: str, head: str) -> Risk:
    if not SHA_RE.fullmatch(base) or not SHA_RE.fullmatch(head):
        raise GitDiffError("base and head must be exact 40-hex Git commit ids")
    try:
        changes = _git_changes(base, head)
    except DiffLimitExceeded:
        # If even the changed-path inventory is outside the trusted bound, there is
        # no safe way to know which product surfaces changed.
        return Risk.full("changed-path-bounded-fallback")

    needs_platform_context = any(
        change.path.endswith(".rs") or PurePosixPath(change.path).name == "Cargo.toml"
        for change in changes
    )
    if not needs_platform_context:
        return classify_changes(changes)

    try:
        platform_context = _git_platform_context(base, head, changes)
    except DiffLimitExceeded:
        # We still know the exact changed paths, so preserve their package/desktop
        # classification and fail closed only for unknown native cfg/architecture
        # semantics. Packaging lanes are not a substitute for platform compilation.
        risk = classify_changes(changes)
        _mark_platform_context_bounded(risk)
        return risk.finalize()
    return classify_changes(changes, platform_context)


def _parse_bool(value: str) -> bool:
    normalized = value.strip().lower()
    if normalized == "true":
        return True
    if normalized == "false":
        return False
    raise argparse.ArgumentTypeError("expected true or false")


def _write_github_output(path: str, outputs: dict[str, str]) -> None:
    with open(path, "a", encoding="utf-8") as handle:
        for key, value in outputs.items():
            handle.write(f"{key}={value}\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", required=True, choices=("pull_request", "push"))
    parser.add_argument("--external-contributor", type=_parse_bool, required=True)
    parser.add_argument("--run-ci", type=_parse_bool, required=True)
    parser.add_argument("--base")
    parser.add_argument("--head")
    parser.add_argument("--github-output")
    args = parser.parse_args(argv)

    risk = forced_risk_for_invocation(
        args.event_name,
        external_contributor=args.external_contributor,
        run_ci=args.run_ci,
    )
    if risk is None:
        if not args.base or not args.head:
            parser.error("--base and --head are required for path-aware pull requests")
        try:
            risk = classify_git_range(args.base, args.head)
        except GitDiffError as exc:
            print(f"ci path risk classification failed: {exc}", file=sys.stderr)
            return 2

    outputs = risk.outputs()
    if args.github_output:
        _write_github_output(args.github_output, outputs)
    print(json.dumps(outputs, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
