#!/usr/bin/env bash
#
# End-to-end smoke test: start a daemon, navigate, screenshot, describe, stop.
# Exits non-zero on any failure. Used locally and in CI.
#
# Assumes `dalzzack` is on PATH. Run `npm run deploy` first if not.

set -euo pipefail

# This script reads whole results, not the short form that a pipe gets.
export DALZZACK_OUTPUT=full

ID="smoke-$$"
WORKDIR=$(mktemp -d -t dalzzack-smoke-XXXXXX)
trap 'dalzzack stop --id "$ID" >/dev/null 2>&1 || true; dalzzack stop --id "$ID-kill" >/dev/null 2>&1 || true; rm -rf "$WORKDIR"' EXIT

step() { printf '\n== %s ==\n' "$*"; }
assert_status() {
  local got
  got=$(printf '%s' "$1" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')
  if [ "$got" != "completed" ]; then
    printf 'FAIL: expected status=completed, got status=%s\nresponse:\n%s\n' "$got" "$1" >&2
    exit 1
  fi
}

step "dalzzack --version"
dalzzack --version

step "dalzzack tools (schema introspection)"
COUNT=$(dalzzack tools | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
echo "tools defined: $COUNT"
if [ "$COUNT" -lt 25 ]; then
  echo "FAIL: expected at least 25 tools, got $COUNT" >&2
  exit 1
fi

step "start daemon (id=$ID)"
# --console-capture: on macOS there is no console transport the page cannot
# observe, so capture is opt-in and off by default. The
# smoke test exercises console_logs, so it has to ask for it. On Windows the
# flag is accepted and ignored — CDP capture is always on there.
START_OUT=$(dalzzack start --id "$ID" --console-capture)
echo "$START_OUT"
echo "$START_OUT" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if d.get("status") not in ("started", "already_running"):
    raise SystemExit(f"unexpected start status: {d}")
'
sleep 2

step "navigate to example.com"
NAV=$(dalzzack navigate -i "$ID" --url "https://example.com")
echo "$NAV"
assert_status "$NAV"

step "exec-js: confirm title"
JS=$(dalzzack exec-js -i "$ID" --script 'return document.title')
echo "$JS"
TITLE=$(printf '%s' "$JS" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("result",""))')
if [ "$TITLE" != "Example Domain" ]; then
  echo "FAIL: expected title 'Example Domain', got '$TITLE'" >&2
  exit 1
fi

step "the toolbar leaves the page the size it was asked to be"
# The window has a strip above the page (history, address, running command). The
# page must still be the default 1200x800, and a screenshot must be the page
# alone: no title bar, no strip.
SIZE=$(dalzzack exec-js -i "$ID" --script 'return innerWidth + "x" + innerHeight' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("result",""))')
if [ "$SIZE" != "1200x800" ]; then
  echo "FAIL: expected a 1200x800 page under the toolbar, got $SIZE" >&2
  exit 1
fi
dalzzack screenshot -i "$ID" --output "$WORKDIR/size.png" >/dev/null
python3 - "$WORKDIR/size.png" <<'PY'
import struct, sys
with open(sys.argv[1], "rb") as f:
    head = f.read(24)
w, h = struct.unpack(">II", head[16:24])
scale = w / 1200
if abs(h - 800 * scale) > 1:
    raise SystemExit(f"FAIL: screenshot is {w}x{h}; a 1200x800 page at {scale}x is {w}x{800 * scale:.0f}")
print(f"screenshot {w}x{h} = the page alone at {scale}x")
PY

step "describe"
DESC=$(dalzzack describe -i "$ID" --output "$WORKDIR/describe.png")
echo "$DESC" | head -10
assert_status "$DESC"

step "screenshot"
SHOT=$(dalzzack screenshot -i "$ID" --output "$WORKDIR/shot.png")
echo "$SHOT" | head -5
assert_status "$SHOT"
test -s "$WORKDIR/shot.png" || { echo "FAIL: screenshot file empty" >&2; exit 1; }

step "click_element with bad selector should return nearby hint"
BAD=$(dalzzack click -i "$ID" --selector "#does-not-exist-12345" || true)
echo "$BAD"
HAS_NEARBY=$(printf '%s' "$BAD" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
    print("yes" if "nearby" in d else "no")
except Exception:
    print("no")
')
if [ "$HAS_NEARBY" != "yes" ]; then
  echo "FAIL: expected `nearby` field in selector-not-found response" >&2
  exit 1
fi

step "navigate again to test browser history (back)"
dalzzack navigate -i "$ID" --url "https://example.org" >/dev/null
sleep 1
BACK=$(dalzzack back -i "$ID")
echo "$BACK"
CHANGED=$(printf '%s' "$BACK" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("changed", False))')
if [ "$CHANGED" != "True" ]; then
  echo "FAIL: expected history.back to change URL" >&2
  exit 1
fi

step "console_logs (after triggering a deliberate log)"
dalzzack exec-js -i "$ID" --script "console.log('smoke-test-marker'); return 'ok'" >/dev/null
LOGS=$(dalzzack console-logs -i "$ID" --max 50)
echo "$LOGS" | head -20
HAS_MARKER=$(printf '%s' "$LOGS" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
    print("yes" if any("smoke-test-marker" in (l.get("message","") or "") for l in d.get("logs", [])) else "no")
except Exception:
    print("no")
')
if [ "$HAS_MARKER" != "yes" ]; then
  # Report which transport answered: an empty buffer from a live transport is a
  # real regression, an empty buffer from one that never armed is a different
  # bug entirely, and the two used to look identical here.
  TRANSPORT=$(printf '%s' "$LOGS" | python3 -c '
import json, sys
try:
    print(json.load(sys.stdin).get("console_transport", "?"))
except Exception:
    print("?")
')
  echo "FAIL: console_logs did not capture the smoke-test-marker (console_transport=$TRANSPORT)" >&2
  exit 1
fi

step "stop daemon"
dalzzack stop --id "$ID"

step "kill ends a real daemon"
dalzzack start --id "$ID-kill" >/dev/null
KILLED=$(dalzzack kill --id "$ID-kill")
if ! printf '%s' "$KILLED" | grep -Eq '"status": *"killed"'; then
  printf 'FAIL: expected status=killed\nresponse:\n%s\n' "$KILLED" >&2
  exit 1
fi

printf '\nOK — smoke test passed.\n'
