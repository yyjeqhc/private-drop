from __future__ import annotations

import hashlib
import io
import json
import tarfile
import tempfile
import unittest
import urllib.request
import zipfile
from pathlib import Path

from scripts import collect_release_bundle as collector


SOURCE_SHA = "a" * 40
RUN_ID = 123456
VERSION = "0.4.0"


def _archive_bytes(platform: str) -> bytes:
    output = io.BytesIO()
    suffix = ".exe" if platform.startswith("win32-") else ""
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for binary in collector.BINARIES:
            payload = f"{platform}:{binary}".encode("ascii")
            info = tarfile.TarInfo(f"{binary}{suffix}")
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
    return output.getvalue()


def _write_bundle(root: Path, tag: str, build_kind: str) -> tuple[str, dict[str, str]]:
    stem = (
        f"webcodex-v{VERSION}"
        if build_kind == "release"
        else f"webcodex-{tag}-{SOURCE_SHA[:12]}-v{VERSION}"
    )
    artifact_hashes: dict[str, str] = {}
    artifact_payload: dict[str, dict[str, str]] = {}
    checksum_lines = []
    for platform in collector.PLATFORMS:
        filename = f"{stem}-{platform}.tar.gz"
        payload = _archive_bytes(platform)
        (root / filename).write_bytes(payload)
        digest = hashlib.sha256(payload).hexdigest()
        artifact_hashes[platform] = digest
        artifact_payload[platform] = {"filename": filename, "sha256": digest}
        checksum_lines.append(f"{digest}  {filename}")

    desktop_artifacts: dict[str, dict[str, str]] = {}
    for platform in collector.DESKTOP_PLATFORMS:
        desktop_name = collector.desktop_artifact_filename(
            VERSION,
            platform,
            build_kind=build_kind,
            tag=tag,
            source_sha=SOURCE_SHA,
        )
        desktop_payload = f"synthetic-{platform}-desktop-artifact".encode("ascii")
        (root / desktop_name).write_bytes(desktop_payload)
        desktop_digest = hashlib.sha256(desktop_payload).hexdigest()
        checksum_lines.append(f"{desktop_digest}  {desktop_name}")
        desktop_artifacts[platform] = {"filename": desktop_name, "sha256": desktop_digest}
    (root / "SHA256SUMS").write_text("\n".join(checksum_lines) + "\n", encoding="ascii")
    (root / "linux-x64-elf.txt").write_text("ELF x64\n", encoding="utf-8")
    (root / "linux-arm64-elf.txt").write_text("ELF arm64\n", encoding="utf-8")
    release_build = {
        "tag": tag,
        "version": VERSION,
        "source_sha": SOURCE_SHA,
        "built_at": 1234567890,
        "build_kind": build_kind,
        "workflow_run_id": RUN_ID,
        "archive_stem": stem,
        "artifacts": artifact_payload,
        "desktop_artifacts": desktop_artifacts,
    }
    (root / "release-build.json").write_text(json.dumps(release_build) + "\n", encoding="utf-8")
    if build_kind == "release":
        manifest = {
            "version": VERSION,
            "binaries": list(collector.BINARIES),
            "artifacts": {
                platform: {
                    "url": f"https://github.com/{collector.DEFAULT_REPO}/releases/download/v{VERSION}/{stem}-{platform}.tar.gz",
                    "sha256": artifact_hashes[platform],
                }
                for platform in collector.PLATFORMS
            },
        }
        (root / "manifest.json").write_text(json.dumps(manifest) + "\n", encoding="utf-8")
    return stem, artifact_hashes


class RedirectSafetyTests(unittest.TestCase):
    def test_cross_host_redirect_drops_github_api_credentials(self) -> None:
        request = urllib.request.Request(
            "https://api.github.com/repos/yyjeqhc/webcodex/actions/artifacts/1/zip",
            headers={
                "Authorization": "Bearer secret-value",
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": collector.API_VERSION,
                "User-Agent": collector.USER_AGENT,
            },
        )
        redirected = collector._SafeRedirectHandler().redirect_request(
            request,
            None,
            302,
            "Found",
            {},
            "https://results-receiver.actions.githubusercontent.com/signed-artifact",
        )
        self.assertIsNotNone(redirected)
        assert redirected is not None
        self.assertIsNone(redirected.get_header("Authorization"))
        self.assertIsNone(redirected.get_header("Accept"))
        self.assertIsNone(redirected.get_header("X-Github-Api-Version"))
        self.assertEqual(redirected.get_header("User-agent"), collector.USER_AGENT)


class ArtifactSelectionTests(unittest.TestCase):
    def test_requires_one_unexpired_bundle_bound_to_run_and_source(self) -> None:
        payload = {
            "total_count": 2,
            "artifacts": [
                {"id": 1, "name": "native-linux", "expired": False},
                {
                    "id": 2,
                    "name": "webcodex-v0.3.8-bundle",
                    "expired": False,
                    "size_in_bytes": 100,
                    "digest": "sha256:" + "b" * 64,
                    "workflow_run": {"id": RUN_ID, "head_sha": SOURCE_SHA},
                },
            ],
        }
        selected = collector.select_bundle_artifact(payload, RUN_ID, SOURCE_SHA)
        self.assertEqual(selected["id"], 2)
        duplicate = json.loads(json.dumps(payload))
        duplicate["artifacts"].append(dict(duplicate["artifacts"][1], id=3, name="other-bundle"))
        duplicate["total_count"] = 3
        with self.assertRaises(collector.CollectionError):
            collector.select_bundle_artifact(duplicate, RUN_ID, SOURCE_SHA)
        expired = json.loads(json.dumps(payload))
        expired["artifacts"][1]["expired"] = True
        with self.assertRaises(collector.CollectionError):
            collector.select_bundle_artifact(expired, RUN_ID, SOURCE_SHA)

    def test_run_requires_success_main_and_exact_source(self) -> None:
        run = {
            "id": RUN_ID,
            "status": "completed",
            "conclusion": "success",
            "head_sha": SOURCE_SHA,
            "event": "workflow_dispatch",
            "path": collector.RELEASE_WORKFLOW_PATH,
            "head_branch": "main",
        }
        collector.validate_run(run, RUN_ID, SOURCE_SHA)
        for key, bad in (("conclusion", "failure"), ("head_sha", "b" * 40), ("head_branch", "other")):
            changed = dict(run)
            changed[key] = bad
            with self.assertRaises(collector.CollectionError):
                collector.validate_run(changed, RUN_ID, SOURCE_SHA)


class BundleTests(unittest.TestCase):
    def test_release_bundle_contract(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            stem, hashes = _write_bundle(root, f"v{VERSION}", "release")
            summary = collector.verify_bundle_directory(
                root,
                repo=collector.DEFAULT_REPO,
                run_id=RUN_ID,
                expected_source_sha=SOURCE_SHA,
                expected_tag=f"v{VERSION}",
                artifact_name=f"{stem}-bundle",
            )
            self.assertEqual(summary["artifacts"], hashes)
            self.assertEqual(summary["build_kind"], "release")
            self.assertEqual(
                summary["desktop_artifacts"]["darwin-x64"]["filename"],
                f"webcodex-desktop-v{VERSION}-darwin-x64.dmg",
            )
            self.assertEqual(
                summary["desktop_artifacts"]["darwin-arm64"]["filename"],
                f"webcodex-desktop-v{VERSION}-darwin-arm64.dmg",
            )
            self.assertEqual(
                summary["desktop_artifacts"]["win32-x64"]["filename"],
                f"webcodex-desktop-v{VERSION}-win32-x64-setup.exe",
            )
            self.assertEqual(
                summary["desktop_artifacts"]["win32-arm64"]["filename"],
                f"webcodex-desktop-v{VERSION}-win32-arm64-setup.exe",
            )

    def test_release_bundle_rejects_missing_desktop(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            stem, _hashes = _write_bundle(root, f"v{VERSION}", "release")
            metadata = json.loads((root / "release-build.json").read_text(encoding="utf-8"))
            desktop_name = metadata["desktop_artifacts"]["darwin-arm64"]["filename"]
            (root / desktop_name).unlink()
            with self.assertRaises(collector.CollectionError):
                collector.verify_bundle_directory(
                    root,
                    repo=collector.DEFAULT_REPO,
                    run_id=RUN_ID,
                    expected_source_sha=SOURCE_SHA,
                    expected_tag=f"v{VERSION}",
                    artifact_name=f"{stem}-bundle",
                )

    def test_release_bundle_rejects_desktop_digest_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            stem, _hashes = _write_bundle(root, f"v{VERSION}", "release")
            metadata = json.loads((root / "release-build.json").read_text(encoding="utf-8"))
            desktop_name = metadata["desktop_artifacts"]["darwin-x64"]["filename"]
            (root / desktop_name).write_bytes(b"drifted-dmg")
            with self.assertRaises(collector.CollectionError):
                collector.verify_bundle_directory(
                    root,
                    repo=collector.DEFAULT_REPO,
                    run_id=RUN_ID,
                    expected_source_sha=SOURCE_SHA,
                    expected_tag=f"v{VERSION}",
                    artifact_name=f"{stem}-bundle",
                )

    def test_release_bundle_rejects_unexpected_desktop_filename(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            stem, _hashes = _write_bundle(root, f"v{VERSION}", "release")
            metadata_path = root / "release-build.json"
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            item = metadata["desktop_artifacts"]["darwin-x64"]
            old_name = item["filename"]
            bad_name = "webcodex-desktop-v0.4.0-macos-x64.dmg"
            (root / old_name).rename(root / bad_name)
            item["filename"] = bad_name
            metadata_path.write_text(json.dumps(metadata) + "\n", encoding="utf-8")
            with self.assertRaises(collector.CollectionError):
                collector.verify_bundle_directory(
                    root,
                    repo=collector.DEFAULT_REPO,
                    run_id=RUN_ID,
                    expected_source_sha=SOURCE_SHA,
                    expected_tag=f"v{VERSION}",
                    artifact_name=f"{stem}-bundle",
                )

    def test_verification_bundle_has_no_manifest(self) -> None:
        tag = "release-build-test-collector"
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            stem, _hashes = _write_bundle(root, tag, "verification")
            summary = collector.verify_bundle_directory(
                root,
                repo=collector.DEFAULT_REPO,
                run_id=RUN_ID,
                expected_source_sha=SOURCE_SHA,
                expected_tag=tag,
                artifact_name=f"{stem}-bundle",
            )
            self.assertEqual(summary["build_kind"], "verification")
            (root / "manifest.json").write_text("{}\n", encoding="utf-8")
            with self.assertRaises(collector.CollectionError):
                collector.verify_bundle_directory(
                    root,
                    repo=collector.DEFAULT_REPO,
                    run_id=RUN_ID,
                    expected_source_sha=SOURCE_SHA,
                    expected_tag=tag,
                    artifact_name=f"{stem}-bundle",
                )

    def test_missing_bundle_metadata_is_a_clean_collection_error(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with self.assertRaises(collector.CollectionError):
                collector.verify_bundle_directory(
                    root,
                    repo=collector.DEFAULT_REPO,
                    run_id=RUN_ID,
                    expected_source_sha=SOURCE_SHA,
                    expected_tag=f"v{VERSION}",
                    artifact_name=f"webcodex-v{VERSION}-bundle",
                )

    def test_bundle_rejects_archive_digest_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            stem, _hashes = _write_bundle(root, f"v{VERSION}", "release")
            target = root / f"{stem}-linux-x64.tar.gz"
            target.write_bytes(target.read_bytes() + b"drift")
            with self.assertRaises(collector.CollectionError):
                collector.verify_bundle_directory(
                    root,
                    repo=collector.DEFAULT_REPO,
                    run_id=RUN_ID,
                    expected_source_sha=SOURCE_SHA,
                    expected_tag=f"v{VERSION}",
                    artifact_name=f"{stem}-bundle",
                )

    def test_safe_zip_extraction_rejects_nested_entries(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / "artifact.zip"
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr("../escape", b"bad")
            with self.assertRaises(collector.CollectionError):
                collector.safe_extract_zip(path, root / "extract")
            self.assertFalse((root / "escape").exists())


if __name__ == "__main__":
    unittest.main()
