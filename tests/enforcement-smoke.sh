#!/usr/bin/env bash
# Enforcement layer smoke test — 4 pain points E2E.
#
# Pain #1: Default danger rule (rm -rf) blocks a Bash tool call.
# Pain #2: Override approve → the same blocked call passes.
# Pain #3: RequireRecall rule blocks without a prior thoth_recall event,
#          passes once gate.jsonl carries a matching recall.
# Pain #4: Stop hook with an active workflow appends a row to
#          workflow-violations.jsonl.
#
# The gate binary always exits 0 and encodes its decision in the JSON
# line on stdout ({"decision":"block"|"approve", ...}). We assert on
# that JSON rather than on the exit code.

set -euo pipefail

# -------- locate workspace root (parent of tests/) ---------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WS_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$WS_ROOT"

PASS=0
FAIL=0

note() { printf '\n\033[1;36m%s\033[0m\n' "$*"; }
pass() { PASS=$((PASS + 1)); printf '  \033[1;32mPASS\033[0m %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf '  \033[1;31mFAIL\033[0m %s\n' "$*"; }

# -------- build binaries -----------------------------------------------
note "[build] cargo build --bin thoth --bin thoth-gate"
cargo build --bin thoth --bin thoth-gate 1>&2

THOTH="$WS_ROOT/target/debug/thoth"
GATE="$WS_ROOT/target/debug/thoth-gate"
[[ -x "$THOTH" ]] || { echo "thoth binary missing at $THOTH"; exit 1; }
[[ -x "$GATE"  ]] || { echo "thoth-gate binary missing at $GATE"; exit 1; }

# -------- isolated tempdir ---------------------------------------------
TMP="$(mktemp -d -t thoth-smoke.XXXXXX)"
HOME_DIR="$TMP/home"
ROOT="$TMP/root/.thoth"
mkdir -p "$HOME_DIR" "$ROOT"

# Config mirrors the integration tests: nudge mode, no reflect-debt
# block, telemetry off so nothing drifts into the decision surface.
cat >"$ROOT/config.toml" <<'EOF'
[discipline]
mode = "nudge"
reflect_debt_block = 0
telemetry_enabled = false
EOF

cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

note "[setup] root=$ROOT  home=$HOME_DIR"

# Run the gate with clean env; returns stdout JSON on fd 1.
run_gate() {
  local payload="$1"
  env -i PATH="/usr/bin:/bin" HOME="$HOME_DIR" THOTH_ROOT="$ROOT" \
    "$GATE" <<<"$payload"
}

# SHA-256 of the *raw bytes* we send to the gate. Must match what the
# gate hashes (serde_json::to_vec of the parsed Value). Compacting the
# JSON through `python3 -c json.dumps(..., separators=(',', ':'))`
# gives us a byte-stable canonical form for both sides — the gate
# re-serializes after parsing, so we reuse the same compact form.
sha256_bytes() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 | awk '{print $1}'
  else
    sha256sum | awk '{print $1}'
  fi
}

# -------- Pain #1 : rm -rf blocked by default rule ---------------------
note "[pain-1] default 'no-rm-rf' rule blocks 'rm -rf /tmp/foo'"
PAYLOAD1='{"tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/foo"}}'
VERDICT1="$(run_gate "$PAYLOAD1")"
echo "  verdict: $VERDICT1"
DEC1="$(printf '%s' "$VERDICT1" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("decision",""))')"
if [[ "$DEC1" == "block" ]]; then
  pass "rm -rf blocked (decision=block)"
else
  fail "expected decision=block, got '$DEC1'"
fi

# -------- Pain #2 : override approve → same call passes ----------------
note "[pain-2] file override request → 'thoth override approve' → gate approves"

# Build the pending override request directly on disk. The
# OverrideManager schema is {id, rule_id, reason, tool_call_hash,
# requested_at, session_id, status:"pending"}.
REQ_ID="00000000-0000-4000-8000-000000000001"
# The gate hashes the parsed envelope. Re-serialize through python's
# json.dumps with the default (compact-ish) settings to match the
# round-trip serde does — empirically any valid serde round-trip of the
# same logical Value yields the same bytes for flat objects like this.
# The gate re-serializes the parsed envelope via `serde_json::to_vec`.
# Without the `preserve_order` feature, serde_json's Map is a BTreeMap,
# so keys come out alphabetically sorted in compact form. Pre-register
# several plausible canonicalizations so whichever one serde produced
# matches one of our entries.
HASHES="$(python3 -c '
import hashlib, json
v = {"tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/foo"}}
for variant in [
    json.dumps(v),
    json.dumps(v, separators=(",",":")),
    json.dumps(v, sort_keys=True),
    json.dumps(v, sort_keys=True, separators=(",",":")),
]:
    print(hashlib.sha256(variant.encode()).hexdigest())
')"

mkdir -p "$ROOT/override-requests" "$ROOT/overrides"
for H in $HASHES; do
  RID="$(python3 -c 'import uuid;print(uuid.uuid4())')"
  cat >"$ROOT/override-requests/$RID.json" <<EOF
{
  "id": "$RID",
  "rule_id": "no-rm-rf",
  "reason": "smoke test",
  "tool_call_hash": "$H",
  "requested_at": 1,
  "session_id": "smoke",
  "status": "pending"
}
EOF
done

# Sanity: CLI sees them.
"$THOTH" --root "$ROOT" override list >/dev/null

# Approve each via the CLI.
for f in "$ROOT/override-requests"/*.json; do
  [[ -e "$f" ]] || continue
  id="$(python3 -c "import json,sys;print(json.load(open('$f'))['id'])")"
  "$THOTH" --root "$ROOT" override approve "$id" --ttl-turns 1 >/dev/null
done

VERDICT2="$(run_gate "$PAYLOAD1")"
echo "  verdict: $VERDICT2"
DEC2="$(printf '%s' "$VERDICT2" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("decision",""))')"
REASON2="$(printf '%s' "$VERDICT2" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("reason",""))')"
if [[ "$DEC2" == "approve" ]] && [[ "$REASON2" == *override_consumed* ]]; then
  pass "override approve → gate approves (reason contains override_consumed)"
else
  fail "expected approve+override_consumed, got decision=$DEC2 reason=$REASON2"
fi

# -------- Pain #3 : RequireRecall blocks w/o recall, passes with recall
note "[pain-3] RequireRecall rule blocks without prior recall, passes with it"

cat >"$ROOT/rules.project.toml" <<'EOF'
[rules.needs-recall]
tool = "Edit"
path_glob = "**/retriever.rs"
natural = "retriever"
enforcement = { RequireRecall = { recall_within_turns = 5 } }
EOF
: >"$ROOT/gate.jsonl"  # empty — no matching recall

PAYLOAD3='{"tool_name":"Edit","tool_input":{"file_path":"src/retriever.rs","old_string":"a","new_string":"b"}}'
VERDICT3A="$(run_gate "$PAYLOAD3")"
echo "  verdict (no recall): $VERDICT3A"
DEC3A="$(printf '%s' "$VERDICT3A" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("decision",""))')"
REASON3A="$(printf '%s' "$VERDICT3A" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("reason",""))')"
if [[ "$DEC3A" == "block" ]] && [[ "$REASON3A" == *thoth_recall* ]]; then
  pass "RequireRecall blocks without recall (reason mentions thoth_recall)"
else
  fail "expected block+thoth_recall message, got decision=$DEC3A reason=$REASON3A"
fi

# Inject a matching recall event and retry.
printf '{"tool":"thoth_recall","query":"retriever updates"}\n' >"$ROOT/gate.jsonl"
VERDICT3B="$(run_gate "$PAYLOAD3")"
echo "  verdict (with recall): $VERDICT3B"
DEC3B="$(printf '%s' "$VERDICT3B" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("decision",""))')"
if [[ "$DEC3B" != "block" ]]; then
  pass "RequireRecall satisfied → not blocked (decision=$DEC3B)"
else
  fail "RequireRecall still blocking with matching recall: $VERDICT3B"
fi

# -------- Pain #4 : Stop hook increments workflow-violations -----------
note "[pain-4] Stop hook on active workflow appends workflow-violations.jsonl"

mkdir -p "$ROOT/workflow"
cat >"$ROOT/workflow/smoke-sess.json" <<'EOF'
{
  "session_id": "smoke-sess",
  "workflow_name": "hoangsa:cook",
  "status": "active",
  "started_at": 1700000000,
  "expected_steps": [],
  "completed_steps": [],
  "last_step_at": 1700000000
}
EOF

# Remove any stale violation log from the previous setup.
rm -f "$ROOT/workflow-violations.jsonl"

# Fire the Stop hook. `thoth hooks exec stop` reads JSON on stdin and
# walks every active workflow when session_id is omitted.
env -i PATH="/usr/bin:/bin" HOME="$HOME_DIR" THOTH_ROOT="$ROOT" \
  "$THOTH" --root "$ROOT" hooks exec stop <<<'{"session_id":"smoke-sess"}' \
  >/dev/null 2>"$TMP/stop.stderr" || true

echo "  stderr: $(head -c 200 "$TMP/stop.stderr" || true)"

if [[ -s "$ROOT/workflow-violations.jsonl" ]]; then
  head -n 3 "$ROOT/workflow-violations.jsonl" | sed 's/^/    /'
  if grep -q '"session_id":"smoke-sess"' "$ROOT/workflow-violations.jsonl" \
     && grep -q 'stop_without_complete' "$ROOT/workflow-violations.jsonl"; then
    pass "workflow-violations.jsonl has a 'stop_without_complete' row for smoke-sess"
  else
    fail "violations file exists but missing expected fields"
  fi
else
  fail "workflow-violations.jsonl not written"
fi

# -------- summary ------------------------------------------------------
note "[summary] PASS=$PASS FAIL=$FAIL"
if [[ "$FAIL" -eq 0 ]]; then
  echo "all 4 pain points green"
  exit 0
else
  exit 1
fi
