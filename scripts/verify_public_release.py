#!/usr/bin/env python3
"""Verify one published WebCodex GitHub/npm release without executing foreign binaries."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import struct
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import BinaryIO

REPO = "yyjeqhc/webcodex"
PACKAGE = "@yyjeqhc/webcodex"
PLATFORMS = ("linux-x64", "linux-arm64", "darwin-x64", "darwin-arm64", "win32-x64", "win32-arm64")
BINARIES = ("webcodex", "webcodex-server", "webcodex-runner")
LEGACY_DESKTOP_PLATFORMS = ("darwin-x64", "darwin-arm64", "win32-x64")
DESKTOP_PLATFORMS = (*LEGACY_DESKTOP_PLATFORMS, "win32-arm64")
DESKTOP_FIRST_VERSION = "0.4.0"
WINDOWS_ARM64_DESKTOP_FIRST_VERSION = "0.4.2"
SERVER_IMAGE = "ghcr.io/yyjeqhc/webcodex-server"
SERVER_IMAGE_METADATA = "webcodex-server-image.json"
SERVER_BOOTSTRAP_ASSET = "webcodex-server-bootstrap.sh"
SERVER_MATERIALIZED_COMPOSE = "webcodex-server-compose.yaml"
SERVER_DEPLOYMENT_ASSETS = (SERVER_BOOTSTRAP_ASSET,)
SERVER_IMAGE_PLATFORMS = ("linux/amd64", "linux/arm64")
SERVER_IMAGE_BASE_METADATA_KEYS = frozenset(
    {
        "schema_version",
        "image",
        "tag",
        "version",
        "image_tag",
        "source_sha",
        "created_at",
        "digest",
        "platforms",
    }
)
MAX_JSON_BYTES = 2 * 1024 * 1024
MAX_NPM_TARBALL_BYTES = 16 * 1024 * 1024
MAX_ARTIFACT_BYTES = 128 * 1024 * 1024
MAX_DESKTOP_ARTIFACT_BYTES = 128 * 1024 * 1024
MAX_UNCOMPRESSED_BYTES = 256 * 1024 * 1024
MAX_MEMBER_BYTES = 96 * 1024 * 1024
MAX_DYNAMIC_BYTES = 1024 * 1024
MAX_DYNSTR_BYTES = 4 * 1024 * 1024
MAX_NPM_TAR_MEMBERS = 128
MAX_RELEASE_TAR_MEMBERS = 16
GLIBC_FLOOR = "2.17"
ALLOWED_NEEDED = frozenset(
    {
        "libc.so.6",
        "libm.so.6",
        "libgcc_s.so.1",
        "libpthread.so.0",
        "libdl.so.2",
        "librt.so.1",
        "libresolv.so.2",
        "ld-linux-x86-64.so.2",
        "ld-linux-aarch64.so.1",
    }
)
VERSION_RE = re.compile(
    r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$"
)
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
USER_AGENT = "webcodex-public-release-verifier/1"


class VerificationError(RuntimeError):
    pass


def normalize_version(value: str) -> str:
    version = value.removeprefix("v")
    if not VERSION_RE.fullmatch(version):
        raise VerificationError(f"invalid release version: {value!r}")
    return version


def canonical_archive_name(version: str, platform: str) -> str:
    return f"webcodex-v{version}-{platform}.tar.gz"


def canonical_desktop_name(version: str, platform: str) -> str:
    if platform not in DESKTOP_PLATFORMS:
        raise VerificationError(f"unsupported Desktop platform: {platform!r}")
    suffix = "-setup.exe" if platform.startswith("win32-") else ".dmg"
    return f"webcodex-desktop-v{version}-{platform}{suffix}"


def expected_artifact_url(version: str, platform: str) -> str:
    return (
        f"https://github.com/{REPO}/releases/download/v{version}/"
        f"{canonical_archive_name(version, platform)}"
    )


def expected_desktop_url(version: str, platform: str) -> str:
    return f"https://github.com/{REPO}/releases/download/v{version}/{canonical_desktop_name(version, platform)}"


def _semver_parts(value: str) -> tuple[tuple[int, int, int], tuple[str, ...] | None]:
    normalized = normalize_version(value)
    core_and_pre = normalized.split("+", 1)[0]
    core, separator, prerelease = core_and_pre.partition("-")
    major, minor, patch = (int(part) for part in core.split("."))
    return (major, minor, patch), tuple(prerelease.split(".")) if separator else None


def desktop_required(version: str) -> bool:
    version_core, _ = _semver_parts(version)
    desktop_first_core, _ = _semver_parts(DESKTOP_FIRST_VERSION)
    return version_core >= desktop_first_core


def desktop_platforms_for_version(version: str) -> tuple[str, ...]:
    if not desktop_required(version):
        return ()
    version_core, _ = _semver_parts(version)
    arm64_first_core, _ = _semver_parts(WINDOWS_ARM64_DESKTOP_FIRST_VERSION)
    if version_core >= arm64_first_core:
        return DESKTOP_PLATFORMS
    return LEGACY_DESKTOP_PLATFORMS


def expected_binary_names(platform: str) -> set[str]:
    suffix = ".exe" if platform.startswith("win32-") else ""
    return {f"{name}{suffix}" for name in BINARIES}


def _request(url: str) -> urllib.request.Request:
    return urllib.request.Request(
        url,
        headers={
            "User-Agent": USER_AGENT,
            "Accept": "application/vnd.github+json, application/json;q=0.9, */*;q=0.1",
        },
    )


def _content_length(headers: object) -> int | None:
    value = getattr(headers, "get", lambda _name: None)("Content-Length")
    if value is None:
        return None
    try:
        length = int(value)
    except (TypeError, ValueError) as exc:
        raise VerificationError(f"invalid Content-Length: {value!r}") from exc
    if length < 0:
        raise VerificationError(f"invalid Content-Length: {value!r}")
    return length


def fetch_bytes(url: str, max_bytes: int, timeout: float) -> bytes:
    try:
        with urllib.request.urlopen(_request(url), timeout=timeout) as response:
            length = _content_length(response.headers)
            if length is not None and length > max_bytes:
                raise VerificationError(f"download exceeds {max_bytes} bytes: {url}")
            data = response.read(max_bytes + 1)
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise VerificationError(f"download failed: {url}: {exc}") from exc
    if len(data) > max_bytes:
        raise VerificationError(f"download exceeds {max_bytes} bytes: {url}")
    return data


def download_file(url: str, path: Path, max_bytes: int, timeout: float) -> tuple[int, str]:
    digest = hashlib.sha256()
    written = 0
    try:
        with urllib.request.urlopen(_request(url), timeout=timeout) as response, path.open("xb") as output:
            length = _content_length(response.headers)
            if length is not None and length > max_bytes:
                raise VerificationError(f"download exceeds {max_bytes} bytes: {url}")
            while True:
                chunk = response.read(1024 * 1024)
                if not chunk:
                    break
                written += len(chunk)
                if written > max_bytes:
                    raise VerificationError(f"download exceeds {max_bytes} bytes: {url}")
                output.write(chunk)
                digest.update(chunk)
    except VerificationError:
        path.unlink(missing_ok=True)
        raise
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        path.unlink(missing_ok=True)
        raise VerificationError(f"download failed: {url}: {exc}") from exc
    return written, digest.hexdigest()


def fetch_json(url: str, timeout: float) -> dict:
    try:
        value = json.loads(fetch_bytes(url, MAX_JSON_BYTES, timeout))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise VerificationError(f"invalid JSON from {url}: {exc}") from exc
    if not isinstance(value, dict):
        raise VerificationError(f"expected JSON object from {url}")
    return value


def hash_file(path: Path, algorithm: str) -> str:
    digest = hashlib.new(algorithm)
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_npm_dist(path: Path, dist: dict) -> None:
    integrity = dist.get("integrity")
    if integrity:
        expected = None
        for token in str(integrity).split():
            if token.startswith("sha512-"):
                expected = token.removeprefix("sha512-")
                break
        if expected is None:
            raise VerificationError("npm dist.integrity does not contain sha512")
        actual = base64.b64encode(bytes.fromhex(hash_file(path, "sha512"))).decode("ascii")
        if actual != expected:
            raise VerificationError("npm tarball sha512 integrity mismatch")
    shasum = dist.get("shasum")
    if shasum and hash_file(path, "sha1") != str(shasum).lower():
        raise VerificationError("npm tarball sha1 shasum mismatch")


def read_npm_package_jsons(path: Path) -> tuple[dict, dict]:
    wanted = {"package/package.json", "package/manifest.json"}
    found: dict[str, bytes] = {}
    try:
        with tarfile.open(path, "r:gz") as archive:
            member_count = 0
            for member in archive:
                member_count += 1
                if member_count > MAX_NPM_TAR_MEMBERS:
                    raise VerificationError("npm tarball contains too many members")
                if member.name not in wanted:
                    continue
                if member.name in found:
                    raise VerificationError(f"npm tarball contains duplicate {member.name}")
                if not member.isfile() or member.size > MAX_JSON_BYTES:
                    raise VerificationError(f"invalid npm tarball member: {member.name}")
                source = archive.extractfile(member)
                if source is None:
                    raise VerificationError(f"cannot read npm tarball member: {member.name}")
                data = source.read(MAX_JSON_BYTES + 1)
                if len(data) > MAX_JSON_BYTES:
                    raise VerificationError(f"npm tarball member is too large: {member.name}")
                found[member.name] = data
    except (tarfile.TarError, OSError) as exc:
        raise VerificationError(f"invalid npm tarball: {exc}") from exc
    if set(found) != wanted:
        raise VerificationError(f"npm tarball is missing release metadata: {sorted(wanted - set(found))}")
    try:
        package = json.loads(found["package/package.json"])
        manifest = json.loads(found["package/manifest.json"])
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise VerificationError(f"invalid npm package JSON: {exc}") from exc
    if not isinstance(package, dict) or not isinstance(manifest, dict):
        raise VerificationError("npm package metadata must be JSON objects")
    return package, manifest


def validate_public_manifest(manifest: dict, version: str) -> dict[str, dict[str, str]]:
    if manifest.get("version") != version:
        raise VerificationError("npm release manifest version mismatch")
    if manifest.get("binaries") != list(BINARIES):
        raise VerificationError("npm release manifest binaries are not canonical")
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, dict) or set(artifacts) != set(PLATFORMS):
        raise VerificationError("npm release manifest must contain exactly the six release platforms")
    result: dict[str, dict[str, str]] = {}
    for platform in PLATFORMS:
        artifact = artifacts.get(platform)
        if not isinstance(artifact, dict):
            raise VerificationError(f"invalid manifest entry for {platform}")
        url = artifact.get("url")
        digest = artifact.get("sha256")
        if url != expected_artifact_url(version, platform):
            raise VerificationError(f"unexpected manifest URL for {platform}: {url!r}")
        if not isinstance(digest, str) or not SHA256_RE.fullmatch(digest):
            raise VerificationError(f"invalid manifest SHA-256 for {platform}")
        result[platform] = {"url": url, "sha256": digest}
    return result


def parse_sha256sums(text: str, version: str) -> dict[str, str]:
    expected_names = {canonical_archive_name(version, platform) for platform in PLATFORMS}
    expected_names.update(
        canonical_desktop_name(version, platform) for platform in desktop_platforms_for_version(version)
    )
    result: dict[str, str] = {}
    for raw_line in text.splitlines():
        if not raw_line:
            continue
        match = re.fullmatch(r"([0-9a-f]{64})  ([^\s]+)", raw_line)
        if match is None:
            raise VerificationError(f"invalid SHA256SUMS line: {raw_line!r}")
        digest, name = match.groups()
        if name in result:
            raise VerificationError(f"duplicate SHA256SUMS entry: {name}")
        result[name] = digest
    if set(result) != expected_names:
        raise VerificationError(
            f"SHA256SUMS has an unexpected downloadable release artifact set: {sorted(result)}"
        )
    return result


def validate_server_image_metadata(value: dict, version: str) -> dict[str, str]:
    expected_keys = {
        "schema_version",
        "image",
        "tag",
        "version",
        "image_tag",
        "deployment_assets",
        "deployment_source_sha",
        "source_sha",
        "created_at",
        "digest",
        "platforms",
    }
    if set(value) != expected_keys or value.get("schema_version") != 1:
        raise VerificationError("server image metadata has an unexpected schema")
    if value.get("image") != SERVER_IMAGE or value.get("tag") != f"v{version}" or value.get("version") != version:
        raise VerificationError("server image metadata does not match the release identity")
    if value.get("image_tag") != f"v{version.replace('+', '_')}":
        raise VerificationError("server image metadata has an invalid container version tag")
    deployment_assets = value.get("deployment_assets")
    if not isinstance(deployment_assets, dict) or set(deployment_assets) != set(SERVER_DEPLOYMENT_ASSETS):
        raise VerificationError("server image metadata does not contain the canonical deployment assets")
    for name, digest in deployment_assets.items():
        if not isinstance(digest, str) or not SHA256_RE.fullmatch(digest):
            raise VerificationError(f"server image metadata has an invalid deployment digest for {name}")
    deployment_source_sha = value.get("deployment_source_sha")
    if not isinstance(deployment_source_sha, str) or not re.fullmatch(r"[0-9a-f]{40}", deployment_source_sha):
        raise VerificationError("server image metadata has an invalid deployment source SHA")
    source_sha = value.get("source_sha")
    if not isinstance(source_sha, str) or not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        raise VerificationError("server image metadata has an invalid source SHA")
    created_at = value.get("created_at")
    if not isinstance(created_at, str) or not created_at:
        raise VerificationError("server image metadata has an invalid creation time")
    digest = value.get("digest")
    if not isinstance(digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        raise VerificationError("server image metadata has an invalid manifest digest")
    platforms = value.get("platforms")
    if not isinstance(platforms, dict) or set(platforms) != set(SERVER_IMAGE_PLATFORMS):
        raise VerificationError("server image metadata does not contain the canonical platforms")
    for platform, child in platforms.items():
        if not isinstance(child, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", child):
            raise VerificationError(f"server image metadata has an invalid child digest for {platform}")
    return {
        "source_sha": source_sha,
        "digest": digest,
        "deployment_assets": deployment_assets,
        "deployment_source_sha": deployment_source_sha,
    }


def validate_server_image_release_record(
    value: dict,
    version: str,
    bootstrap_bytes: bytes,
    *,
    expected_base: dict | None = None,
) -> dict[str, str]:
    identity = validate_server_image_metadata(value, version)
    if expected_base is not None:
        if set(expected_base) != SERVER_IMAGE_BASE_METADATA_KEYS:
            raise VerificationError("expected server image base metadata has an unexpected schema")
        for key in SERVER_IMAGE_BASE_METADATA_KEYS:
            if value.get(key) != expected_base.get(key):
                raise VerificationError(f"existing server image release record differs from canonical {key}")

    deployment_assets = identity["deployment_assets"]
    if not isinstance(deployment_assets, dict):
        raise VerificationError("server deployment asset metadata is invalid")
    actual_digest = hashlib.sha256(bootstrap_bytes).hexdigest()
    if actual_digest != deployment_assets[SERVER_BOOTSTRAP_ASSET]:
        raise VerificationError("server deployment metadata digest mismatch")
    try:
        bootstrap_text = bootstrap_bytes.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise VerificationError("server deployment bootstrap is not UTF-8") from exc
    pinned_image = f"{SERVER_IMAGE}@{identity['digest']}"
    if bootstrap_text.count(pinned_image) != 1 or f"{SERVER_IMAGE}:latest" in bootstrap_text:
        raise VerificationError("server deployment bootstrap is not pinned to the recorded image digest")
    if f"compose_target={SERVER_MATERIALIZED_COMPOSE}" not in bootstrap_text:
        raise VerificationError("server deployment bootstrap does not materialize the canonical compose file")
    return identity


def validate_github_assets(release: dict, version: str) -> dict[str, dict]:
    if (
        release.get("tag_name") != f"v{version}"
        or release.get("draft") is not False
        or release.get("prerelease") is not False
    ):
        raise VerificationError("GitHub Release is missing, draft/prerelease, or attached to the wrong tag")
    required = {canonical_archive_name(version, platform) for platform in PLATFORMS}
    required.add("SHA256SUMS")
    required.update(
        canonical_desktop_name(version, platform) for platform in desktop_platforms_for_version(version)
    )
    server_assets = {SERVER_IMAGE_METADATA, *SERVER_DEPLOYMENT_ASSETS}
    assets = release.get("assets")
    if not isinstance(assets, list):
        raise VerificationError("GitHub Release assets are missing")
    result: dict[str, dict] = {}
    for asset in assets:
        if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
            raise VerificationError("GitHub Release contains malformed asset metadata")
        name = asset["name"]
        if name in result:
            raise VerificationError(f"GitHub Release contains duplicate asset: {name}")
        result[name] = asset
    names = set(result)
    post_publication = names & server_assets
    expected = required | server_assets if post_publication else required
    if names != expected:
        raise VerificationError(f"GitHub Release asset set mismatch: {sorted(result)}")
    for name, asset in result.items():
        if asset.get("state") != "uploaded" or not asset.get("browser_download_url"):
            raise VerificationError(f"GitHub Release asset is not uploaded: {name}")
    return result


def _normalize_tar_name(name: str) -> str:
    while name.startswith("./"):
        name = name[2:]
    if not name or "/" in name or "\\" in name:
        raise VerificationError(f"archive contains nested or invalid path: {name!r}")
    return name


def _copy_tar_member(source: BinaryIO, destination: Path, expected_size: int) -> None:
    written = 0
    with destination.open("xb") as output:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            written += len(chunk)
            if written > expected_size or written > MAX_MEMBER_BYTES:
                raise VerificationError(f"archive member exceeds declared size: {destination.name}")
            output.write(chunk)
    if written != expected_size:
        raise VerificationError(f"archive member size mismatch: {destination.name}")


def _elf_header(path: Path) -> tuple[int, bytes]:
    with path.open("rb") as handle:
        header = handle.read(64)
    if len(header) < 64 or header[:4] != b"\x7fELF" or header[4] != 2 or header[5] != 1:
        raise VerificationError(f"expected little-endian ELF64 binary: {path.name}")
    return struct.unpack_from("<H", header, 18)[0], header


def inspect_binary_architecture(path: Path, platform: str) -> None:
    if platform.startswith("linux-"):
        machine, _header = _elf_header(path)
        expected = 62 if platform == "linux-x64" else 183
        if machine != expected:
            raise VerificationError(f"unexpected ELF machine for {platform}: {machine}")
        return
    if platform in {"darwin-x64", "darwin-arm64"}:
        with path.open("rb") as handle:
            data = handle.read(8)
        if len(data) < 8 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise VerificationError(f"expected thin little-endian Mach-O 64 binary: {path.name}")
        expected = {"darwin-x64": 0x01000007, "darwin-arm64": 0x0100000C}[platform]
        if struct.unpack_from("<I", data, 4)[0] != expected:
            raise VerificationError(f"unexpected Mach-O CPU type for {platform}: {path.name}")
        return
    if platform.startswith("win32-"):
        size = path.stat().st_size
        with path.open("rb") as handle:
            header = handle.read(64)
            if len(header) < 64 or header[:2] != b"MZ":
                raise VerificationError(f"expected PE binary: {path.name}")
            pe_offset = struct.unpack_from("<I", header, 0x3C)[0]
            if pe_offset > size - 6:
                raise VerificationError(f"invalid PE header offset: {path.name}")
            handle.seek(pe_offset)
            pe = handle.read(6)
        if pe[:4] != b"PE\0\0":
            raise VerificationError(f"invalid PE signature: {path.name}")
        machine = struct.unpack_from("<H", pe, 4)[0]
        expected = 0x8664 if platform == "win32-x64" else 0xAA64
        if machine != expected:
            raise VerificationError(f"unexpected PE machine for {platform}: 0x{machine:04x}")
        return
    raise VerificationError(f"unsupported release platform: {platform}")


def _version_key(value: str) -> tuple[int, ...]:
    return tuple(int(part) for part in value.split("."))


def _version_greater(left: str, right: str) -> bool:
    a = _version_key(left)
    b = _version_key(right)
    width = max(len(a), len(b))
    return a + (0,) * (width - len(a)) > b + (0,) * (width - len(b))


def inspect_linux_abi(path: Path) -> tuple[str, tuple[str, ...]]:
    machine, header = _elf_header(path)
    if machine not in {62, 183}:
        raise VerificationError(f"unsupported Linux ELF machine: {machine}")
    file_size = path.stat().st_size
    phoff = struct.unpack_from("<Q", header, 32)[0]
    phentsize = struct.unpack_from("<H", header, 54)[0]
    phnum = struct.unpack_from("<H", header, 56)[0]
    if phentsize < 56 or phnum == 0 or phnum > 128:
        raise VerificationError(f"invalid ELF program header table: {path.name}")
    if phoff + phentsize * phnum > file_size:
        raise VerificationError(f"ELF program headers exceed file: {path.name}")

    segments: list[tuple[int, int, int, int, int]] = []
    with path.open("rb") as handle:
        for index in range(phnum):
            handle.seek(phoff + index * phentsize)
            raw = handle.read(56)
            if len(raw) != 56:
                raise VerificationError(f"truncated ELF program header: {path.name}")
            p_type, _flags, p_offset, p_vaddr, _paddr, p_filesz, _memsz, _align = struct.unpack(
                "<IIQQQQQQ", raw
            )
            segments.append((p_type, p_offset, p_vaddr, p_filesz, file_size))

        dynamic = next((segment for segment in segments if segment[0] == 2), None)
        if dynamic is None:
            raise VerificationError(f"ELF has no PT_DYNAMIC segment: {path.name}")
        _kind, dyn_offset, _dyn_vaddr, dyn_size, _ = dynamic
        if dyn_size <= 0 or dyn_size > MAX_DYNAMIC_BYTES or dyn_offset + dyn_size > file_size:
            raise VerificationError(f"invalid PT_DYNAMIC segment: {path.name}")

        strtab_vaddr = None
        strtab_size = None
        needed_offsets: list[int] = []
        handle.seek(dyn_offset)
        for _ in range(dyn_size // 16):
            raw = handle.read(16)
            if len(raw) != 16:
                raise VerificationError(f"truncated ELF dynamic entry: {path.name}")
            tag, value = struct.unpack("<qQ", raw)
            if tag == 0:
                break
            if tag == 1:
                needed_offsets.append(value)
            elif tag == 5:
                strtab_vaddr = value
            elif tag == 10:
                strtab_size = value
        if strtab_vaddr is None or strtab_size is None:
            raise VerificationError(f"ELF dynamic string table is missing: {path.name}")
        if strtab_size <= 0 or strtab_size > MAX_DYNSTR_BYTES:
            raise VerificationError(f"invalid ELF dynamic string table size: {path.name}")

        strtab_offset = None
        for p_type, p_offset, p_vaddr, p_filesz, _ in segments:
            if p_type != 1:
                continue
            if p_vaddr <= strtab_vaddr and strtab_vaddr + strtab_size <= p_vaddr + p_filesz:
                strtab_offset = p_offset + (strtab_vaddr - p_vaddr)
                break
        if strtab_offset is None or strtab_offset + strtab_size > file_size:
            raise VerificationError(f"ELF dynamic string table is outside PT_LOAD: {path.name}")
        handle.seek(strtab_offset)
        dynstr = handle.read(strtab_size)
        if len(dynstr) != strtab_size:
            raise VerificationError(f"truncated ELF dynamic string table: {path.name}")

    needed: list[str] = []
    for offset in needed_offsets:
        if offset >= len(dynstr):
            raise VerificationError(f"ELF DT_NEEDED offset is out of range: {path.name}")
        end = dynstr.find(b"\0", offset)
        if end < 0:
            raise VerificationError(f"unterminated ELF DT_NEEDED string: {path.name}")
        try:
            needed.append(dynstr[offset:end].decode("ascii"))
        except UnicodeDecodeError as exc:
            raise VerificationError(f"non-ASCII ELF DT_NEEDED string: {path.name}") from exc
    unexpected = sorted(set(needed) - ALLOWED_NEEDED)
    if unexpected:
        raise VerificationError(f"unexpected DT_NEEDED for {path.name}: {unexpected}")

    versions = sorted(
        {match.decode("ascii") for match in re.findall(rb"GLIBC_([0-9]+(?:\.[0-9]+)*)", dynstr)},
        key=_version_key,
    )
    if not versions:
        raise VerificationError(f"no GLIBC version requirements found: {path.name}")
    max_glibc = versions[-1]
    if _version_greater(max_glibc, GLIBC_FLOOR):
        raise VerificationError(
            f"{path.name} requires GLIBC_{max_glibc}, exceeding the {GLIBC_FLOOR} floor"
        )
    return max_glibc, tuple(sorted(set(needed)))


def inspect_archive(path: Path, platform: str, extraction_root: Path) -> dict:
    expected = expected_binary_names(platform)
    files: dict[str, tarfile.TarInfo] = {}
    total = 0
    try:
        with tarfile.open(path, "r:gz") as archive:
            member_count = 0
            for member in archive:
                member_count += 1
                if member_count > MAX_RELEASE_TAR_MEMBERS:
                    raise VerificationError("release archive contains too many members")
                if member.isdir():
                    continue
                if not member.isfile():
                    raise VerificationError(f"archive contains non-file member: {member.name}")
                name = _normalize_tar_name(member.name)
                if name in files:
                    raise VerificationError(f"archive contains duplicate member: {name}")
                if member.size < 0 or member.size > MAX_MEMBER_BYTES:
                    raise VerificationError(f"archive member size is invalid: {name}")
                total += member.size
                if total > MAX_UNCOMPRESSED_BYTES:
                    raise VerificationError("archive exceeds the uncompressed byte limit")
                files[name] = member
            if set(files) != expected:
                raise VerificationError(
                    f"unexpected archive contents for {platform}: {sorted(files)}"
                )

            platform_dir = extraction_root / platform
            platform_dir.mkdir(parents=True)
            linux_abi: dict[str, tuple[str, tuple[str, ...]]] = {}
            for name, member in files.items():
                source = archive.extractfile(member)
                if source is None:
                    raise VerificationError(f"cannot read archive member: {name}")
                destination = platform_dir / name
                _copy_tar_member(source, destination, member.size)
                inspect_binary_architecture(destination, platform)
                if platform.startswith("linux-"):
                    linux_abi[name] = inspect_linux_abi(destination)
    except VerificationError:
        raise
    except (tarfile.TarError, OSError) as exc:
        raise VerificationError(f"invalid release archive {path.name}: {exc}") from exc

    result: dict[str, object] = {"members": sorted(files)}
    if platform.startswith("linux-"):
        max_glibc = max((value[0] for value in linux_abi.values()), key=_version_key)
        needed = sorted({item for _glibc, libs in linux_abi.values() for item in libs})
        result.update({"max_glibc": max_glibc, "needed": needed})
    return result


def _asset_digest(asset: dict) -> str | None:
    value = asset.get("digest")
    if not value:
        return None
    if not isinstance(value, str) or not value.startswith("sha256:"):
        raise VerificationError(f"unsupported GitHub asset digest: {value!r}")
    digest = value.removeprefix("sha256:")
    if not SHA256_RE.fullmatch(digest):
        raise VerificationError(f"invalid GitHub asset SHA-256: {value!r}")
    return digest


def verify_desktop_asset(
    version: str,
    platform: str,
    asset: dict,
    sums: dict[str, str],
    root: Path,
    timeout: float,
) -> tuple[int, str]:
    desktop_name = canonical_desktop_name(version, platform)
    desktop_url = asset.get("browser_download_url")
    if desktop_url != expected_desktop_url(version, platform):
        raise VerificationError(f"unexpected GitHub Desktop asset URL: {desktop_url!r}")
    if desktop_name not in sums:
        raise VerificationError(f"SHA256SUMS is missing the Desktop artifact for {platform}")
    desktop_path = root / desktop_name
    desktop_size, desktop_digest = download_file(
        desktop_url,
        desktop_path,
        MAX_DESKTOP_ARTIFACT_BYTES,
        timeout,
    )
    if desktop_size <= 0 or desktop_digest != sums[desktop_name]:
        raise VerificationError(f"Desktop artifact SHA-256 disagreement for {platform}")
    github_desktop_digest = _asset_digest(asset)
    if github_desktop_digest is not None and desktop_digest != github_desktop_digest:
        raise VerificationError(f"GitHub Desktop artifact digest mismatch for {platform}")
    return desktop_size, desktop_digest


def verify_public_release(version: str, timeout: float) -> None:
    encoded_package = urllib.parse.quote(PACKAGE, safe="@")
    npm_url = f"https://registry.npmjs.org/{encoded_package}/{version}"
    npm_metadata = fetch_json(npm_url, timeout)
    if npm_metadata.get("name") != PACKAGE or npm_metadata.get("version") != version:
        raise VerificationError("npm registry returned the wrong package/version")
    dist = npm_metadata.get("dist")
    if not isinstance(dist, dict) or not isinstance(dist.get("tarball"), str):
        raise VerificationError("npm registry metadata is missing dist.tarball")

    release_url = f"https://api.github.com/repos/{REPO}/releases/tags/v{version}"
    release = fetch_json(release_url, timeout)
    assets = validate_github_assets(release, version)

    with tempfile.TemporaryDirectory(prefix=f"webcodex-v{version}-verify-") as temp:
        root = Path(temp)
        npm_tgz = root / "package.tgz"
        download_file(dist["tarball"], npm_tgz, MAX_NPM_TARBALL_BYTES, timeout)
        verify_npm_dist(npm_tgz, dist)
        package, manifest = read_npm_package_jsons(npm_tgz)
        if package.get("name") != PACKAGE or package.get("version") != version:
            raise VerificationError("published npm tarball has the wrong package/version")
        manifest_artifacts = validate_public_manifest(manifest, version)

        sums_asset = assets["SHA256SUMS"]
        sums_url = sums_asset["browser_download_url"]
        sums_bytes = fetch_bytes(sums_url, MAX_JSON_BYTES, timeout)
        sums_digest = _asset_digest(sums_asset)
        if sums_digest is not None and hashlib.sha256(sums_bytes).hexdigest() != sums_digest:
            raise VerificationError("GitHub SHA256SUMS asset digest mismatch")
        try:
            sums_text = sums_bytes.decode("ascii")
        except UnicodeDecodeError as exc:
            raise VerificationError("SHA256SUMS is not ASCII") from exc
        sums = parse_sha256sums(sums_text, version)

        for platform in desktop_platforms_for_version(version):
            desktop_name = canonical_desktop_name(version, platform)
            desktop_size, desktop_digest = verify_desktop_asset(
                version,
                platform,
                assets[desktop_name],
                sums,
                root,
                timeout,
            )
            print(f"desktop_{platform.replace('-', '_')} sha256={desktop_digest} bytes={desktop_size}")

        image_identity = None
        image_asset = assets.get(SERVER_IMAGE_METADATA)
        if image_asset is not None:
            image_bytes = fetch_bytes(image_asset["browser_download_url"], MAX_JSON_BYTES, timeout)
            image_digest = _asset_digest(image_asset)
            if image_digest is not None and hashlib.sha256(image_bytes).hexdigest() != image_digest:
                raise VerificationError("GitHub server-image metadata asset digest mismatch")
            try:
                image_metadata = json.loads(image_bytes)
            except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise VerificationError("server image metadata asset is not valid JSON") from exc
            if not isinstance(image_metadata, dict):
                raise VerificationError("server image metadata asset must be a JSON object")
            asset = assets[SERVER_BOOTSTRAP_ASSET]
            bootstrap_bytes = fetch_bytes(asset["browser_download_url"], MAX_JSON_BYTES, timeout)
            github_digest = _asset_digest(asset)
            actual_digest = hashlib.sha256(bootstrap_bytes).hexdigest()
            if github_digest is not None and actual_digest != github_digest:
                raise VerificationError("GitHub deployment bootstrap digest mismatch")
            image_identity = validate_server_image_release_record(image_metadata, version, bootstrap_bytes)

        print(f"npm={PACKAGE}@{version} manifest=ok")
        print(f"github_release=v{version} assets={len(assets)}")
        if image_identity is not None:
            print(
                f"server_image={SERVER_IMAGE}@{image_identity['digest']} "
                f"source={image_identity['source_sha']} platforms={','.join(SERVER_IMAGE_PLATFORMS)}"
            )
        else:
            print("server_image=not_recorded_historical_release")
        for platform in PLATFORMS:
            name = canonical_archive_name(version, platform)
            asset = assets[name]
            url = asset["browser_download_url"]
            if url != expected_artifact_url(version, platform):
                raise VerificationError(f"unexpected GitHub asset URL for {platform}: {url!r}")
            archive_path = root / name
            _size, digest = download_file(url, archive_path, MAX_ARTIFACT_BYTES, timeout)
            expected_digest = manifest_artifacts[platform]["sha256"]
            if digest != expected_digest or digest != sums[name]:
                raise VerificationError(f"SHA-256 disagreement for {platform}")
            github_digest = _asset_digest(asset)
            if github_digest is not None and digest != github_digest:
                raise VerificationError(f"GitHub asset digest mismatch for {platform}")
            inspection = inspect_archive(archive_path, platform, root)
            if platform.startswith("linux-"):
                print(
                    f"{platform} sha256={digest} max_glibc={inspection['max_glibc']} "
                    f"needed={','.join(inspection['needed'])}"
                )
            else:
                print(f"{platform} sha256={digest} architecture=ok")

    print("public_release_verification=passed")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify published WebCodex npm/GitHub release bytes on one network host."
    )
    parser.add_argument("version", help="release version, for example 0.3.8 or v0.3.8")
    parser.add_argument("--timeout", type=float, default=60.0, help="per-request timeout in seconds")
    args = parser.parse_args()
    version = normalize_version(args.version)
    if args.timeout <= 0 or args.timeout > 300:
        raise VerificationError("--timeout must be in (0, 300]")
    verify_public_release(version, args.timeout)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as exc:
        print(f"verification failed: {exc}", file=sys.stderr)
        raise SystemExit(1)
