import unittest
from unittest.mock import patch
import os
import tempfile
from pathlib import Path
import triage_gate as gate

OLD = "2026-01-01T00:00:00Z"
NOW = "2026-02-01T00:00:00Z"
HUMAN = {"type": "User", "login": "reporter"}
BOT = {"type": "Bot", "login": "triage[bot]"}


def issue(number=1, author=HUMAN, created=OLD, edited=None):
    return {"number": number, "author": author, "createdAt": created, "lastEditedAt": edited}


def comment(number=1, author=HUMAN, updated=NOW, repo="fileman"):
    return {"issue_url": f"https://api.github.com/repos/navigato-rs/{repo}/issues/{number}", "user": author, "updated_at": updated}


class Tests(unittest.TestCase):
    def test_compiled_policy_stays_constrained(self):
        # No YAML dependency: the compiler records a machine-readable manifest.
        import json
        text = (Path(__file__).resolve().parents[1] / ".github/workflows/org-triage.lock.yml").read_text()
        manifest = json.loads(next(line.split(": ", 1)[1] for line in text.splitlines() if line.startswith("# gh-aw-manifest:")))
        outputs = next(server["tools"] for server in manifest["mcp_servers"] if server["name"] == "safeoutputs")
        self.assertEqual(set(outputs), {"add_labels", "update_issue", "missing_tool", "missing_data", "noop"})
        self.assertIn("--deny-tool=shell", text)
        self.assertIn("--deny-tool=write", text)
        self.assertNotIn("--allow-all-tools", text)
        self.assertIn("needs.pre_activation.outputs.has_work == 'true'", text)
        self.assertIn('\\"target\\":\\"81\\"', text)
        for action in manifest["actions"]:
            self.assertRegex(action["sha"], r"^[a-f0-9]{40}$")

    def test_new_issue_and_body_edit(self):
        self.assertEqual(gate.candidates("fileman", [issue(1, created=NOW), issue(2, edited=NOW)], [], NOW, 99), ["navigato-rs/fileman#1", "navigato-rs/fileman#2"])

    def test_old_reply_edited_recently_is_work(self):
        self.assertEqual(gate.candidates("fileman", [issue()], [comment()], NOW, 99), ["navigato-rs/fileman#1"])

    def test_bot_changes_never_feedback(self):
        self.assertEqual(gate.candidates("fileman", [issue(), issue(2, BOT, NOW)], [comment(author=BOT)], NOW, 99), [])

    def test_summary_closed_issues_prs_and_wrong_repo_are_excluded(self):
        self.assertEqual(gate.candidates("fileman", [issue(99, created=NOW), issue()], [comment(99), comment(2), comment(repo="starcom")], NOW, 99), [])

    def test_human_reply_to_bot_issue_is_not_ignored(self):
        self.assertEqual(gate.candidates("fileman", [issue(author=BOT)], [comment()], NOW, 99), ["navigato-rs/fileman#1"])

    def test_disabled_does_not_access_api_or_require_credentials(self):
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / "output"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True), patch.object(gate, "GitHub", side_effect=AssertionError("network")):
                gate.main()
            self.assertIn("has_work=false", output.read_text())

    def test_cutoff_uses_previous_run_start(self):
        api = gate.GitHub("unused")
        with patch.object(api, "get", side_effect=[{"workflow_runs": [{"id": 4, "head_branch": "main", "event": "schedule", "created_at": NOW}]}, {"total_count": 1, "jobs": [{"name": "agent", "conclusion": "success"}]}]):
            self.assertEqual(api.cutoff(5), NOW)

    def test_disabled_and_no_work_runs_do_not_advance_watermark(self):
        api = gate.GitHub("unused")
        with patch.object(api, "get", side_effect=[{"workflow_runs": [{"id": 4, "head_branch": "main", "event": "schedule", "created_at": NOW}]}, {"total_count": 1, "jobs": [{"name": "agent", "conclusion": "skipped"}]}]):
            self.assertEqual(api.cutoff(5), "1970-01-01T00:00:00Z")

    def test_bounded_pagination_fails_closed(self):
        api = gate.GitHub("unused")
        with patch.object(gate, "PAGE_LIMIT", 1), patch.object(api, "get", return_value=[comment()] * 100):
            with self.assertRaises(RuntimeError): api.comments("fileman", NOW)

    def test_external_or_injected_url_is_not_followed(self):
        data = comment()
        data["issue_url"] = "https://evil.test/repos/navigato-rs/fileman/issues/1\nGITHUB_OUTPUT=x"
        self.assertEqual(gate.candidates("fileman", [issue()], [data], NOW, 99), [])

    def test_summary_must_be_designated_issue(self):
        api = gate.GitHub("unused")
        with patch.object(api, "get", return_value={"title": "User report", "state": "open"}):
            with self.assertRaises(RuntimeError): gate.gate(api, 5, 99)


if __name__ == "__main__":
    unittest.main()
