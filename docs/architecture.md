# Architecture

```
CLI command ──→ TCP socket ──→ Daemon process (Tauri WebView + socket server)
                                       │
                                  execute_tool()
                                       │
                          Platform-specific native WebView APIs
                          ├── macOS:   WKWebView (objc2)
                          ├── Windows: WebView2 (COM)
                          └── Linux:   WebKitGTK (webkit2gtk)
```

```
src-tauri/src/
├── lib.rs              # Thin facade (re-exports)
├── app.rs              # Tauri setup, window creation, inline landing page
├── toolbar/            # the strip above the page: bar.html, layout, address and history
├── activity.rs         # which commands a window is running (read by the toolbar)
├── main.rs             # CLI entry point (clap)
├── state.rs            # APP_HANDLE, INSTANCE_ID, CDP_CONTEXT_STORE
├── types.rs            # Result types
├── logging.rs          # File logging, panic hook
├── injection.rs        # Init script (dialog dismiss, network interceptor)
├── platform/           # OS-specific WebView access
├── webview/            # HTML/form/frame/CDP operations
├── tools/              # Tool implementations + execute_tool() dispatch
├── daemon/             # Socket server + client
├── chrome/             # --engine chrome: launch, CDP WebSocket, native tools, Fetch intercept
├── engine.rs           # webview | chrome, engine-neutral page CDP call
├── netlog.rs           # Always-on request index + navigation timeline
├── cookies.rs          # cookies list|clear
├── profile.rs          # Which profile a daemon uses, who shares it
├── reset.rs            # reset
├── upgrade.rs          # upgrade: release download, checks, binary swap
└── cli/                # Batch steps, short form in a pipe, --field/--jq, output limit
```

## Platform matrix

The Linux column describes code that exists but that CI does not build or test.

| Feature | macOS | Windows | Linux | `--engine chrome` (Windows verified) |
|---------|-------|---------|-------|------------------|
| Navigation | WKWebView `loadRequest` | WebView2 `Navigate` | WebKitGTK `load_uri` | `Page.navigate` + `load` lifecycle |
| JS execution | `evaluateJavaScript` | `ExecuteScript` | `run_javascript` | `Runtime.evaluate` |
| JS execution (`--await`) | `callAsyncJavaScript` | CDP `Runtime.evaluate {awaitPromise}` | — | `Runtime.evaluate {awaitPromise}` |
| `clear-site-data` | `WKWebsiteDataStore` (host scope) | CDP `Storage.clearDataForOrigin` (origin scope) | — | CDP `Storage.clearDataForOrigin` |
| `reload --hard` | `reloadFromOrigin` | CDP `Page.reload {ignoreCache}` | — | CDP `Page.reload {ignoreCache}` |
| `sw-console` / `exec-js --target sw` | — | CDP `Target.setAutoAttach` + session-scoped `Runtime.*` | — | browser-level auto-attach, held at start |
| `nav-log` / `failed-requests` / `cookies` | — | CDP `Network.*` / `Page.*` | — | CDP `Network.*` / `Page.*` |
| `reset` (browsing data) | current origin only | `ICoreWebView2Profile2::ClearBrowsingDataAll` | — | new Chrome on a new profile |
| Cross-origin iframe | WKScriptMessageHandler bridge | CDP Runtime contexts | SOP disabled (WebKitSettings) | OOPIF sessions (auto-attach) |
| Screenshot | xcap | WebView2 `CapturePreview` | xcap | `Page.captureScreenshot` |
| Native mouse click | CGEvent | SendInput | — | `Input.dispatchMouseEvent` (trusted) |

## Design decisions

- **Per-instance processes, shared profile by default.** Each `--id` is a separate process with its own WebView, but plain instances share one WebView2 profile, so a login made in one carries over to the next. `--private-profile` gives an instance its own profile (wiped at start), and `--engine chrome` a throw-away one. `cookies clear` and `reset` refuse on the shared profile unless `--force-shared`, even with no other instance running, since the profile outlives them (`--dry-run` shows what is at stake).
- **SPA-safe form fill.** `fill-form` uses `nativeInputValueSetter` to bypass React/Vue virtual DOM and trigger `input`/`change` events correctly.
- **Network interception without a proxy.** The init script wraps XHR/fetch and captures requests with a 180s TTL.
- **Cross-origin iframes via CDP.** No SOP-disabling hacks on Windows/macOS. dalzzack routes execution contexts through CDP `Runtime.evaluate`.
- **The toolbar is not part of the page.** Every window has a strip above the page: history, an address a person can type in, and the command the window is running. It is a second webview in the same window, so nothing is injected into the page's DOM, the page cannot see or reach it, and the page keeps the size it was asked for (the window is taller by the strip, and on macOS by the title bar, which is measured because the content area reaches under it). It has no Tauri IPC and no network: it asks for things by navigating to a reserved address (`dalzzack.invalid`) that the host answers and cancels. It shows a label the CLI built from the command line as typed, with every value masked unless its flag is on an allow-list of places and settings (`cli::label`), so what an agent typed into a form never reaches the screen, a screen recording, or the daemon. Back and Forward follow the history position dalzzack keeps itself, because no page can be asked. WebView engine only; `--engine chrome` is real Chrome and has its own.
- **Inline landing page.** The daemon ships a `data:` URL landing screen so `start` shows the instance id and ready status before you call `navigate`.
- **The instrumentation must not change what it measures.** Console capture used to wrap `console.log` and serialize arguments with `JSON.stringify`, which invokes accessors. That is the exact probe `disable-devtool` uses to detect DevTools. Protected sites concluded a debugger was attached and redirected away, so every observation made through dalzzack on such a site was worthless. Capture now happens outside the page (CDP on Windows, WebKit's system-console logging on macOS); `console.log` is untouched and dalzzack renders object arguments from the engine's own preview, which marks accessors without calling them. The in-page fallback that remains for other platforms reads only property *descriptors*, never values behind a getter.
- **Injected state carries no brand.** Page-scope globals live under one per-daemon randomized name, and the init script writes nothing to the page's console. This defeats static name/string detection, not behavioural detection, and the docs don't claim otherwise.
- **A capability that silences evidence must announce itself.** CSP enforcement is off by default, because cross-origin iframe inspection depends on it. The side effect is that the browser stops being a witness: no load is ever blocked, so no violation is ever reported, and "zero CSP violations" means nothing. That used to be invisible and cost a whole investigation. Now `list` reports `csp_bypassed`, `console-logs --source csp` says so when it returns empty, and `--enforce-csp` turns enforcement back on.
- **Fingerprint consistency over fingerprint spoofing.** The engine generates `Sec-CH-UA` / `Sec-CH-UA-Full-Version-List` itself and they cannot be overridden. A UA string that disagrees with them is a *stronger* bot signal than an unusual UA, so dalzzack changes the UA only where the default is itself the giveaway (macOS), reads the real engine version at runtime rather than hardcoding one, and never injects a fingerprint it cannot make consistent.
