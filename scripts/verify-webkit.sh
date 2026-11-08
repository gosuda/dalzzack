#!/usr/bin/env bash
#
# Check, on a Mac, how the WKWebView engine handles certificates and cookies:
# a load that failed is reported as failed, `start --trust-cert` adds one trust
# anchor and keeps Apple's other checks, and `cookies` reads the WebKit jar.
#
#   bash scripts/verify-webkit.sh
#
# It needs node and openssl for the local HTTPS fixture
# (scripts/fixtures/tls-fixture.mjs). Nothing is added to a keychain.
#
# Environment:
#   DALZZACK=<path>     the binary to test (default: `dalzzack` on PATH)
#   LOCAL_ONLY=1        skip the public badssl.com sites
#   PORTAL_CERT=<file>  also run the Portal checks with this fullchain.pem. An app
#                       named `app` must be connected to the relay.
#   PORTAL_HOST=<host>  the Portal relay host (default: portal.localhost:4643)
#
# It prints PASS or FAIL for each check, then `ALL PASSED` or the number of
# failures, and exits non-zero when a check failed.

set -u
export DALZZACK_OUTPUT=full
D=${DALZZACK:-dalzzack}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d -t dalzzack-webkit-XXXXXX)
ID="webkit-$$"
FAILS=0
FIXTURE_PID=""
OUT=""

cleanup() {
  for i in "$ID" "$ID-trust" "$ID-portal"; do
    "$D" stop --id "$i" >/dev/null 2>&1 || true
  done
  if [ -n "$FIXTURE_PID" ]; then kill "$FIXTURE_PID" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# expect NAME CONDITION COMMAND...: run a dalzzack COMMAND. PASS when the
# JavaScript CONDITION is true for its JSON result `d`.
expect() {
  local name=$1 cond=$2
  shift 2
  OUT=$("$D" "$@" 2>&1)
  if node -e 'const d = JSON.parse(process.argv[1]); process.exit(eval(process.argv[2]) ? 0 : 1)' \
    "$OUT" "$cond" 2>/dev/null; then
    printf 'PASS %s\n' "$name"
  else
    printf 'FAIL %s\n     %s\n' "$name" "$(printf '%s' "$OUT" | tr '\n' ' ' | cut -c1-800)"
    FAILS=$((FAILS + 1))
  fi
}

# Print what Security.framework said about the last refused certificate.
trust_note() {
  node -e 'const d = JSON.parse(process.argv[1]); if (d.trust_error) console.log("     trust: " + d.trust_error.message + " (" + d.trust_error.code + ")")' \
    "$OUT" 2>/dev/null || true
}

FAILED='d.status === "failed" && d.error === "NAVIGATION_FAILED"'
UNTRUSTED="$FAILED"' && d.error_domain === "NSURLErrorDomain" && d.error_code === -1202 && !!d.trust_error'
REFUSED="$FAILED"' && !!d.trust_error'
LOADED='d.status === "completed"'

echo "binary: $("$D" --version)"
"$D" start --id "$ID" --ttl 15m >/dev/null

if [ "${LOCAL_ONLY:-0}" != 1 ]; then
  echo "== Public sites =="
  expect "self-signed.badssl.com fails with NSURLErrorDomain -1202" "$UNTRUSTED" \
    navigate -i "$ID" --url https://self-signed.badssl.com/
  trust_note
  expect "expired.badssl.com fails" "$REFUSED" navigate -i "$ID" --url https://expired.badssl.com/
  trust_note
  expect "wrong.host.badssl.com fails" "$REFUSED" navigate -i "$ID" --url https://wrong.host.badssl.com/
  trust_note
  expect "example.com loads" "$LOADED"' && d.title === "Example Domain"' \
    navigate -i "$ID" --url https://example.com/
  expect "failed-requests lists the three failed loads" \
    'd.status === "completed" && d.coverage === "main_frame_only" && d.failures.length >= 3' \
    failed-requests -i "$ID"
fi

echo "== Local HTTPS fixture =="
node "$HERE/fixtures/tls-fixture.mjs" "$WORK/fixture" >"$WORK/fixture.json" 2>"$WORK/fixture.err" &
FIXTURE_PID=$!
for _ in $(seq 1 100); do
  [ -s "$WORK/fixture.json" ] && break
  sleep 0.2
done
if ! [ -s "$WORK/fixture.json" ]; then
  echo "FAIL the fixture did not start: $(cat "$WORK/fixture.err")"
  exit 1
fi
fixture() {
  node -e 'console.log(JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))[process.argv[2]])' \
    "$WORK/fixture.json" "$1"
}
CA=$(fixture ca)
PAGE=$(fixture page)
MISMATCH=$(fixture mismatch)
LONG=$(fixture long)

expect "without --trust-cert, the test CA is refused (-1202)" "$UNTRUSTED" \
  navigate -i "$ID" --url "$PAGE"
trust_note
"$D" navigate -i "$ID" --url https://example.com/ >/dev/null
# The load fails before the wait starts, as after a click an agent waits on.
"$D" exec-js -i "$ID" --script "setTimeout(function () { location.href = '$PAGE'; }, 100); return 1" >/dev/null
sleep 3
expect "wait-navigation, 3 s after the action, reports the failed load at once" "$FAILED"' && d.elapsed_ms < 5000' \
  wait-navigation -i "$ID" --timeout-ms 20000

echo "== --trust-cert =="
expect "start reports the trusted anchor" \
  'd.trusted_certs.length === 1 && d.trusted_certs[0].sha256.length === 64' \
  start --id "$ID-trust" --ttl 15m --trust-cert "$CA"
expect "list reports the trusted anchor" \
  'd.instances.some(i => i.id === "'"$ID-trust"'" && i.trusted_certs.length === 1)' list
expect "the fixture page loads" "$LOADED"' && d.title === "dalzzack tls fixture"' \
  navigate -i "$ID-trust" --url "$PAGE"
# The page loads an image, calls fetch, and opens a WebSocket, each on its own
# origin, so each is a new TLS connection and a new challenge.
WAIT_TLS='return await new Promise(function (done) {
  var t = Date.now();
  (function poll() {
    var s = window.__tls;
    if (s.img !== "pending" && s.fetch !== "pending" && s.ws !== "pending") return done(JSON.stringify(s));
    if (Date.now() - t > 10000) return done(JSON.stringify(s));
    setTimeout(poll, 200);
  })();
});'
for k in img fetch ws; do
  case $k in
    img) what="an image from another origin loads" ;;
    fetch) what="fetch to another origin works" ;;
    ws) what="a WebSocket to another origin opens" ;;
  esac
  expect "$what" 'JSON.parse(d.result).'"$k"' === "ok"' \
    exec-js -i "$ID-trust" --await --script "$WAIT_TLS"
done
expect "127.0.0.1 is still refused: the certificate names localhost" "$REFUSED" \
  navigate -i "$ID-trust" --url "$MISMATCH"
trust_note
expect "a 10-year certificate is still refused (the 825-day rule)" "$REFUSED" \
  navigate -i "$ID-trust" --url "$LONG"
trust_note
expect "example.com loads with the flag" "$LOADED"' && d.title === "Example Domain"' \
  navigate -i "$ID-trust" --url https://example.com/

echo "== cookies =="
"$D" navigate -i "$ID-trust" --url "${PAGE}set-cookie" >/dev/null
expect "cookies shows a session cookie's Secure, HttpOnly, SameSite and host-only" \
  'const c = d.cookies.find(x => x.name === "sid"); c && c.value === "s3cret" && c.secure && c.http_only && c.same_site === "Lax" && c.host_only && c.session' \
  cookies -i "$ID-trust" --url "$PAGE" --no-redact
expect "cookies shows a persistent cookie's expiry" \
  'const c = d.cookies.find(x => x.name === "pref"); c && c.same_site === "Strict" && !c.session && !!c.expires' \
  cookies -i "$ID-trust" --url "$PAGE"
expect "cookies clear deletes them" 'd.status === "completed" && d.remaining.length === 0 && d.deleted >= 2' \
  cookies clear -i "$ID-trust" --url "$PAGE" --force-shared

if [ -n "${PORTAL_CERT:-}" ]; then
  HOST=${PORTAL_HOST:-portal.localhost:4643}
  PORT=${HOST##*:}
  echo "== Portal ($HOST) =="
  expect "start trusts the Portal certificate" 'd.trusted_certs.length >= 1' \
    start --id "$ID-portal" --ttl 15m --trust-cert "$PORTAL_CERT"
  expect "https://$HOST/api/healthz loads" "$LOADED" navigate -i "$ID-portal" --url "https://$HOST/api/healthz"
  expect "https://app.$HOST/ loads" "$LOADED" navigate -i "$ID-portal" --url "https://app.$HOST/"
  # The relay serves only its own host and the apps connected to it. For any
  # other name it closes the connection before it sends a certificate, so the
  # load fails with -1200 and there is no trust_error to check.
  expect "https://app.localhost:$PORT/ does not load (the relay serves no such host)" "$FAILED" \
    navigate -i "$ID-portal" --url "https://app.localhost:$PORT/"
  expect "example.com loads with the flag" "$LOADED" navigate -i "$ID-portal" --url https://example.com/
fi

echo
if [ "$FAILS" -eq 0 ]; then
  echo "ALL PASSED"
else
  echo "$FAILS FAILED"
  exit 1
fi
