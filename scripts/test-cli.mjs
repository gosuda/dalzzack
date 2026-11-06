#!/usr/bin/env node
/**
 * dalzzack CLI Integration Test
 *
 * Tests the daemon + CLI command workflow end-to-end.
 *
 * Usage:
 *   node scripts/test-cli.mjs                    # Run all tests
 *   node scripts/test-cli.mjs navigate            # Run single test
 *   node scripts/test-cli.mjs --binary ./dalzzack   # Custom binary path
 */

import { execFileSync, execSync, spawn } from "child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "fs";
import { join, dirname, resolve } from "path";
import { fileURLToPath, pathToFileURL } from "url";
import { platform, tmpdir } from "os";
import { createServer } from "http";

const __dirname = dirname(fileURLToPath(import.meta.url));
const PROJECT_ROOT = join(__dirname, "..");

// --- Binary resolution ---

function findBinary() {
  const ext = platform() === "win32" ? ".exe" : "";
  const candidates = [
    resolve(PROJECT_ROOT, "src-tauri/target/release/dalzzack" + ext),
    resolve(PROJECT_ROOT, "src-tauri/target/debug/dalzzack" + ext),
  ];
  for (const p of candidates) {
    if (existsSync(p)) return p;
  }
  return null;
}

// --- CLI argument parsing ---

let binaryPath = null;
let filterTest = null;

for (let i = 2; i < process.argv.length; i++) {
  if (process.argv[i] === "--binary" && process.argv[i + 1]) {
    binaryPath = resolve(process.argv[++i]);
  } else {
    filterTest = process.argv[i];
  }
}

if (!binaryPath) binaryPath = findBinary();
if (!binaryPath) {
  console.error("[ERROR] dalzzack binary not found. Build first or pass --binary <path>");
  process.exit(1);
}

const INSTANCE_ID = `test-${Date.now()}`;
const TIMEOUT_MS = 15000;

// --- Helpers ---

function run(args, { expectFail = false, timeout = TIMEOUT_MS } = {}) {
  const cmd = `"${binaryPath}" ${args}`;
  try {
    const output = execSync(cmd, {
      timeout,
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
      // These tests read whole results, not the short form that a pipe gets.
      env: { ...process.env, DALZZACK_OUTPUT: "full" },
    });
    return { ok: true, output: output.trim(), parsed: tryParse(output) };
  } catch (e) {
    if (expectFail) {
      return { ok: false, output: (e.stdout || e.stderr || e.message).toString().trim() };
    }
    throw new Error(`Command failed: ${cmd}\n${e.stderr || e.message}`);
  }
}

function tryParse(str) {
  try {
    return JSON.parse(str.trim());
  } catch {
    return null;
  }
}

function assert(condition, msg) {
  if (!condition) throw new Error(`Assertion failed: ${msg}`);
}

function cleanup(files) {
  for (const f of files) {
    try { if (existsSync(f)) unlinkSync(f); } catch {}
  }
}

// --- Test definitions ---

const tests = {
  async "start/list/stop"() {
    // Start
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    // List
    const list = run("list");
    assert(list.output.includes(INSTANCE_ID), `Instance ${INSTANCE_ID} should appear in list`);

    // Stop
    run(`stop --id ${INSTANCE_ID}`);
    await sleep(500);

    // Verify stopped
    const list2 = run("list");
    assert(!list2.output.includes(INSTANCE_ID), "Instance should be gone after stop");
  },

  async navigate() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      const result = run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      assert(result.parsed, "Should return JSON");
      assert(result.parsed.status === "completed", `Navigate should complete, got: ${result.parsed.status}`);
      assert(result.parsed.url.includes("example.com"), "URL should contain example.com");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async "get-url"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      const result = run(`get-url -i ${INSTANCE_ID}`);
      assert(result.parsed, "Should return JSON");
      assert(result.parsed.url.includes("example.com"), "URL should match");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async screenshot() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      const result = run(`screenshot -i ${INSTANCE_ID}`);
      assert(result.parsed, "Should return JSON");
      assert(result.parsed.status === "completed", "Screenshot should complete");
      assert(result.parsed.file, "Should have file path");

      const filePath = result.parsed.file;
      assert(existsSync(filePath), `Screenshot file should exist: ${filePath}`);
      cleanup([filePath]);
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async "exec-js"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      const result = run(`exec-js -i ${INSTANCE_ID} --script "return document.title"`);
      assert(result.parsed, "Should return JSON");
      assert(result.parsed.status === "completed", "JS exec should complete");
      assert(typeof result.parsed.result === "string", "Should return a string result");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async "get-html"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      // example.com no longer has an <h1> (2026); <p> is what it serves.
      const result = run(`get-html -i ${INSTANCE_ID} --selector "p"`);
      assert(result.parsed, "Should return JSON");
      assert(result.parsed.html, "Should contain HTML");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async wait() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      const t0 = Date.now();
      const result = run(`wait -i ${INSTANCE_ID} --ms 500`);
      const elapsed = Date.now() - t0;
      assert(result.parsed, "Should return JSON");
      assert(elapsed >= 400, `Should wait at least 400ms, got ${elapsed}ms`);
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async "wait-for"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      const result = run(`wait-for -i ${INSTANCE_ID} --selector "body" --timeout-ms 5000`);
      assert(result.parsed, "Should return JSON");
      assert(result.parsed.found === true, "body should be found");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async "check-visible"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);

      // Visible element
      const result = run(`check-visible -i ${INSTANCE_ID} --selector "p"`);
      assert(result.parsed, "Should return JSON");
      assert(result.parsed.exists === true, "p should exist");

      // Non-existent element
      const result2 = run(`check-visible -i ${INSTANCE_ID} --selector ".nonexistent-xyz"`);
      assert(result2.parsed, "Should return JSON");
      assert(result2.parsed.exists === false, "Nonexistent should not exist");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async "get-frames"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      const result = run(`get-frames -i ${INSTANCE_ID}`);
      assert(result.parsed, "Should return JSON");
      // example.com has no iframes, so empty array is expected
      assert(Array.isArray(result.parsed.frames), "Should return frames array");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  // Regressions: --frame N uses the index space of get-frames, an out-of-range index is rejected,
  // name is cut at 200 chars (--full opts out), and a stale context can be targeted again after a reload.
  async "frame-targeting"() {
    if (platform() !== "win32") {
      console.log("(skipped: --frame is Windows-only) ");
      return;
    }

    // Serve the fixture over HTTP so the src iframes are same-origin.
    const fixturesDir = join(__dirname, "fixtures");
    const server = createServer((req, res) => {
      const file = join(fixturesDir, req.url.split("?")[0].replace(/^\//, "") || "frames.html");
      try {
        res.setHeader("content-type", "text/html; charset=utf-8");
        res.end(readFileSync(file));
      } catch {
        res.statusCode = 404;
        res.end("not found");
      }
    });
    await new Promise((r) => server.listen(0, "127.0.0.1", r));
    const base = `http://127.0.0.1:${server.address().port}`;

    // run() goes through execSync, which waits for the child's stdout pipe to
    // close — this line doubles as the regression test for the daemon
    // inheriting the CLI's stdout handle (start | pipe used to hang forever).
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "${base}/frames.html" --timeout-ms 8000`);
      await sleep(1000);

      // 1. get-frames: 4 frames, each with a frame_id, truncated name marked.
      const frames = run(`get-frames -i ${INSTANCE_ID}`).parsed;
      assert(frames.total_count === 4, `expected 4 iframes, got ${frames.total_count}`);
      for (const f of frames.frames) {
        assert(typeof f.frame_id === "string" && f.frame_id.length > 0,
          `frame ${f.index} should carry a CDP frame_id`);
      }
      const srcB = frames.frames.find((f) => f.id === "srcB");
      assert(srcB.name.length <= 200, `name should be truncated (${srcB.name.length})`);
      assert(srcB.nameTruncated === true, "nameTruncated should be set");
      assert(srcB.nameLen > 200, `nameLen should report the original (${srcB.nameLen})`);

      // --full returns the whole attribute.
      const fullB = run(`get-frames -i ${INSTANCE_ID} --full`).parsed
        .frames.find((f) => f.id === "srcB");
      assert(fullB.name.length === srcB.nameLen, "--full should return the complete name");

      // 2. Distinct markers per index — the ① regression. DOM title is
      // per-frame, and the isolated world shares the DOM, so this proves
      // index i really is get-frames[i] regardless of world bookkeeping.
      for (let i = 0; i < 4; i++) {
        run(`exec-js -i ${INSTANCE_ID} --frame ${i} --world isolated --script "document.title='marker${i}'"`);
      }
      for (let i = 0; i < 4; i++) {
        const r = run(`exec-js -i ${INSTANCE_ID} --frame ${i} --world isolated --script "return document.title"`).parsed;
        assert(r.result === `marker${i}`,
          `frame ${i} read back '${r.result}' — indexes still collide`);
      }

      // frame_id is a stable handle too.
      const viaId = run(`exec-js -i ${INSTANCE_ID} --frame ${frames.frames[0].frame_id} --world isolated --script "return document.title"`).parsed;
      assert(viaId.result === "marker0", "frame_id handle should hit the same frame");

      // 3. Out of range is an ERROR, not a lucky hit.
      const oor = run(`exec-js -i ${INSTANCE_ID} --frame 4 --script "return 1"`, { expectFail: true });
      const oorParsed = tryParse(oor.output);
      assert(oorParsed && oorParsed.error === "FRAME_INDEX_OUT_OF_RANGE",
        `--frame 4 should be rejected, got: ${oor.output.slice(0, 200)}`);
      assert(oorParsed.dom_frame_count === 4, "payload should carry dom_frame_count");

      // 4. main world targeting works, and survives an iframe reload — the ②
      // regression (stale context ids used to surface as HRESULT 0x80070057).
      const main0 = run(`exec-js -i ${INSTANCE_ID} --frame 0 --world main --script "return location.pathname"`).parsed;
      assert(main0.status === "completed", `--frame 0 --world main failed: ${JSON.stringify(main0)}`);
      run(`exec-js -i ${INSTANCE_ID} --script "document.querySelectorAll('iframe')[0].contentWindow.location.reload()"`);
      await sleep(1500);
      const main0again = run(`exec-js -i ${INSTANCE_ID} --frame 0 --world main --script "return location.pathname"`).parsed;
      assert(main0again.status === "completed",
        `--frame 0 --world main after reload failed: ${JSON.stringify(main0again)}`);

      // 5. Miss payloads keep known_frames names under the cap.
      const miss = run(`exec-js -i ${INSTANCE_ID} --frame "no-such-frame-xyz" --script "return 1"`, { expectFail: true });
      const missParsed = tryParse(miss.output);
      assert(missParsed && missParsed.error === "FRAME_NOT_FOUND", "miss should be FRAME_NOT_FOUND");
      for (const kf of missParsed.known_frames || []) {
        assert((kf.name || "").length <= 200, "known_frames names must be truncated");
      }
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
      server.close();
    }
  },

  // 0.17.0: exec-js --file, wait-navigation, usage errors on stdout.
  async "v0.17-features"() {
    if (platform() !== "win32") {
      console.log("(skipped: Windows-only smoke) ");
      return;
    }

    // A usage error must reach STDOUT as JSON — a runner with 2>/dev/null
    // used to see a silent no-op (`wait` without --ms slept 0 s).
    const usage = run(`wait`, { expectFail: true });
    const usageParsed = tryParse(usage.output);
    assert(usageParsed && usageParsed.error === "USAGE",
      `usage error should be JSON on stdout, got: ${usage.output.slice(0, 200)}`);

    // file:// on purpose: an in-process HTTP server cannot serve a navigation
    // while execSync blocks the event loop, so navigation-dependent commands
    // (wait-navigation especially) would deadlock against their own fixture.
    const fixtureUrl = pathToFileURL(join(__dirname, "fixtures", "frames.html")).href;

    const probePath = join(tmpdir(), `tw-probe-${Date.now()}.js`);
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "${fixtureUrl}" --timeout-ms 8000`);
      await sleep(500);

      // exec-js --file: shell-hostile characters must survive verbatim.
      writeFileSync(probePath,
        'const s = "double\\"quote" + \'single\' + `back\\`tick` + "$dollar $(sub)";\nreturn s;',
        "utf-8");
      const fileRun = run(`exec-js -i ${INSTANCE_ID} --file "${probePath}"`).parsed;
      assert(fileRun.status === "completed", `--file run failed: ${JSON.stringify(fileRun)}`);
      assert(fileRun.result === 'double"quote' + "single" + "back`tick" + "$dollar $(sub)",
        `shell-hostile chars mangled: ${fileRun.result}`);

      // wait-navigation: trigger a delayed document replacement, then wait.
      // 1500ms so wait-navigation (next execSync, ~100-300ms spawn overhead)
      // reliably plants its sentinel in the OLD document before the swap.
      run(`exec-js -i ${INSTANCE_ID} --script "setTimeout(() => location.href = 'frames-child.html?nav', 1500); return true"`);
      const nav = run(`wait-navigation -i ${INSTANCE_ID} --to "frames-child" --timeout-ms 15000`, { timeout: 20000 }).parsed;
      assert(nav.status === "completed" && nav.navigated === true,
        `wait-navigation failed: ${JSON.stringify(nav)}`);
      assert(nav.after_url.includes("frames-child"), `unexpected after_url: ${nav.after_url}`);
      assert(nav.before_url.includes("frames.html"), `unexpected before_url: ${nav.before_url}`);

      // 0.17.1 ①: the --to gate must not be a one-shot check. The destination
      // is already loaded here, so a gate that only fires on the poll that
      // witnesses the swap would time out — the reported field failure.
      run(`exec-js -i ${INSTANCE_ID} --script "setTimeout(() => location.href = 'frames.html?back', 1200); return true"`);
      const back = run(`wait-navigation -i ${INSTANCE_ID} --to "frames.html" --timeout-ms 15000`, { timeout: 20000 }).parsed;
      assert(back.status === "completed",
        `--to gate re-check failed: ${JSON.stringify(back)}`);
      assert(back.after_url.includes("frames.html"), `unexpected after_url: ${back.after_url}`);

      // 0.17.2: already landed before the call → answer in the grace window,
      // not at the timeout. (The document is complete and matches --to, and
      // nothing replaces it.)
      const t0 = Date.now();
      const already = run(`wait-navigation -i ${INSTANCE_ID} --to "frames.html" --timeout-ms 20000`, { timeout: 25000 }).parsed;
      assert(already.status === "completed" && already.already_at_target === true,
        `expected already_at_target, got: ${JSON.stringify(already)}`);
      assert(Date.now() - t0 < 6000, `already_at_target took ${Date.now() - t0} ms — timeout was burned`);

      // 0.17.2: a shell-path-converted value is flagged next to the result.
      const conv = run(`wait-navigation -i ${INSTANCE_ID} --to "C:/Program Files/Git/zp/p/" --timeout-ms 1000`, { timeout: 10000, expectFail: true });
      const convParsed = tryParse(conv.output);
      assert(convParsed && typeof convParsed.hint_shell_pathconv === "string" && convParsed.hint_shell_pathconv.includes("MSYS_NO_PATHCONV"),
        `expected hint_shell_pathconv, got: ${conv.output.slice(0, 300)}`);

      // A --to that never matches must still report honestly, echoing `to`.
      const miss = run(`wait-navigation -i ${INSTANCE_ID} --to "never-appears-xyz" --timeout-ms 3000`, { timeout: 12000, expectFail: true });
      const missParsed = tryParse(miss.output);
      assert(missParsed && missParsed.error === "NAVIGATION_TIMEOUT",
        `expected NAVIGATION_TIMEOUT, got: ${miss.output.slice(0, 200)}`);
      assert(missParsed.to === "never-appears-xyz",
        `timeout payload must echo --to, got: ${JSON.stringify(missParsed.to)}`);
      assert(typeof missParsed.polls === "number" && missParsed.polls > 0,
        "timeout payload must report how many times it polled");

      // 0.17.1 ②: the default (untargeted) main-world call reports where it ran.
      const where = run(`exec-js -i ${INSTANCE_ID} --script "return 1"`).parsed;
      assert(where.world === "main", `unexpected world: ${where.world}`);
      assert(typeof where.context_id === "number",
        `main world must report context_id, got: ${JSON.stringify(where.context_id)}`);
      assert(typeof where.frame_origin === "string" && where.frame_origin.length > 0,
        `main world must report frame_origin, got: ${JSON.stringify(where.frame_origin)}`);

      // 0.17.1 ③: console lines say which frame logged them.
      run(`exec-js -i ${INSTANCE_ID} --script "console.log('tw-top-marker'); return true"`);
      run(`exec-js -i ${INSTANCE_ID} --frame 0 --world main --script "console.log('tw-frame0-marker'); return true"`);
      await sleep(500);
      const logs = run(`console-logs -i ${INSTANCE_ID} --max 200`).parsed;
      const top = logs.logs.find((l) => l.message.includes("tw-top-marker"));
      const inner = logs.logs.find((l) => l.message.includes("tw-frame0-marker"));
      assert(top && inner, "both console markers should be captured");
      assert(typeof top.frame_id === "string" && typeof inner.frame_id === "string",
        `console entries must carry frame_id: ${JSON.stringify([top, inner])}`);
      assert(top.frame_id !== inner.frame_id,
        "the top frame and iframe[0] must report DIFFERENT frame_ids");
      const frames = run(`get-frames -i ${INSTANCE_ID}`).parsed;
      assert(frames.frames[0].frame_id === inner.frame_id,
        "console frame_id must match the get-frames handle for the same frame");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
      cleanup([probePath]);
    }
  },

  // 0.18.0: navigation timeline, failed-requests, cookies, record on/off, list
  // state, --field, exec-js --target sw, reset, the shared-profile guard.
  async "v0.18-features"() {
    if (platform() !== "win32") {
      console.log("(skipped: Windows-only smoke) ");
      return;
    }
    const { base, stop: stopFixture } = await startHttpFixture();
    const B = `${INSTANCE_ID}-b`;
    // A private profile: this test clears cookies and resets, and must never
    // touch the WebView2 profile other daemons on the machine share.
    run(`start --id ${INSTANCE_ID} --private-profile`);
    try {
      // navigate reports the main document and every hop it went through.
      const nav = run(`navigate -i ${INSTANCE_ID} --url "${base}/" --timeout-ms 2500`).parsed;
      assert(nav.document && nav.document.redirects === 2, `expected 2 redirects: ${JSON.stringify(nav.document)}`);
      assert(JSON.stringify(nav.document.hops.map((h) => h.status)) === "[302,301,200]",
        `unexpected hop statuses: ${JSON.stringify(nav.document.hops)}`);
      assert(nav.title === "Fixture Page", `navigate should report the title, got ${nav.title}`);

      // response-headers keeps the redirect hops it used to drop.
      const rh = run(`response-headers -i ${INSTANCE_ID} --resource-type document --url-pattern "127.0.0.1"`).parsed;
      assert(rh.responses.some((r) => r.redirect === true && r.status === 302),
        "response-headers must include the 302 hop");

      // failed-requests: a URL for every failure, of any resource type.
      const fr = run(`failed-requests -i ${INSTANCE_ID}`).parsed;
      const kinds = fr.failures.map((f) => `${f.kind} ${f.resource_type} ${f.url}`);
      assert(kinds.some((k) => k.startsWith("http_error Image") && k.endsWith("/missing.png")), `no 404 image: ${kinds}`);
      assert(kinds.some((k) => k.startsWith("network_error Script") && k.includes(":1/nope.js")), `no network error: ${kinds}`);
      assert(kinds.some((k) => k.startsWith("http_error Fetch") && k.endsWith("/api/fail")), `no 500 fetch: ${kinds}`);

      // A challenge that solves itself is two navigations, not one success.
      const ch = run(`navigate -i ${INSTANCE_ID} --url "${base}/challenge" --timeout-ms 3000`).parsed;
      assert(ch.challenge === "cloudflare:challenge", `challenge not detected: ${JSON.stringify(ch)}`);
      const log = run(`nav-log -i ${INSTANCE_ID} --max 2`).parsed;
      assert(log.navigations.length === 2 && log.navigations[0].status === 403 && log.navigations[1].status === 200,
        `nav-log should show 403 then 200: ${JSON.stringify(log.navigations.map((n) => n.status))}`);

      // cookies: HttpOnly is visible, values masked; clear really clears.
      run(`navigate -i ${INSTANCE_ID} --url "${base}/page" --timeout-ms 1500`);
      const ck = run(`cookies -i ${INSTANCE_ID} --url "${base}/page"`).parsed;
      assert(ck.cookies.some((c) => c.name === "sid" && c.http_only === true && c.value.includes("REDACTED")),
        `HttpOnly sid missing or unmasked: ${JSON.stringify(ck.cookies)}`);
      const cl = run(`cookies clear -i ${INSTANCE_ID} --domain 127.0.0.1`).parsed;
      assert(cl.status === "completed" && cl.remaining.length === 0, `cookies clear left: ${JSON.stringify(cl)}`);

      // --field prints one value; a failure is printed whole with exit 1.
      const ids = run(`list --field "instances[].id" --raw`).output.split(/\r?\n/);
      assert(ids.includes(INSTANCE_ID), `list --field should print ids, got ${ids}`);
      const thrown = run(`exec-js -i ${INSTANCE_ID} --script "throw new Error('boom')" --field result`, { expectFail: true });
      const thrownParsed = tryParse(thrown.output);
      assert(!thrown.ok && thrownParsed && thrownParsed.status === "failed",
        `a failed result must not be filtered into null: ${thrown.output.slice(0, 200)}`);

      // record on/off on a live daemon; the stopped tape stays readable.
      run(`record on -i ${INSTANCE_ID}`);
      run(`navigate -i ${INSTANCE_ID} --url "${base}/page" --timeout-ms 1500`);
      run(`record off -i ${INSTANCE_ID}`);
      const tape = run(`dump-recording -i ${INSTANCE_ID} --filter network`).parsed;
      assert(tape.status === "completed" && tape.counts.network > 0 && /OFF/.test(tape.note || ""),
        `stopped tape should be readable: ${JSON.stringify({ status: tape.status, counts: tape.counts, note: tape.note })}`);

      // list: where the instance is and who touched it last.
      const me = run(`list`).parsed.instances.find((i) => i.id === INSTANCE_ID);
      assert(me && me.current_url && me.current_url.startsWith(base), `list current_url: ${JSON.stringify(me)}`);
      assert(me.last_tool && me.last_used, "list should report last_tool/last_used");
      assert(me.profile && me.profile.kind === "private", `profile kind: ${JSON.stringify(me.profile)}`);

      // exec-js --target sw reaches the service worker's own globals.
      run(`navigate -i ${INSTANCE_ID} --url "${base}/sw.html" --timeout-ms 2500`);
      // No fixed sleep: right after registration is exactly when a held
      // worker used to be evaluated before its own script had run.
      run(`wait-for-condition -i ${INSTANCE_ID} --script "return document.title === 'SW registered'" --timeout-ms 8000`);
      const sw = run(`exec-js -i ${INSTANCE_ID} --target sw --script "return self.swMarker"`).parsed;
      assert(sw.status === "completed" && sw.result === "hello-from-sw", `sw eval: ${JSON.stringify(sw)}`);

      // reset keeps the id; the profile and the timeline are empty again.
      const rs = run(`reset -i ${INSTANCE_ID}`, { timeout: 60000 }).parsed;
      assert(rs.status === "completed", `reset: ${JSON.stringify(rs)}`);
      assert(run(`cookies -i ${INSTANCE_ID} --url "${base}/page"`).parsed.count === 0, "cookies survived reset");
      assert(run(`nav-log -i ${INSTANCE_ID}`).parsed.returned <= 1, "nav-log should restart after reset");

      // Two PLAIN daemons share a profile, so a clear on one must refuse. The
      // selection names nothing real, so even a broken guard deletes nothing.
      run(`start --id ${B}`);
      const other = `${INSTANCE_ID}-c`;
      run(`start --id ${other}`);
      try {
        const refuse = run(`cookies clear -i ${B} --domain tw-smoke.invalid --name __tw_smoke_none__`, { expectFail: true });
        const refuseParsed = tryParse(refuse.output);
        assert(refuseParsed && refuseParsed.error === "SHARED_PROFILE" && refuseParsed.shared_with.includes(other),
          `expected SHARED_PROFILE: ${refuse.output.slice(0, 300)}`);
      } finally {
        run(`stop --id ${other}`, { expectFail: true });
      }
    } finally {
      run(`stop --id ${B}`, { expectFail: true });
      run(`stop --id ${INSTANCE_ID}`, { expectFail: true });
      stopFixture();
    }
  },

  // 0.19.0: field reports from using 0.18 — cross-site frames on
  // WebView2, the console ring, the shared-profile guard, the tape's state,
  // a lifetime bound.
  async "v0.19-features"() {
    if (platform() !== "win32") {
      console.log("(skipped: Windows-only smoke) ");
      return;
    }
    const { base, stop: stopFixture } = await startHttpFixture();
    const xbase = base.replace("127.0.0.1", "localhost");
    const B = `${INSTANCE_ID}-b`;
    const T = `${INSTANCE_ID}-t`;
    run(`start --id ${INSTANCE_ID} --private-profile --console-buffer 2000`);
    try {
      // A cross-site child floods the console: every line arrives, tagged.
      run(`navigate -i ${INSTANCE_ID} --url "${xbase}/xlog" --timeout-ms 3000`);
      await sleep(2500);
      const all = run(`console-logs -i ${INSTANCE_ID} --max 2000`).parsed;
      assert(all.buffer_size === 2000 && all.dropped === 0, `buffer: ${JSON.stringify({ b: all.buffer_size, d: all.dropped })}`);
      const child = run(`console-logs -i ${INSTANCE_ID} --frame 0 --max 2000`).parsed;
      assert(child.matched === 722, `the cross-site child logged 722 lines, got ${child.matched}`);
      const errs = run(`console-logs -i ${INSTANCE_ID} --text child-error`).parsed;
      assert(errs.matched === 2 && errs.logs.every((l) => l.frame_id), `errors: ${JSON.stringify(errs.logs)}`);
      const top = run(`console-logs -i ${INSTANCE_ID} --frame top --text parent-`).parsed;
      assert(top.matched === 5, `parent lines: ${top.matched}`);

      // exec-js reaches the cross-site frame by name, and says where it ran.
      const inside = run(`exec-js -i ${INSTANCE_ID} --frame flood --script "return location.host"`).parsed;
      assert(inside.status === "completed" && /^127\.0\.0\.1:/.test(inside.result) && inside.out_of_process === true,
        `cross-site frame eval: ${JSON.stringify(inside)}`);
      const frames = run(`get-frames -i ${INSTANCE_ID}`).parsed;
      assert(frames.frames[0].out_of_process === true && /xlog-child/.test(frames.frames[0].url || ""),
        `get-frames should mark and locate the cross-site frame: ${JSON.stringify(frames.frames[0])}`);

      // A busy frame is not a missing frame.
      run(`navigate -i ${INSTANCE_ID} --url "${base}/busy" --timeout-ms 3000`);
      await sleep(1800);
      const busy = tryParse(run(`exec-js -i ${INSTANCE_ID} --frame spin --timeout-ms 2000 --script "return 1"`, { expectFail: true }).output);
      assert(busy && busy.error === "RENDERER_BUSY" && busy.frame_id, `busy frame: ${JSON.stringify(busy)}`);
      const missing = tryParse(run(`exec-js -i ${INSTANCE_ID} --frame nosuch --script "return 1"`, { expectFail: true }).output);
      assert(missing && missing.error === "FRAME_NOT_FOUND", `missing frame: ${JSON.stringify(missing)}`);

      // `record off`: the transport stays armed, the tape says `off`.
      run(`record on -i ${INSTANCE_ID}`);
      run(`record off -i ${INSTANCE_ID}`);
      const rec = run(`record status -i ${INSTANCE_ID}`).parsed.recording;
      assert(rec.recording === "off", `record state: ${JSON.stringify(rec)}`);

      // The shared profile is refused even with no other daemon of ours up:
      // it outlives this one. The selection names nothing real, so even a
      // broken guard deletes nothing.
      run(`start --id ${B}`);
      const refuse = tryParse(run(`cookies clear -i ${B} --domain tw-smoke.invalid --name __tw_smoke_none__`, { expectFail: true }).output);
      assert(refuse && refuse.error === "SHARED_PROFILE", `expected SHARED_PROFILE: ${JSON.stringify(refuse)}`);
      const dry = run(`cookies clear -i ${B} --domain tw-smoke.invalid --dry-run`).parsed;
      assert(dry.dry_run === true && dry.would_be_refused === true && dry.would_delete === 0, `dry run: ${JSON.stringify(dry)}`);
      const rdry = run(`reset -i ${B} --dry-run`).parsed;
      assert(rdry.dry_run === true && rdry.would_be_refused === true, `reset dry run: ${JSON.stringify(rdry)}`);

      // --ttl: the daemon stops itself, private folder and all.
      const t = run(`start --id ${T} --private-profile --ttl 12s`).parsed;
      assert(t.ttl_s === 12 && t.expires_at_ms > Date.now(), `ttl start: ${JSON.stringify(t)}`);
      const tdir = t.profile && t.profile.path;
      let gone = false;
      for (let i = 0; i < 40 && !gone; i++) {
        await sleep(500);
        gone = !run(`list`).parsed.instances.some((x) => x.id === T);
      }
      assert(gone, "the --ttl daemon should have stopped itself");
      await sleep(1500);
      assert(!tdir || !existsSync(tdir), `ttl daemon left its profile: ${tdir}`);
    } finally {
      run(`stop --id ${T}`, { expectFail: true });
      run(`stop --id ${B}`, { expectFail: true });
      run(`stop --id ${INSTANCE_ID}`, { expectFail: true });
      stopFixture();
    }
  },

  // 0.18.0: a real Chrome, launched without automation flags. Runs on macOS
  // too — the engine is CDP on both, and the macOS launch/kill paths are
  // exactly what docs/macos-verification.md asks to have checked.
  async "chrome-engine"() {
    if (platform() !== "win32" && platform() !== "darwin") {
      console.log("(skipped: needs Windows or macOS) ");
      return;
    }
    const started = run(`start --id ${INSTANCE_ID} --engine chrome`, { expectFail: true, timeout: 45000 });
    const s = tryParse(started.output);
    if (!started.ok && /not found/i.test(started.output)) {
      console.log("(skipped: Google Chrome not installed) ");
      return;
    }
    assert(s && s.status === "started" && s.engine === "chrome", `chrome start: ${started.output.slice(0, 300)}`);
    assert(s.navigator_webdriver === false, `Chrome must not announce automation: ${JSON.stringify(s)}`);
    const profileDir = s.profile && s.profile.path;
    const { base, stop: stopFixture } = await startHttpFixture();
    try {
      const nav = run(`navigate -i ${INSTANCE_ID} --url "${base}/" --timeout-ms 5000`).parsed;
      assert(nav.status === "completed" && nav.document.redirects === 2, `chrome navigate: ${JSON.stringify(nav)}`);
      run(`navigate -i ${INSTANCE_ID} --url "${base}/form" --timeout-ms 5000`);
      const click = run(`click -i ${INSTANCE_ID} --selector "#b"`).parsed;
      assert(click.status === "completed" && click.trusted === true, `trusted click: ${JSON.stringify(click)}`);
      const clicks = run(`exec-js -i ${INSTANCE_ID} --script "return window.__clicks"`).parsed;
      assert(JSON.stringify(clicks.result) === "[true]", `isTrusted should be true: ${JSON.stringify(clicks.result)}`);
      run(`navigate -i ${INSTANCE_ID} --url "${base}/frames" --timeout-ms 5000`);
      await sleep(500);
      const xsite = run(`exec-js -i ${INSTANCE_ID} --frame xsite --script "return window.innerMarker"`).parsed;
      assert(xsite.status === "completed" && /^localhost:/.test(xsite.result),
        `cross-site iframe eval: ${JSON.stringify(xsite)}`);

      // intercept is enforced by Chrome: an out-of-process frame's very first
      // fetch is mocked, and a status with no standard reason phrase is not
      // refused (a refused answer used to leave the request paused forever).
      const rule = run(`intercept -i ${INSTANCE_ID} --pattern "*/api/first*" --action mock --status 299 --body "{}"`).parsed;
      assert(rule.status === "completed" && rule.sync && rule.sync.applied_to_current_page === true,
        `chrome intercept must apply: ${JSON.stringify(rule)}`);
      run(`navigate -i ${INSTANCE_ID} --url "${base}/xframe" --timeout-ms 5000`);
      run(`wait-for-condition -i ${INSTANCE_ID} --script "return document.querySelector('iframe') != null" --timeout-ms 5000`);
      let first = null;
      for (let i = 0; i < 20 && first == null; i++) {
        await sleep(250);
        first = run(`exec-js -i ${INSTANCE_ID} --frame xf --script "return window.firstStatus"`, { expectFail: true }).parsed?.result ?? null;
      }
      assert(first === 299, `OOPIF first fetch should be mocked with 299, got ${first}`);
      // A credentialed cross-origin mock: its CORS preflight is answered too.
      run(`intercept -i ${INSTANCE_ID} --pattern "*/api/xo*" --action mock --status 201 --body "{}"`);
      run(`navigate -i ${INSTANCE_ID} --url "${base}/cors" --timeout-ms 5000`);
      const cors = run(`exec-js -i ${INSTANCE_ID} --await --timeout-ms 8000 --script "return await window.go()"`).parsed;
      assert(cors.result === 201, `credentialed cross-origin mock: ${JSON.stringify(cors)}`);
      run(`intercept-clear -i ${INSTANCE_ID}`);

      // The page's service worker, evaluated as soon as it registered — not
      // one of Chrome's own extension workers.
      run(`navigate -i ${INSTANCE_ID} --url "${base}/sw.html" --timeout-ms 5000`);
      run(`wait-for-condition -i ${INSTANCE_ID} --script "return document.title === 'SW registered'" --timeout-ms 8000`);
      const csw = run(`exec-js -i ${INSTANCE_ID} --target sw --script "return self.swMarker"`).parsed;
      assert(csw.result === "hello-from-sw" && /^http/.test(csw.script_url || ""),
        `chrome sw eval: ${JSON.stringify(csw)}`);

      // A server that never answers must not hold navigate past its budget.
      const t0 = Date.now();
      const stalled = run(`navigate -i ${INSTANCE_ID} --url "${base}/hang" --timeout-ms 3000`, { timeout: 20000 }).parsed;
      assert(Date.now() - t0 < 10000, `navigate to a stalled origin took ${Date.now() - t0} ms`);
      assert(stalled.status === "completed" && /not committed/.test(stalled.load_note || ""),
        `stalled navigate should say it has not committed: ${JSON.stringify(stalled)}`);
    } finally {
      run(`stop --id ${INSTANCE_ID}`, { expectFail: true });
      stopFixture();
    }
    // The throw-away profile goes with the daemon.
    let gone = false;
    for (let i = 0; i < 20 && !gone; i++) {
      await sleep(500);
      gone = !profileDir || !existsSync(profileDir);
    }
    assert(gone, `chrome profile not removed: ${profileDir}`);
  },

  async "dismiss-modals"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      const result = run(`dismiss-modals -i ${INSTANCE_ID}`);
      assert(result.parsed, "Should return JSON");
      assert(typeof result.parsed.dismissed_count === "number", "Should have dismissed_count");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  async "get-requests"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);

    try {
      run(`navigate -i ${INSTANCE_ID} --url "https://example.com" --timeout-ms 8000`);
      const result = run(`get-requests -i ${INSTANCE_ID}`);
      assert(result.parsed, "Should return JSON");
    } finally {
      run(`stop --id ${INSTANCE_ID}`);
    }
  },

  // Every path is redirected, also the temp folder: uninstall stops every
  // daemon it can find, and the port files are there.
  async uninstall() {
    const root = mkdtempSync(join(tmpdir(), "dalzzack-uninstall-"));
    const ext = platform() === "win32" ? ".exe" : "";
    const bin = join(root, "bin");
    const home = join(root, "home");
    const temp = join(root, "temp");
    const exe = join(bin, "dalzzack" + ext);
    const claude = join(home, ".claude", "CLAUDE.md");
    const codex = join(home, ".codex");
    const zshenv = join(home, ".zshenv");
    const export_line = `export PATH="${bin}:$PATH"`;
    for (const dir of [bin, temp, dirname(claude)]) mkdirSync(dir, { recursive: true });
    copyFileSync(binaryPath, exe);
    writeFileSync(claude, "# Mine\n");
    writeFileSync(zshenv, `alias a=b\n\n# Added by the dalzzack installer\n${export_line}\n`);
    const env = {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      CODEX_HOME: codex,
      TMP: temp,
      TEMP: temp,
      TMPDIR: temp,
      DALZZACK_OUTPUT: "full",
    };
    const project = join(root, "project");
    mkdirSync(project);
    const dalzzack = (...args) =>
      JSON.parse(
        execFileSync(exe, args, {
          cwd: project,
          env,
          encoding: "utf-8",
          stdio: ["pipe", "pipe", "pipe"],
        }),
      );

    try {
      dalzzack("install", "all");
      const installed = readFileSync(claude, "utf8");
      assert(installed.includes("dalzzack-begin"), "install should add the guide");

      const plan = dalzzack("uninstall");
      assert(plan.status === "planned", `Expected planned, got ${plan.status}`);
      assert(plan.guides.length === 2, "The plan should name both guides");
      assert(existsSync(exe), "The plan should keep the binary");
      assert(readFileSync(claude, "utf8") === installed, "The plan should change no file");

      dalzzack("install", "codex-project");
      const left = dalzzack("uninstall").left.map((entry) => entry.item).join(" ");
      assert(left.includes("AGENTS.md"), "The plan should name the project guide");
      const cleaned = dalzzack("uninstall", "--project", "--yes");
      assert(cleaned.status === "completed", `Expected completed, got ${cleaned.status}`);
      assert(!existsSync(join(project, "AGENTS.md")), "The project guide should go");
      assert(existsSync(exe), "--project should keep the binary");
      assert(readFileSync(claude, "utf8") === installed, "--project should keep the global guide");

      // A pid file left by a daemon that died: uninstall must not kill this pid.
      const other = spawn(process.execPath, ["-e", "setTimeout(() => {}, 60000)"], { stdio: "ignore" });
      const isAlive = () => {
        try {
          process.kill(other.pid, 0);
          return true;
        } catch {
          return false;
        }
      };
      await sleep(300);
      writeFileSync(join(temp, "dalzzack-stale.port"), "1");
      writeFileSync(join(temp, "dalzzack-stale.pid"), `${other.pid}\nanother process\n`);
      writeFileSync(join(temp, "dalzzack-old.port"), "1");
      writeFileSync(join(temp, "dalzzack-old.pid"), `${other.pid}`);

      const done = dalzzack("uninstall", "--yes");
      await sleep(500);
      const survived = isAlive();
      other.kill();
      assert(survived, "uninstall ended another process");
      assert(
        done.left.some((entry) => entry.item === "daemon old"),
        "The report should name the daemon with no identity",
      );
      assert(done.status === "completed", `Expected completed, got ${done.status}`);
      assert(readFileSync(claude, "utf8") === "# Mine\n", "The user's text should stay");
      assert(!existsSync(codex), "The guide that only install made should go");
      if (platform() !== "win32") {
        assert(readFileSync(zshenv, "utf8") === "alias a=b\n", "The PATH lines should go");
      }
      // Windows deletes the running binary after the process ends.
      for (let i = 0; i < 40 && (existsSync(exe) || existsSync(exe + ".old")); i++) await sleep(250);
      assert(!existsSync(exe), "The binary should go");
      assert(!existsSync(exe + ".old"), "The renamed binary should go");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  },

  async "kill-real-daemon"() {
    run(`start --id ${INSTANCE_ID}`);
    await sleep(2000);
    const result = run(`kill --id ${INSTANCE_ID}`);
    assert(result.parsed?.status === "killed", `Expected killed, got: ${result.output}`);
    assert(!run("list").output.includes(INSTANCE_ID), "The daemon should be gone");
  },

  // A pid file can outlive its daemon. The pid then belongs to another program,
  // and only the process that wrote the file may be killed.
  async "kill-spares-another-process"() {
    const other = spawn(process.execPath, ["-e", "setTimeout(() => {}, 60000)"], { stdio: "ignore" });
    const id = `stale-${Date.now()}`;
    const portFile = join(tmpdir(), `dalzzack-${id}.port`);
    const pidFile = join(tmpdir(), `dalzzack-${id}.pid`);
    const isAlive = () => {
      try {
        process.kill(other.pid, 0);
        return true;
      } catch {
        return false;
      }
    };
    try {
      await sleep(300);
      for (const record of [`${other.pid}\nanother process\n`, `${other.pid}`]) {
        writeFileSync(portFile, "1");
        writeFileSync(pidFile, record);
        run(`kill --id ${id}`, { expectFail: true });
        await sleep(500);
        assert(isAlive(), `kill ended another process, with the pid file ${JSON.stringify(record)}`);
      }
    } finally {
      other.kill();
      cleanup([portFile, pidFile]);
    }
  },

  async "error-no-instance"() {
    const result = run(`navigate -i nonexistent-instance --url "https://example.com"`, {
      expectFail: true,
    });
    assert(!result.ok, "Should fail when instance doesn't exist");
  },
};

// --- Runner ---

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

/// Start scripts/fixtures/http-fixture.mjs in its own process on a free-ish
/// port and wait until it answers.
async function startHttpFixture() {
  const port = 18900 + Math.floor(Math.random() * 90);
  const child = spawn(process.execPath, [join(__dirname, "fixtures", "http-fixture.mjs"), String(port)], {
    stdio: "ignore",
  });
  const base = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 50; i++) {
    try {
      const r = await fetch(`${base}/page`);
      if (r.ok) break;
    } catch {}
    await sleep(100);
  }
  return { base, stop: () => child.kill() };
}

async function main() {
  console.log("=".repeat(60));
  console.log("dalzzack CLI Integration Tests");
  console.log(`Binary: ${binaryPath}`);
  console.log("=".repeat(60));
  console.log();

  const testNames = filterTest
    ? Object.keys(tests).filter((n) => n.includes(filterTest))
    : Object.keys(tests);

  if (testNames.length === 0) {
    console.error(`No tests matching: ${filterTest}`);
    console.log("Available:", Object.keys(tests).join(", "));
    process.exit(1);
  }

  let passed = 0;
  let failed = 0;
  const failures = [];

  for (const name of testNames) {
    process.stdout.write(`  ${name} ... `);
    try {
      await tests[name]();
      console.log("PASS");
      passed++;
    } catch (e) {
      console.log(`FAIL: ${e.message}`);
      failed++;
      failures.push({ name, error: e.message });
    }

    // Ensure cleanup between tests
    try { run(`stop --id ${INSTANCE_ID}`, { expectFail: true }); } catch {}
    await sleep(300);
  }

  console.log();
  console.log("=".repeat(60));
  console.log(`Results: ${passed} passed, ${failed} failed, ${testNames.length} total`);

  if (failures.length > 0) {
    console.log("\nFailures:");
    for (const f of failures) {
      console.log(`  ${f.name}: ${f.error}`);
    }
  }

  console.log("=".repeat(60));
  process.exit(failed > 0 ? 1 : 0);
}

main();
