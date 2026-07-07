#!/usr/bin/env python3
"""In-memory GitHub API stub for the reference demo (issue #19).

Stands in for api.github.com so the full operating loop — scoped token
minting, Check Runs, marker-backed status comments, draft PRs, and the
maintainer permission gate — runs end to end with zero network access and
zero real credentials.

The stub keeps one world of state and records every API call in an audit
trail. Two demo-only endpoints expose it:

    GET /_demo/state   full world state as JSON (assertions read this)
    GET /_demo/audit   the audit trail as JSON

Run: python3 github-stub.py --port 8091
"""

import argparse
import json
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Who may command the familiar. Everyone else defaults to read — the worker's
# pre-flight permission gate declines their commands (issue #13).
COLLABORATOR_PERMISSIONS = {
    "octocat": "admin",
    "coven-cody[bot]": "write",
}
DEFAULT_PERMISSION = "read"

BRANCH_SHA = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0"


class World:
    """All mutable stub state, guarded by one lock."""

    def __init__(self):
        self.lock = threading.Lock()
        self.tokens = {}  # token value -> role
        self.token_seq = 0
        self.check_runs = {}  # id -> dict
        self.check_seq = 1000
        self.comments = []  # [{id, issue_number, body, user, edits}]
        self.comment_seq = 5000
        self.pulls = {}  # number -> dict
        self.pull_seq = 100
        self.audit = []  # [{seq, at, actor, action}]
        self.audit_seq = 0

    def record(self, actor, action):
        self.audit_seq += 1
        self.audit.append(
            {
                "seq": self.audit_seq,
                "at": time.strftime("%H:%M:%S"),
                "actor": actor,
                "action": action,
            }
        )

    def state(self):
        return {
            "tokens_minted": [
                {"role": role} for _, role in sorted(self.tokens.items())
            ],
            "check_runs": list(self.check_runs.values()),
            "comments": self.comments,
            "pulls": list(self.pulls.values()),
            "audit_len": len(self.audit),
        }


WORLD = World()


def token_role(permissions):
    """Names the scoped-token role from its permission set (issue #4)."""
    if permissions.get("checks") == "write":
        return "orchestration"
    if permissions.get("pull_requests") == "write":
        return "publication"
    if permissions == {"contents": "write"}:
        return "agent-git"
    return "unknown"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "github-stub/0.1"

    def log_message(self, fmt, *args):  # quiet the default access log
        pass

    def _json(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        return json.loads(raw) if raw else {}

    def _actor(self):
        auth = self.headers.get("Authorization", "")
        token = auth.removeprefix("Bearer ").strip()
        return WORLD.tokens.get(token, "app-jwt")

    # ── Routing ──────────────────────────────────────────────────────────

    def do_GET(self):
        path = self.path.split("?")[0]
        with WORLD.lock:
            if path == "/_demo/state":
                return self._json(200, WORLD.state())
            if path == "/_demo/audit":
                return self._json(200, WORLD.audit)

            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)", path)
            if m:
                WORLD.record(self._actor(), f"read repo metadata {m[1]}/{m[2]}")
                return self._json(200, {"default_branch": "main"})

            m = re.fullmatch(r"/repos/[^/]+/[^/]+/branches/([^/]+)", path)
            if m:
                WORLD.record(self._actor(), f"resolve branch '{m[1]}' head SHA")
                return self._json(200, {"commit": {"sha": BRANCH_SHA}})

            m = re.fullmatch(
                r"/repos/[^/]+/[^/]+/collaborators/([^/]+)/permission", path
            )
            if m:
                perm = COLLABORATOR_PERMISSIONS.get(m[1], DEFAULT_PERMISSION)
                WORLD.record(
                    self._actor(),
                    f"permission check for @{m[1]} -> {perm}",
                )
                return self._json(200, {"permission": perm})

            m = re.fullmatch(r"/repos/[^/]+/[^/]+/issues/(\d+)/comments", path)
            if m:
                number = int(m[1])
                items = [
                    {"id": c["id"], "body": c["body"], "user": {"login": c["user"]}}
                    for c in WORLD.comments
                    if c["issue_number"] == number
                ]
                WORLD.record(
                    self._actor(),
                    f"list #{number} comments ({len(items)} found)",
                )
                return self._json(200, items)

        self._json(404, {"message": f"stub: no route for GET {path}"})

    def do_POST(self):
        path = self.path.split("?")[0]
        body = self._read_body()
        with WORLD.lock:
            m = re.fullmatch(r"/app/installations/(\d+)/access_tokens", path)
            if m:
                role = token_role(body.get("permissions", {}))
                WORLD.token_seq += 1
                token = f"demo-token-{role}-{WORLD.token_seq}"
                WORLD.tokens[token] = role
                repos = ",".join(body.get("repositories", []))
                WORLD.record(
                    "app-jwt",
                    f"mint {role} token (repo-scoped: {repos})",
                )
                return self._json(201, {"token": token})

            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/check-runs", path)
            if m:
                WORLD.check_seq += 1
                run = {
                    "id": WORLD.check_seq,
                    "name": body.get("name", ""),
                    "head_sha": body.get("head_sha", ""),
                    "status": body.get("status", "queued"),
                    "conclusion": None,
                    "title": None,
                    "summary": None,
                    "details_url": body.get("details_url"),
                }
                WORLD.check_runs[run["id"]] = run
                WORLD.record(
                    self._actor(),
                    f"create check run {run['id']} '{run['name']}' "
                    f"on {run['head_sha'][:12]} (queued)",
                )
                return self._json(201, {"id": run["id"]})

            m = re.fullmatch(r"/repos/[^/]+/[^/]+/issues/(\d+)/comments", path)
            if m:
                WORLD.comment_seq += 1
                comment = {
                    "id": WORLD.comment_seq,
                    "issue_number": int(m[1]),
                    "body": body.get("body", ""),
                    "user": "coven-cody[bot]",
                    "edits": 0,
                }
                WORLD.comments.append(comment)
                WORLD.record(
                    self._actor(),
                    f"post comment {comment['id']} on #{m[1]}",
                )
                return self._json(201, {"id": comment["id"]})

            m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/pulls", path)
            if m:
                WORLD.pull_seq += 1
                pull = {
                    "number": WORLD.pull_seq,
                    "title": body.get("title", ""),
                    "body": body.get("body", ""),
                    "head": body.get("head", ""),
                    "base": body.get("base", ""),
                    "draft": bool(body.get("draft")),
                }
                WORLD.pulls[pull["number"]] = pull
                WORLD.record(
                    self._actor(),
                    f"open draft PR #{pull['number']} "
                    f"({pull['head']} -> {pull['base']})",
                )
                return self._json(201, {"number": pull["number"]})

        self._json(404, {"message": f"stub: no route for POST {path}"})

    def do_PATCH(self):
        path = self.path.split("?")[0]
        body = self._read_body()
        with WORLD.lock:
            m = re.fullmatch(r"/repos/[^/]+/[^/]+/check-runs/(\d+)", path)
            if m:
                run = WORLD.check_runs.get(int(m[1]))
                if not run:
                    return self._json(404, {"message": "no such check run"})
                run["status"] = body.get("status", run["status"])
                if "conclusion" in body:
                    run["conclusion"] = body["conclusion"]
                output = body.get("output") or {}
                run["title"] = output.get("title", run["title"])
                run["summary"] = output.get("summary", run["summary"])
                detail = run["conclusion"] or run["status"]
                WORLD.record(
                    self._actor(),
                    f"check run {run['id']} -> {detail} ('{run['title']}')",
                )
                return self._json(200, {"id": run["id"]})

            m = re.fullmatch(r"/repos/[^/]+/[^/]+/issues/comments/(\d+)", path)
            if m:
                cid = int(m[1])
                for comment in WORLD.comments:
                    if comment["id"] == cid:
                        comment["body"] = body.get("body", comment["body"])
                        comment["edits"] += 1
                        first_line = next(
                            (
                                line
                                for line in comment["body"].splitlines()
                                if line and not line.startswith("<!--")
                            ),
                            "",
                        )
                        WORLD.record(
                            self._actor(),
                            f"edit comment {cid} in place "
                            f"(edit #{comment['edits']}: '{first_line}')",
                        )
                        return self._json(200, {"id": cid})
                return self._json(404, {"message": "no such comment"})

        self._json(404, {"message": f"stub: no route for PATCH {path}"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"github-stub listening on 127.0.0.1:{args.port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
