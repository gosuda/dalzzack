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
trap 'dalzzack stop --id "$ID" >/dev/null 2>&1 || true; dalzzack stop --id "$ID-kill" >/dev/null 2>&1 || true; [ -n "${FIXTURE_PID:-}" ] && kill "$FIXTURE_PID" >/dev/null 2>&1 || true; rm -rf "$WORKDIR"' EXIT

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
#
# The page size fits a small screen. macOS keeps a window inside the screen's
# visible area, and the CI runner's screen is too short for the default 1200x800
# page plus the toolbar and the title bar: the page came out 1200x581 there.
PAGE_W=960
PAGE_H=480
START_OUT=$(dalzzack start --id "$ID" --console-capture --width "$PAGE_W" --height "$PAGE_H")
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
# page must still be the size `start` asked for, and a screenshot must be the
# page alone: no title bar, no strip.
SIZE=$(dalzzack exec-js -i "$ID" --script 'return innerWidth + "x" + innerHeight' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("result",""))')
if [ "$SIZE" != "${PAGE_W}x${PAGE_H}" ]; then
  echo "FAIL: expected a ${PAGE_W}x${PAGE_H} page under the toolbar, got $SIZE" >&2
  exit 1
fi
dalzzack screenshot -i "$ID" --output "$WORKDIR/size.png" >/dev/null
python3 - "$WORKDIR/size.png" "$PAGE_W" "$PAGE_H" <<'PY'
import struct, sys
with open(sys.argv[1], "rb") as f:
    head = f.read(24)
page_w, page_h = int(sys.argv[2]), int(sys.argv[3])
w, h = struct.unpack(">II", head[16:24])
scale = w / page_w
if abs(h - page_h * scale) > 1:
    raise SystemExit(f"FAIL: screenshot is {w}x{h}; a {page_w}x{page_h} page at {scale}x is {w}x{page_h * scale:.0f}")
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

step "scroll finds the container the page really scrolls in"
# A page whose text scrolls inside #page while the window itself cannot: what a
# documentation site looks like. `scroll` must pick #page, move it, and say so.
SCROLL_URL=$(python3 - <<'PY'
import base64
html = ('<!doctype html><html><body style="margin:0;overflow:hidden">'
        '<div id="page" style="height:100vh;overflow:auto"><div style="height:3000px">'
        '<p data-x="y">top</p></div></div></body></html>')
print("data:text/html;base64," + base64.b64encode(html.encode()).decode())
PY
)
dalzzack navigate -i "$ID" --url "$SCROLL_URL" >/dev/null
MOVE=$(dalzzack scroll -i "$ID" --amount 300)
printf '%s' "$MOVE" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if not (d.get("moved") is True and d.get("container") == "#page" and d["scrolled"]["dy"] == 300):
    raise SystemExit(f"FAIL: scroll did not move #page by 300: {d}")
print("scrolled", d["container"], d["scrolled"])
'
END=$(dalzzack scroll -i "$ID" --to bottom)
printf '%s' "$END" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if not (d.get("moved") is True and d.get("at_end") is True):
    raise SystemExit(f"FAIL: scroll --to bottom did not reach the end: {d}")
'
STUCK=$(dalzzack scroll -i "$ID" --amount 300)
printf '%s' "$STUCK" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if d.get("moved") is not False or "end" not in d.get("hint", ""):
    raise SystemExit(f"FAIL: scrolling past the end should report moved=false with a hint: {d}")
'
# A selector with a quote in it used to break the script it was pasted into.
QUOTED=$(dalzzack scroll -i "$ID" --selector "p[data-x='y']" --amount 10)
printf '%s' "$QUOTED" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if d.get("status") != "completed" or d.get("moved") is not False:
    raise SystemExit(f"FAIL: a selector with a quote should run and report moved=false: {d}")
'

step "resize to a phone size reports the size the page really has"
RESIZED=$(dalzzack resize -i "$ID" --width 375 --height 700)
printf '%s' "$RESIZED" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if d.get("exact") is not True or d["viewport"] != {"width": 375, "height": 700}:
    raise SystemExit(f"FAIL: expected a 375x700 page, got {d}")
print("page is", d["viewport"])
'
INNER=$(dalzzack exec-js -i "$ID" --script 'return innerWidth + "x" + innerHeight' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin).get("result",""))')
if [ "$INNER" != "375x700" ]; then
  echo "FAIL: the page says it is $INNER, the window reported 375x700" >&2
  exit 1
fi

step "nav-log sees redirects, statuses and a bot challenge"
# The fixture answers `/` with 302 -> 301 -> 200, `/challenge` with a 403 carrying
# cf-mitigated, and `/missing` with a 404. On macOS the navigation delegate feeds
# the log (navigations only); elsewhere CDP does. Needs node, which CI has.
if command -v node >/dev/null 2>&1; then
  FIXTURE_PORT=$((20000 + RANDOM % 20000))
  node "$(dirname "$0")/fixtures/http-fixture.mjs" "$FIXTURE_PORT" >/dev/null 2>&1 &
  FIXTURE_PID=$!
  # Not a job to report on when the trap ends it.
  disown "$FIXTURE_PID" 2>/dev/null || true
  sleep 1
  for route in "" challenge missing; do
    dalzzack navigate -i "$ID" --url "http://127.0.0.1:$FIXTURE_PORT/$route" >/dev/null
    sleep 1
  done
  dalzzack nav-log -i "$ID" --max 10 | FIXTURE_PORT="$FIXTURE_PORT" python3 -c '
import json, os, sys
port = os.environ["FIXTURE_PORT"]
d = json.load(sys.stdin)
by = {n["requested_url"]: n for n in d["navigations"]}
root = by.get(f"http://127.0.0.1:{port}/")
if not root or root["status"] != 200 or len(root["hops"]) != 3:
    raise SystemExit(f"FAIL: / should be a 3-hop chain ending in 200: {root}")
first_hop = root["hops"][0]
if not first_hop["key_headers"].get("location", "").endswith("/hop2"):
    raise SystemExit(f"FAIL: the first hop should say where it led: {first_hop}")
challenge = by.get(f"http://127.0.0.1:{port}/challenge")
if not challenge or challenge["status"] != 403 or challenge.get("challenge") != "cloudflare:challenge":
    raise SystemExit(f"FAIL: /challenge should be a 403 flagged as a challenge: {challenge}")
missing = by.get(f"http://127.0.0.1:{port}/missing")
if not missing or missing["status"] != 404:
    raise SystemExit(f"FAIL: /missing should be a 404: {missing}")
print("nav-log: redirect chain, challenge and 404 seen; source =", d.get("source"))
'
else
  echo "node is not installed: skipping the nav-log fixture check"
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
