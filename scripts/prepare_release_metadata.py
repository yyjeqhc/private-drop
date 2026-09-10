#!/usr/bin/env python3
"""Validate native WebCodex release archives and generate publish metadata."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PLATFORMS = ("linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "win32-x64", "win32-arm64")
DESKTOP_PLATFORMS = ("darwin-x64", "darwin-arm64", "win32-x64", "win32-arm64")
BINARIES = ("webcodex", "webcodex-server", "webcodex-runner")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def archive_filename(version: str, platform: str) -> str:
    return f"webcodex-v{version}-{platform}.tar.gz"


def desktop_filename(version: str, platform: str) -> str:
    if platform not in DESKTOP_PLATFORMS:
        raise SystemExit(f"unsupported Desktop platform: {platform}")
    suffix = "-setup.exe" if platform.startswith("win32-") else ".dmg"
    return f"webcodex-desktop-v{version}-{platform}{suffix}"


def expected_members(platform: str) -> set[str]:
    suffix = ".exe" if platform in {"win32-x64", "win32-arm64"} else ""
    return {f"{name}{suffix}" for name in BINARIES}


def normalized_members(path: Path) -> set[str]:
    members: set[str] = set()
    with tarfile.open(path, "r:gz") as archive:
        for member in archive.getmembers():
            if member.isdir():
                continue
            if not member.isfile():
                raise SystemExit(f"archive contains non-file member: {path.name}: {member.name}")
            name = member.name
            while name.startswith("./"):
                name = name[2:]
            if not name or "/" in name or "\\" in name:
                raise SystemExit(f"archive contains nested or invalid path: {path.name}: {member.name}")
            if name in members:
                raise SystemExit(f"archive contains duplicate member: {path.name}: {name}")
            members.add(name)
    return members


def atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(content, encoding="utf-8", newline="\n")
    os.replace(tmp, path)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--artifact-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--repo", default="yyjeqhc/webcodex")
    parser.add_argument(
        "--package-json",
        type=Path,
        default=ROOT / "npm" / "webcodex" / "package.json",
    )
    args = parser.parse_args()

    version = args.version.removeprefix("v")
    if not version or any(ch.isspace() for ch in version):
        raise SystemExit("invalid release version")

    package = json.loads(args.package_json.read_text(encoding="utf-8"))
    if package.get("version") != version:
        raise SystemExit(
            f"package version mismatch: expected {version}, found {package.get('version')!r}"
        )

    artifacts: dict[str, dict[str, str]] = {}
    checksum_lines: list[str] = []

    for platform in PLATFORMS:
        filename = archive_filename(version, platform)
        path = args.artifact_dir / filename
        if not path.is_file() or path.stat().st_size <= 0:
            raise SystemExit(f"missing or empty artifact: {path}")

        actual = normalized_members(path)
        expected = expected_members(platform)
        if actual != expected:
            raise SystemExit(
                f"unexpected archive contents for {platform}: "
                f"expected={sorted(expected)} actual={sorted(actual)}"
            )

        digest = sha256(path)
        checksum_lines.append(f"{digest}  {filename}")
        artifacts[platform] = {
            "url": f"https://github.com/{args.repo}/releases/download/v{version}/{filename}",
            "sha256": digest,
        }

    for platform in DESKTOP_PLATFORMS:
        desktop_name = desktop_filename(version, platform)
        desktop_path = args.artifact_dir / desktop_name
        if not desktop_path.is_file() or desktop_path.stat().st_size <= 0:
            raise SystemExit(f"missing or empty Desktop distribution artifact: {desktop_path}")
        desktop_digest = sha256(desktop_path)
        checksum_lines.append(f"{desktop_digest}  {desktop_name}")

    manifest = {
        "version": version,
        "binaries": list(BINARIES),
        "artifacts": artifacts,
    }

    atomic_write(args.output_dir / "manifest.json", json.dumps(manifest, indent=2) + "\n")
    atomic_write(args.output_dir / "SHA256SUMS", "\n".join(checksum_lines) + "\n")

    print(f"release metadata prepared for {version}")
    for platform, artifact in artifacts.items():
        print(f"{platform} {artifact['sha256']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
