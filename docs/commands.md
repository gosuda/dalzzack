# Command reference

Every command and flag, with platform notes. `dalzzack <command> --help` prints the same flags, and `dalzzack tools` prints them as JSON schemas. For agent-oriented usage rules, see [AGENTS.md](../AGENTS.md).

All commands except `start`, `stop`, `list`, `run`, `guide`, `install`, `tools`, `uninstall`, and `upgrade` require `--id <ID>` (`-i <ID>`) to target a browser instance.

## Instance management

| Command | Description |
|---------|-------------|
| `start [--id <ID>] [--width PX] [--height PX]` | Spawn a browser daemon. Default 1200×800. If an `--id` already has an unresponsive daemon, `start` kills it and respawns. A daemon from before 1.1 has no process identity, so `start` leaves that one running. |
| `start … --user-agent <UA>` | Override the User-Agent. No private user data folder needed, so cookies/logins stay shared (unlike `--browser-arg "--user-agent=…"`). **The engine's `Sec-CH-UA` headers cannot be overridden**, so a UA whose major disagrees with the engine is a *stronger* bot signal than the default. Windows already sends a consistent Edge UA, so dalzzack leaves it alone unless you pass this. |
| `start … --raw-user-agent` | Keep the engine's raw UA. macOS only in practice: its default ends at `(KHTML, like Gecko)` with no browser product token, and dalzzack completes it with the installed Safari's version by default. |
| `start … --no-console-capture` | Collect no console output at all. Rarely needed, since capture is already invisible to the page, but it is the escape hatch. |
| `start … --enforce-csp` | Keep the page's Content Security Policy enforced. dalzzack disables it by default (`Page.setBypassCSP`) so cross-origin iframes can be inspected. That also means nothing is ever blocked and `console-logs --source csp` stays empty regardless of the policy. Costs cross-origin iframe access. `list` reports `csp_bypassed`. |
| `start … --private-profile` | Give this instance its own browser profile (cookies, storage, cache): empty at start, deleted by `stop`. Plain instances share one WebView2 profile, so `cookies clear` / `reset` on them refuse with `SHARED_PROFILE` rather than wipe another session's logins. |
| `start … --engine chrome [--chrome-path PATH]` | Drive a real Google Chrome on a throw-away profile instead of WebView2. See [Chrome engine](#chrome-engine). |
| `start … --ttl 10m` | Stop by itself after `90s` / `10m` / `2h` / `1h30m` (bare = seconds), through the same path as `stop`, so Chrome and a private-profile folder go with it. For short-lived comparison instances. |
| `start … --console-buffer N` | Console ring size (default 500, max 100000). |
| `resize --id <ID> --width PX --height PX` | Change window size of a running daemon |
| `stop --id <ID>` | Graceful stop (sends `__shutdown` over TCP). |
| `kill --id <ID>` | Force-kill by PID, the escape hatch when `stop` times out on a wedged daemon. It kills only the process that wrote the pid file: the pid file also holds that process's identity, so a pid that now belongs to another program is left alone. A pid file from before 1.1 has no identity, so `kill` refuses it with `unverified_pid`. On a Chrome-engine id it also ends Chrome and deletes its profile. |
| `list` | List running daemons (pings each, cleans up stale port/pid files). A daemon that hangs shows `status: unresponsive`. Reports `renderer_ok`, `console_transport`, `user_agent` / `user_agent_source`, `record` / `recording` (`on` \| `off` \| `never`), `version`, and, from daemon memory (so a wedged renderer still answers), `current_url`, `last_used` / `last_tool`, `engine`, `browser_version`, `navigator_webdriver`, `profile`, `ttl_s` / `expires_at_ms`. |
| `reset [--keep-browsing-data] [--keep-rules] [--force-shared] [--dry-run] [--url URL]` | First-visit state without stopping (id, port, window and start flags kept): the profile's cookies/cache/storage/service workers, intercept rules, injected scripts, dalzzack's buffers; then about:blank. `reset` leaves the page BEFORE clearing, so unload handlers cannot write back into the emptied profile. On `--engine chrome`, a new Chrome on a new profile. Refuses on the shared profile even with no other daemon running (it outlives this one) unless `--force-shared`; `--dry-run` shows what it would clear. |
| `record on\|off\|status [--clear]` | Switch the `--record` tape on a live daemon. `off` keeps the tape readable. The state is `recording` (`on`/`off`/`never`); `armed` only says the transport is up and stays true after `off`. |
| `tools [NAME] [--names]` | Print the JSON schema of one tool, or only the tool names. With neither, print every schema (64 KB), for MCP / LLM discovery. |
| `guide [TOPIC] [--all]` | Print one part of the usage guide, the list of topics, or all of it. |
| `install claude-code\|codex\|codex-project\|all` | Add the short guide to an agent's instruction file. |
| `install --refresh` | Rewrite the guide only in the global files that already have it. It creates no file. |
| `upgrade [--check] [--force] [--tag TAG] [--keep-daemons]` | Check the latest release and replace this binary if it is older. `update` is the same command. It reads the tag from the `releases/latest` redirect, downloads the archive, checks its SHA-256 hash and the new binary's version, stops running daemons, replaces the binary, and refreshes the installed guides. A failed download changes nothing. `--check` prints the versions, `--force` reinstalls the same version, and `--tag` installs one tag. |
| `uninstall [--yes] [--purge] [--project]` | Print what would change, and change nothing. `--yes` applies it: stops daemons, removes the guide from the global files, removes the `PATH` entry, and deletes this binary last. `--purge` also deletes the data folders, browser logins, and dalzzack's temp files. `--project` removes the guide from `./AGENTS.md` and changes nothing else, so run it in each project first. `left` lists what stays and why, and `other_copies` lists other binaries on `PATH`. After a failed step the binary stays, so the command can run again. |
| `upgrade --path DIR \| --git URL [--rev REV] [--no-locked]` | Build from source with `cargo install --force`. Stops running daemons first. Needs cargo. If this binary is not the one cargo installs (a release binary, say), the result carries a `note` that names both copies. |

## Navigation & page state

| Command | Description |
|---------|-------------|
| `navigate --url <URL> [--timeout-ms 5000]` | Navigate to URL. Returns the `title` and the main document's outcome: `document` with its status and every redirect hop, a detected `challenge` (e.g. `cloudflare:challenge`), and `later_navigations`. A challenge that solves itself is not reported as a plain success |
| `back` / `forward` | Walk browser history (`history.back/forward()`). Polls for up to 3 s; returns `before_url`, `after_url`, `elapsed_ms`, and a `changed` flag |
| `reload [--hard]` | Reload the current page. `--hard` bypasses the HTTP cache, the Ctrl+Shift+R equivalent (Windows: CDP `Page.reload {ignoreCache:true}`; macOS: `-[WKWebView reloadFromOrigin]`) |
| `clear-site-data [--origin URL] [--types LIST]` | Wipe an origin's storage out-of-band: service worker registrations, CacheStorage, localStorage, IndexedDB, cookies. Bypasses the page's JS world, so it still works when a proxy/membrane or a policy hides `navigator.serviceWorker`. `--origin` defaults to the current page. Types: `appcache,cache_storage,cookies,file_systems,indexeddb,local_storage,service_workers,shader_cache,websql`, or `all`. **macOS clears by HOST, not origin**: `WKWebsiteDataRecord` doesn't distinguish scheme or port, and the response says so (`scope: "host"`) |
| `get-url` | Current URL + title |
| `get-text [--selector <CSS>] [--max-length N]` | The visible text of the page or one element, with empty space removed. This is the cheapest way to read a page. |
| `get-html [--selector <CSS>] [--no-strip-noise] [--max-length N]` | Page or element HTML. By default it removes script/style/noscript/svg/comments and `style`, `data-v-*`, and `data-reactid` attributes. `--no-strip-noise` keeps them. |
| `screenshot [--output <PATH>] [--selector <CSS>]` | PNG capture, full viewport or cropped to element (scrolled into view first if it is not fully visible) |

## Interaction

| Command | Description |
|---------|-------------|
| `click --x X --y Y` / `--selector CSS` / `--text "..."` | Click by coordinates, selector, or visible text |
| `hover --selector CSS` / `--x X --y Y` | Move the cursor over an element. On macOS uses a native CGEvent so CSS `:hover` actually fires (dropdown menus, tooltips). Windows/Linux dispatch JS events only and set `native_hover: false` |
| `fill-form --selector CSS --value TEXT` | Fill single field (SPA-safe: uses native input value setter) |
| `fill-form --fields '<JSON>'` | Multiple fields: `'[{"selector":"#a","value":"1"}]'` |
| `keyboard --text "hello" [--paste]` | Type character-by-character, or paste at once |
| `keyboard --key Enter [--modifiers ctrl,shift]` | Press special keys |
| `scroll --to top\|bottom [--selector CSS]` | Absolute scroll (page or SPA container) |
| `scroll [--direction down] [--amount 500]` | Relative scroll |
| `upload-file --selector "input[type=file]" --file PATH` | Upload file (max 50 MB) |
| `save-image --selector CSS --output PATH [--index -1]` | Save DOM `<img>` to disk |

## Inspection & waiting

| Command | Description |
|---------|-------------|
| `describe [--output PATH]` | Annotated screenshot with numbered interactive elements + JSON index |
| `check-visible --selector CSS` | Instant visibility check |
| `element-at --x X --y Y` | Find nearest interactive element at coordinates |
| `wait-for --selector CSS [--state present\|added\|removed] [--text "..."] [--timeout-ms 10000]` | Event-driven wait |
| `wait --ms MS` | Sleep |
| `nav-log [--frames] [--headers] [--max 20]` | Navigation timeline: every document request chain, each redirect hop with its status, `location`, `cf-mitigated`, set-cookie names and whether it committed. `--headers` adds the request headers as sent on the wire (cookies masked, `sec-fetch-*`). Always on, no `--record`. |
| `failed-requests [--resource-type T] [--no-http-errors] [--include-canceled]` | Every failed request of any resource type, with its URL: `blocked` / `cors` / `network_error` / `http_error` / `canceled`. Always on. |
| `cookies [list\|clear] [--url U \| --domain D \| --all] [--name N]` | The cookie jar via CDP, HttpOnly and parent-domain cookies included. `clear` re-reads the jar and reports what `remaining`. |

### `describe` output

```json
{
  "status": "completed",
  "file": "/abs/path/to/describe.png",
  "title": "Page Title",
  "url": "https://example.com/...",
  "elementCount": 30,
  "elements": [
    { "id": 0, "tag": "button", "text": "Dismiss", "x": 724, "y": 30 },
    { "id": 5, "tag": "textarea", "text": "", "x": 560, "y": 680 },
    { "id": 23, "tag": "button", "text": "Run", "x": 829, "y": 726 }
  ],
  "formValues": { "textarea": "current prompt text" }
}
```

## Advanced

| Command | Description |
|---------|-------------|
| `exec-js --script <CODE> [--timeout-ms 30000] [--await]` | Execute JS, return JSON. Use `return` to get a value. Results are hard-capped at 5 MB. `--await` resolves a promise result instead of failing on it, so the script can use top-level `await` (Windows: CDP `Runtime.evaluate {awaitPromise:true}`; macOS: `callAsyncJavaScript`). Without it, returning a promise fails with *"Promise result not supported in sync mode"* |
| `exec-js --frame <top\|INDEX\|origin\|frame-id>` | Run inside a specific frame. **`exec-js` runs in the page's main world (page globals are visible) but only in the top frame**, so a global set inside an iframe reads as `undefined` until you point it at the right frame. `INDEX` is the `get-frames` index (same order by construction); `frame-id` is the stable handle `get-frames` reports. Out-of-range indexes are rejected; a miss returns `known_frames`. **Cross-site (out-of-process) frames work too**, by index, id, name or URL, through the frame's own session (`out_of_process: true`). `RENDERER_BUSY` = the frame exists but did not answer in time; `FRAME_NOT_FOUND` = no such frame. Windows. |
| `exec-js --world isolated` | An isolated world via `Page.createIsolatedWorld`, hidden from page hooks and blind to page globals. It is **not** a CSP bypass: created with `grantUniveralAccess`, it lifts the same-origin policy but not the page's Content Security Policy (measured: both worlds are blocked identically). Windows. |
| `exec-js --target sw[:<url substring>]` | Evaluate inside the page's **service worker**: `self`, `caches`, `clients`, the worker's globals. Windows / `--engine chrome`. |
| `response-headers [--url-pattern STR] [--resource-type document] [--max 50] [--no-redact]` | Response headers, **including the navigation document** (the one request no other tool can see) and its redirect hops (`redirect: true`). This is where CSP, CORP/COEP, cache policy and cookies are decided. No `--record` needed. `set-cookie` values are masked (name and attributes kept). Windows / `--engine chrome`. |
| `console-logs [--level LEVEL] [--frame top\|INDEX\|ID] [--text S] [--max N] [--clear]` | Read captured page console output (log/info/warn/error/debug + uncaught errors) from the top document and every frame, cross-site ones included (each line has `frame_id`). Ring buffer of 500 (`start --console-buffer N` for more); `dropped` counts what fell off the front. **`console.log` is never wrapped.** Windows uses CDP browser-process events and macOS uses WebKit's system-console logging, so the page cannot detect the capture, and `console-logs` keeps answering through a wedged renderer. Every response carries `console_transport`. |
| `get-requests [--url-pattern <STR>] [--seconds 60] [--max-count N] [--include-headers] [--no-redact]` | Captured XHR/fetch requests (180s TTL, no proxy setup). `--include-headers` adds request headers + timestamps (Authorization/Cookie redacted to a 12-char prefix unless `--no-redact`) |
| `get-frames [--full]` | List iframes (origin, dimensions, and on Windows each frame's CDP `frame_id`, the handle `exec-js --frame` accepts; a cross-site frame is `out_of_process: true` with its current `url`). `name` is truncated to 200 chars (`name_len`/`name_truncated` mark the cut) unless `--full`. |
| `get-frame-html [--selector CSS \| --index N]` | iframe HTML (cross-origin via CDP) |
| `captcha --input-file PATH` | Preprocess captcha for vision model |
| `dismiss-modals [--max-attempts 3]` | Auto-dismiss modals/popups |

## Fault injection (network interception)

Break the network on purpose to reproduce request-queue freezes, races, and timeout
bugs with zero server access. dalzzack injects rules at document start, so they apply
before the app bundle captures its `fetch` reference. Rules persist across navigations
and clear when the daemon stops.

```bash
dalzzack intercept -i b --pattern "*/portfolio*" --action hang           # permanent black hole
dalzzack intercept -i b --pattern "*/markets*" --action delay --ms 8000  # hold 8s, then pass through
dalzzack intercept -i b --pattern "*/orders" --action fail               # instant network error
dalzzack intercept -i b --pattern "*/quote*" --action mock --status 500 --body '{"err":1}'
dalzzack intercept-list -i b                                             # active rules + page sync state
dalzzack intercept-clear -i b [--pattern "*/orders"]                     # remove all (or one pattern)
dalzzack get-requests -i b --include-headers                             # inspect fired Authorization headers
```

| Command | Description |
|---------|-------------|
| `intercept --pattern <P> --action hang [--ms N]` | Matching requests never resolve. Honors the app's own AbortController/timeout; `--ms` auto-errors after N ms. Hung requests show in `get-requests` as `intercepted:"hang"`, `pending:true` |
| `intercept --pattern <P> --action delay --ms N` | Hold matching requests N ms, then pass through unchanged |
| `intercept --pattern <P> --action fail` | Immediate `TypeError('Failed to fetch')` |
| `intercept --pattern <P> --action mock [--status S] [--body B] [--content-type CT] [--ms N]` | Respond locally without hitting the network |
| `intercept-list` | Active rules + the rule-set version the live page has applied |
| `intercept-clear [--pattern <P>]` | Remove all rules, or just one pattern |

Pattern without `*` = substring match; with `*` = glob over the full URL.
`--method POST` restricts a rule to one HTTP method. First matching rule wins;
re-adding the same pattern replaces it.


## Document-start injection

Run your own script **before any page script**, on every navigation. Every other
tool observes the page after it exists. `exec-js` needs a loaded document, so by
the time it runs, the first API call and the first DOM insertion have already
happened.

```bash
dalzzack inject-script -i b --name hook --world main \
  --script "Object.defineProperty(window,'__x',{set:v=>{window.__caught=v}})"
dalzzack navigate -i b --url "https://example.com"     # takes effect here
dalzzack exec-js -i b --world main --script "return window.__caught"
dalzzack inject-list -i b
dalzzack inject-clear -i b --name hook
```

| Command | Description |
|---------|-------------|
| `inject-script --name <L> (--script <JS> \| --file <PATH>) [--world isolated\|main]` | Register a document-start script. Applies from the **next** navigation. |
| `inject-list` | Registered scripts and the world each runs in |
| `inject-clear [--name <L>]` | Unregister one or all |

**The world is the important choice.** `isolated` (the default) shares the DOM
but **cannot see page globals**: it cannot wrap `window.fetch` or anything the
page created, so a script written for that world runs without error and observes nothing.
Use `--world main` to pre-empt a page API. Measured: a `main`-world
`defineProperty` hook catches the page's *first* assignment, while from
`isolated` the same global reads `undefined` and only the DOM is visible.

The isolated world is the same one `exec-js --world isolated` uses, so whatever
the injected script leaves behind is readable from there. Confirmed to run at
document start: `readyState: "loading"`, `document.scripts.length: 0`,
`document.body === null`.

Windows uses CDP `Page.addScriptToEvaluateOnNewDocument`; macOS uses
`WKUserScript` in a named `WKContentWorld`. **macOS cannot remove an individual
`WKUserScript`**, so `inject-clear` there says the script keeps running until the
daemon restarts rather than implying a clean page. Scripts live in daemon memory.

## Diagnostics (hung renderer)

When `exec-js` or `navigate` times out, the page's main thread is wedged. These commands read it from the OUTSIDE without depending on the renderer.

| Command | Description |
|---------|-------------|
| `pause [--duration-ms 1200]` | **Start here.** WHERE is the renderer frozen right now. macOS: external `sample` of the WebContent process (works on a fully hung renderer). Windows: out-of-band CDP JS stack + a kernel-driven minidump of every WebView2 process. |
| `pause --full-memory [--only-pid PID] [--all-processes] [--max-bytes N]` | Windows: dump the heap as well (`MiniDumpWithFullMemory`), so page/JS strings and allocations are searchable in the `.dmp`. Answers "what is allocated?", which the stacks-only default cannot. |
| `pause --symbolize [--download-symbols] [--all-threads]` | Windows: resolve native stacks to function names. dalzzack excludes threads blocked in a syscall and counts them separately, because otherwise "the process was idle" tops every result. A thread inside `NtAllocateVirtualMemory` still counts as *work*, since an allocation storm is usually the thing being hunted. Unresolved frames degrade to `module+0xRVA`. `--download-symbols` needs `dbghelp.dll`+`symsrv.dll` beside the executable (dalzzack installs them and asks you to re-run); the System32 dbghelp cannot reach a symbol server. |
| `pause --samples N [--interval-ms M]` | N dumps of one self-selected process (+ a frame histogram with `--symbolize`), which separates "spinning here" from "happened to be here". |
| `profile-start` / `profile-stop [--output PATH]` | Pair that captures CPU sampling, so you can tell a busy loop from an idle wait. |
| `heap-snapshot [--output PATH]` | Memory report. macOS: native memory via `vmmap`. Windows: JS heap usage via CDP. |
| `console-dump [--level LVL] [--max N]` | Same buffer as `console-logs`, plus a `source` naming the transport that answered. On `cdp` / `os_log` it is a daemon-memory read and works straight through a hang; only the in-page fallback can fail during a hang, and it reports `renderer: "unresponsive"` instead of returning an empty console. |
| `sw-console [--level LVL] [--max N] [--clear]` | The Service Worker's own console output and uncaught exceptions. A SW runs in its own isolate, so nothing it logs ever reaches `console-logs` / `console-dump`. Windows-only: needs a CDP session attached to the `service_worker` target (armed at daemon start, so startup logs aren't missed). If the runtime won't attach it reports `SW_ATTACH_UNAVAILABLE` rather than an empty list. |
| `network-log [--since-ms N] [--url-pattern STR]` | Captured fetch/XHR, surfaces requests still pending (a common hang cause). |
| `attach-devtools` | macOS: enable Safari Web Inspector. Windows: open the WebView2 DevTools window. |
| `exec-js-isolated --script <JS>` | Run JS in a world isolated from page globals (e.g. when the page broke `Array.prototype`). |
| `wait-for-condition --script <JS> [--timeout-ms N] [--interval-ms 250]` | Poll a JS expression until truthy. Hung-renderer aware: reports the last error instead of blocking forever. |
| `trace [--duration-ms 2000] [--output PATH] [--categories STR]` | chrome-tracing timeline capture (Windows: CDP `Tracing.*`, hang-resistant; macOS: Performance API, degraded). |
| `debugger-arm [--strategy first-script\|exceptions\|url] [--url-pattern REGEX]` | Windows-only: pre-arm V8 so `Debugger.paused` events buffer for `debugger-snapshot`. Daemon auto-resumes. |
| `debugger-arm --strategy exceptions [--uncaught-only] [--max-frames N] [--sample-every N]` | Pause on every throw. This is the only way to observe engine-thrown errors (Proxy-invariant `TypeError`, `Illegal invocation`), which never pass through the page's `Error` constructors. |
| `debugger-snapshot [--clear]` | Pull buffered call frames from `debugger-arm`, each with its `scriptId` resolved to a URL via buffered `Debugger.scriptParsed`. Without that, frames from eval'd/bundled/inline scripts arrive with an empty `url`. |
| `debugger-disarm` | Tear down the V8 debugger. |

## Background recording (hang-resistant)

Diagnostics that query the page hit the renderer at call time, so when it's wedged, they wedge too. **Background recording** does the opposite: subscribe to events before navigation starts, buffer them in daemon memory, and read the buffer with `dump-recording`. Because the read never touches the page, it answers even when every other tool would time out.

```bash
dalzzack start --id b --record                                    # ARM capture before any navigation
dalzzack navigate -i b --url "https://example.com"                # events stream into daemon memory
dalzzack dump-recording -i b --output tape.json                   # pure daemon-memory read, hang-resistant
dalzzack dump-recording -i b --filter network --since-ms $TS      # only recent network events
```

| Platform | Transport | Hang resistance |
|----------|-----------|-----------------|
| Windows | CDP `Network.*` / `Runtime.consoleAPICalled` / `Runtime.exceptionThrown` via WebView2 | Full: browser-process event firehose, survives a fully hung renderer |
| macOS | 500 ms poll of in-page `__tw_logs` / `__cr` ring buffers | Partial: events captured up to the freeze survive; `renderer_wedged_at_ms` marks when polling stopped |

Tape shape: `enabled`, `armed`, `arm_error`, `armed_events`, `transport`, `started_at_ms`, `now_ms`, `last_poll_ok_ms`, `renderer_wedged_at_ms`, `counts`, `dropped`, `network[]`, `console[]`, `exceptions[]`. Network events have `kind: request|response|failed|finished` and unify across platforms.

**An empty tape is never ambiguous.** The tape reports three separate facts instead of collapsing them into a count of zero:

| Field | Question it answers |
|-------|---------------------|
| `enabled` | Was `--record` passed at all? (`dalzzack list` shows this per daemon) |
| `armed` / `arm_error` | Did the capture transport come up? If not, `dump-recording` fails with `RECORDING_NOT_ARMED` and the reason |
| `arm_verified` / `arm_probe_ms` | Did an event actually flow end to end? At startup the daemon logs one console line on its own landing page (never a real page), confirms it arrives, and deletes it from the tape |

`dalzzack_version` is included too. If a daemon reports an older version than the CLI you just ran, it's a stale process that predates your `dalzzack upgrade`. Run `stop`, then `start`, to fix it.

A daemon keeps the start flags it was born with. Re-running `start --record` against a live instance returns `already_running` and an `ignored_flags` list, so a session can't silently proceed with recording it never got.

## Auto hang detection (`--watch-hang`)

Don't want to remember to `dump-recording` mid-incident? Add `--watch-hang` to `start` and the daemon will dump for you.

```bash
dalzzack start --id b --record --watch-hang                       # 500 ms ping × 5 fails ⇒ "hang"
# … walk away …
ls /tmp/dalzzack-hangs/                                           # auto-dumped recording + marker JSON
```

On detection the daemon writes `dalzzack-hang-<id>-<ts>-recording.json` (full tape) and `dalzzack-hang-<id>-<ts>-marker.json` (timing metadata) to `<temp>/dalzzack-hangs` (override with `--auto-dump-dir DIR`). One dump per hang; auto-reset after the renderer recovers.

## Timeline tracing (`trace`)

Need to answer "where did time go between event X and Y?" instead of "what's the current state?"

```bash
dalzzack trace --id b --duration-ms 3000 --output /tmp/page.trace.json
# Open the JSON in chrome://tracing or ui.perfetto.dev
```

| Platform | Capture path | Hang resistance |
|----------|--------------|-----------------|
| Windows | Real CDP `Tracing.dataCollected` events with V8 + blink + devtools.timeline categories | Full: browser-process firehose |
| macOS | Performance API + paint timing + longtask via injection (no programmatic CDP available) | Degraded: won't return events from a fully hung renderer |

## V8 Debugger pre-arm (`debugger-arm`, Windows only)

For "what JS is running at startup?" investigations, pre-arm the V8 debugger before any script loads. Pause events buffer daemon-side; the daemon auto-resumes after each pause so the page keeps running.

```bash
dalzzack debugger-arm --id b --strategy first-script
dalzzack navigate -i b --url "https://example.com"
dalzzack debugger-snapshot --id b --clear                         # pull captured call frames
dalzzack debugger-disarm --id b
```

Frames come back with the script identified:

```json
{ "functionName": "b2", "url": "tw-eval-test.js", "url_source": "script_parsed",
  "lineNumber": 0, "pageLineNumber": 0, "scriptId": "27",
  "script_origin_stack": "(anonymous) @ line 3" }
```

`Debugger.paused` call frames carry a `scriptId` and an **empty `url`** for anything that isn't a plain `<script src>`: eval'd code, `new Function`, bundled and inline scripts. That's the code you're usually chasing, and "which script is this function in?" is unanswerable from the pause payload alone. The daemon buffers `Debugger.scriptParsed` alongside the pauses and joins them at snapshot time; `Debugger.enable` re-emits it for already-parsed scripts, so arming late still yields the full table. `url_source` says where the name came from, and `script_origin_stack` names whoever called `eval`.

macOS: `attach-devtools` + Safari Web Inspector instead (WebKit's debugger isn't programmatically accessible from native code).

## Windows native pause

On Windows, `dalzzack pause --id b` also writes an out-of-band minidump of each renderer process via `MiniDumpWriteDump`, in addition to the CDP JS stack capture. The minidump is driven by Windows kernel debug APIs (not V8), so it works on a fully wedged renderer. Open the `.dmp` files in WinDbg or Visual Studio.

## Chrome engine

`start --engine chrome` drives the installed Google Chrome instead of the embedded WebView. WebView2 is Chromium, not Chrome, and bot defences score exactly those differences. Measured while dogfooding this repo: Chrome 154's TLS ClientHello carries 19 extensions and WebView2's 18 (no `trust_anchors`, 0xCA34), so the JA4 differs. "Does *Chrome* get challenged here?" cannot be answered from WebView2.

```bash
dalzzack start --id c --engine chrome      # → browser_version, navigator_webdriver: false
dalzzack navigate -i c --url "https://site.example" --field document.status
dalzzack click -i c --selector "#go"       # Input.dispatchMouseEvent → isTrusted: true
dalzzack stop -i c                         # closes Chrome, deletes the throw-away profile
```

- Chrome launches with only `--user-data-dir=<fresh temp dir> --remote-debugging-port=<free port> --no-first-run --no-default-browser-check`, with no `--enable-automation` and no headless mode. The port is never `0`: measured, `--remote-debugging-port=0` alone turns `navigator.webdriver` to `true`. `start`/`list` report `navigator_webdriver` from the page itself.
- Same commands and output shapes. Input is trusted (`Input.*` events, real `:hover`). Frames include out-of-process cross-site iframes through their own sessions. Chrome's `Fetch` domain enforces `intercept` (nothing injected). CDP serves `get-requests`. `sw-console` / `exec-js --target sw` attach to service workers held at start. Chrome enforces CSP.
- `stop` closes Chrome and deletes its profile before it answers, so `stop && start` is safe. `kill` reaps both, and kills the recorded pid only if it is still that same Chrome process. Closing the window stops the daemon. `reset` restarts on a fresh profile under the same id (`--keep-rules` re-registers injected scripts in the new browser).
- WebView2-internals tools (`pause`, `profile-*`, `heap-snapshot`, `trace`, `debugger-*`, `exec-js-isolated`) answer `UNSUPPORTED_ON_ENGINE`. Open DevTools (F12) in the Chrome window instead.
- One measured difference from a user's Chrome: **no prerendering.** Chrome refuses speculation-rules prerenders while a DevTools client is attached (`PrerenderingDisabledByDevTools`, Chrome 154, with or without `intercept` rules); the same rules still prefetch. A link a user's Chrome would activate from a prerender loads normally here (`activationStart: 0`).

## Output filtering

Every command that prints one JSON document takes `--field PATH` (repeatable; `a.b[0].c`, `items[].id`), `--jq EXPR` (built-in jq, no binary needed) and `--raw`:

```bash
dalzzack list --field "instances[].id" --raw
dalzzack nav-log -i b --jq '.navigations[] | {status, challenge, final_url}'
```

With `--field`, a `status: "failed"` result still prints whole and exits 1, so a filter never turns an error into a plausible `null`.

## Output in a pipe

In a terminal, a command prints indented JSON with every field. In a pipe, which is what an agent reads, it prints compact JSON in a short form, up to a size limit. `--field` and `--jq` always read the complete result. A failure is never shortened.

| Flag | Effect |
|---|---|
| `--full` | The complete result, with no size limit. `DALZZACK_OUTPUT=full` does the same for every command of a script. On `get-frames`, also the complete `name` attributes. |
| `--pretty` | Indented JSON in a pipe. |
| `--max-output N` | Above N bytes (default 16384 in a pipe), write the output to a file and print `status`, `truncated`, `bytes`, `saved_to`, `keys`, and a `preview`. 0 turns this off. A failure, the output of `--field` or `--jq`, and a result with a `--max-length` you set are always printed whole. |

The short form changes these commands:

| Command | Short form |
|---|---|
| `navigate`, `wait-navigation` | A normal load prints `http` and not `document`. A redirect, HTTP 400 or more, a challenge, a second navigation, or a response that did not commit keeps `document`. |
| `describe` | `elements` are `<id> <tag> <x>,<y> <text>` strings, and `formValues` has only what no row shows. |
| `console-logs` | `logs` are `<level> [source] L<line> <message> @<url>` strings. The shared URL is at the top, and identical lines are merged with `(xN)`. |
| `exec-js` | Only `status` and `result`, unless `--frame`, `--world`, or `--target` was given. |
| `list` | The fields an agent reads, plus any field that is not at its normal value. |
| All | `null` fields and `data:` URLs of more than 200 characters go. |

## Batch mode

```bash
dalzzack run -i browser --steps workflow.json      # use a live instance
dalzzack run -i browser --steps -                  # read the steps from stdin
dalzzack run --steps workflow.json                 # start a browser, run, exit
```

Steps can be a list of command lines, which any command that acts on a page can be:

```json
[["fill-form", "--selector", "#email", "--value", "$EMAIL"],
 ["click", "--text", "Sign In"],
 ["wait-for", "--selector", ".dashboard"]]
```

All steps are read before the first one runs, and a failed step stops the run with exit 1. A step takes its instance from `run`, so a step with `-i` is refused. With the id of a live instance, one result prints: `status`, `steps_run`, `steps_total`, and `last`, or `failed_step` and the failed `result`. Without one, `run` is self-contained: it starts a browser, prints one JSON line per step with the whole result, and exits.

The other step format uses the tool names from `dalzzack tools`:

```json
{
  "name": "Login Flow",
  "steps": [
    { "tool": "navigate", "args": { "url": "https://example.com/login" } },
    { "tool": "fill_form", "args": { "fields": [
      { "selector": "#email", "value": "$EMAIL" },
      { "selector": "#password", "value": "$PASSWORD" }
    ]}},
    { "tool": "screenshot", "args": {} }
  ]
}
```

Environment variables (`$VAR` / `${VAR}`) are expanded in step args.
