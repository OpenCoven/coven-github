import importlib.util
import tempfile
import unittest
from pathlib import Path


def load_adapter():
    path = Path(__file__).with_name("coven_github_adapter.py")
    spec = importlib.util.spec_from_file_location("coven_github_adapter", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class HostedAdapterTests(unittest.TestCase):
    def test_mentions_are_boundary_aware(self):
        adapter = load_adapter()
        policy = {"bot_usernames": ["cody"]}

        for text in ("@cody please review", "Please review, @Cody.", "(@cody)"):
            self.assertTrue(adapter.mentioned(text, policy), text)

        for text in (
            "@codybot please review",
            "@cody_bot please review",
            "@cody/team please review",
            "email me at x@cody.example",
            "prefix@cody",
        ):
            self.assertFalse(adapter.mentioned(text, policy), text)

    def test_route_signed_delivery_returns_500_when_webhook_secret_missing(self):
        adapter = load_adapter()

        result = adapter.route_signed_delivery(
            {
                "X-GitHub-Event": "ping",
                "X-GitHub-Delivery": "delivery-1",
                "X-Hub-Signature-256": "sha256=deadbeef",
            },
            b"{}",
            lambda *_args, **_kwargs: None,
            webhook_secret="",
        )

        self.assertEqual(
            result,
            {
                "ok": False,
                "status": 500,
                "error": "GITHUB_WEBHOOK_SECRET is required",
            },
        )

    def test_prepare_review_context_rejects_stale_pr_head_evidence(self):
        adapter = load_adapter()

        def fake_github_request(method, url, token, body=None):
            if "/pulls/123/files" in url:
                return []
            if "/pulls/123" in url:
                return {
                    "number": 123,
                    "head": {"sha": "metadata-sha"},
                    "base": {"sha": "base-sha"},
                }
            raise AssertionError(url)

        def fake_run_command(args, cwd=None, env=None, timeout=300):
            if args[:2] == ["git", "fetch"]:
                return {"args": args, "returncode": 0, "stdout": "", "stderr": ""}
            if args[:3] == ["git", "checkout", "--detach"]:
                return {"args": args, "returncode": 0, "stdout": "", "stderr": ""}
            if args[:3] == ["git", "rev-parse", "HEAD"]:
                return {
                    "args": args,
                    "returncode": 0,
                    "stdout": "different-sha\n",
                    "stderr": "",
                }
            if args[:3] == ["git", "status", "--short"]:
                return {"args": args, "returncode": 0, "stdout": "## HEAD\n", "stderr": ""}
            raise AssertionError(args)

        adapter.github_request = fake_github_request
        adapter.run_command = fake_run_command

        with tempfile.TemporaryDirectory() as tmp:
            task = {"task": {"pr_number": 123}, "repository": "OpenCoven/coven-github"}
            with self.assertRaisesRegex(RuntimeError, "does not match GitHub metadata head"):
                adapter.prepare_review_context(task, Path(tmp), "tok", {}, Path(tmp))


if __name__ == "__main__":
    unittest.main()
