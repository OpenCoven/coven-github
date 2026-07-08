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

    def test_route_signed_delivery_reports_missing_secret(self):
        adapter = load_adapter()
        result = adapter.route_signed_delivery(
            {"x-hub-signature-256": "sha256=abc"},
            b"{}",
            debug=True,
            webhook_secret="",
        )

        self.assertEqual(result["status"], 500)
        self.assertIn("GITHUB_WEBHOOK_SECRET", result["error"])

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

    def test_publication_body_links_file_mentions(self):
        adapter = load_adapter()

        body = adapter.publication_comment_body(
            {
                "task_id": "task-file-links",
                "repository": "OpenCoven/coven-github",
                "default_branch": "main",
                "review_evidence": {"head_sha": "abc123def456"},
            },
            {
                "status": "success",
                "summary": "\n".join(
                    [
                        "### Files inspected",
                        "",
                        "- `src/lib/server/skills-directory.ts`",
                        "- `Read src/lib/server/skill-scan.ts`",
                        "- Read src/lib/server/skill-scan.ts - passed: inspected adapter implementation.",
                        "- Read AGENTS.md - passed: reviewed guidance.",
                        "- Fixed a bug, e.g. the parser broke.",
                        "- In other words, i.e. no bogus abbreviation links.",
                        "- Mentioned foo.bar.baz.qux in prose.",
                        "- Grep for https://github.com/OpenCoven/coven-github/blob/main/src/app.ts and tests_run[].output_summary.",
                        "- `README.md:12`",
                        "- `README.md:12-14`",
                        "- `tests_run[].output_summary`",
                        "- `npm test`",
                        "",
                        "```ts",
                        "`src/not-linked-inside-fence.ts`",
                        "```",
                    ]
                ),
                "review": {
                    "supporting_files": ["AGENTS.md"],
                },
            },
        )

        self.assertIn(
            "[`src/lib/server/skills-directory.ts`](https://github.com/OpenCoven/coven-github/blob/abc123def456/src/lib/server/skills-directory.ts)",
            body,
        )
        self.assertIn(
            "`Read` [`src/lib/server/skill-scan.ts`](https://github.com/OpenCoven/coven-github/blob/abc123def456/src/lib/server/skill-scan.ts)",
            body,
        )
        self.assertIn(
            "Read [`src/lib/server/skill-scan.ts`](https://github.com/OpenCoven/coven-github/blob/abc123def456/src/lib/server/skill-scan.ts) - passed",
            body,
        )
        self.assertIn(
            "Read [`AGENTS.md`](https://github.com/OpenCoven/coven-github/blob/abc123def456/AGENTS.md) - passed",
            body,
        )
        self.assertIn(
            "https://github.com/OpenCoven/coven-github/blob/main/src/app.ts",
            body,
        )
        self.assertIn("e.g. the parser broke", body)
        self.assertIn("i.e. no bogus abbreviation links", body)
        self.assertIn("foo.bar.baz.qux in prose", body)
        self.assertNotIn("[`e.g`]", body)
        self.assertNotIn("[`i.e`]", body)
        self.assertNotIn("[`foo.bar.baz.qux`]", body)
        self.assertNotIn("blob/main/[`src/app.ts`]", body)
        self.assertIn(
            "[`README.md:12`](https://github.com/OpenCoven/coven-github/blob/abc123def456/README.md#L12)",
            body,
        )
        self.assertIn(
            "[`README.md:12-14`](https://github.com/OpenCoven/coven-github/blob/abc123def456/README.md#L12-L14)",
            body,
        )
        self.assertIn("- `tests_run[].output_summary`", body)
        self.assertNotIn("[`tests_run[].output_summary`]", body)
        self.assertIn("- `npm test`", body)
        self.assertIn("`src/not-linked-inside-fence.ts`", body)
        self.assertNotIn("[`src/not-linked-inside-fence.ts`]", body)

    def test_publication_body_links_structured_review_files(self):
        adapter = load_adapter()

        body = adapter.publication_comment_body(
            {
                "task_id": "task-structured-links",
                "repository": "OpenCoven/coven-github",
                "default_branch": "main",
                "review_evidence": {
                    "head_sha": "feedface",
                    "changed_files": ["src/app.ts"],
                    "changed_file_count": 1,
                },
            },
            {
                "status": "success",
                "summary": "Done.",
                "review": {
                    "mode": "pull_request",
                    "evidence_status": "complete",
                    "reviewed_files": ["src/app.ts"],
                    "supporting_files": ["tests/app.test.ts"],
                    "findings": [
                        {
                            "severity": "medium",
                            "file": "src/app.ts",
                            "line": 7,
                            "title": "Example finding",
                        }
                    ],
                    "no_findings_reason": "Checked `tests/app.test.ts` with `npm test`.",
                    "tests_run": [
                        {
                            "command": "Read src/app.ts",
                            "status": "passed",
                            "output_summary": "inspected `tests/app.test.ts` coverage.",
                        },
                        {
                            "command": "npm test",
                            "status": "passed",
                        },
                    ],
                },
            },
        )

        self.assertIn(
            "[`src/app.ts`](https://github.com/OpenCoven/coven-github/blob/feedface/src/app.ts)",
            body,
        )
        self.assertIn(
            "[`tests/app.test.ts`](https://github.com/OpenCoven/coven-github/blob/feedface/tests/app.test.ts)",
            body,
        )
        self.assertIn(
            "[`src/app.ts:7`](https://github.com/OpenCoven/coven-github/blob/feedface/src/app.ts#L7)",
            body,
        )
        self.assertIn(
            "`Read` [`src/app.ts`](https://github.com/OpenCoven/coven-github/blob/feedface/src/app.ts): `passed`",
            body,
        )
        self.assertIn("with `npm test`", body)
        self.assertIn("- `npm test`: `passed`", body)


if __name__ == "__main__":
    unittest.main()
