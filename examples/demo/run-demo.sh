#!/usr/bin/env bash
# Reference demo of the coven-github operating loop (issue #19).
#
# Runs the REAL adapter binary end to end against an in-memory GitHub API stub
# and a contract-conformant fake coven-code runtime — no network, no GitHub
# App, no credentials. It replays signed webhook deliveries and verifies the
# ClawSweeper-style bot ergonomics after every act:
#
#   1. issue assigned      -> scoped tokens, Check Run, status comment,
#                             familiar-voice draft PR back to the issue
#   1b. webhook redelivery  -> same X-GitHub-Delivery id: deduplicated
#                             durably, zero additional work (issue #2)
#   2. casual mention      -> ignored (no mutation at all)
#   3. bot's own comment   -> ignored (self-trigger loop guard)
#   4. unknown verb        -> clarification reply, edited in place
#   5. @cody status        -> durable task state answered from the audit trail
#   6. retry w/o write     -> declined at the permission gate, no work spent
#   7. retry as maintainer -> full re-run; STILL exactly one status comment
#
# Every assertion is checked programmatically: a green exit proves the loop.
#
# Usage:
#   examples/demo/run-demo.sh          # run all acts, clean up after
#   KEEP=1 examples/demo/run-demo.sh   # keep the scratch dir for inspection
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEMO_SRC="$ROOT/examples/demo"
DEMO_DIR="$(mktemp -d /tmp/coven-github-demo.XXXXXX)"

REPO_OWNER="OpenCoven"
REPO_NAME="demo-service"
INSTALLATION_ID=424242
ISSUE=42
ISSUE_TITLE="Fix OAuth token refresh"
ISSUE_BODY="The refresh path drops clock skew between client and auth server."
MAINTAINER="octocat"   # admin in the stub's collaborator table
DRIVE_BY="mallory"     # read-only in the stub's collaborator table

SERVER_PID=""
STUB_PID=""

cleanup() {
  local code=$?
  [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true
  [[ -n "$STUB_PID" ]] && kill "$STUB_PID" 2>/dev/null || true
  if [[ "${KEEP:-}" == "1" || $code -ne 0 ]]; then
    echo
    echo "scratch dir kept at $DEMO_DIR (server.log, stub.log, config.toml)"
  else
    rm -rf "$DEMO_DIR"
  fi
  exit $code
}
trap cleanup EXIT

for dep in cargo python3 openssl curl; do
  command -v "$dep" >/dev/null || { echo "error: $dep is required" >&2; exit 64; }
done

banner() { printf '\n\033[1m── %s\033[0m\n' "$*"; }
note()   { printf '   %s\n' "$*"; }

# ── Provision throwaway credentials and config ──────────────────────────────

banner "Provisioning demo environment (throwaway key, random secret)"
openssl genrsa -out "$DEMO_DIR/app-key.pem" 2048 2>/dev/null
WEBHOOK_SECRET="$(openssl rand -hex 24)"

read -r GH_PORT SRV_PORT < <(python3 - <<'PY'
import socket
def free():
    s = socket.socket(); s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]; s.close(); return port
print(free(), free())
PY
)
GH_URL="http://127.0.0.1:$GH_PORT"
SRV_URL="http://127.0.0.1:$SRV_PORT"

cat > "$DEMO_DIR/config.toml" <<EOF
[server]
bind = "127.0.0.1:$SRV_PORT"
cave_base_url = "https://cave.opencoven.ai"

[github]
app_id = 1
private_key_path = "$DEMO_DIR/app-key.pem"
webhook_secret = "$WEBHOOK_SECRET"
api_base_url = "$GH_URL"

[worker]
concurrency = 2
coven_code_bin = "$DEMO_SRC/fake-coven-code"
workspace_root = "$DEMO_DIR/tasks"
timeout_secs = 120
max_retries = 0

[storage]
path = "$DEMO_DIR/store.db"

[[familiars]]
id = "cody"
display_name = "Cody"
bot_username = "coven-cody[bot]"
skills = ["systematic-debugging"]
trigger_labels = ["coven:fix"]
EOF
note "GitHub API stub: $GH_URL"
note "adapter server:  $SRV_URL"

banner "Building the adapter (cargo build -p coven-github)"
cargo build -p coven-github --quiet --manifest-path "$ROOT/Cargo.toml"

banner "Starting the GitHub API stub and the real adapter"
python3 "$DEMO_SRC/github-stub.py" --port "$GH_PORT" \
  > "$DEMO_DIR/stub.log" 2>&1 &
STUB_PID=$!
RUST_LOG=info "$ROOT/target/debug/coven-github" serve \
  --config "$DEMO_DIR/config.toml" > "$DEMO_DIR/server.log" 2>&1 &
SERVER_PID=$!
# Detach both from job control so shutdown doesn't print 'Terminated' noise.
disown "$STUB_PID" "$SERVER_PID"

# ── Helpers ──────────────────────────────────────────────────────────────────

# Signs a payload the way GitHub does and delivers it to the webhook endpoint.
# Every delivery carries a unique X-GitHub-Delivery id — the adapter requires
# it as the idempotency key (issue #2).
send_event() { # $1=event type  $2=payload json
  local event="$1" payload="$2" sig
  sig="sha256=$(printf '%s' "$payload" \
    | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" | awk '{print $2}')"
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$SRV_URL/webhook" \
    -H "X-GitHub-Event: $event" \
    -H "X-Hub-Signature-256: $sig" \
    -H "X-GitHub-Delivery: ${3:-demo-$(openssl rand -hex 8)}" \
    -H 'Content-Type: application/json' \
    -d "$payload")"
  [[ "$code" == "200" ]] || { echo "delivery of '$event' failed: HTTP $code" >&2; exit 1; }
}

state() { curl -sf "$GH_URL/_demo/state"; }

# Polls the stub state until a Python predicate over `s` holds.
wait_for() { # $1=predicate  $2=description  [$3=timeout seconds]
  local predicate="$1" desc="$2" timeout="${3:-30}" waited=0
  while true; do
    if state | python3 -c "
import json, sys
s = json.load(sys.stdin)
sys.exit(0 if ($predicate) else 1)
"; then
      note "ok: $desc"
      return 0
    fi
    sleep 0.25
    waited=$((waited + 1))
    if (( waited >= timeout * 4 )); then
      echo "TIMEOUT waiting for: $desc" >&2
      echo "--- stub state ---" >&2; state >&2 || true
      echo "--- server log tail ---" >&2; tail -20 "$DEMO_DIR/server.log" >&2 || true
      exit 1
    fi
  done
}

# Asserts a Python predicate over the current stub state `s`.
assert_state() { # $1=predicate  $2=description
  if state | python3 -c "
import json, sys
s = json.load(sys.stdin)
sys.exit(0 if ($1) else 1)
"; then
    note "ok: $2"
  else
    echo "ASSERTION FAILED: $2" >&2
    echo "--- stub state ---" >&2; state >&2 || true
    exit 1
  fi
}

show_status_comment() {
  state | python3 -c "
import json, sys
s = json.load(sys.stdin)
for c in s['comments']:
    print(f\"   ┌─ comment {c['id']} on #{c['issue_number']} \"
          f\"(edited in place {c['edits']}x)\")
    for line in c['body'].splitlines():
        print('   │ ' + line)
    print('   └─')
"
}

issue_comment_payload() { # $1=commenter login  $2=comment body
  cat <<EOF
{
  "action": "created",
  "issue": {
    "number": $ISSUE,
    "title": "$ISSUE_TITLE",
    "body": "$ISSUE_BODY",
    "user": { "login": "$MAINTAINER" }
  },
  "comment": { "body": "$2", "user": { "login": "$1" } },
  "repository": {
    "name": "$REPO_NAME",
    "owner": { "login": "$REPO_OWNER" }
  },
  "installation": { "id": $INSTALLATION_ID },
  "sender": { "login": "$1" }
}
EOF
}

audit_len() { state | python3 -c "import json,sys; print(json.load(sys.stdin)['audit_len'])"; }

# ── Readiness ────────────────────────────────────────────────────────────────

for _ in $(seq 1 40); do curl -sf "$GH_URL/_demo/state" >/dev/null && break; sleep 0.25; done
PING_BODY='{"zen":"Keep it logically awesome.","hook_id":1}'
for _ in $(seq 1 120); do
  sig="sha256=$(printf '%s' "$PING_BODY" | openssl dgst -sha256 -hmac "$WEBHOOK_SECRET" | awk '{print $2}')"
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$SRV_URL/webhook" \
    -H 'X-GitHub-Event: ping' -H "X-Hub-Signature-256: $sig" \
    -H "X-GitHub-Delivery: demo-ready-$(openssl rand -hex 8)" -d "$PING_BODY" || true)"
  [[ "$code" == "200" ]] && break
  sleep 0.25
done
[[ "$code" == "200" ]] || { echo "server never became ready" >&2; exit 1; }
note "webhook endpoint verified with a signed ping (HMAC path proven)"

# ═════════════════════════════════════════════════════════════════════════════

banner "ACT 1 — Issue assigned to the familiar: the full loop"
note "octocat assigns $REPO_OWNER/$REPO_NAME#$ISSUE to @coven-cody"
ASSIGN_PAYLOAD="$(cat <<EOF
{
  "action": "assigned",
  "issue": {
    "number": $ISSUE,
    "title": "$ISSUE_TITLE",
    "body": "$ISSUE_BODY",
    "user": { "login": "$MAINTAINER" }
  },
  "assignee": { "login": "coven-cody[bot]" },
  "repository": {
    "name": "$REPO_NAME",
    "owner": { "login": "$REPO_OWNER" }
  },
  "installation": { "id": $INSTALLATION_ID },
  "sender": { "login": "$MAINTAINER" }
}
EOF
)"
send_event issues "$ASSIGN_PAYLOAD" "demo-assign-1"
wait_for "len(s['pulls']) == 1" "draft PR opened back to the issue"
wait_for "any(c['conclusion'] == 'success' for c in s['check_runs'])" \
  "Check Run completed: success"
wait_for "any('Status: done' in c['body'] for c in s['comments'])" \
  "status comment reached its terminal state"
assert_state "len(s['comments']) == 1" \
  "ONE visible status comment — created once, then edited in place"
assert_state "s['comments'][0]['edits'] >= 1" \
  "the comment was edited (working -> done), not re-posted"
assert_state "s['pulls'][0]['draft'] == True" "the PR is a draft"
assert_state "'Cody' in s['pulls'][0]['body']" \
  "PR body speaks in the familiar's voice"
assert_state "{t['role'] for t in s['tokens_minted']} == {'orchestration', 'agent-git', 'publication'}" \
  "three separately-scoped tokens minted (orchestration / agent-git / publication)"
note ""
note "The one visible status surface on issue #$ISSUE:"
show_status_comment

banner "ACT 1b — GitHub redelivers the webhook: idempotent, no duplicate work"
note "same X-GitHub-Delivery id delivered again (manual redelivery / retry)"
BEFORE=$(audit_len)
send_event issues "$ASSIGN_PAYLOAD" "demo-assign-1"
sleep 2
assert_state "s['audit_len'] == $BEFORE" \
  "redelivery acknowledged without a single GitHub API call"
assert_state "len(s['pulls']) == 1 and len(s['check_runs']) == 1" \
  "no second session, Check Run, or PR — delivery id deduplicated durably"

banner "ACT 2 — Casual mention: ignored, zero API calls"
BEFORE=$(audit_len)
send_event issue_comment "$(issue_comment_payload "$MAINTAINER" \
  "thanks @coven-cody, great work on this!")"
sleep 2
assert_state "s['audit_len'] == $BEFORE" \
  "casual mid-sentence mention triggered nothing (audit unchanged)"

banner "ACT 3 — The familiar's own comment: self-trigger loop guard"
BEFORE=$(audit_len)
send_event issue_comment "$(issue_comment_payload "coven-cody[bot]" \
  "@coven-cody status")"
sleep 2
assert_state "s['audit_len'] == $BEFORE" \
  "the bot's own comments never re-trigger it (audit unchanged)"

banner "ACT 4 — Unknown verb: clarification instead of guesswork"
note "octocat comments: '@coven-cody explain'"
send_event issue_comment "$(issue_comment_payload "$MAINTAINER" \
  "@coven-cody explain")"
wait_for "any('explain' in c['body'] and 'Supported commands' in c['body'] for c in s['comments'])" \
  "clarification reply listing the real command set"
assert_state "len(s['comments']) == 1" \
  "the reply edited the SAME status comment — still one surface"

banner "ACT 5 — Maintainer steering: @coven-cody status"
send_event issue_comment "$(issue_comment_payload "$MAINTAINER" \
  "@coven-cody status")"
wait_for "any('Tasks for $REPO_OWNER/$REPO_NAME#$ISSUE' in c['body'] for c in s['comments'])" \
  "status command answered from the durable task store"
show_status_comment

banner "ACT 6 — Permission gate: a drive-by 'retry' is declined"
note "$DRIVE_BY (read-only) comments: '@coven-cody retry'"
CHECKS_BEFORE=$(state | python3 -c "import json,sys; print(len(json.load(sys.stdin)['check_runs']))")
send_event issue_comment "$(issue_comment_payload "$DRIVE_BY" \
  "@coven-cody retry")"
wait_for "any('Status: declined' in c['body'] for c in s['comments'])" \
  "declined on the status surface: commands need write access"
assert_state "len(s['check_runs']) == $CHECKS_BEFORE" \
  "no Check Run created, no session spent — the gate held pre-flight"

banner "ACT 7 — Maintainer 'retry': full re-run, still one status comment"
note "$MAINTAINER (admin) comments: '@coven-cody retry'"
send_event issue_comment "$(issue_comment_payload "$MAINTAINER" \
  "@coven-cody retry")"
wait_for "len([c for c in s['check_runs'] if c['conclusion'] == 'success']) == 2" \
  "a second Check Run ran to success"
wait_for "any('Status: done' in c['body'] for c in s['comments'])" \
  "status surface back to done after the re-run"
assert_state "len(s['comments']) == 1" \
  "repeated runs NEVER stack duplicate comments — one surface, many edits"
show_status_comment

# ═════════════════════════════════════════════════════════════════════════════

banner "The durable audit trail (every GitHub mutation, by token role)"
curl -sf "$GH_URL/_demo/audit" | python3 -c "
import json, sys
for entry in json.load(sys.stdin):
    print(f\"   {entry['seq']:>3}  {entry['at']}  {entry['actor']:<14} {entry['action']}\")
"

banner "Cave oversight view (GET /api/github/tasks — what the dashboard polls)"
curl -sf "$SRV_URL/api/github/tasks" | python3 -m json.tool | sed 's/^/   /'

banner "Demo complete — every assertion above passed"
note "one status comment, edited in place through the whole lifecycle"
note "steering commands: status answered, unknown clarified, retry gated + re-run"
note "familiar-voice draft PR opened back to the issue"
note "auth split: orchestration / agent-git / publication tokens per task"
