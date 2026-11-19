import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release


class ReleaseTests(unittest.TestCase):
    def test_main_requires_merged_pr_final_approval_and_exact_push_ci(self):
        main = "main-sha"
        pull = {"number": 1, "base": {"ref": "main"}, "merged_at": "now",
                "merge_commit_sha": main, "head": {"sha": "pr-sha"}}
        approval = {"user": {"login": "hechuan9"}, "state": "APPROVED", "commit_id": "pr-sha"}
        ci_run = {"id": 2, "head_sha": main, "head_branch": "main", "event": "push",
                  "conclusion": "success", "head_repository": {"full_name": release.REPO}}
        responses = {
            "git/ref/heads/main": {"object": {"sha": main}},
            f"commits/{main}/pulls?per_page=100": [pull],
            "pulls/1/reviews?per_page=100": [approval],
            f"actions/workflows/ci.yml/runs?branch=main&head_sha={main}&event=push&status=success":
                {"workflow_runs": [ci_run]},
        }
        with patch.object(release, "api", side_effect=responses.__getitem__):
            release.require_approved_main(main)
            with self.assertRaisesRegex(ValueError, "not the current origin/main"):
                release.require_approved_main("pr-sha")
            pull["merged_at"] = None
            with self.assertRaisesRegex(ValueError, "merged PR"):
                release.require_approved_main(main)
            pull["merged_at"] = "now"
            approval["commit_id"] = "older-pr-sha"
            with self.assertRaisesRegex(ValueError, "final revision"):
                release.require_approved_main(main)
            approval["commit_id"] = "pr-sha"
            ci_run["head_sha"] = "older-main"
            with self.assertRaisesRegex(ValueError, "No successful ci.yml"):
                release.require_approved_main(main)

    def test_fetch_does_not_publish_a_stale_or_modified_package(self):
        spk = b"test SPK"
        main = "main-sha"
        source = {"revision": main, "checksum": hashlib.sha256(spk).hexdigest()}

        def download(command, check):
            self.assertIn("keeps-nas-spk-main-sha", command)
            self.assertTrue(check)
            artifact = Path(command[command.index("--dir") + 1])
            artifact.mkdir()
            (artifact / release.PACKAGE).write_bytes(spk)
            (artifact / "source-revision").write_text(source["revision"])
            (artifact / "SHA256SUMS").write_text(f"{source['checksum']}  {release.PACKAGE}\n")

        with tempfile.TemporaryDirectory() as directory, \
                patch.object(release, "successful_run", return_value=12), \
                patch.object(release.subprocess, "run", side_effect=download):
            output = Path(directory) / "release"
            with patch.object(release, "main_revision", side_effect=[main, "new-main-sha"]):
                with self.assertRaisesRegex(ValueError, "not the current origin/main"):
                    release.fetch(output)
            self.assertFalse(output.exists())
            with patch.object(release, "main_revision", return_value=main):
                digest = source["checksum"]
                source["checksum"] = "invalid"
                with self.assertRaisesRegex(ValueError, "SPK checksum"):
                    release.fetch(output)
                self.assertFalse(output.exists())
                source["checksum"] = digest
                release.fetch(output)
                self.assertEqual((output / release.PACKAGE).read_bytes(), spk)
