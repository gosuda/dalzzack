#!/usr/bin/env node
/**
 * A local HTTPS fixture for the certificate tests.
 *
 *   node scripts/fixtures/tls-fixture.mjs <work-dir>
 *
 * It makes a test CA and leaf certificates with `openssl` in <work-dir>, starts
 * the servers, and prints one JSON line with the ports and files. It serves
 * until it is killed. Nothing is added to a keychain or a system store.
 *
 * Certificates (all signed by <work-dir>/ca.pem, names `localhost` only):
 *   good  365 days: a browser that trusts ca.pem accepts it for `localhost`,
 *                   and refuses it for `127.0.0.1` (host name mismatch).
 *   long  3650 days: Apple refuses a TLS certificate valid for more than
 *                   825 days, whatever the anchor.
 *
 * Servers: `page`, `asset`, `api`, and `ws` use `good`; `long` uses `long`.
 * `page` serves `/` with a script that loads an image from `asset`, calls
 * `fetch` on `api`, and opens a WebSocket to `ws`, each a new TLS connection,
 * and puts the outcome in `window.__tls`. `/set-cookie` sets two cookies, and
 * `/link` is a page with one link (`#go`) to the URL in `?to=`.
 */

import { execFileSync } from "child_process";
import { createHash } from "crypto";
import { mkdirSync, readFileSync, writeFileSync } from "fs";
import { createServer } from "https";
import { join, resolve } from "path";

const dir = resolve(process.argv[2] || "tls-fixture");
mkdirSync(dir, { recursive: true });
const file = (name) => join(dir, name);

function openssl(args) {
  execFileSync("openssl", args, { stdio: ["ignore", "ignore", "pipe"] });
}

// --- Certificates -----------------------------------------------------------

writeFileSync(
  file("ca.cnf"),
  [
    "[req]",
    "distinguished_name = dn",
    "prompt = no",
    "[dn]",
    "CN = dalzzack test CA",
    "[v3_ca]",
    "basicConstraints = critical, CA:TRUE",
    "keyUsage = critical, keyCertSign, cRLSign",
    "subjectKeyIdentifier = hash",
    "",
  ].join("\n"),
);
writeFileSync(
  file("leaf.cnf"),
  [
    "[v3_leaf]",
    "basicConstraints = critical, CA:FALSE",
    "keyUsage = critical, digitalSignature, keyEncipherment",
    "extendedKeyUsage = serverAuth",
    "subjectAltName = DNS:localhost",
    "",
  ].join("\n"),
);

openssl(["genrsa", "-out", file("ca.key"), "2048"]);
openssl([
  "req", "-x509", "-new", "-key", file("ca.key"), "-sha256", "-days", "3650",
  "-config", file("ca.cnf"), "-extensions", "v3_ca", "-out", file("ca.pem"),
]);

function leaf(name, days) {
  openssl(["genrsa", "-out", file(`${name}.key`), "2048"]);
  openssl([
    "req", "-new", "-key", file(`${name}.key`), "-subj", `/CN=localhost ${name}`,
    "-out", file(`${name}.csr`),
  ]);
  openssl([
    "x509", "-req", "-in", file(`${name}.csr`), "-CA", file("ca.pem"),
    "-CAkey", file("ca.key"), "-CAcreateserial", "-sha256", "-days", String(days),
    "-extfile", file("leaf.cnf"), "-extensions", "v3_leaf", "-out", file(`${name}.pem`),
  ]);
  return { key: readFileSync(file(`${name}.key`)), cert: readFileSync(file(`${name}.pem`)) };
}

const good = leaf("good", 365);
const long = leaf("long", 3650);

// --- Servers ----------------------------------------------------------------

const ports = {};

function page() {
  const asset = `https://localhost:${ports.asset}`;
  const api = `https://localhost:${ports.api}`;
  const ws = `wss://localhost:${ports.ws}`;
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>dalzzack tls fixture</title></head>
<body><h1>dalzzack tls fixture</h1>
<script>
window.__tls = { img: "pending", fetch: "pending", ws: "pending" };
const img = new Image();
img.onload = () => { window.__tls.img = "ok"; };
img.onerror = () => { window.__tls.img = "error"; };
img.src = "${asset}/pixel.svg";
fetch("${api}/api/healthz")
  .then((r) => r.json())
  .then((j) => { window.__tls.fetch = j.ok ? "ok" : "bad answer"; })
  .catch((e) => { window.__tls.fetch = "error: " + e.message; });
try {
  const sock = new WebSocket("${ws}/ws");
  sock.onmessage = (m) => { window.__tls.ws = m.data === "hello" ? "ok" : "bad answer"; sock.close(); };
  sock.onerror = () => { if (window.__tls.ws === "pending") window.__tls.ws = "error"; };
} catch (e) { window.__tls.ws = "error: " + e.message; }
</script></body></html>`;
}

const PIXEL =
  '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1"/></svg>';

function handle(req, res) {
  const url = new URL(req.url, "https://localhost");
  const send = (status, type, body, headers = {}) => {
    res.writeHead(status, { "content-type": type, "access-control-allow-origin": "*", ...headers });
    res.end(body);
  };
  switch (url.pathname) {
    case "/":
      return send(200, "text/html; charset=utf-8", page());
    case "/pixel.svg":
      return send(200, "image/svg+xml", PIXEL);
    case "/api/healthz":
      return send(200, "application/json", JSON.stringify({ ok: true }));
    case "/set-cookie":
      return send(200, "text/html; charset=utf-8", "<title>cookies set</title>cookies set", {
        "set-cookie": [
          "sid=s3cret; Path=/; Secure; HttpOnly; SameSite=Lax",
          "pref=1; Path=/; Max-Age=3600; SameSite=Strict",
        ],
      });
    case "/link": {
      const to = url.searchParams.get("to") || "/";
      const safe = to.replace(/[<>"]/g, "");
      return send(
        200,
        "text/html; charset=utf-8",
        `<title>link</title><a id="go" href="${safe}">go</a>`,
      );
    }
    default:
      return send(404, "text/plain", "not found");
  }
}

/** The WebSocket handshake and one text frame, "hello". */
function upgrade(req, socket) {
  const key = req.headers["sec-websocket-key"];
  if (!key) return socket.destroy();
  const accept = createHash("sha1")
    .update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
    .digest("base64");
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  const text = Buffer.from("hello");
  socket.write(Buffer.concat([Buffer.from([0x81, text.length]), text]));
  socket.on("data", () => {});
  socket.on("error", () => {});
}

function listen(name, creds) {
  return new Promise((done) => {
    const server = createServer(creds, handle);
    server.on("upgrade", upgrade);
    server.on("tlsClientError", () => {});
    server.listen(0, "127.0.0.1", () => {
      ports[name] = server.address().port;
      done();
    });
  });
}

for (const name of ["page", "asset", "api", "ws"]) await listen(name, good);
await listen("long", long);

console.log(
  JSON.stringify({
    ca: file("ca.pem"),
    ports,
    page: `https://localhost:${ports.page}/`,
    mismatch: `https://127.0.0.1:${ports.page}/`,
    long: `https://localhost:${ports.long}/`,
  }),
);
