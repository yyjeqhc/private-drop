from __future__ import annotations

import hashlib
import io
import json
import tarfile
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

from scripts import collect_release_bundle as collector
from scripts import release_publication as publication

SOURCE = "a" * 40
REQUEST = "rb_" + "b" * 24
RUN_ID = 123456
VERSION = "0.4.0"
TAG = f"v{VERSION}"


def _versions(**overrides: str) -> dict[str, str]:
    values = {
        "cargo": VERSION,
        "npm": VERSION,
        "desktop_package": VERSION,
        "desktop_cargo": VERSION,
        "desktop_tauri": VERSION,
    }
    values.update(overrides)
    return values


def _state() -> dict:
    return {
        "schema_version": publication.BUILD_STATE_SCHEMA_VERSION,
        "kind": "release-build",
        "repo": collector.DEFAULT_REPO,
        "tag": TAG,
        "source_sha": SOURCE,
        "workflow_file": publication.BUILD_WORKFLOW_FILE,
        "workflow_path": publication.BUILD_WORKFLOW_PATH,
        "request_id": REQUEST,
        "run_name": publication._build_run_name(TAG, REQUEST),
        "dispatch_state": "dispatched",
        "created_at": 1234567890,
        "run_id": None,
        "run_head_sha": None,
        "source_matches": None,
        "run_url": None,
        "run_status": None,
        "run_conclusion": None,
        "last_observed_at": None,
    }


def _run(run_id: int = RUN_ID, source: str = SOURCE) -> dict:
    return {
        "id": run_id,
        "path": publication.BUILD_WORKFLOW_PATH,
        "event": "workflow_dispatch",
        "head_branch": "main",
        "head_sha": source,
        "display_title": publication._build_run_name(TAG, REQUEST),
        "html_url": f"https://github.com/yyjeqhc/webcodex/actions/runs/{run_id}",
        "status": "in_progress",
        "conclusion": None,
    }


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


def _write_bundle(root: Path) -> dict:
    stem = f"webcodex-v{VERSION}"
    artifact_payload = {}
    checksum_lines = []
    for platform in collector.PLATFORMS:
        name = f"{stem}-{platform}.tar.gz"
        payload = _archive_bytes(platform)
        (root / name).write_bytes(payload)
        digest = hashlib.sha256(payload).hexdigest()
        artifact_payload[platform] = {"filename": name, "sha256": digest}
        checksum_lines.append(f"{digest}  {name}")
    desktop_artifacts = {}
    for platform in collector.DESKTOP_PLATFORMS:
        desktop_name = collector.desktop_artifact_filename(
            VERSION,
            platform,
            build_kind="release",
            tag=TAG,
            source_sha=SOURCE,
        )
        desktop_bytes = f"synthetic-{platform}-desktop".encode("ascii")
        (root / desktop_name).write_bytes(desktop_bytes)
        desktop_digest = hashlib.sha256(desktop_bytes).hexdigest()
        checksum_lines.append(f"{desktop_digest}  {desktop_name}")
        desktop_artifacts[platform] = {"filename": desktop_name, "sha256": desktop_digest}
    (root / "SHA256SUMS").write_text("\n".join(checksum_lines) + "\n", encoding="ascii")
    (root / "linux-x64-elf.txt").write_text("ELF x64\n", encoding="utf-8")
    (root / "linux-arm64-elf.txt").write_text("ELF arm64\n", encoding="utf-8")
    manifest = {
        "version": VERSION,
        "binaries": list(collector.BINARIES),
        "artifacts": {
            platform: {
                "url": f"https://github.com/{collector.DEFAULT_REPO}/releases/download/{TAG}/{stem}-{platform}.tar.gz",
                "sha256": artifact_payload[platform]["sha256"],
            }
            for platform in collector.PLATFORMS
        },
    }
    (root / "manifest.json").write_text(json.dumps(manifest) + "\n", encoding="utf-8")
    release_build = {
        "tag": TAG,
        "version": VERSION,
        "source_sha": SOURCE,
        "built_at": 1234567890,
        "build_kind": "release",
        "workflow_run_id": RUN_ID,
        "archive_stem": stem,
        "artifacts": artifact_payload,
        "desktop_artifacts": desktop_artifacts,
    }
    (root / "release-build.json").write_text(json.dumps(release_build) + "\n", encoding="utf-8")
    return release_build


class _JsonResponse:
    headers: dict[str, str] = {}

    def __init__(self, payload: dict):
        self.payload = json.dumps(payload).encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def read(self, _size=-1):
        return self.payload


class _RecordingOpener:
    def __init__(self, payload: dict):
        self.payload = payload
        self.requests = []

    def open(self, request, timeout):
        self.requests.append((request, timeout))
        return _JsonResponse(self.payload)


class GitHubClientIdentityTests(unittest.TestCase):
    def test_authenticated_user_uses_global_api_endpoint(self) -> None:
        client = collector.GitHubClient(collector.DEFAULT_REPO, "fake-token", 5)
        opener = _RecordingOpener({"login": "publisher"})
        client.opener = opener

        self.assertEqual(client.fetch_authenticated_user(), {"login": "publisher"})
        self.assertEqual(client.api_url("/branches/main"), f"https://api.github.com/repos/{collector.DEFAULT_REPO}/branches/main")
        self.assertEqual(len(opener.requests), 1)
        request, timeout = opener.requests[0]
        self.assertEqual(request.full_url, "https://api.github.com/user")
        self.assertEqual(timeout, 5)


class PreflightTests(unittest.TestCase):
    def test_preflight_requires_exact_versions_and_unused_namespaces(self) -> None:
        client = mock.Mock()
        client.fetch_authenticated_user.return_value = {"login": "publisher"}
        with mock.patch.object(publication, "_require_exact_clean_root"), mock.patch.object(
            publication, "_package_versions", return_value=_versions()
        ), mock.patch.object(publication.collector, "resolve_github_token", return_value="fake"), mock.patch.object(
            publication.collector, "GitHubClient", return_value=client
        ), mock.patch.object(publication, "_github_main_sha", return_value=SOURCE), mock.patch.object(
            publication, "_github_optional_json", return_value=None
        ) as optional_json, mock.patch.object(
            publication, "_fetch_public_json_optional", return_value=None
        ), mock.patch.object(publication, "_git", return_value=""), mock.patch.object(
            publication, "_run_capture", return_value="npm-publisher"
        ) as run_capture:
            summary = publication.preflight_release(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                source_sha=SOURCE,
                root=Path("/tmp/exact-release-source"),
                timeout=5,
            )
        self.assertTrue(summary["tag_available"])
        self.assertTrue(summary["github_release_available"])
        self.assertTrue(summary["npm_version_available"])
        self.assertEqual(summary["github_user"], "publisher")
        self.assertEqual(summary["npm_user"], "npm-publisher")
        run_capture.assert_called_once_with(
            ["npm", "whoami", "--registry", publication.NPM_REGISTRY], timeout=5
        )
        self.assertEqual(optional_json.call_count, 2)
        client.fetch_authenticated_user.assert_called_once_with()

    def test_preflight_rejects_existing_local_tag(self) -> None:
        client = mock.Mock()
        with mock.patch.object(publication, "_require_exact_clean_root"), mock.patch.object(
            publication, "_package_versions", return_value=_versions()
        ), mock.patch.object(publication.collector, "resolve_github_token", return_value="fake"), mock.patch.object(
            publication.collector, "GitHubClient", return_value=client
        ), mock.patch.object(publication, "_github_main_sha", return_value=SOURCE), mock.patch.object(
            publication, "_git", return_value=TAG
        ), self.assertRaises(publication.PublicationError):
            publication.preflight_release(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                source_sha=SOURCE,
                root=Path("/tmp/exact-release-source"),
                timeout=5,
            )

    def test_preflight_fails_before_namespace_checks_on_version_mismatch(self) -> None:
        with mock.patch.object(publication, "_require_exact_clean_root"), mock.patch.object(
            publication, "_package_versions", return_value=_versions(cargo="0.3.9")
        ), self.assertRaises(publication.PublicationError):
            publication.preflight_release(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                source_sha=SOURCE,
                root=Path("/tmp/exact-release-source"),
                timeout=5,
            )

    def test_preflight_fails_closed_on_desktop_version_mismatch(self) -> None:
        with mock.patch.object(publication, "_require_exact_clean_root"), mock.patch.object(
            publication, "_package_versions", return_value=_versions(desktop_package="0.3.9")
        ), self.assertRaisesRegex(publication.PublicationError, "desktop_package=0.3.9"):
            publication.preflight_release(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                source_sha=SOURCE,
                root=Path("/tmp/exact-release-source"),
                timeout=5,
            )


class ReclaimTagTests(unittest.TestCase):
    def test_reclaim_requires_exact_confirmation(self) -> None:
        with self.assertRaises(publication.PublicationError):
            publication.reclaim_prepublication_tag(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                root=Path("/tmp/reclaim"),
                confirm="wrong",
                timeout=5,
                allow_public_release_check=False,
            )

    def test_reclaim_rejects_successful_authoritative_build(self) -> None:
        with mock.patch.object(Path, "is_dir", return_value=True), mock.patch.object(
            publication, "_git", side_effect=["", "https://github.com/yyjeqhc/webcodex.git", SOURCE, TAG, SOURCE]
        ), mock.patch.object(publication, "_remote_main_source", return_value=SOURCE), mock.patch.object(
            publication, "_package_versions", return_value=_versions()
        ), mock.patch.object(
            publication, "_remote_annotated_tag_identity", return_value=("b" * 40, SOURCE)
        ), mock.patch.object(publication, "_reclaim_release_check", return_value=(None, "authenticated")), mock.patch.object(
            publication, "_fetch_public_json_optional", return_value=None
        ), mock.patch.object(
            publication,
            "_reclaim_build_history",
            return_value=[{"run_id": 1, "status": "completed", "conclusion": "success", "source_sha": SOURCE}],
        ), self.assertRaises(publication.PublicationError):
            publication.reclaim_prepublication_tag(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                root=Path("/tmp/reclaim"),
                confirm=TAG,
                timeout=5,
                allow_public_release_check=False,
            )

    def test_reclaim_failed_prepublication_tag_deletes_remote_then_local(self) -> None:
        root = Path("/tmp/reclaim")
        calls: list[tuple[list[str], Path | None]] = []

        def run_checked(argv, *, cwd=None, timeout=600.0):
            calls.append((list(argv), cwd))

        with mock.patch.object(Path, "is_dir", return_value=True), mock.patch.object(
            publication, "_git", side_effect=["", "https://github.com/yyjeqhc/webcodex.git", SOURCE, TAG, SOURCE]
        ), mock.patch.object(publication, "_remote_main_source", return_value=SOURCE), mock.patch.object(
            publication, "_package_versions", return_value=_versions()
        ), mock.patch.object(
            publication,
            "_remote_annotated_tag_identity",
            side_effect=[("b" * 40, SOURCE), None],
        ), mock.patch.object(publication, "_reclaim_release_check", return_value=(None, "authenticated")), mock.patch.object(
            publication, "_fetch_public_json_optional", return_value=None
        ), mock.patch.object(
            publication,
            "_reclaim_build_history",
            return_value=[{"run_id": 1, "status": "completed", "conclusion": "failure", "source_sha": SOURCE}],
        ), mock.patch.object(publication, "_run_checked", side_effect=run_checked):
            summary = publication.reclaim_prepublication_tag(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                root=root,
                confirm=TAG,
                timeout=5,
                allow_public_release_check=False,
            )
        self.assertTrue(summary["remote_tag_deleted"])
        self.assertTrue(summary["local_tag_deleted"])
        self.assertEqual(
            calls[0][0],
            [
                "git",
                "push",
                f"--force-with-lease=refs/tags/{TAG}:{'b' * 40}",
                "origin",
                f":refs/tags/{TAG}",
            ],
        )
        self.assertEqual(calls[1][0], ["git", "tag", "-d", TAG])

    def test_reclaim_rejects_tag_changed_after_validation(self) -> None:
        root = Path("/tmp/reclaim")
        replacement_identity = ("c" * 40, "d" * 40)
        calls: list[tuple[list[str], Path | None]] = []

        def reject_push(argv, *, cwd=None, timeout=600.0):
            calls.append((list(argv), cwd))
            raise publication.PublicationError("lease rejected")

        with mock.patch.object(Path, "is_dir", return_value=True), mock.patch.object(
            publication,
            "_git",
            side_effect=["", "https://github.com/yyjeqhc/webcodex.git", SOURCE, ""],
        ), mock.patch.object(publication, "_remote_main_source", return_value=SOURCE), mock.patch.object(
            publication, "_package_versions", return_value=_versions()
        ), mock.patch.object(
            publication,
            "_remote_annotated_tag_identity",
            side_effect=[("b" * 40, SOURCE), replacement_identity],
        ), mock.patch.object(publication, "_reclaim_release_check", return_value=(None, "authenticated")), mock.patch.object(
            publication, "_fetch_public_json_optional", return_value=None
        ), mock.patch.object(
            publication,
            "_reclaim_build_history",
            return_value=[{"run_id": 1, "status": "completed", "conclusion": "failure", "source_sha": SOURCE}],
        ), mock.patch.object(publication, "_run_checked", side_effect=reject_push), self.assertRaises(
            publication.PublicationError
        ) as raised:
            publication.reclaim_prepublication_tag(
                repo=collector.DEFAULT_REPO,
                version=VERSION,
                root=root,
                confirm=TAG,
                timeout=5,
                allow_public_release_check=False,
            )

        self.assertIn("remote tag deletion failed", str(raised.exception))
        self.assertEqual(len(calls), 1)
        self.assertEqual(
            calls[0][0],
            [
                "git",
                "push",
                f"--force-with-lease=refs/tags/{TAG}:{'b' * 40}",
                "origin",
                f":refs/tags/{TAG}",
            ],
        )


class BuildStateTests(unittest.TestCase):
    def test_selects_exact_request_and_rejects_duplicate(self) -> None:
        selected = publication.select_build_run({"workflow_runs": [_run()]}, _state())
        self.assertEqual(selected["id"], RUN_ID)
        with self.assertRaises(publication.PublicationError):
            publication.select_build_run({"workflow_runs": [_run(1), _run(2)]}, _state())

    def test_bound_run_cannot_change_and_source_mismatch_fails_closed(self) -> None:
        state = _state()
        state["run_id"] = RUN_ID
        with self.assertRaises(publication.PublicationError):
            publication._apply_build_run_snapshot(state, _run(RUN_ID + 1))
        state = _state()
        publication._apply_build_run_snapshot(state, _run(source="c" * 40))
        self.assertFalse(state["source_matches"])

    def test_state_round_trip_rejects_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / "build.json"
            publication._write_state(path, _state())
            self.assertEqual(publication._load_state(path)["request_id"], REQUEST)
            path.unlink()
            target = root / "target.json"
            target.write_text("{}\n", encoding="utf-8")
            path.symlink_to(target)
            with self.assertRaises(publication.PublicationError):
                publication._load_state(path)


class _Response:
    status = 204

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def read(self, _size=-1):
        return b""

    def getcode(self):
        return self.status


class _Opener:
    def __init__(self, outcome):
        self.outcome = outcome

    def open(self, request, timeout):
        if isinstance(self.outcome, Exception):
            raise self.outcome
        return self.outcome


class BuildDispatchTests(unittest.TestCase):
    def _client(self):
        return collector.GitHubClient(collector.DEFAULT_REPO, "fake-token", 5)

    def test_204_is_accepted(self) -> None:
        client = self._client()
        client.opener = _Opener(_Response())
        publication._post_build_dispatch(client, TAG, REQUEST)

    def test_4xx_is_rejected_and_transport_is_unknown(self) -> None:
        client = self._client()
        client.opener = _Opener(urllib.error.HTTPError("https://api.github.test", 422, "bad", {}, None))
        with self.assertRaises(publication.DispatchRejected):
            publication._post_build_dispatch(client, TAG, REQUEST)
        client.opener = _Opener(urllib.error.URLError("lost"))
        with self.assertRaises(publication.DispatchOutcomeUnknown):
            publication._post_build_dispatch(client, TAG, REQUEST)


class DraftReleaseLookupTests(unittest.TestCase):
    def test_authenticated_lookup_uses_release_listing_not_tag_endpoint(self) -> None:
        release = {"id": 99, "tag_name": TAG, "draft": True, "prerelease": False}
        client = mock.sentinel.github_client
        with mock.patch.object(publication, "_github_json_array", return_value=[release]) as fetch:
            found = publication._find_authenticated_release_by_tag(client, TAG)
        self.assertIs(found, release)
        suffix = fetch.call_args.args[1]
        self.assertTrue(suffix.startswith("/releases?"))
        self.assertNotIn("/releases/tags/", suffix)

    def test_authenticated_lookup_fails_closed_on_missing_or_duplicate_tag(self) -> None:
        client = mock.sentinel.github_client
        other = {"id": 1, "tag_name": "v0.3.7", "draft": True, "prerelease": False}
        with mock.patch.object(publication, "_github_json_array", return_value=[other]):
            with self.assertRaisesRegex(publication.PublicationError, "was not found"):
                publication._find_authenticated_release_by_tag(client, TAG)

        release = {"id": 99, "tag_name": TAG, "draft": True, "prerelease": False}
        with mock.patch.object(publication, "_github_json_array", return_value=[release, dict(release)]):
            with self.assertRaisesRegex(publication.PublicationError, "duplicate tag"):
                publication._find_authenticated_release_by_tag(client, TAG)


class NpmStagingTests(unittest.TestCase):
    def test_stage_npm_uses_existing_bundle_binaries_and_never_cargo(self) -> None:
        summary = {
            "tag": TAG,
            "version": VERSION,
            "source_sha": SOURCE,
            "built_at": 1234567890,
            "build_kind": "release",
            "workflow_run_id": RUN_ID,
            "archive_stem": f"webcodex-v{VERSION}",
            "artifacts": {platform: "a" * 64 for platform in collector.PLATFORMS},
            "desktop_artifacts": {
                platform: {
                    "filename": collector.desktop_artifact_filename(
                        VERSION,
                        platform,
                        build_kind="release",
                        tag=TAG,
                        source_sha=SOURCE,
                    ),
                    "sha256": "b" * 64,
                }
                for platform in collector.DESKTOP_PLATFORMS
            },
        }
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bundle = root / "bundle"
            source = root / "source"
            output = root / "stage"
            bundle.mkdir()
            source.mkdir()
            (bundle / "manifest.json").write_text("{}\n", encoding="utf-8")
            calls: list[list[str]] = []

            def record(argv, **_kwargs):
                calls.append(list(argv))

            with mock.patch.object(publication, "verify_bundle", return_value=summary), mock.patch.object(
                publication, "_require_exact_clean_root"
            ), mock.patch.object(publication, "_git", return_value=SOURCE), mock.patch.object(
                publication, "_extract_linux_x64_binaries"
            ) as extract, mock.patch.object(publication, "_run_checked", side_effect=record):
                result = publication.stage_npm(
                    repo=collector.DEFAULT_REPO,
                    bundle_dir=bundle,
                    source_root=source,
                    output_dir=output,
                )

        self.assertEqual(result["npm_smoke"], "passed")
        self.assertEqual(len(calls), 2)
        self.assertTrue(calls[0][1].endswith("scripts/stage_npm_release.sh"))
        self.assertTrue(calls[1][1].endswith("scripts/npm_package_smoke.sh"))
        self.assertIn("--binary-dir", calls[1])
        self.assertFalse(any("cargo" in argument for call in calls for argument in call))
        extract.assert_called_once()


class DraftVerificationTests(unittest.TestCase):
    def test_draft_asset_digests_replace_full_redownload(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            meta = _write_bundle(root)
            desktop_names = [
                meta["desktop_artifacts"][platform]["filename"] for platform in collector.DESKTOP_PLATFORMS
            ]
            names = [
                "SHA256SUMS",
                *(entry["filename"] for entry in meta["artifacts"].values()),
                *desktop_names,
            ]
            assets = []
            for index, name in enumerate(names, 1):
                path = root / name
                assets.append(
                    {
                        "id": index,
                        "name": name,
                        "state": "uploaded",
                        "size": path.stat().st_size,
                        "digest": "sha256:" + collector.sha256_file(path),
                    }
                )
            release = {
                "id": 99,
                "tag_name": TAG,
                "draft": True,
                "prerelease": False,
                "html_url": f"https://github.com/{collector.DEFAULT_REPO}/releases/tag/{TAG}",
                "assets": assets,
            }
            with mock.patch.object(publication.collector, "resolve_github_token", return_value="fake"), mock.patch.object(
                publication.collector, "GitHubClient", return_value=mock.sentinel.github_client
            ), mock.patch.object(publication, "_find_authenticated_release_by_tag", return_value=release):
                summary = publication.verify_draft_assets(
                    repo=collector.DEFAULT_REPO,
                    bundle_dir=root,
                    timeout=5,
                )
            self.assertTrue(summary["draft"])
            self.assertEqual(len(summary["assets"]), 10)

            desktop_name = meta["desktop_artifacts"]["darwin-arm64"]["filename"]
            desktop_asset = next(asset for asset in release["assets"] if asset["name"] == desktop_name)
            desktop_asset["digest"] = "sha256:" + "0" * 64
            with mock.patch.object(publication.collector, "resolve_github_token", return_value="fake"), mock.patch.object(
                publication.collector, "GitHubClient", return_value=mock.sentinel.github_client
            ), mock.patch.object(publication, "_find_authenticated_release_by_tag", return_value=release):
                with self.assertRaises(publication.PublicationError):
                    publication.verify_draft_assets(
                        repo=collector.DEFAULT_REPO,
                        bundle_dir=root,
                        timeout=5,
                    )

            release["assets"] = [asset for asset in release["assets"] if asset["name"] != desktop_name]
            with mock.patch.object(publication.collector, "resolve_github_token", return_value="fake"), mock.patch.object(
                publication.collector, "GitHubClient", return_value=mock.sentinel.github_client
            ), mock.patch.object(publication, "_find_authenticated_release_by_tag", return_value=release):
                with self.assertRaises(publication.PublicationError):
                    publication.verify_draft_assets(
                        repo=collector.DEFAULT_REPO,
                        bundle_dir=root,
                        timeout=5,
                    )


class NpmPublicationContractTests(unittest.TestCase):
    def test_package_pins_public_npm_registry(self) -> None:
        package = json.loads(Path("npm/webcodex/package.json").read_text(encoding="utf-8"))
        self.assertEqual(package["publishConfig"]["access"], "public")
        self.assertEqual(package["publishConfig"]["registry"], publication.NPM_REGISTRY)


class WorkflowContractTests(unittest.TestCase):
    def test_release_build_exposes_durable_request_id(self) -> None:
        workflow = Path(".github/workflows/release-build.yml").read_text(encoding="utf-8")
        self.assertIn("run-name: Release build ${{ inputs.tag }} ${{ inputs.request_id }}", workflow)
        self.assertIn("^rb_[0-9a-f]{24}$", workflow)
        self.assertIn("default: manual", workflow)
        self.assertIn("platform: darwin-x64", workflow)
        self.assertIn("runner: macos-15-intel", workflow)
        self.assertIn("rust_host: x86_64-apple-darwin", workflow)
        self.assertNotIn('rustc -vV | grep -Fxq "host:', workflow)
        self.assertNotIn('file "$binary" | grep -Fq "$EXPECTED_FILE_ARCH"', workflow)

    def test_release_build_stages_desktop_candidates_in_workspace_dist(self) -> None:
        workflow = Path(".github/workflows/release-build.yml").read_text(encoding="utf-8")

        self.assertIn('desktop_dist="$GITHUB_WORKSPACE/dist"', workflow)
        self.assertIn('desktop="$desktop_dist/${{ steps.desktop_bundle.outputs.desktop_name }}"', workflow)
        self.assertIn('printf \'%s  %s\\n\' "$digest" "$(basename "$desktop")" > "$desktop.sha256"', workflow)
        self.assertIn('echo "path=$desktop" >> "$GITHUB_OUTPUT"', workflow)
        self.assertIn('--dmg "${{ steps.desktop_dmg.outputs.path }}"', workflow)
        self.assertIn(
            '--evidence "$GITHUB_WORKSPACE/dist/${{ steps.desktop_bundle.outputs.desktop_name }}.evidence.json"',
            workflow,
        )

        self.assertIn('$desktopDist = Join-Path $env:GITHUB_WORKSPACE "dist"', workflow)
        self.assertIn('$installerPath = Join-Path $desktopDist $installerName', workflow)
        self.assertIn('"DESKTOP_INSTALLER_PATH=$installerPath" >> $env:GITHUB_ENV', workflow)
        self.assertIn('Copy-Item -LiteralPath $built[0].FullName -Destination $env:DESKTOP_INSTALLER_PATH', workflow)
        self.assertIn('"$($env:DESKTOP_INSTALLER_PATH).sha256"', workflow)
        self.assertIn('-Installer $env:DESKTOP_INSTALLER_PATH', workflow)

        self.assertIn("dist/webcodex-desktop-*.dmg", workflow)
        self.assertIn("dist/webcodex-desktop-*.dmg.sha256", workflow)
        self.assertIn("dist/webcodex-desktop-*.dmg.evidence.json", workflow)
        self.assertIn("dist/webcodex-desktop-*-win32-x64-setup.exe", workflow)
        self.assertIn("dist/webcodex-desktop-*-win32-x64-setup.exe.sha256", workflow)

        self.assertNotIn('desktop="dist/${{ steps.desktop_bundle.outputs.desktop_name }}"', workflow)
        self.assertNotIn('$installer = Join-Path "dist" $env:DESKTOP_INSTALLER_NAME', workflow)
        self.assertNotIn('-Installer (Join-Path "dist" $env:DESKTOP_INSTALLER_NAME)', workflow)

    def test_server_image_publication_is_separate_and_multi_arch(self) -> None:
        candidate = Path(".github/workflows/release-build.yml").read_text(encoding="utf-8")
        image = Path(".github/workflows/release-image.yml").read_text(encoding="utf-8")
        self.assertIn("permissions:\n  contents: read\n", candidate)
        self.assertNotIn("packages: write", candidate)
        self.assertIn("types: [published]", image)
        self.assertIn("packages: write", image)
        self.assertIn("platform: linux/amd64", image)
        self.assertIn("platform: linux/arm64", image)
        self.assertIn("runner: ubuntu-24.04-arm", image)
        self.assertIn("push-by-digest=true", image)
        self.assertIn("webcodex-server-image.json", image)
        self.assertIn("scripts/prepare_server_deployment_assets.py", image)
        self.assertIn("validate_server_image_release_record", image)
        self.assertIn("ref: ${{ github.workflow_sha }}", image)
        self.assertIn("deployment_source_sha", image)
        self.assertIn("webcodex-server-bootstrap.sh", image)
        self.assertIn("webcodex-server-compose.yaml", image)
        self.assertIn("durable_record_exists=false", image)
        self.assertIn("Existing immutable GitHub Release deployment record reconciled without regeneration.", image)
        self.assertIn("Require anonymous GHCR availability", image)
        self.assertIn('gh release download "$TAG" --repo "$GITHUB_REPOSITORY"', image)

    def test_compose_defaults_to_published_image_with_explicit_source_override(self) -> None:
        compose = Path("compose.yaml").read_text(encoding="utf-8")
        source = Path("compose.build.yaml").read_text(encoding="utf-8")
        bootstrap = Path("deploy/docker/bootstrap.sh").read_text(encoding="utf-8")
        self.assertIn("ghcr.io/yyjeqhc/webcodex-server:latest", compose)
        self.assertIn("pull_policy: always", compose)
        self.assertNotIn("build:\n", compose)
        self.assertIn("webcodex-server-local", source)
        self.assertIn("pull_policy: build", source)
        self.assertIn("build:\n", source)
        self.assertIn("--build-from-source", bootstrap)
        self.assertIn("COMPOSE_FILE=${COMPOSE_FILE:-compose.yaml}", bootstrap)
        self.assertIn("compose_base config --images", bootstrap)
        self.assertIn("compose_base pull webcodex", bootstrap)
        self.assertIn("compose_full up -d --build", bootstrap)


if __name__ == "__main__":
    unittest.main()
