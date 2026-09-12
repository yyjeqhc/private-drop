"""Exercise Cargo invalidation against disposable Git metadata, without dependencies."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


BUILD_SCRIPT = Path(__file__).resolve().parents[2] / "crates/webcodex-core/build.rs"


class BuildIdentityTests(unittest.TestCase):
    def test_packed_branch_without_reflog_refreshes_and_then_stays_cached(self):
        for linked_worktree in (False, True):
            with self.subTest(linked_worktree=linked_worktree), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                env = os.environ.copy()
                for key in (
                    "WEBCODEX_GIT_COMMIT", "WEBCODEX_GIT_DIRTY", "WEBCODEX_BUILT_AT",
                    "SOURCE_DATE_EPOCH", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE",
                ):
                    env.pop(key, None)
                env["CARGO_TARGET_DIR"] = str(root / "target")

                def run(cwd, *args):
                    return subprocess.run(
                        args, cwd=cwd, env=env, check=True, capture_output=True,
                        text=True, timeout=60,
                    ).stdout.strip()

                repo = root / "repo"
                repo.mkdir()
                run(repo, "git", "init", "-b", "review/packed")
                run(repo, "git", "config", "core.logAllRefUpdates", "false")
                run(repo, "git", "config", "user.name", "Build fixture")
                run(repo, "git", "config", "user.email", "fixture@example.invalid")
                package = repo / "crates/fixture"
                (package / "src").mkdir(parents=True)
                (package / "Cargo.toml").write_text(
                    '[package]\nname = "identity-fixture"\nversion = "0.0.0"\n'
                    'edition = "2021"\n', encoding="utf-8",
                )
                shutil.copyfile(BUILD_SCRIPT, package / "build.rs")
                (package / "src/main.rs").write_text(
                    'fn main() { println!("{}", env!("WEBCODEX_BUILD_GIT_COMMIT")); }\n',
                    encoding="utf-8",
                )
                run(repo, "git", "add", ".")
                run(repo, "git", "commit", "-m", "fixture")
                if linked_worktree:
                    checkout = root / "checkout"
                    run(repo, "git", "worktree", "add", "-b", "review/linked", str(checkout))
                    repo = checkout
                    package = repo / "crates/fixture"
                run(repo, "git", "pack-refs", "--all", "--prune")
                head_log = Path(run(repo, "git", "rev-parse", "--git-path", "logs/HEAD"))
                self.assertFalse((repo / head_log).exists())

                def build():
                    return run(package, "cargo", "run", "--offline", "--quiet")

                before = build()
                outputs = list((root / "target/debug/build").glob("identity-fixture-*/output"))
                self.assertEqual(len(outputs), 1)
                stamp = outputs[0].stat().st_mtime_ns
                self.assertEqual(build(), before)
                self.assertEqual(outputs[0].stat().st_mtime_ns, stamp)
                run(repo, "git", "commit", "--allow-empty", "-m", "new identity")
                expected = run(repo, "git", "rev-parse", "--short=12", "HEAD")
                self.assertNotEqual(before, expected)
                self.assertEqual(build(), expected)
                stamp = outputs[0].stat().st_mtime_ns
                self.assertEqual(build(), expected)
                self.assertEqual(outputs[0].stat().st_mtime_ns, stamp)

    def test_tracked_dirty_state_refreshes_at_same_head(self):
        for linked_worktree in (False, True):
            with self.subTest(linked_worktree=linked_worktree), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                env = os.environ.copy()
                for key in (
                    "WEBCODEX_GIT_COMMIT", "WEBCODEX_GIT_DIRTY", "WEBCODEX_BUILT_AT",
                    "SOURCE_DATE_EPOCH", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE",
                ):
                    env.pop(key, None)
                env["CARGO_TARGET_DIR"] = str(root / "target")

                def run(cwd, *args):
                    return subprocess.run(
                        args, cwd=cwd, env=env, check=True, capture_output=True,
                        text=True, timeout=60,
                    ).stdout.strip()

                repo = root / "repo"
                repo.mkdir()
                run(repo, "git", "init", "-b", "review/dirty")
                run(repo, "git", "config", "user.name", "Build fixture")
                run(repo, "git", "config", "user.email", "fixture@example.invalid")
                (repo / "tracked.txt").write_text("clean\n", encoding="utf-8")
                (repo / "other.txt").write_text("clean\n", encoding="utf-8")
                package = repo / "crates/fixture"
                (package / "src").mkdir(parents=True)
                (package / "Cargo.toml").write_text(
                    '[package]\nname = "identity-fixture"\nversion = "0.0.0"\n'
                    'edition = "2021"\n', encoding="utf-8",
                )
                shutil.copyfile(BUILD_SCRIPT, package / "build.rs")
                (package / "src/main.rs").write_text(
                    'fn main() { println!("{}", env!("WEBCODEX_BUILD_GIT_DIRTY")); }\n',
                    encoding="utf-8",
                )
                run(repo, "git", "add", ".")
                run(repo, "git", "commit", "-m", "fixture")
                if linked_worktree:
                    checkout = root / "checkout"
                    run(repo, "git", "worktree", "add", "-b", "review/linked", str(checkout))
                    repo = checkout
                    package = repo / "crates/fixture"

                def build():
                    return run(package, "cargo", "run", "--offline", "--quiet")

                head = run(repo, "git", "rev-parse", "HEAD")
                self.assertEqual(build(), "false")
                (repo / "tracked.txt").write_text("dirty\n", encoding="utf-8")
                self.assertEqual(run(repo, "git", "rev-parse", "HEAD"), head)
                self.assertEqual(build(), "true")
                outputs = list((root / "target/debug/build").glob("identity-fixture-*/output"))
                self.assertEqual(len(outputs), 1)
                dirty_stamp = outputs[0].stat().st_mtime_ns
                (repo / "other.txt").write_text("also dirty\n", encoding="utf-8")
                self.assertEqual(build(), "true")
                self.assertEqual(outputs[0].stat().st_mtime_ns, dirty_stamp)

                run(repo, "git", "restore", "tracked.txt")
                self.assertEqual(run(repo, "git", "rev-parse", "HEAD"), head)
                self.assertEqual(build(), "true")
                run(repo, "git", "restore", "other.txt")
                self.assertEqual(run(repo, "git", "rev-parse", "HEAD"), head)
                self.assertEqual(build(), "false")

                env["WEBCODEX_GIT_DIRTY"] = "false"
                self.assertEqual(build(), "false")
                outputs = list((root / "target/debug/build").glob("identity-fixture-*/output"))
                self.assertEqual(len(outputs), 1)
                stamp = outputs[0].stat().st_mtime_ns
                (repo / "tracked.txt").write_text("dirty with pinned identity\n", encoding="utf-8")
                self.assertEqual(build(), "false")
                self.assertEqual(outputs[0].stat().st_mtime_ns, stamp)


if __name__ == "__main__":
    unittest.main()
