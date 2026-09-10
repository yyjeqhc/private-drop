import unittest
from pathlib import Path
from unittest import mock

from scripts import release_doctor as doctor
from scripts import release_publication as publication
from scripts import release_readiness as readiness


SOURCE = "b" * 40
VERSION = "0.4.0"


class ReleaseDoctorTests(unittest.TestCase):
    def test_current_repository_release_contract_is_six_platform(self) -> None:
        detail = doctor._platform_contract(Path.cwd())
        self.assertIn("darwin-x64", detail)
        self.assertIn("win32-arm64", detail)
        self.assertIn("Desktop darwin-x64, darwin-arm64, win32-x64, win32-arm64", detail)
        workflow = doctor._workflow_contract(Path.cwd())
        self.assertIn("authoritative build", workflow)

    def test_version_contract_rejects_desktop_mismatch(self) -> None:
        versions = {
            "cargo": VERSION,
            "npm": VERSION,
            "desktop_package": "0.3.9",
            "desktop_cargo": VERSION,
            "desktop_tauri": VERSION,
        }
        with mock.patch.object(publication, "_package_versions", return_value=versions), self.assertRaisesRegex(
            doctor.DoctorError, "desktop_package=0.3.9"
        ):
            doctor._version_contract(Path.cwd(), VERSION)

    def test_doctor_aggregates_read_only_preflight_and_ci_proof(self) -> None:
        ci = {
            "id": 123,
            "run_attempt": 2,
            "html_url": "https://github.com/yyjeqhc/webcodex/actions/runs/123",
        }
        with (
            mock.patch.object(doctor, "_require_tools", return_value="tools ok"),
            mock.patch.object(doctor, "_platform_contract", return_value="platforms ok"),
            mock.patch.object(doctor, "_version_contract", return_value="versions ok"),
            mock.patch.object(doctor, "_workflow_contract", return_value="workflows ok"),
            mock.patch.object(doctor, "_compile_verifiers", return_value="python ok"),
            mock.patch.object(doctor, "_actionlint", return_value="actionlint ok"),
            mock.patch.object(publication, "preflight_release", return_value={"version": VERSION}) as preflight,
            mock.patch.object(readiness, "_successful_main_ci_run", return_value=ci) as main_ci,
            mock.patch.object(doctor.collector, "resolve_github_token", return_value="token"),
            mock.patch.object(publication, "_github_json_array", return_value=[]),
        ):
            result = doctor.run_doctor(
                repo="yyjeqhc/webcodex",
                version=VERSION,
                source_sha=SOURCE,
                root=Path.cwd(),
                timeout=30.0,
            )
        self.assertEqual(result["status"], "passed")
        self.assertFalse(result["mutations_performed"])
        self.assertEqual(result["main_ci"]["run_id"], 123)
        self.assertEqual(result["failed_checks"], [])
        preflight.assert_called_once()
        main_ci.assert_called_once()

    def test_doctor_reports_failed_checks_without_running_mutations(self) -> None:
        with (
            mock.patch.object(doctor, "_require_tools", side_effect=doctor.DoctorError("missing gh")),
            mock.patch.object(doctor, "_platform_contract", return_value="platforms ok"),
            mock.patch.object(doctor, "_version_contract", return_value="versions ok"),
            mock.patch.object(doctor, "_workflow_contract", return_value="workflows ok"),
            mock.patch.object(doctor, "_compile_verifiers", return_value="python ok"),
            mock.patch.object(doctor, "_actionlint", return_value="optional"),
            mock.patch.object(publication, "preflight_release", side_effect=publication.PublicationError("npm unavailable")),
            mock.patch.object(readiness, "_successful_main_ci_run", side_effect=readiness.ReadinessError("CI missing")),
            mock.patch.object(doctor.collector, "resolve_github_token", return_value="token"),
            mock.patch.object(publication, "_github_json_array", return_value=[]),
        ):
            result = doctor.run_doctor(
                repo="yyjeqhc/webcodex",
                version=VERSION,
                source_sha=SOURCE,
                root=Path.cwd(),
                timeout=30.0,
            )
        self.assertEqual(result["status"], "failed")
        self.assertIn("required-tools", result["failed_checks"])
        self.assertIn("publication-preflight", result["failed_checks"])
        self.assertIn("exact-main-ci", result["failed_checks"])
        self.assertFalse(result["mutations_performed"])


if __name__ == "__main__":
    unittest.main()
