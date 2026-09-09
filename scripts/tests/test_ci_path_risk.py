from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import ci_path_risk as risk


def classify(
    *paths: str,
    statuses: tuple[str, ...] | None = None,
    platform_diff: str = "",
) -> dict[str, str]:
    if statuses is None:
        statuses = ("M",) * len(paths)
    changes = [risk.Change(status=status, path=path) for status, path in zip(statuses, paths)]
    return risk.classify_changes(changes, platform_diff).outputs()


class PathRiskFixtureTests(unittest.TestCase):
    def test_docs_only_does_not_upgrade_native(self) -> None:
        result = classify("docs/TESTING.md")
        self.assertEqual(result["needs_full_native"], "false")
        self.assertEqual(result["needs_windows"], "false")
        self.assertEqual(result["needs_macos"], "false")
        self.assertEqual(result["needs_linux_arm64"], "false")

    def test_desktop_rust_requires_windows_macos_and_desktop_packages(self) -> None:
        result = classify("apps/desktop/src-tauri/src/process/supervisor.rs")
        self.assertEqual(result["needs_windows_desktop"], "true")
        self.assertEqual(result["needs_macos"], "true")
        self.assertEqual(result["needs_macos_desktop"], "true")
        self.assertEqual(result["needs_desktop_package"], "true")

    def test_process_requires_windows_core_and_macos_without_desktop_package(self) -> None:
        result = classify("crates/webcodex-process/src/lib.rs")
        self.assertEqual(result["needs_windows_core"], "true")
        self.assertEqual(result["needs_macos"], "true")
        self.assertEqual(result["needs_desktop_package"], "false")

    def test_runner_plugin_requires_windows_runner_and_macos(self) -> None:
        result = classify("crates/webcodex-runner/src/webcodex_runner/plugin.rs")
        self.assertEqual(result["needs_windows_runner"], "true")
        self.assertEqual(result["needs_macos"], "true")

    def test_npm_installer_change_requires_native_windows_package_lane(self) -> None:
        result = classify("npm/webcodex/install.js")
        self.assertEqual(result["needs_windows_package"], "true")
        self.assertEqual(result["needs_windows_desktop"], "false")
        self.assertEqual(result["needs_macos"], "false")

    def test_runner_shell_and_persistent_shell_require_windows_and_macos(self) -> None:
        runner = classify("crates/webcodex-runner/src/webcodex_runner/shell.rs")
        self.assertEqual(runner["needs_windows"], "true")
        self.assertEqual(runner["needs_macos"], "true")

        persistent = classify("crates/webcodex-persistent-shell/src/lib.rs")
        self.assertEqual(persistent["needs_windows"], "true")
        self.assertEqual(persistent["needs_macos"], "true")

    def test_runner_process_owners_require_native_runner_lanes(self) -> None:
        for path in (
            "crates/webcodex-runner/src/webcodex_runner/coding_agent.rs",
            "crates/webcodex-runner/src/webcodex_runner/detached_job/tests.rs",
            "crates/webcodex-runner/src/webcodex_runner/external_tools.rs",
            "crates/webcodex-runner/src/webcodex_runner/job_manager.rs",
            "crates/webcodex-runner/src/webcodex_runner/lsp/tests.rs",
            "crates/webcodex-runner/src/webcodex_runner/mcp_gateway.rs",
            "crates/webcodex-runner/src/webcodex_runner/projects.rs",
            "crates/webcodex-runner/src/webcodex_runner/ssh.rs",
            "crates/webcodex-runner/src/webcodex_runner/validation/execute.rs",
        ):
            with self.subTest(path=path):
                result = classify(path)
                self.assertEqual(result["needs_windows_runner"], "true")
                self.assertEqual(result["needs_macos"], "true")

    def test_windows_installer_and_npm_package_choose_windows_package_lanes(self) -> None:
        desktop = classify("scripts/desktop_install_windows_smoke.ps1")
        self.assertEqual(desktop["needs_windows_desktop"], "true")
        self.assertEqual(desktop["needs_windows_package"], "true")
        self.assertEqual(desktop["needs_desktop_package"], "true")
        self.assertEqual(desktop["needs_macos"], "false")

        npm = classify("scripts/npm_install_windows_smoke.ps1")
        self.assertEqual(npm["needs_windows_package"], "true")
        self.assertEqual(npm["needs_windows_desktop"], "false")

        msi = classify("packaging/windows/webcodex-desktop.msi")
        self.assertEqual(msi["needs_windows_package"], "true")
        self.assertEqual(msi["needs_windows_desktop"], "true")
        self.assertEqual(msi["needs_macos"], "false")

    def test_macos_packaging_is_macos_only(self) -> None:
        result = classify("scripts/prepare_desktop_bundle_macos.py")
        self.assertEqual(result["needs_macos"], "true")
        self.assertEqual(result["needs_macos_desktop"], "true")
        self.assertEqual(result["needs_windows"], "false")

        dmg = classify("packaging/macos/WebCodex.dmg")
        self.assertEqual(dmg["needs_macos"], "true")
        self.assertEqual(dmg["needs_macos_desktop"], "true")
        self.assertEqual(dmg["needs_windows"], "false")

    def test_release_or_signing_is_full_native(self) -> None:
        for path in (
            ".github/workflows/release-build.yml",
            "scripts/macos_sign_local_runner.sh",
        ):
            with self.subTest(path=path):
                result = classify(path)
                self.assertEqual(result["needs_full_native"], "true")
                self.assertEqual(result["needs_windows_arm64"], "true")
                self.assertEqual(result["needs_linux_arm64"], "true")
                self.assertEqual(result["needs_macos_desktop"], "true")

    def test_any_workflow_policy_change_is_full_native(self) -> None:
        result = classify(".github/workflows/future-native-policy.yml")
        self.assertEqual(result["needs_full_native"], "true")
        self.assertIn("ci-policy", result["categories"])

    def test_mixed_docs_and_process_uses_highest_risk(self) -> None:
        result = classify("docs/README.md", "crates/webcodex-process/src/windows.rs")
        self.assertEqual(result["needs_windows_core"], "true")
        self.assertEqual(result["needs_macos"], "true")

    def test_rename_into_risky_path_classifies_destination(self) -> None:
        result = classify(
            "docs/old.rs",
            "crates/webcodex-process/src/renamed.rs",
            statuses=("D", "A"),
        )
        self.assertEqual(result["needs_windows_core"], "true")
        self.assertEqual(result["needs_macos"], "true")

    def test_deleted_risky_file_still_requires_native(self) -> None:
        result = classify("crates/webcodex-process/src/windows.rs", statuses=("D",))
        self.assertEqual(result["needs_windows_core"], "true")
        self.assertEqual(result["needs_macos"], "true")

    def test_platform_cfg_change_upgrades_native_even_from_normal_rust_path(self) -> None:
        result = classify(
            "src/runtime.rs",
            platform_diff='+#[cfg(target_os = "windows")]\n-fn old() {}',
        )
        self.assertEqual(result["needs_windows_core"], "true")
        self.assertEqual(result["needs_macos"], "true")
        self.assertIn("platform-cfg", result["categories"])

    def test_aarch64_cfg_requests_all_architecture_native_lanes(self) -> None:
        result = classify(
            "src/runtime.rs",
            platform_diff='+#[cfg(target_arch = "aarch64")]\n+fn arm_only() {}',
        )
        self.assertEqual(result["needs_linux_arm64"], "true")
        self.assertEqual(result["needs_windows_arm64"], "true")
        self.assertEqual(result["needs_macos"], "true")
        self.assertIn("aarch64-cfg", result["categories"])

    def test_changed_paths_are_repository_relative_and_bounded(self) -> None:
        with self.assertRaises(risk.DiffLimitExceeded):
            classify("../escape.rs")
        with self.assertRaises(risk.DiffLimitExceeded):
            classify("x" * (risk.MAX_PATH_BYTES + 1))


class InvocationOverrideFixtureTests(unittest.TestCase):
    def test_run_ci_override_forces_full_native(self) -> None:
        forced = risk.forced_risk_for_invocation(
            "pull_request", external_contributor=False, run_ci=True
        )
        self.assertIsNotNone(forced)
        assert forced is not None
        result = forced.outputs()
        self.assertEqual(result["needs_full_native"], "true")
        self.assertIn("override-run-ci", result["reason"])

    def test_push_main_forces_full_native(self) -> None:
        forced = risk.forced_risk_for_invocation(
            "push", external_contributor=False, run_ci=False
        )
        self.assertIsNotNone(forced)
        assert forced is not None
        self.assertEqual(forced.outputs()["needs_full_native"], "true")

    def test_external_contributor_preserves_full_native_policy(self) -> None:
        forced = risk.forced_risk_for_invocation(
            "pull_request", external_contributor=True, run_ci=False
        )
        self.assertIsNotNone(forced)
        assert forced is not None
        self.assertEqual(forced.outputs()["needs_full_native"], "true")

    def test_owner_pr_without_override_uses_path_classifier(self) -> None:
        self.assertIsNone(
            risk.forced_risk_for_invocation(
                "pull_request", external_contributor=False, run_ci=False
            )
        )


class GitRangeIntegrationTests(unittest.TestCase):
    def init_repo(self, root: Path) -> None:
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(
            ["git", "config", "user.email", "ci-risk@example.invalid"],
            cwd=root,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "CI Risk Fixture"],
            cwd=root,
            check=True,
        )

    def commit(self, root: Path, message: str) -> str:
        subprocess.run(["git", "add", "."], cwd=root, check=True)
        subprocess.run(["git", "commit", "-q", "-m", message], cwd=root, check=True)
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=root, text=True
        ).strip()

    def test_platform_context_bound_falls_back_to_native_core_without_packaging(self) -> None:
        change = risk.Change(status="M", path="src/runtime.rs")
        with (
            mock.patch.object(risk, "_git_changes", return_value=[change]),
            mock.patch.object(
                risk,
                "_git_platform_context",
                side_effect=risk.DiffLimitExceeded("fixture bound"),
            ),
        ):
            result = risk.classify_git_range("0" * 40, "1" * 40).outputs()
        self.assertEqual(result["needs_full_native"], "false")
        self.assertEqual(result["needs_windows_core"], "true")
        self.assertEqual(result["needs_macos"], "true")
        self.assertEqual(result["needs_linux_arm64"], "true")
        self.assertEqual(result["needs_windows_arm64"], "true")
        self.assertEqual(result["needs_windows_package"], "false")
        self.assertEqual(result["needs_windows_desktop"], "false")
        self.assertEqual(result["needs_macos_desktop"], "false")
        self.assertIn("platform-context-bounded", result["categories"])

    def test_changed_path_bound_still_falls_back_to_full_native(self) -> None:
        with mock.patch.object(
            risk,
            "_git_changes",
            side_effect=risk.DiffLimitExceeded("fixture path bound"),
        ):
            result = risk.classify_git_range("0" * 40, "1" * 40).outputs()
        self.assertEqual(result["needs_full_native"], "true")
        self.assertIn("changed-path-bounded-fallback", result["categories"])

    def test_broad_rust_change_does_not_escalate_to_packaging_or_arm64(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.init_repo(root)
            sources = []
            for index in range(14):
                source = root / "src" / f"wide_{index:02}.rs"
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text(
                    f"pub const VALUE_{index}: usize = {index};\n" + "// filler\n" * 18000,
                    encoding="utf-8",
                )
                sources.append(source)
            runner = root / "crates" / "webcodex-runner" / "src" / "webcodex_runner" / "projects.rs"
            runner.parent.mkdir(parents=True, exist_ok=True)
            runner.write_text("pub fn managed() -> usize { 1 }\n" + "// runner filler\n" * 12000, encoding="utf-8")
            sources.append(runner)
            base = self.commit(root, "broad base")

            for source in sources:
                source.write_text(source.read_text(encoding="utf-8") + "// changed\n", encoding="utf-8")
            head = self.commit(root, "broad rust change")

            previous = Path.cwd()
            try:
                os.chdir(root)
                result = risk.classify_git_range(base, head).outputs()
            finally:
                os.chdir(previous)

            self.assertEqual(result["needs_full_native"], "false")
            self.assertEqual(result["needs_windows_runner"], "true")
            self.assertEqual(result["needs_macos"], "true")
            self.assertEqual(result["needs_windows_package"], "false")
            self.assertEqual(result["needs_windows_desktop"], "false")
            self.assertEqual(result["needs_macos_desktop"], "false")
            self.assertEqual(result["needs_linux_arm64"], "false")
            self.assertEqual(result["needs_windows_arm64"], "false")

    def test_real_git_rename_is_observed_as_delete_plus_add_and_upgrades_risk(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.init_repo(root)
            old = root / "docs" / "old.rs"
            old.parent.mkdir(parents=True)
            old.write_text("fn fixture() {}\n", encoding="utf-8")
            base = self.commit(root, "base")

            new = root / "crates" / "webcodex-process" / "src" / "renamed.rs"
            new.parent.mkdir(parents=True)
            subprocess.run(["git", "mv", str(old.relative_to(root)), str(new.relative_to(root))], cwd=root, check=True)
            head = self.commit(root, "rename")

            previous = Path.cwd()
            try:
                os.chdir(root)
                changes = risk._git_changes(base, head)
                result = risk.classify_git_range(base, head).outputs()
            finally:
                os.chdir(previous)

            self.assertEqual(
                sorted((change.status, change.path) for change in changes),
                sorted(
                    [
                        ("D", "docs/old.rs"),
                        ("A", "crates/webcodex-process/src/renamed.rs"),
                    ]
                ),
            )
            self.assertEqual(result["needs_windows_core"], "true")
            self.assertEqual(result["needs_macos"], "true")

    def test_body_only_edit_inside_existing_windows_cfg_upgrades_native(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.init_repo(root)
            source = root / "src" / "runtime.rs"
            source.parent.mkdir(parents=True)
            source.write_text(
                '#[cfg(windows)]\nfn platform_value() -> u32 {\n    1\n}\n',
                encoding="utf-8",
            )
            base = self.commit(root, "base")
            source.write_text(
                '#[cfg(windows)]\nfn platform_value() -> u32 {\n    2\n}\n',
                encoding="utf-8",
            )
            head = self.commit(root, "body-only windows change")

            previous = Path.cwd()
            try:
                os.chdir(root)
                result = risk.classify_git_range(base, head).outputs()
            finally:
                os.chdir(previous)

            self.assertEqual(result["needs_windows_core"], "true")
            self.assertEqual(result["needs_macos"], "true")
            self.assertIn("platform-cfg", result["categories"])

    def test_body_only_edit_inside_existing_target_cargo_section_upgrades_native(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.init_repo(root)
            manifest = root / "crates" / "example" / "Cargo.toml"
            manifest.parent.mkdir(parents=True)
            manifest.write_text(
                "[package]\nname = \"example\"\nversion = \"0.1.0\"\n\n"
                "[target.'cfg(windows)'.dependencies]\nwindows-sys = \"0.60\"\n",
                encoding="utf-8",
            )
            base = self.commit(root, "base")
            manifest.write_text(
                "[package]\nname = \"example\"\nversion = \"0.1.0\"\n\n"
                "[target.'cfg(windows)'.dependencies]\nwindows-sys = \"0.61\"\n",
                encoding="utf-8",
            )
            head = self.commit(root, "target dependency change")

            previous = Path.cwd()
            try:
                os.chdir(root)
                result = risk.classify_git_range(base, head).outputs()
            finally:
                os.chdir(previous)

            self.assertEqual(result["needs_windows_core"], "true")
            self.assertEqual(result["needs_macos"], "true")
            self.assertIn("platform-cfg", result["categories"])


if __name__ == "__main__":
    unittest.main()
