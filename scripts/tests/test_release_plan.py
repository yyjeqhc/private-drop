import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import release_plan as plan
from scripts import release_publication as publication


SOURCE = "a" * 40
VERSION = "0.4.0"
TAG = f"v{VERSION}"


def _state(root: Path, *, phase: str) -> dict:
    work = root / "work"
    work.mkdir(exist_ok=True)
    now = 1_900_000_000
    return {
        "schema_version": plan.STATE_SCHEMA_VERSION,
        "kind": plan.KIND,
        "repo": "yyjeqhc/webcodex",
        "version": VERSION,
        "tag": TAG,
        "source_sha": SOURCE,
        "root": str(root.absolute()),
        "work_dir": str(work.absolute()),
        "readiness_state_file": str((work / "readiness.json").absolute()),
        "build_state_file": str((work / "build.json").absolute()),
        "bundle_dir": str((work / "bundle").absolute()),
        "stage_dir": str((work / "npm-stage").absolute()),
        "phase": phase,
        "created_at": now,
        "updated_at": now,
        "readiness_run_id": None,
        "build_run_id": None,
        "last_action": "test",
    }


class ReleasePlanStateTests(unittest.TestCase):
    def test_state_round_trip_is_mode_0600_and_rejects_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = root / "state.json"
            plan._write_state(state_path, _state(root, phase=plan.PHASE_PREFLIGHT))
            loaded = plan._load_state(state_path)
            self.assertEqual(loaded["phase"], plan.PHASE_PREFLIGHT)
            if os.name != "nt":
                self.assertEqual(stat.S_IMODE(state_path.stat().st_mode), 0o600)
            state_path.unlink()
            target = root / "target.json"
            target.write_text("{}\n", encoding="utf-8")
            state_path.symlink_to(target)
            with self.assertRaises(plan.ReleasePlanError):
                plan._load_state(state_path)

    def test_init_runs_preflight_once_and_records_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = root / "state.json"
            work = root / "release-work"
            with mock.patch.object(publication, "preflight_release", return_value={"version": VERSION}) as preflight:
                summary = plan.init_plan(
                    repo="yyjeqhc/webcodex",
                    version=VERSION,
                    source_sha=SOURCE,
                    root=root,
                    state_file=state_path,
                    work_dir=work,
                    timeout=30.0,
                )
            preflight.assert_called_once()
            loaded = plan._load_state(state_path)
            self.assertEqual(loaded["phase"], plan.PHASE_PREFLIGHT)
            self.assertEqual(Path(loaded["work_dir"]), work.absolute())
            self.assertEqual(summary["status"], "ready")


class ReleasePlanStatusTests(unittest.TestCase):
    def test_status_preserves_durable_wait_failure_reconciliation_and_authorization(self) -> None:
        cases = (
            (plan.PHASE_READINESS, "readiness_waiting", "waiting"),
            (plan.PHASE_READINESS, "readiness_failed", "failed"),
            (plan.PHASE_BUILD, "build_failed", "failed"),
            (plan.PHASE_BUNDLE, "stage_requires_reconciliation", "needs_reconciliation"),
            (plan.PHASE_AWAIT_TAG, "awaiting_tag_authorization", "needs_authorization"),
            (plan.PHASE_PREFLIGHT, "preflight_passed", "ready"),
        )
        for phase, last_action, expected_status in cases:
            with self.subTest(phase=phase, last_action=last_action):
                with tempfile.TemporaryDirectory() as temp:
                    root = Path(temp)
                    state = _state(root, phase=phase)
                    state["last_action"] = last_action
                    state_path = root / "state.json"
                    plan._write_state(state_path, state)
                    summary = plan.status_plan(state_file=state_path)
                    self.assertEqual(summary["status"], expected_status)
                    if expected_status == "ready":
                        self.assertNotIn("next_action", summary)
                    else:
                        self.assertIn("next_action", summary)


class ReleasePlanResumeTests(unittest.TestCase):
    def _write(self, root: Path, phase: str) -> Path:
        path = root / "state.json"
        plan._write_state(path, _state(root, phase=phase))
        return path

    def test_existing_readiness_state_is_recovered_without_redispatch(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = self._write(root, plan.PHASE_PREFLIGHT)
            nested = Path(plan._load_state(state_path)["readiness_state_file"])
            nested.write_text("placeholder", encoding="utf-8")
            with (
                mock.patch.object(plan.readiness, "start_readiness") as start,
                mock.patch.object(plan.readiness, "status_readiness", return_value=({"run_id": 33}, 2)) as status,
            ):
                summary, code = plan.resume_plan(state_file=state_path, timeout=30.0, wait_secs=0)
            self.assertEqual(code, 2)
            self.assertEqual(summary["status"], "waiting")
            start.assert_not_called()
            status.assert_called_once()
            self.assertEqual(plan._load_state(state_path)["phase"], plan.PHASE_READINESS)

    def test_awaiting_tag_is_explicit_authorization_boundary(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = self._write(root, plan.PHASE_AWAIT_TAG)
            with mock.patch.object(plan, "_tag_source", return_value=None):
                summary, code = plan.resume_plan(state_file=state_path, timeout=30.0, wait_secs=0)
            self.assertEqual(code, 3)
            self.assertEqual(summary["status"], "needs_authorization")
            self.assertIn("create and push annotated", summary["next_action"])

    def test_existing_tag_advances_to_one_bound_build_and_waits(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = self._write(root, plan.PHASE_AWAIT_TAG)
            with (
                mock.patch.object(plan, "_tag_source", return_value=SOURCE),
                mock.patch.object(publication, "start_build", return_value=({"run_id": 44}, 0)) as start,
                mock.patch.object(publication, "status_build", return_value=({"run_id": 44}, 2)) as status,
            ):
                summary, code = plan.resume_plan(state_file=state_path, timeout=30.0, wait_secs=0)
            self.assertEqual(code, 2)
            self.assertEqual(summary["status"], "waiting")
            self.assertEqual(plan._load_state(state_path)["build_run_id"], 44)
            start.assert_called_once()
            status.assert_called_once()

    def test_existing_build_state_is_recovered_without_redispatch(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = self._write(root, plan.PHASE_AWAIT_TAG)
            nested = Path(plan._load_state(state_path)["build_state_file"])
            nested.write_text("placeholder", encoding="utf-8")
            with (
                mock.patch.object(plan, "_tag_source", return_value=SOURCE),
                mock.patch.object(publication, "start_build") as start,
                mock.patch.object(publication, "status_build", return_value=({"run_id": 44}, 2)) as status,
            ):
                summary, code = plan.resume_plan(state_file=state_path, timeout=30.0, wait_secs=0)
            self.assertEqual(code, 2)
            self.assertEqual(summary["status"], "waiting")
            start.assert_not_called()
            status.assert_called_once()
            self.assertEqual(plan._load_state(state_path)["phase"], plan.PHASE_BUILD)

    def test_successful_build_collects_and_stages_then_pauses_for_draft(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state = _state(root, phase=plan.PHASE_BUILD_PASSED)
            state["build_run_id"] = 55
            state_path = root / "state.json"
            plan._write_state(state_path, state)
            with (
                mock.patch.object(collector := plan.collector, "collect_bundle", return_value={"source_sha": SOURCE, "tag": TAG}) as collect,
                mock.patch.object(publication, "stage_npm", return_value={"npm_smoke": "passed"}) as stage,
                mock.patch.object(
                    publication,
                    "verify_draft_assets",
                    side_effect=publication.PublicationError(f"GitHub draft Release was not found for tag: {TAG}"),
                ),
            ):
                summary, code = plan.resume_plan(state_file=state_path, timeout=30.0, wait_secs=0)
            self.assertEqual(code, 3)
            self.assertEqual(summary["phase"], plan.PHASE_AWAIT_DRAFT)
            collect.assert_called_once()
            stage.assert_called_once()

    def test_unrecorded_existing_stage_fails_closed_for_reconciliation(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = self._write(root, plan.PHASE_BUNDLE)
            Path(plan._load_state(state_path)["stage_dir"]).mkdir()
            summary, code = plan.resume_plan(state_file=state_path, timeout=30.0, wait_secs=0)
            self.assertEqual(code, 4)
            self.assertEqual(summary["status"], "needs_reconciliation")

    def test_verified_draft_pauses_before_publication(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            state_path = self._write(root, plan.PHASE_AWAIT_DRAFT)
            with mock.patch.object(publication, "verify_draft_assets", return_value={"draft": True}) as verify:
                summary, code = plan.resume_plan(state_file=state_path, timeout=30.0, wait_secs=0)
            self.assertEqual(code, 3)
            self.assertEqual(summary["phase"], plan.PHASE_AWAIT_PUBLICATION)
            self.assertIn("publish the verified GitHub draft", summary["next_action"])
            verify.assert_called_once()


if __name__ == "__main__":
    unittest.main()
