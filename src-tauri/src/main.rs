//! dalzzack — Browser automation CLI for AI agents
//!
//! Daemon mode: `dalzzack start --id hn` → persistent browser instance
//! Individual tools: `dalzzack navigate --id hn --url "..."`
//! Batch mode: `dalzzack run --id hn --steps file.json`

// NOTE: windows_subsystem = "windows" intentionally NOT set.
// dalzzack is a CLI tool — it needs console output (stdout/stderr).
// A GUI-only build would need it; we use the console subsystem instead.

use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dalzzack",
    about = "Browser automation CLI for AI agents.\n\nStart a browser instance, then control it with individual commands.\nEach --id creates an isolated browser window for parallel execution.\n\nRecommended workflow:\n  1. start → navigate → describe (annotated screenshot + element list)\n  2. click/fill-form/keyboard (interact using coordinates from describe)\n  3. screenshot (quick visual check)\n  4. save-image (download images from page)\n  5. stop\n\nEvery command that prints one JSON document accepts --field PATH (repeatable), --jq EXPR\nand --raw to print only part of it.",
    version
)]
struct Args {
    #[command(subcommand)]
    cmd: Cli,
    /// Print only this part of the JSON result — a dotted path such as `status`,
    /// `document.hops[0].status` or `instances[].id` (`[]` fans out over an array). Repeat for
    /// several (printed as an object keyed by path). A missing path prints `null` — and when a
    /// differently spelled key exists (`httpOnly` vs `http_only`) stderr says so. If the result
    /// has `status: "failed"`, the WHOLE failure is printed and the exit code is 1 — a filter
    /// never turns an error into a plausible null.
    #[arg(long, global = true)]
    field: Vec<String>,
    /// Filter the JSON result through a jq program (built in — no jq binary needed), e.g.
    /// `--jq '.navigations[] | {status, final_url}'`. One output per line. Sees failures too.
    /// This is jaq: jq's language and standard library, incl. `test`/`match`/`capture`/`scan`/
    /// `sub`/`gsub` — but regexes are Rust `regex` syntax: no look-around (`(?=…)`) and no
    /// backreferences (`\1`). A regex backslash is doubled inside a jq string (`"\\d"`).
    #[arg(long, global = true, conflicts_with = "field")]
    jq: Option<String>,
    /// With --field / --jq: print string results without JSON quotes (like `jq -r`).
    #[arg(long, global = true)]
    raw: bool,
    /// Print indented JSON. This is the default in a terminal; a pipe gets compact JSON.
    #[arg(long, global = true)]
    pretty: bool,
    /// Print the complete result. This is the default in a terminal; a pipe gets a
    /// short form, unless DALZZACK_OUTPUT=full is set. On `get-frames`, also print
    /// complete `name` attributes: by default they are cut at 200 chars
    /// (`name_len`/`name_truncated` mark the cut), because SafeFrame ad slots carry
    /// multi-KB JSON in `name`.
    #[arg(long, global = true)]
    full: bool,
    /// Save output larger than N bytes to a file and print a summary. 0 turns this
    /// off. The default is 16384 in a pipe, and no limit in a terminal or with --full.
    #[arg(long, global = true, value_name = "N")]
    max_output: Option<usize>,
}

#[derive(Subcommand)]
enum Cli {
    /// Start a NEW browser daemon (no-op if id already exists — use 'resize' to change a live window).
    /// --width/--height set the INITIAL size only; they're ignored if the daemon is already running.
    /// Run `dalzzack list` first if you're unsure whether an id is alive.
    Start {
        /// Unique instance ID (auto-generates PID-based if omitted)
        #[arg(long, short)]
        id: Option<String>,
        /// Initial WebView window width in pixels (default: 1200). Ignored if daemon already running.
        #[arg(long)]
        width: Option<u32>,
        /// Initial WebView window height in pixels (default: 800). Ignored if daemon already running.
        #[arg(long)]
        height: Option<u32>,
        /// Enable always-on background event recording so `dump-recording` works even when the
        /// renderer is wedged. Windows uses CDP events (true browser-process firehose); macOS
        /// uses a 500 ms in-page poll with explicit wedge detection. Off by default.
        #[arg(long)]
        record: bool,
        /// Run a daemon-side hang watchdog: pings the renderer every 500 ms; 5 consecutive
        /// timeouts auto-dump the recording (and a `hang.json` marker) so post-mortem data
        /// survives even if you forget to call `dump-recording`. Implies recording is useful
        /// but not required.
        #[arg(long = "watch-hang")]
        watch_hang: bool,
        /// Directory to write auto-dumps to when --watch-hang fires (default: <temp>/dalzzack-hangs).
        #[arg(long = "auto-dump-dir")]
        auto_dump_dir: Option<String>,
        /// Extra Chromium flag for the WebView2 browser process. Repeatable, e.g.
        /// `--browser-arg "--enable-heap-profiling" --browser-arg "--js-flags=--stack-trace-limit=0"`.
        /// Only applied at daemon start (WebView2 reads these at environment creation).
        /// Such an instance gets its OWN user data folder — WebView2 allows one set of browser
        /// arguments per folder — so it does not share cookies/logins with other ids.
        /// Windows-only; ignored elsewhere.
        // allow_hyphen_values: every useful value here starts with `--`, which clap
        // would otherwise parse as the next flag.
        #[arg(long = "browser-arg", allow_hyphen_values = true)]
        browser_arg: Vec<String>,
        /// Collect no console output at all: no CDP subscription (Windows), no in-page
        /// hook anywhere. On Windows, capture is out-of-band and invisible to the page, so
        /// this is only needed for sites that treat even CDP as instrumentation. On macOS
        /// capture is already off unless `--console-capture` is given, so this flag mainly
        /// makes that explicit. `console-logs` then reports `console_transport: "disabled"`
        /// rather than an empty buffer that reads like a quiet page.
        #[arg(long = "no-console-capture")]
        no_console_capture: bool,
        /// macOS only: turn console capture ON, accepting that the page can see it.
        ///
        /// There is no invisible console transport on macOS. WebKit's system-console
        /// setting adds a second `toString` pass over every logged argument, and
        /// anti-debug libraries (`disable-devtool`) count those calls — two instead of
        /// one reads as "DevTools is open", which is what made protected sites redirect
        /// away. So macOS collects nothing by default and `console-logs` reports
        /// `unavailable` with the reason. Pass this when you know the page is not
        /// anti-debug and you want its console: it installs the in-page hook, and
        /// `console-logs` then reports `in_page_fallback` so the visibility is on record.
        #[arg(long = "console-capture")]
        console_capture: bool,
        /// Override the WebView's User-Agent. Unlike `--browser-arg "--user-agent=…"` this
        /// needs no private user data folder, so cookies/logins stay shared with other ids.
        /// The engine still generates its own `Sec-CH-UA` headers, so a UA whose major
        /// version disagrees with the engine is a STRONGER bot signal than the default —
        /// change nothing unless you know why.
        #[arg(long = "user-agent")]
        user_agent: Option<String>,
        /// Keep the engine's raw User-Agent even where the default is a giveaway. Only
        /// affects macOS, whose WKWebView default ends at `(KHTML, like Gecko)` with no
        /// browser product token and is completed with the installed Safari's version by
        /// default. Windows already sends a consistent Edge UA and is never rewritten.
        #[arg(long = "raw-user-agent")]
        raw_user_agent: bool,
        /// Keep the page's Content Security Policy ENFORCED. dalzzack disables CSP by
        /// default (CDP `Page.setBypassCSP`) so cross-origin iframes can be inspected —
        /// which also means nothing is ever blocked, so `console-logs --source csp` stays
        /// empty no matter what the policy says, and a load a real browser would refuse
        /// succeeds here. Use this whenever the question IS the policy. Costs cross-origin
        /// iframe access. Windows-only — `Page.setBypassCSP` needs CDP, so macOS and Linux
        /// never disable CSP and this flag is a no-op there. `list` reports `csp_bypassed`.
        #[arg(long = "enforce-csp")]
        enforce_csp: bool,
        /// Give this instance its OWN browser profile (cookies, storage, cache): empty at start,
        /// deleted by `stop`. Plain instances share one WebView2 profile — a login in one carries over to
        /// the next, and `cookies clear` / `reset` in one would hit all of them (so those refuse
        /// on a shared profile). Windows/WebView2; `--engine chrome` is always private.
        #[arg(long = "private-profile")]
        private_profile: bool,
        /// Browser engine: `webview` (default — WebView2 / WKWebView) or `chrome` (a real Google
        /// Chrome with a throw-away profile, driven over CDP). Use `chrome` when the question is
        /// what Chrome itself sees: WebView2 differs in its TLS ClientHello, client hints and
        /// how bot defences score it. Chrome is launched WITHOUT automation flags.
        #[arg(long)]
        engine: Option<String>,
        /// `--engine chrome`: path to the Chrome executable (default: the installed Google
        /// Chrome; also read from DALZZACK_CHROME).
        #[arg(long = "chrome-path")]
        chrome_path: Option<String>,
        /// Stop this daemon by itself after this long: `90s`, `10m`, `2h`, `1h30m` (a bare
        /// number is seconds). It runs the same path as `stop` — Chrome and a
        /// `--private-profile` folder go with it — so a short-lived comparison instance cannot
        /// be left running. `start` and `list` report `ttl_s` and `expires_at_ms`.
        #[arg(long)]
        ttl: Option<String>,
        /// Console ring-buffer size (default 500, max 100000). The oldest lines are dropped
        /// first; `console-logs` reports `dropped` and `buffer_size`.
        #[arg(long = "console-buffer")]
        console_buffer: Option<usize>,
    },

    /// Resize the window of an ALREADY-RUNNING daemon (use this for responsive testing or mid-session resize).
    /// For initial sizing on a fresh instance, use `start --width --height` instead.
    Resize {
        /// Instance ID of the running daemon
        #[arg(long, short, default_value = "")]
        id: String,
        /// New window width in pixels (>= 100)
        #[arg(long)]
        width: u32,
        /// New window height in pixels (>= 100)
        #[arg(long)]
        height: u32,
    },

    /// Navigate one step back in browser history.
    Back {
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Navigate one step forward in browser history.
    Forward {
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Reload the current page.
    Reload {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Bypass the HTTP cache (Ctrl+Shift+R). Windows: CDP Page.reload
        /// {ignoreCache:true}; macOS: -[WKWebView reloadFromOrigin].
        #[arg(long)]
        hard: bool,
    },

    /// Clear storage for an origin out-of-band (service workers, caches,
    /// localStorage, IndexedDB, cookies). Bypasses page JS, so it works even
    /// when a membrane/policy hides `navigator.serviceWorker`.
    #[command(name = "clear-site-data")]
    ClearSiteData {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Origin to clear, e.g. "http://proxy.localhost:18080".
        /// Defaults to the current page's origin.
        #[arg(long)]
        origin: Option<String>,
        /// Comma-separated storage types, or "all". Known: appcache,
        /// cache_storage, cookies, file_systems, indexeddb, local_storage,
        /// service_workers, shader_cache, websql.
        #[arg(
            long,
            default_value = "service_workers,cache_storage,local_storage,indexeddb,cookies"
        )]
        types: String,
    },

    /// Hover over an element by CSS selector or coordinates (reveals dropdown menus, tooltips).
    Hover {
        #[arg(long, short, default_value = "")]
        id: String,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        x: Option<f64>,
        #[arg(long)]
        y: Option<f64>,
    },

    /// Read the page's captured console output (log/info/warn/error/debug).
    #[command(name = "console-logs")]
    ConsoleLogs {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Filter by level: log, info, warn, error, debug
        #[arg(long)]
        level: Option<String>,
        /// Filter by origin: `console` (the page's own console.* calls), `csp`
        /// (Content Security Policy violations), `security`, `network`,
        /// `deprecation`, `violation`. Omit for everything.
        #[arg(long)]
        source: Option<String>,
        /// Only lines from this frame: `top` (plus lines with no frame id), an index from
        /// `get-frames`, or a `frame_id`. Cross-site (out-of-process) frames are captured too.
        #[arg(long)]
        frame: Option<String>,
        /// Only lines whose message contains this text (case-insensitive).
        #[arg(long)]
        text: Option<String>,
        /// Maximum entries to return — the newest (default 100, up to the buffer size).
        #[arg(long, default_value = "100")]
        max: u32,
        /// Clear the buffer after reading.
        #[arg(long)]
        clear: bool,
    },

    /// Response headers for captured responses, INCLUDING the navigation document —
    /// the one request no other tool can see (`get-requests` is an in-page hook that
    /// does not exist yet when the document is fetched). This is where CSP, CORP,
    /// cache policy and cookies are decided. Windows-only. No `--record` needed.
    #[command(name = "response-headers")]
    ResponseHeaders {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Filter by URL: substring match, or a glob if it contains `*`.
        #[arg(long = "url-pattern")]
        url_pattern: Option<String>,
        /// CDP resource type, case-insensitive: `document` (the navigation — usually
        /// what you want), `xhr`, `fetch`, `script`, `stylesheet`, `image`, …
        #[arg(long = "resource-type")]
        resource_type: Option<String>,
        /// Maximum entries — newest last (default 50, cap 500).
        #[arg(long, default_value = "50")]
        max: u32,
        /// Show `set-cookie` values unmasked. By default the cookie name and its
        /// attributes (Path/Domain/HttpOnly/SameSite/Secure) are kept and only the
        /// value is hidden.
        #[arg(long = "no-redact")]
        no_redact: bool,
    },

    /// Navigation timeline: every document request chain with EVERY hop (redirects included),
    /// each hop's status, `location`, `cf-mitigated`, set-cookie names, a detected `challenge`,
    /// and whether the response committed. Always on — no `--record` needed. A challenge that
    /// solves itself shows as a `403 cf-mitigated` entry followed by a second navigation.
    #[command(name = "nav-log")]
    NavLog {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Only chains that started within the last N ms.
        #[arg(long = "since-ms")]
        since_ms: Option<u64>,
        /// Keep chains where any hop's URL matches (substring, or glob with `*`).
        #[arg(long = "url-pattern")]
        url_pattern: Option<String>,
        /// Include iframe documents (default: top frame only).
        #[arg(long)]
        frames: bool,
        /// Newest N chains (default 20, cap 200).
        #[arg(long, default_value = "20")]
        max: u32,
        /// Add full request headers as sent on the wire (cookies, sec-fetch-*) and full
        /// response headers to every hop.
        #[arg(long)]
        headers: bool,
        /// Do not mask cookie / set-cookie / authorization values.
        #[arg(long = "no-redact")]
        no_redact: bool,
    },

    /// Requests that failed, of EVERY resource type, with their URLs: blocked (CSP/mixed
    /// content), cors, network_error (DNS/TLS/reset), http_error (>= 400), canceled (hidden
    /// unless --include-canceled). Always on — no `--record` needed.
    #[command(name = "failed-requests")]
    FailedRequests {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Only requests that started within the last N ms.
        #[arg(long = "since-ms")]
        since_ms: Option<u64>,
        /// Substring, or glob with `*`.
        #[arg(long = "url-pattern")]
        url_pattern: Option<String>,
        /// CDP resource type: document, script, stylesheet, image, font, xhr, fetch, …
        #[arg(long = "resource-type")]
        resource_type: Option<String>,
        /// Network-layer failures only; leave out HTTP >= 400 responses.
        #[arg(long = "no-http-errors")]
        no_http_errors: bool,
        /// Also list canceled (net::ERR_ABORTED) requests.
        #[arg(long = "include-canceled")]
        include_canceled: bool,
        /// Newest N (default 50).
        #[arg(long, default_value = "50")]
        max: u32,
        /// Add the failing hop's request/response headers.
        #[arg(long)]
        headers: bool,
        /// Do not mask cookie values.
        #[arg(long = "no-redact")]
        no_redact: bool,
    },

    /// The browser's cookie jar through CDP (`cookies list|clear`) — HttpOnly cookies and
    /// cookies on parent/third-party domains included. Default scope: what the browser would
    /// send to the current page. Fields are snake_case: `http_only`, `same_site`, `host_only`.
    /// `clear` re-reads the jar and reports what remains, and refuses on the shared profile
    /// (even with no other daemon running — it outlives this one) unless --force-shared;
    /// `--dry-run` lists what it would delete.
    Cookies {
        /// list | clear
        #[arg(value_parser = ["list", "clear"], default_value = "list")]
        action: String,
        #[arg(long, short, default_value = "")]
        id: String,
        /// Cookies the browser would send to this URL (default: the current page).
        #[arg(long)]
        url: Option<String>,
        /// Cookies on this domain, its subdomains and its parent domains.
        #[arg(long)]
        domain: Option<String>,
        /// The whole cookie jar.
        #[arg(long)]
        all: bool,
        /// Only cookies with this exact name.
        #[arg(long)]
        name: Option<String>,
        /// Show cookie values unmasked.
        #[arg(long = "no-redact")]
        no_redact: bool,
        /// clear: proceed on the shared profile anyway.
        #[arg(long = "force-shared")]
        force_shared: bool,
        /// clear: list what would be deleted, delete nothing (never refused).
        #[arg(long = "dry-run")]
        dry_run: bool,
    },

    /// Return this daemon to a first-visit state WITHOUT stopping it (id, port, window and start
    /// flags are kept): clears the profile's browsing data, intercept rules, injected scripts and
    /// dalzzack's buffers, then opens about:blank. Refuses on the shared profile (even with no
    /// other daemon running — it outlives this one) unless --force-shared or
    /// --keep-browsing-data; `--dry-run` says what each step would do.
    Reset {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Reset only dalzzack's own state (rules, buffers, page) — leave cookies/storage.
        #[arg(long = "keep-browsing-data")]
        keep_browsing_data: bool,
        /// Keep intercept rules and injected scripts.
        #[arg(long = "keep-rules")]
        keep_rules: bool,
        /// Clear the shared profile anyway.
        #[arg(long = "force-shared")]
        force_shared: bool,
        /// Report what each step would do — cookie counts per domain included — and do nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Where to leave the page afterwards (default about:blank).
        #[arg(long)]
        url: Option<String>,
    },

    /// Turn the `--record` tape on or off on a LIVE daemon (`record on|off|status`). `on`
    /// captures from now on; `off` stops appending but keeps the tape for `dump-recording`.
    Record {
        /// on | off | status
        #[arg(value_parser = ["on", "off", "status"], default_value = "status")]
        action: String,
        #[arg(long, short, default_value = "")]
        id: String,
        /// With `off`: also drop what was captured.
        #[arg(long)]
        clear: bool,
    },

    /// Stop a running browser instance (graceful — sends __shutdown over TCP).
    /// If the daemon is hung and won't accept the shutdown, use `kill` instead.
    Stop {
        /// Instance ID to stop
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Force-kill a daemon process by PID. Use this when a daemon is wedged
    /// on a tool call and `stop` times out. Bypasses TCP entirely.
    Kill {
        /// Instance ID to kill
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// List running browser instances, with the state that explains a misbehaving one:
    /// `renderer_ok` (can this daemon run JS at all?), `version` (a long-lived daemon keeps the
    /// binary it started with), `record`, `recording_armed`, `watch_hang`.
    /// Check this FIRST when tools time out or a recording tape is empty.
    List,

    /// Navigate to a URL
    Navigate {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Target URL
        #[arg(long)]
        url: String,
        /// Page load timeout in ms (default: 5000)
        #[arg(long, default_value = "5000")]
        timeout_ms: u32,
    },

    /// Take a clean screenshot (returns absolute file path to PNG). Use 'describe' instead if you need to identify interactive elements
    Screenshot {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Output file path (auto-generated if omitted)
        #[arg(long)]
        output: Option<PathBuf>,
        /// CSS selector to crop to specific element
        #[arg(long)]
        selector: Option<String>,
    },

    /// Click an element by coordinates, CSS selector, or text content.
    /// Provide --x + --y for coordinate click, OR --selector / --text for element click.
    /// Example: --x 100 --y 200
    /// Example: --selector "button.submit"
    /// Example: --text "Sign In"
    Click {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// X coordinate (viewport pixels)
        #[arg(long)]
        x: Option<f64>,
        /// Y coordinate (viewport pixels)
        #[arg(long)]
        y: Option<f64>,
        /// CSS selector for the element to click
        #[arg(long)]
        selector: Option<String>,
        /// Visible text to find and click (XPath-based search)
        #[arg(long)]
        text: Option<String>,
        /// Click type: left, right, double (default: left)
        #[arg(long, default_value = "left")]
        click_type: String,
    },

    /// Scroll the page or a specific element.
    /// Use --to for absolute positioning, or --direction + --amount for relative.
    /// Example: --to bottom --selector ".chat-container"
    /// Example: --direction down --amount 500
    Scroll {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Direction: up, down, left, right (default: down)
        #[arg(long, default_value = "down")]
        direction: String,
        /// Scroll amount in pixels (default: 500)
        #[arg(long, default_value = "500", alias = "pixels")]
        amount: i32,
        /// CSS selector to scroll within (default: page)
        #[arg(long)]
        selector: Option<String>,
        /// Scroll to absolute position: top, bottom (overrides --direction/--amount)
        #[arg(long)]
        to: Option<String>,
    },

    /// Execute JavaScript and return the result as JSON.
    /// Runs in the page's MAIN world (page globals ARE visible) but only in the TOP frame —
    /// a global set inside an iframe reads as undefined unless you pass --frame.
    /// Every result reports where it ran (`world`, `frame`, `context_id`).
    #[command(name = "exec-js")]
    ExecuteJs {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// JavaScript code to execute (use 'return' to get a value, e.g. 'return document.title')
        #[arg(
            long,
            alias = "code",
            required_unless_present = "file",
            conflicts_with = "file"
        )]
        script: Option<String>,
        /// Read the script from a file instead — no shell quoting to fight.
        /// `--script "$(cat probe.js)"` breaks on quotes/backticks/$; this
        /// doesn't. Same as inject-script's --file.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Execution timeout in ms (default: 30000)
        #[arg(long, default_value = "30000")]
        timeout_ms: u32,
        /// Await a promise result instead of failing on it. Lets the script use
        /// top-level `await`. Windows: CDP Runtime.evaluate {awaitPromise:true};
        /// macOS: -[WKWebView callAsyncJavaScript:].
        #[arg(long = "await")]
        await_promise: bool,
        /// Run inside a specific frame: `top` (default), an index from `get-frames`
        /// (SAME order by construction), a CDP frame id from `get-frames`/`known_frames`
        /// (the stable handle), or a substring of the frame's origin/name. Out-of-range
        /// indexes are rejected; a miss returns the `known_frames` list. Windows only.
        #[arg(long)]
        frame: Option<String>,
        /// JS world: `main` (default — sees page globals) or `isolated`
        /// (Page.createIsolatedWorld; hidden from page hooks, cannot see page globals).
        /// Note on CSP: the isolated world is created with `grantUniveralAccess`, which
        /// lifts the same-origin policy but NOT the page's Content Security Policy —
        /// measured, both worlds are blocked identically. What does remove CSP is
        /// dalzzack's own default (`Page.setBypassCSP`), which applies to every world;
        /// start with `--enforce-csp` before drawing any conclusion about a policy.
        /// Windows only.
        #[arg(long, default_value = "main")]
        world: String,
        /// Which JS target: `page` (default) or `sw` — the page's service worker, its own
        /// isolate that no page-side tool can reach (`self`, `caches`, `clients`, the SW's
        /// globals). `sw:<substring>` picks the worker whose script URL contains it; plain
        /// `sw` the most recently attached one. Windows/WebView2 or `--engine chrome`.
        #[arg(long, default_value = "page")]
        target: String,
    },

    /// Fill form fields (SPA-compatible with native input setters).
    /// Use --selector + --value for a single field, or --fields for multiple.
    /// Example: --selector "textarea" --value "hello"
    /// Example: --fields '[{"selector":"#email","value":"a@b.com"},{"selector":"#pw","value":"123"}]'
    #[command(name = "fill-form")]
    FillForm {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// JSON array of fields: [{"selector": "...", "value": "..."}] (for multiple fields)
        #[arg(long)]
        fields: Option<String>,
        /// CSS selector for single field (use with --value)
        #[arg(long, requires = "value")]
        selector: Option<String>,
        /// Value for single field (use with --selector)
        #[arg(long, requires = "selector")]
        value: Option<String>,
        /// CSS selector of submit button to click after filling
        #[arg(long)]
        submit: Option<String>,
    },

    /// Get page HTML (full page or specific element)
    #[command(name = "get-html")]
    GetHtml {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// CSS selector (default: full page)
        #[arg(long)]
        selector: Option<String>,
        /// Keep script/style/svg/comment noise. Stripping is the default.
        #[arg(long)]
        no_strip_noise: bool,
        /// Stripping is the default, so this flag changes nothing.
        #[arg(long, hide = true, conflicts_with = "no_strip_noise")]
        strip_noise: bool,
        /// Max output length in chars
        #[arg(long)]
        max_length: Option<u64>,
    },

    /// Get current page URL and title
    #[command(name = "get-url")]
    GetUrl {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Wait for an element condition in the DOM.
    /// --state present: wait for element to exist (default)
    /// --state added: wait for NEW elements matching selector (count increases)
    /// --state removed: wait for element to disappear from DOM
    /// Example: --selector "img" --state added --timeout-ms 30000
    #[command(name = "wait-for")]
    WaitFor {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// CSS selector to wait for
        #[arg(long)]
        selector: String,
        /// Wait condition: present (default), added (count increases), removed (disappears)
        #[arg(long, default_value = "present")]
        state: String,
        /// Wait for element containing this text
        #[arg(long)]
        text: Option<String>,
        /// Timeout in ms (default: 10000)
        #[arg(long, default_value = "10000")]
        timeout_ms: u32,
    },

    /// Sleep for specified milliseconds
    Wait {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Duration in milliseconds
        #[arg(long)]
        ms: u64,
    },

    /// Wait until the document is REPLACED — form submit, click-through,
    /// redirect. Detects same-URL navigations too (a sentinel planted in the
    /// old document vanishes when the new one commits), so it works where
    /// "did the URL change?" doesn't. Use after click/fill-form instead of a
    /// wait-for-condition loop; proxied navigations can take 20–30 s.
    #[command(name = "wait-navigation")]
    WaitNavigation {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Overall timeout in ms (default 30000 — raise for slow proxies)
        #[arg(long, default_value = "30000")]
        timeout_ms: u64,
        /// Only finish when the NEW url contains this substring (redirect
        /// chains keep waiting until it matches)
        #[arg(long)]
        to: Option<String>,
        /// Poll interval in ms (default 250)
        #[arg(long, default_value = "250")]
        interval_ms: u64,
    },

    /// Type text or press keyboard keys
    #[command(name = "keyboard")]
    KeyboardInput {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Text to type character by character
        #[arg(long)]
        text: Option<String>,
        /// Special key (Enter, Tab, Escape, ArrowDown, Backspace, etc.)
        #[arg(long)]
        key: Option<String>,
        /// Focus this element before typing
        #[arg(long)]
        selector: Option<String>,
        /// Modifier keys (ctrl, shift, alt, meta), comma-separated
        #[arg(long)]
        modifiers: Option<String>,
        /// Paste text at once using native setter (faster than character-by-character)
        #[arg(long)]
        paste: bool,
    },

    /// Find the nearest interactive element at coordinates (returns HTML + selector)
    #[command(name = "element-at")]
    ElementAt {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// X coordinate (viewport pixels)
        #[arg(long)]
        x: f64,
        /// Y coordinate (viewport pixels)
        #[arg(long)]
        y: f64,
    },

    /// Get captured XHR/fetch network requests
    #[command(name = "get-requests")]
    GetRequests {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Filter by URL pattern (substring match)
        #[arg(long)]
        url_pattern: Option<String>,
        /// Time window in seconds (default: 60)
        #[arg(long, default_value = "60")]
        seconds: u32,
        /// Max number of requests to return
        #[arg(long)]
        max_count: Option<u32>,
        /// Include captured request headers (Authorization etc.). Sensitive
        /// values are redacted to a 12-char prefix unless --no-redact is set.
        #[arg(long = "include-headers")]
        include_headers: bool,
        /// With --include-headers: return sensitive header values in full
        #[arg(long = "no-redact")]
        no_redact: bool,
    },

    /// Add a network fault-injection rule: hang, delay, fail, or mock matching
    /// fetch/XHR requests. Rules persist across navigations and are cleared when
    /// the daemon stops. Same --pattern (+ --method) replaces the existing rule.
    Intercept {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// URL pattern: substring match, or glob with `*` (e.g. "*/portfolio*")
        #[arg(long)]
        pattern: String,
        /// hang | delay | fail | mock
        #[arg(long)]
        action: String,
        /// delay: wait duration (required). mock: response latency.
        /// hang: optional auto-error timeout (default: hang forever).
        #[arg(long)]
        ms: Option<u64>,
        /// mock: HTTP status code (default 200)
        #[arg(long)]
        status: Option<u16>,
        /// mock: response body (default empty)
        #[arg(long)]
        body: Option<String>,
        /// mock: Content-Type header (default application/json)
        #[arg(long = "content-type")]
        content_type: Option<String>,
        /// Only intercept this HTTP method (GET, POST, …). Default: all.
        #[arg(long)]
        method: Option<String>,
    },

    /// List active network fault-injection rules.
    #[command(name = "intercept-list")]
    InterceptList {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Clear network fault-injection rules (all, or only a matching --pattern).
    #[command(name = "intercept-clear")]
    InterceptClear {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Only remove rules with exactly this pattern (default: remove all)
        #[arg(long)]
        pattern: Option<String>,
    },

    /// Run a script at DOCUMENT START on every navigation, before any page script.
    /// This is the only way to observe what happens before the page's own code runs —
    /// `exec-js` needs a loaded document, so by the time it arrives the first API call
    /// and the first DOM insertion are already history.
    ///
    /// CHOOSING THE WORLD MATTERS. `isolated` (default) shares the DOM but CANNOT see
    /// page globals, so it cannot wrap `window.fetch` or anything the page created —
    /// such a script runs fine and observes nothing. Use `--world main` to pre-empt a
    /// page API. The isolated world is the same one `exec-js --world isolated` uses, so
    /// values the script leaves behind are readable from there.
    ///
    /// Takes effect from the NEXT navigation. Scripts live in daemon memory.
    #[command(name = "inject-script")]
    InjectScript {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Label for this script, unique per daemon. Used by inject-clear.
        #[arg(long)]
        name: String,
        /// JavaScript source to run at document start
        #[arg(long, conflicts_with = "file")]
        script: Option<String>,
        /// Read the script from a file instead of --script
        #[arg(long, conflicts_with = "script")]
        file: Option<std::path::PathBuf>,
        /// `isolated` (default) or `main`
        #[arg(long, default_value = "isolated")]
        world: String,
    },

    /// List registered document-start scripts and the world each runs in.
    #[command(name = "inject-list")]
    InjectList {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Unregister document-start scripts (all, or one by --name).
    /// On macOS an individual WKUserScript cannot be removed, so the response says so
    /// rather than implying a clean page.
    #[command(name = "inject-clear")]
    InjectClear {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Only remove the script with this name (default: remove all)
        #[arg(long)]
        name: Option<String>,
    },

    /// List all iframes on the page (with each frame's CDP frame_id on Windows —
    /// the stable handle `exec-js --frame` accepts; the listing index is also
    /// `--frame`'s index space)
    #[command(name = "get-frames")]
    GetFrames {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Get iframe HTML content (supports cross-origin via CDP)
    #[command(name = "get-frame-html")]
    GetFrameHtml {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// iframe CSS selector
        #[arg(long)]
        selector: Option<String>,
        /// iframe index (0-based)
        #[arg(long)]
        index: Option<usize>,
    },

    /// Preprocess captcha image for vision model
    Captcha {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Path to captcha image file
        #[arg(long)]
        input_file: String,
    },

    /// Dismiss modals, popups, and overlays
    #[command(name = "dismiss-modals")]
    DismissModals {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Max dismiss attempts (default: 3)
        #[arg(long, default_value = "3")]
        max_attempts: u32,
    },

    /// Check if an element is visible on the page (instant, no polling)
    #[command(name = "check-visible")]
    CheckVisible {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// CSS selector to check visibility
        #[arg(long)]
        selector: String,
    },

    /// Describe current page: takes an annotated screenshot with numbered interactive elements + returns element index as JSON
    #[command(name = "describe")]
    Describe {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// Output file path for annotated screenshot (auto-generated if omitted)
        #[arg(long)]
        output: Option<std::path::PathBuf>,
    },

    /// Click an element by CSS selector or visible text content
    #[command(name = "click-element")]
    ClickElement {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// CSS selector for the element to click
        #[arg(long)]
        selector: Option<String>,
        /// Visible text to find and click (XPath-based search)
        #[arg(long)]
        text: Option<String>,
        /// Click type: left, right, double (default: left)
        #[arg(long, default_value = "left")]
        click_type: String,
    },

    /// Upload a local file to an <input type="file"> element
    #[command(name = "upload-file")]
    UploadFile {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// CSS selector for the file input element
        #[arg(long)]
        selector: String,
        /// Path to the file to upload
        #[arg(long)]
        file: std::path::PathBuf,
    },

    /// Save a DOM image element to a local file (supports base64 data URIs and blob URLs).
    /// --index: Element index when multiple match (default: -1 = last). Negative counts from end.
    /// Example: --selector "img.result" --index -1 --output photo.png
    #[command(name = "save-image")]
    SaveImage {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// CSS selector for the img element
        #[arg(long)]
        selector: String,
        /// Output file path
        #[arg(long)]
        output: std::path::PathBuf,
        /// Element index when multiple match (default: -1 = last). Negative counts from end
        #[arg(long, default_value = "-1")]
        index: i32,
    },

    /// [DIAG] Capture WHERE the renderer is executing right now. Works on a HUNG renderer.
    /// macOS: external `sample` of the WKWebView WebContent process. Windows: out-of-band CDP JS stack.
    Pause {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Sampling window in ms (macOS, default 1200; rounded up to whole seconds, max 30000).
        #[arg(long)]
        duration_ms: Option<u64>,
        /// File path for the full report (auto temp file if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Windows: dump the HEAP too (MiniDumpWithFullMemory + full memory info + handle data),
        /// so page/JS strings and allocations are searchable in the .dmp. File size ≈ the target's
        /// working set. Without --only-pid this dumps only the largest WebView2 process.
        #[arg(long = "full-memory")]
        full_memory: bool,
        /// Windows: dump only this PID (see `candidate_processes` in a previous pause output).
        #[arg(long = "only-pid")]
        only_pid: Option<u32>,
        /// Windows: with --full-memory, dump every WebView2 process instead of just the largest.
        /// Warning: that is GBs of disk.
        #[arg(long = "all-processes")]
        all_processes: bool,
        /// Windows: warn (don't fail) when a dump exceeds this many bytes.
        #[arg(long = "max-bytes")]
        max_bytes: Option<u64>,
        /// Windows: resolve native stacks to function names (dbghelp). Threads blocked in a
        /// syscall are excluded and counted as `waiting_threads` — they'd otherwise dominate the
        /// result and say nothing. Classified by SYMBOL, so a thread inside
        /// NtAllocateVirtualMemory counts as work while NtWaitForSingleObject counts as a wait.
        #[arg(long)]
        symbolize: bool,
        /// Windows: with --symbolize, keep the blocked threads too.
        #[arg(long = "all-threads")]
        all_threads: bool,
        /// Windows: allow downloading PDBs from the Microsoft symbol server (needed for
        /// msedge-internal names like V8's allocator). Off by default — the first fetch of
        /// msedge.dll.pdb is ~630 MB. REQUIRES dbghelp.dll + symsrv.dll next to dalzzack.exe;
        /// they're installed automatically on first use and take effect on the next run, because
        /// the System32 dbghelp has no symbol-server support and is bound at process start.
        /// Without all that, unresolved frames come back as `module+0xRVA`.
        #[arg(long = "download-symbols")]
        download_symbols: bool,
        /// Windows: symbol cache directory (default <temp>/dalzzack-symcache).
        #[arg(long = "symbol-cache")]
        symbol_cache: Option<PathBuf>,
        /// Take N dumps of ONE process instead of one dump of every process — the way to tell
        /// "spinning here" from "happened to be here". Pairs with --symbolize for a histogram.
        #[arg(long, default_value = "1")]
        samples: u64,
        /// Delay between samples in ms (default 250).
        #[arg(long = "interval-ms", default_value = "250")]
        interval_ms: u64,
    },

    /// [DIAG] Dump the in-page console buffer with explicit hung-renderer detection.
    #[command(name = "console-dump")]
    ConsoleDump {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Filter by level: log, info, warn, error, debug.
        #[arg(long)]
        level: Option<String>,
        /// Max entries to return (default 500).
        #[arg(long, default_value = "500")]
        max: u32,
        /// Clear the buffer after reading.
        #[arg(long)]
        clear: bool,
    },

    /// [DIAG] List captured fetch/XHR requests, flagging ones still pending (a common hang trigger).
    #[command(name = "network-log")]
    NetworkLog {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Only requests newer than this many ms ago (default 60000).
        #[arg(long = "since-ms", alias = "since")]
        since_ms: Option<u64>,
        /// Substring filter on URL.
        #[arg(long)]
        url_pattern: Option<String>,
        /// Max requests to return.
        #[arg(long)]
        max: Option<u32>,
    },

    /// [DIAG] Read the Service Worker's own console output (the page console
    /// never shows it). Windows-only: needs CDP session attach.
    #[command(name = "sw-console")]
    SwConsole {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Filter by level: log, info, warn, error, debug.
        #[arg(long)]
        level: Option<String>,
        /// Maximum entries to return (default 100).
        #[arg(long, default_value = "100")]
        max: u32,
        /// Clear the buffer after reading.
        #[arg(long)]
        clear: bool,
    },

    /// [DIAG] Begin a CPU sampling capture (pair with profile-stop).
    #[command(name = "profile-start")]
    ProfileStart {
        #[arg(long, short, default_value = "")]
        id: String,
        /// macOS: max sampling duration before auto-stop (default 600, max 3600).
        #[arg(long)]
        max_seconds: Option<u64>,
    },

    /// [DIAG] End the CPU capture started by profile-start and return the report.
    #[command(name = "profile-stop")]
    ProfileStop {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Windows: path for the .cpuprofile JSON (auto temp file if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// [DIAG] Capture a memory report (macOS native memory; Windows JS heap usage).
    #[command(name = "heap-snapshot")]
    HeapSnapshot {
        #[arg(long, short, default_value = "")]
        id: String,
        /// File path for the report (auto temp file if omitted).
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// [DIAG] Make the renderer inspectable and return how to attach a debugger.
    /// macOS: enables Safari Web Inspector. Windows: opens the WebView2 DevTools window.
    #[command(name = "attach-devtools")]
    AttachDevtools {
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// [DIAG] Run JS in a world isolated from page globals.
    #[command(name = "exec-js-isolated")]
    ExecJsIsolated {
        #[arg(long, short, default_value = "")]
        id: String,
        /// JavaScript to evaluate (use 'return' to get a value).
        #[arg(long, alias = "code")]
        script: String,
        /// Execution timeout in ms (default 30000).
        #[arg(long, default_value = "30000")]
        timeout_ms: u32,
    },

    /// [DIAG] Poll a JS condition until it's truthy or timeout (hung-renderer aware).
    #[command(name = "wait-for-condition")]
    WaitForCondition {
        #[arg(long, short, default_value = "")]
        id: String,
        /// JS expression/body; a truthy return ends the wait (use 'return').
        #[arg(long, alias = "js")]
        script: String,
        /// Overall timeout in ms (default 30000).
        #[arg(long, default_value = "30000")]
        timeout_ms: u64,
        /// Poll interval in ms (default 250, min 50).
        #[arg(long, default_value = "250")]
        interval_ms: u64,
    },

    /// Print the JSON schema for every tool the daemon exposes.
    /// Use this so an LLM (or MCP client) can discover the available
    /// tool surface without hardcoding names. The whole list is 64 KB:
    /// use --names, then ask for one tool.
    Tools {
        /// Print one tool, such as `get_html` or `get-html`
        name: Option<String>,
        /// Print the tool names only
        #[arg(long, conflicts_with = "name")]
        names: bool,
    },

    /// Upgrade dalzzack in place via `cargo install`.
    /// Stops any running daemons first so they pick up the new code.
    /// Default source is the repository URL baked into the binary at build time.
    /// Needs cargo; without it, prints the download link of the prebuilt binary.
    Upgrade {
        /// Use `cargo install --path <PATH>` instead of `--git <URL>`.
        /// Handy when you're hacking on the repo locally — point this at the
        /// repo root or `src-tauri/`.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Override the git URL (defaults to the repository URL compiled into the binary).
        #[arg(long)]
        git: Option<String>,
        /// Git revision (branch / tag / sha) to install from. Only used with --git or the baked-in URL.
        #[arg(long)]
        rev: Option<String>,
        /// Don't try to stop running daemons — fail if any are alive.
        #[arg(long)]
        keep_daemons: bool,
        /// Skip the `--locked` flag passed to cargo install.
        #[arg(long)]
        no_locked: bool,
    },

    /// Run batch steps from a JSON file (self-contained: starts browser, runs, exits)
    Run {
        /// Instance ID (auto-generates if omitted)
        #[arg(long, short)]
        id: Option<String>,
        /// Path to JSON steps file
        #[arg(long)]
        steps: PathBuf,
    },

    /// Inject dalzzack usage docs into AI agent config files.
    /// claude-code → ~/.claude/CLAUDE.md (global, all projects)
    /// codex → ~/.codex/AGENTS.md or $CODEX_HOME/AGENTS.md (global, all projects)
    /// codex-project → ./AGENTS.md (current project; Codex reads only its first 32 KiB)
    /// all → claude-code + codex
    Install {
        /// Target agent: claude-code, codex, codex-project, or all
        #[arg(required_unless_present = "refresh")]
        target: Option<String>,
        /// Rewrite the guide only in global files that already have it.
        /// This creates no file.
        #[arg(long, conflicts_with = "target")]
        refresh: bool,
    },

    /// Capture a chrome-tracing-compatible timeline for `duration_ms`. Windows uses real
    /// CDP Tracing.* over the browser process (hang-resistant). macOS captures Performance
    /// API + paint timing via in-page injection (degraded, won't survive a fully wedged renderer).
    /// Output JSON opens in chrome://tracing or perfetto.
    Trace {
        #[arg(long, short, default_value = "")]
        id: String,
        #[arg(long = "duration-ms", default_value = "2000")]
        duration_ms: u64,
        #[arg(long)]
        output: Option<PathBuf>,
        /// CDP categories (Windows). Default covers V8 + blink + devtools.timeline.
        #[arg(long)]
        categories: Option<String>,
    },

    /// Pre-arm the V8 debugger so any subsequent script execution is captured.
    /// Daemon auto-resumes after each pause; the captured call frames are buffered
    /// for `debugger-snapshot`. Windows-only (CDP Debugger domain). macOS: not supported.
    #[command(name = "debugger-arm")]
    DebuggerArm {
        #[arg(long, short, default_value = "")]
        id: String,
        /// `first-script` (default) | `exceptions` | `url`.
        /// `exceptions` pauses V8 on every throw (CDP Debugger.setPauseOnExceptions) — the only
        /// way to see engine-thrown errors, which never pass through the page's Error constructors.
        #[arg(long, default_value = "first-script")]
        strategy: String,
        /// Regex pattern of script URL to break on (only with `--strategy url`).
        #[arg(long = "url-pattern")]
        url_pattern: Option<String>,
        /// With `--strategy exceptions`: only pause on UNCAUGHT exceptions (default: all throws).
        #[arg(long = "uncaught-only")]
        uncaught_only: bool,
        /// Keep at most N call frames per pause (default: all). Use on an exception storm.
        #[arg(long = "max-frames")]
        max_frames: Option<u64>,
        /// Record only every Nth pause (default 1 = all). Use on an exception storm.
        #[arg(long = "sample-every")]
        sample_every: Option<u64>,
    },

    /// Pull the call frames buffered since `debugger-arm`. Pass `--clear` to also empty the buffer.
    #[command(name = "debugger-snapshot")]
    DebuggerSnapshot {
        #[arg(long, short, default_value = "")]
        id: String,
        #[arg(long)]
        clear: bool,
    },

    /// Disable the V8 debugger and drop any buffered frames.
    #[command(name = "debugger-disarm")]
    DebuggerDisarm {
        #[arg(long, short, default_value = "")]
        id: String,
    },

    /// Dump the always-on background recording (network/console/exceptions) buffered
    /// daemon-side. Reads from daemon memory — does NOT hit the renderer — so it answers
    /// even when the page is wedged. Requires `start --record`.
    #[command(name = "dump-recording")]
    DumpRecording {
        #[arg(long, short, default_value = "")]
        id: String,
        /// Write the full snapshot as JSON to this path. If omitted, prints to stdout.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Filter: all (default), network, console, exceptions.
        #[arg(long)]
        filter: Option<String>,
        /// Drop events older than this epoch-ms cutoff.
        #[arg(long)]
        since_ms: Option<u64>,
        /// Empty the buffer after snapshotting.
        #[arg(long)]
        clear: bool,
    },

    /// [INTERNAL] Run as daemon process (used by 'start' command)
    #[command(hide = true)]
    Daemon {
        #[arg(long, short, default_value = "")]
        id: String,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        record: bool,
        #[arg(long = "watch-hang")]
        watch_hang: bool,
        #[arg(long = "auto-dump-dir")]
        auto_dump_dir: Option<String>,
        #[arg(long = "browser-arg", allow_hyphen_values = true)]
        browser_arg: Vec<String>,
        #[arg(long = "no-console-capture")]
        no_console_capture: bool,
        #[arg(long = "console-capture")]
        console_capture: bool,
        #[arg(long = "user-agent")]
        user_agent: Option<String>,
        #[arg(long = "raw-user-agent")]
        raw_user_agent: bool,
        #[arg(long = "enforce-csp")]
        enforce_csp: bool,
        #[arg(long = "private-profile")]
        private_profile: bool,
        #[arg(long, default_value = "webview")]
        engine: String,
        #[arg(long = "chrome-path")]
        chrome_path: Option<String>,
        #[arg(long = "console-buffer")]
        console_buffer: Option<usize>,
        #[arg(long = "ttl-secs")]
        ttl_secs: Option<u64>,
    },
}

/// Resolve an empty `--id` to the sole live instance, or die with a JSON
/// error **on stdout**. Runners routinely swallow stderr (`2>/dev/null`), and
/// a usage error that only reaches stderr turns into a silent no-op — that is
/// how a missing `-i` on `wait --ms 150000` once became a 0-second sleep and
/// produced a fake regression report.
fn resolve_instance_id(id: String) -> String {
    if !id.is_empty() {
        return id;
    }
    let live = dalzzack_lib::daemon::live_instance_ids();
    match live.len() {
        1 => {
            let sole = live.into_iter().next().unwrap();
            eprintln!("(no --id given — using the only running instance '{sole}')");
            sole
        }
        0 => {
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "NO_INSTANCE",
                    "message": "no --id given and no instance is running — start one with `dalzzack start --id <ID>`",
                })
            );
            std::process::exit(1);
        }
        _ => {
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "AMBIGUOUS_INSTANCE",
                    "message": "no --id given and more than one instance is running — pass -i explicitly",
                    "instances": live,
                })
            );
            std::process::exit(1);
        }
    }
}

/// `--engine` value, or a JSON usage error on stdout (same contract as clap's).
fn parse_engine_or_exit(raw: &str) -> dalzzack_lib::engine::Engine {
    match dalzzack_lib::engine::Engine::parse(raw) {
        Some(e) => e,
        None => {
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "USAGE",
                    "message": format!("unknown --engine '{raw}' — use webview or chrome"),
                })
            );
            std::process::exit(2);
        }
    }
}

/// Print one JSON document through the user's `--field` / `--jq` filter and
/// return the exit code.
///
/// With `--field`, a `status: "failed"` result is printed WHOLE and exits 1:
/// `--field result` on a failure would otherwise print `null` with exit 0,
/// which reads as "the script returned null". `--jq` sees everything — it is
/// the tool for handling failures yourself.
/// Print a result. `tool` and `args` say which short form applies.
fn emit(
    v: &serde_json::Value,
    filter: &dalzzack_lib::cli::output::OutputFilter,
    tool: &str,
    args: &serde_json::Value,
) -> i32 {
    use dalzzack_lib::cli::{
        brief::brief,
        output::{limited, show},
    };
    if !filter.is_active() {
        let text = if filter.full {
            show(v, filter)
        } else {
            show(&brief(tool, args, v), filter)
        };
        println!("{}", limited(text, v, tool, args, filter));
        return 0;
    }
    let failed = v.get("status").and_then(|s| s.as_str()) == Some("failed");
    if !filter.fields.is_empty() && failed {
        println!("{}", dalzzack_lib::cli::output::show(v, filter));
        eprintln!("(result is a failure — printed whole instead of --field)");
        return 1;
    }
    match dalzzack_lib::cli::output::render(v, filter) {
        Ok(text) => {
            println!("{}", limited(text, v, tool, args, filter));
            // On stderr, so stdout stays exactly what was asked for: a key
            // the output does not have, but a differently spelled one does
            // (`httpOnly` vs `http_only`), silently reads as null otherwise.
            let programs = filter.jq.iter().cloned().chain(
                filter
                    .fields
                    .iter()
                    .map(|f| dalzzack_lib::cli::output::field_to_jq(f)),
            );
            for program in programs {
                for hint in dalzzack_lib::cli::output::missing_key_hints(&program, v) {
                    eprintln!("[dalzzack] note: {hint}");
                }
            }
            0
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({ "status": "failed", "error": "FILTER_ERROR", "message": e })
            );
            2
        }
    }
}

/// `Ok` → stdout through the filter; `Err` → stderr, exit 1 (the long-standing
/// contract of start/stop/kill failures).
fn emit_result(
    r: Result<serde_json::Value, serde_json::Value>,
    filter: &dalzzack_lib::cli::output::OutputFilter,
) -> ! {
    match r {
        Ok(v) => std::process::exit(emit(&v, filter, "", &serde_json::Value::Null)),
        Err(e) => {
            // Unfiltered, failures keep their long-standing place on stderr.
            // A filter promises "the whole failure, on stdout, exit 1" — a
            // runner that asked for one field reads stdout only.
            if filter.is_active() {
                println!("{}", dalzzack_lib::cli::output::show(&e, filter));
            } else {
                eprintln!("{e}");
            }
            std::process::exit(1);
        }
    }
}

fn main() {
    let full_flag;
    // Usage errors must reach stdout too: clap prints them to stderr and a
    // runner with `2>/dev/null` sees a successful-looking empty result.
    let (cli, filter) = match Args::try_parse() {
        // clap's `conflicts_with` is not enforced when the two global flags
        // sit on opposite sides of the subcommand (`--field x tools --jq .`),
        // and one would then be silently ignored.
        Ok(a) if !a.field.is_empty() && a.jq.is_some() => {
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "USAGE",
                    "message": "--field and --jq cannot be used together",
                })
            );
            std::process::exit(2);
        }
        Ok(a) => {
            let terminal = std::io::stdout().is_terminal();
            let full_env = std::env::var_os("DALZZACK_OUTPUT").is_some_and(|v| v == "full");
            let full = a.full || terminal || full_env;
            full_flag = a.full;
            (
                a.cmd,
                dalzzack_lib::cli::output::OutputFilter {
                    fields: a.field,
                    jq: a.jq,
                    raw: a.raw,
                    pretty: a.pretty || terminal,
                    full,
                    max_output: match a.max_output {
                        Some(0) => None,
                        Some(limit) => Some(limit),
                        None if full => None,
                        None => Some(dalzzack_lib::cli::output::DEFAULT_MAX_OUTPUT),
                    },
                },
            )
        }
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                e.exit();
            }
            let first_line = e
                .render()
                .to_string()
                .lines()
                .next()
                .unwrap_or("invalid usage")
                .to_string();
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "USAGE",
                    "message": first_line,
                })
            );
            // Full clap rendering still goes to stderr for humans.
            e.exit();
        }
    };

    // A previous `upgrade` may have left its own (then-running) image behind
    // as `dalzzack.exe.old`; this is the first moment it can be deleted.
    sweep_moved_aside();

    // These print progress or several documents; a filter has nothing single
    // to apply to, and silently ignoring it would be worse than saying so.
    if filter.is_active()
        && matches!(
            cli,
            Cli::Run { .. } | Cli::Install { .. } | Cli::Upgrade { .. } | Cli::Daemon { .. }
        )
    {
        println!(
            "{}",
            serde_json::json!({
                "status": "failed",
                "error": "USAGE",
                "message": "--field/--jq apply to commands that print one JSON document; run, \
                            install, upgrade and daemon print several",
            })
        );
        std::process::exit(2);
    }

    match cli {
        Cli::Start {
            id,
            width,
            height,
            record,
            watch_hang,
            auto_dump_dir,
            browser_arg,
            no_console_capture,
            console_capture,
            user_agent,
            raw_user_agent,
            enforce_csp,
            private_profile,
            engine,
            chrome_path,
            ttl,
            console_buffer,
        } => {
            let engine_given = engine.is_some();
            let engine = parse_engine_or_exit(engine.as_deref().unwrap_or("webview"));
            let ttl_secs = match ttl.as_deref().map(dalzzack_lib::daemon::parse_ttl) {
                Some(Err(message)) => {
                    println!(
                        "{}",
                        serde_json::json!({ "status": "failed", "error": "USAGE", "message": message })
                    );
                    std::process::exit(2);
                }
                Some(Ok(s)) => Some(s),
                None => None,
            };
            let instance_id = id.unwrap_or_else(|| std::process::id().to_string());
            emit_result(
                dalzzack_lib::daemon::start_daemon(
                    &instance_id,
                    dalzzack_lib::daemon::DaemonOptions {
                        width,
                        height,
                        record,
                        watch_hang,
                        auto_dump_dir,
                        browser_args: browser_arg,
                        no_console_capture,
                        console_capture,
                        user_agent,
                        raw_user_agent,
                        enforce_csp,
                        private_profile,
                        engine,
                        engine_given,
                        chrome_path,
                        console_buffer,
                        ttl_secs,
                    },
                ),
                &filter,
            );
        }
        Cli::Stop { id } => {
            emit_result(
                dalzzack_lib::daemon::stop_daemon(&resolve_instance_id(id)),
                &filter,
            );
        }
        Cli::Kill { id } => {
            emit_result(
                dalzzack_lib::daemon::kill_daemon(&resolve_instance_id(id)),
                &filter,
            );
        }
        Cli::List => {
            let instances = dalzzack_lib::daemon::list_instances_json();
            std::process::exit(emit(&instances, &filter, "list", &serde_json::Value::Null));
        }
        Cli::Daemon {
            id,
            width,
            height,
            record,
            watch_hang,
            auto_dump_dir,
            browser_arg,
            no_console_capture,
            console_capture,
            user_agent,
            raw_user_agent,
            enforce_csp,
            private_profile,
            engine,
            chrome_path,
            console_buffer,
            ttl_secs,
        } => {
            // Internal: run as daemon process (Tauri app + socket server).
            // The empty-id default exists for tool commands; a daemon must
            // always be told who it is (the spawner always passes --id).
            if id.is_empty() {
                eprintln!("daemon requires an explicit --id");
                std::process::exit(2);
            }
            let engine = parse_engine_or_exit(&engine);
            dalzzack_lib::daemon::run_daemon(
                id,
                dalzzack_lib::daemon::DaemonOptions {
                    width,
                    height,
                    record,
                    watch_hang,
                    auto_dump_dir,
                    browser_args: browser_arg,
                    no_console_capture,
                    console_capture,
                    user_agent,
                    raw_user_agent,
                    enforce_csp,
                    private_profile,
                    engine,
                    engine_given: true,
                    chrome_path,
                    console_buffer,
                    ttl_secs,
                },
            );
        }
        Cli::Run { id, steps } => {
            let instance_id = id.unwrap_or_else(|| format!("run-{}", std::process::id()));
            dalzzack_lib::cli::run_steps(&instance_id, &steps);
        }
        Cli::Install { target, refresh } => match target {
            Some(target) if !refresh => install_docs(&target),
            _ => refresh_docs(),
        },
        Cli::Tools { name, names } => {
            let defs = dalzzack_lib::tools::definitions::get_tool_definitions();
            let v = tools_output(&defs, name.as_deref(), names);
            std::process::exit(emit(&v, &filter, "", &serde_json::Value::Null));
        }
        Cli::Upgrade {
            path,
            git,
            rev,
            keep_daemons,
            no_locked,
        } => {
            upgrade(path, git, rev, keep_daemons, no_locked);
        }
        // All tool commands → send to daemon via socket
        cmd => {
            let (id, tool_name, mut args) = cli_to_tool_call(cmd);
            if full_flag && tool_name == "get_frame_list" {
                args["full"] = serde_json::json!(true);
            }
            let id = resolve_instance_id(id);
            let pathconv = shell_pathconv_suspects(&args);
            let result = dalzzack_lib::daemon::send_command(&id, &tool_name, args.clone());
            match result {
                Ok(mut output) => {
                    // Git-Bash (MSYS2) rewrites any argv starting with `/` into a
                    // Windows path, so `--to "/zp/p/"` arrives as
                    // `C:/Program Files/Git/zp/p/` and matches nothing. The tool
                    // cannot know, but the shape is unmistakable — say so next
                    // to the result instead of letting it read as a broken matcher.
                    if !pathconv.is_empty() {
                        if let Some(obj) = output.as_object_mut() {
                            obj.insert(
                                "hint_shell_pathconv".into(),
                                serde_json::json!(format!(
                                    "{} look like a value the shell rewrote into a Windows path \
                                     (Git-Bash/MSYS2 converts leading-slash arguments). Re-run with \
                                     MSYS_NO_PATHCONV=1, or prefix the value with a scheme/host so \
                                     it does not start with '/'.",
                                    pathconv.join(", ")
                                )),
                            );
                        }
                    }
                    std::process::exit(emit(&output, &filter, &tool_name, &args));
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        serde_json::json!({"status": "failed", "error": e.to_string()})
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}

/// `tools`: all schemas, the names only, or one tool. A CLI spelling with
/// hyphens finds the same tool.
fn tools_output(
    defs: &[dalzzack_lib::tools::definitions::ToolDefinition],
    name: Option<&str>,
    names_only: bool,
) -> serde_json::Value {
    if names_only {
        return serde_json::json!(defs.iter().map(|d| &d.name).collect::<Vec<_>>());
    }
    let Some(name) = name else {
        return serde_json::json!(defs);
    };
    let wanted = name.replace('-', "_");
    match defs.iter().find(|d| d.name == wanted) {
        Some(def) => serde_json::json!(def),
        None => serde_json::json!({
            "status": "failed",
            "error": "UNKNOWN_TOOL",
            "message": format!("No tool is named {name}. `dalzzack tools --names` lists them."),
        }),
    }
}

/// Convert a CLI tool subcommand into (instance_id, tool_name, args_json)
fn cli_to_tool_call(cmd: Cli) -> (String, String, serde_json::Value) {
    match cmd {
        Cli::Navigate {
            id,
            url,
            timeout_ms,
        } => (
            id,
            "navigate".into(),
            serde_json::json!({ "url": url, "timeout_ms": timeout_ms }),
        ),
        Cli::Screenshot {
            id,
            output,
            selector,
        } => (
            id,
            "screenshot".into(),
            serde_json::json!({
                "output_file": output.map(|p| p.to_string_lossy().to_string()),
                "selector": selector,
            }),
        ),
        Cli::Click {
            id,
            x,
            y,
            selector,
            text,
            click_type,
        } => {
            if selector.is_some() || text.is_some() {
                // Delegate to click_element when selector/text provided
                (
                    id,
                    "click_element".into(),
                    serde_json::json!({
                        "selector": selector,
                        "text": text,
                        "click_type": click_type,
                    }),
                )
            } else if let (Some(x), Some(y)) = (x, y) {
                (
                    id,
                    "click".into(),
                    serde_json::json!({ "x": x, "y": y, "click_type": click_type }),
                )
            } else {
                eprintln!(
                    "{}",
                    serde_json::json!({"status": "failed", "error": "Provide --x + --y, or --selector, or --text"})
                );
                std::process::exit(1);
            }
        }
        Cli::Scroll {
            id,
            direction,
            amount,
            selector,
            to,
        } => (
            id,
            "scroll".into(),
            serde_json::json!({ "direction": direction, "amount": amount, "selector": selector, "to": to }),
        ),
        Cli::ExecuteJs {
            id,
            script,
            file,
            timeout_ms,
            await_promise,
            frame,
            world,
            target,
        } => {
            // clap guarantees exactly one of --script / --file.
            let script = match (script, file) {
                (Some(s), _) => s,
                (None, Some(path)) => match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "status": "failed",
                                "error": "FILE_READ_ERROR",
                                "message": format!("cannot read --file {}: {e}", path.display()),
                            })
                        );
                        std::process::exit(1);
                    }
                },
                (None, None) => unreachable!("clap enforces script|file"),
            };
            (
                id,
                "execute_js".into(),
                serde_json::json!({
                    "script": script,
                    "timeout_ms": timeout_ms,
                    "await_promise": await_promise,
                    "frame": frame,
                    "world": world,
                    "target": target,
                }),
            )
        }
        Cli::FillForm {
            id,
            fields,
            selector,
            value,
            submit,
        } => {
            let fields_val: serde_json::Value = if let (Some(sel), Some(val)) = (selector, value) {
                // Shorthand: --selector + --value → single-element fields array
                serde_json::json!([{"selector": sel, "value": val}])
            } else if let Some(f) = fields {
                serde_json::from_str(&f).unwrap_or_else(|_| serde_json::json!([]))
            } else {
                eprintln!(
                    "{}",
                    serde_json::json!({"status": "failed", "error": "Provide --selector + --value, or --fields JSON"})
                );
                std::process::exit(1);
            };
            (
                id,
                "fill_form".into(),
                serde_json::json!({ "fields": fields_val, "submit_selector": submit }),
            )
        }
        Cli::GetHtml {
            id,
            selector,
            no_strip_noise,
            strip_noise,
            max_length,
        } => (
            id,
            "get_html".into(),
            serde_json::json!({
                "selector": selector,
                "strip_noise": strip_noise || !no_strip_noise,
                "max_length": max_length,
            }),
        ),
        Cli::GetUrl { id } => (id, "get_current_url".into(), serde_json::json!({})),
        Cli::WaitFor {
            id,
            selector,
            state,
            text,
            timeout_ms,
        } => (
            id,
            "wait_for_selector".into(),
            serde_json::json!({ "selector": selector, "timeout_ms": timeout_ms, "state": state, "text": text }),
        ),
        Cli::Wait { id, ms } => (id, "wait".into(), serde_json::json!({ "ms": ms })),
        Cli::WaitNavigation {
            id,
            timeout_ms,
            to,
            interval_ms,
        } => (
            id,
            "wait_navigation".into(),
            serde_json::json!({
                "timeout_ms": timeout_ms,
                "to": to,
                "interval_ms": interval_ms,
            }),
        ),
        Cli::KeyboardInput {
            id,
            text,
            key,
            selector,
            modifiers,
            paste,
        } => (
            id,
            "keyboard_input".into(),
            serde_json::json!({
                "text": text,
                "key": key,
                "selector": selector,
                "modifiers": modifiers,
                "paste": paste,
            }),
        ),
        Cli::ElementAt { id, x, y } => (
            id,
            "element_at".into(),
            serde_json::json!({ "x": x, "y": y }),
        ),
        Cli::GetRequests {
            id,
            url_pattern,
            seconds,
            max_count,
            include_headers,
            no_redact,
        } => (
            id,
            "get_captured_requests".into(),
            serde_json::json!({
                "seconds": seconds,
                "url_pattern": url_pattern,
                "max_count": max_count,
                "include_headers": include_headers,
                "no_redact": no_redact,
            }),
        ),
        Cli::Intercept {
            id,
            pattern,
            action,
            ms,
            status,
            body,
            content_type,
            method,
        } => (
            id,
            "intercept".into(),
            serde_json::json!({
                "pattern": pattern,
                "action": action,
                "ms": ms,
                "status": status,
                "body": body,
                "content_type": content_type,
                "method": method,
            }),
        ),
        Cli::InterceptList { id } => (id, "intercept_list".into(), serde_json::json!({})),
        Cli::InterceptClear { id, pattern } => (
            id,
            "intercept_clear".into(),
            serde_json::json!({ "pattern": pattern }),
        ),
        Cli::InjectScript {
            id,
            name,
            script,
            file,
            world,
        } => {
            // Reading the file here (rather than in the daemon) keeps the path
            // relative to the user's shell, and surfaces a missing file as a
            // plain error instead of an empty script that injects successfully
            // and does nothing.
            let source = match (script, file) {
                (Some(s), _) => s,
                (None, Some(p)) => match std::fs::read_to_string(&p) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Failed to read --file {}: {e}", p.display());
                        std::process::exit(1);
                    }
                },
                (None, None) => {
                    eprintln!("Either --script or --file is required");
                    std::process::exit(1);
                }
            };
            (
                id,
                "inject_script".into(),
                serde_json::json!({ "name": name, "script": source, "world": world }),
            )
        }
        Cli::InjectList { id } => (id, "inject_list".into(), serde_json::json!({})),
        Cli::InjectClear { id, name } => (
            id,
            "inject_clear".into(),
            serde_json::json!({ "name": name }),
        ),
        Cli::GetFrames { id } => (id, "get_frame_list".into(), serde_json::json!({})),
        Cli::GetFrameHtml {
            id,
            selector,
            index,
        } => (
            id,
            "get_frame_html".into(),
            serde_json::json!({ "selector": selector, "index": index }),
        ),
        Cli::Captcha { id, input_file } => (
            id,
            "preprocess_captcha".into(),
            serde_json::json!({ "input_file": input_file }),
        ),
        Cli::DismissModals { id, max_attempts } => (
            id,
            "auto_dismiss_modals".into(),
            serde_json::json!({ "max_attempts": max_attempts }),
        ),
        Cli::CheckVisible { id, selector } => (
            id,
            "check_visible".into(),
            serde_json::json!({ "selector": selector }),
        ),
        Cli::Describe { id, output } => (
            id,
            "describe".into(),
            serde_json::json!({
                "output": output.map(|p| p.to_string_lossy().to_string()),
            }),
        ),
        Cli::ClickElement {
            id,
            selector,
            text,
            click_type,
        } => (
            id,
            "click_element".into(),
            serde_json::json!({
                "selector": selector,
                "text": text,
                "click_type": click_type,
            }),
        ),
        Cli::UploadFile { id, selector, file } => (
            id,
            "upload_file".into(),
            serde_json::json!({
                "selector": selector,
                "file_path": file.to_string_lossy().to_string(),
            }),
        ),
        Cli::SaveImage {
            id,
            selector,
            output,
            index,
        } => (
            id,
            "save_image".into(),
            serde_json::json!({
                "selector": selector,
                "output_path": output.to_string_lossy().to_string(),
                "index": index,
            }),
        ),
        Cli::Resize { id, width, height } => (
            id,
            "resize_window".into(),
            serde_json::json!({ "width": width, "height": height }),
        ),
        Cli::Back { id } => (id, "back".into(), serde_json::json!({})),
        Cli::Forward { id } => (id, "forward".into(), serde_json::json!({})),
        Cli::Reload { id, hard } => (id, "reload".into(), serde_json::json!({ "hard": hard })),
        Cli::ClearSiteData { id, origin, types } => (
            id,
            "clear_site_data".into(),
            serde_json::json!({ "origin": origin, "types": types }),
        ),
        Cli::Hover { id, selector, x, y } => (
            id,
            "hover".into(),
            serde_json::json!({ "selector": selector, "x": x, "y": y }),
        ),
        Cli::ConsoleLogs {
            id,
            level,
            source,
            frame,
            text,
            max,
            clear,
        } => (
            id,
            "console_logs".into(),
            serde_json::json!({
                "level": level,
                "source": source,
                "frame": frame,
                "text": text,
                "max": max,
                "clear": clear,
            }),
        ),
        Cli::ResponseHeaders {
            id,
            url_pattern,
            resource_type,
            max,
            no_redact,
        } => (
            id,
            "get_response_headers".into(),
            serde_json::json!({
                "url_pattern": url_pattern,
                "resource_type": resource_type,
                "max": max,
                "no_redact": no_redact,
            }),
        ),
        Cli::NavLog {
            id,
            since_ms,
            url_pattern,
            frames,
            max,
            headers,
            no_redact,
        } => (
            id,
            "navigation_log".into(),
            serde_json::json!({
                "since_ms": since_ms,
                "url_pattern": url_pattern,
                "include_frames": frames,
                "max": max,
                "headers": headers,
                "no_redact": no_redact,
            }),
        ),
        Cli::FailedRequests {
            id,
            since_ms,
            url_pattern,
            resource_type,
            no_http_errors,
            include_canceled,
            max,
            headers,
            no_redact,
        } => (
            id,
            "failed_requests".into(),
            serde_json::json!({
                "since_ms": since_ms,
                "url_pattern": url_pattern,
                "resource_type": resource_type,
                "no_http_errors": no_http_errors,
                "include_canceled": include_canceled,
                "max": max,
                "headers": headers,
                "no_redact": no_redact,
            }),
        ),
        Cli::Record { action, id, clear } => (
            id,
            "record".into(),
            serde_json::json!({ "action": action, "clear": clear }),
        ),
        Cli::Cookies {
            action,
            id,
            url,
            domain,
            all,
            name,
            no_redact,
            force_shared,
            dry_run,
        } => (
            id,
            "cookies".into(),
            serde_json::json!({
                "action": action,
                "url": url,
                "domain": domain,
                "all": all,
                "name": name,
                "no_redact": no_redact,
                "force_shared": force_shared,
                "dry_run": dry_run,
            }),
        ),
        Cli::Reset {
            id,
            keep_browsing_data,
            keep_rules,
            force_shared,
            dry_run,
            url,
        } => (
            id,
            "reset".into(),
            serde_json::json!({
                "keep_browsing_data": keep_browsing_data,
                "keep_rules": keep_rules,
                "force_shared": force_shared,
                "dry_run": dry_run,
                "url": url,
            }),
        ),
        Cli::Pause {
            id,
            duration_ms,
            output,
            full_memory,
            only_pid,
            all_processes,
            max_bytes,
            symbolize,
            all_threads,
            download_symbols,
            symbol_cache,
            samples,
            interval_ms,
        } => (
            id,
            "diag_pause".into(),
            serde_json::json!({
                "duration_ms": duration_ms,
                "output": output.map(|p| p.to_string_lossy().to_string()),
                "full_memory": full_memory,
                "only_pid": only_pid,
                "all_processes": all_processes,
                "max_bytes": max_bytes,
                "symbolize": symbolize,
                "all_threads": all_threads,
                "download_symbols": download_symbols,
                "symbol_cache": symbol_cache.map(|p| p.to_string_lossy().to_string()),
                "samples": samples,
                "interval_ms": interval_ms,
            }),
        ),
        Cli::ConsoleDump {
            id,
            level,
            max,
            clear,
        } => (
            id,
            "diag_console_dump".into(),
            serde_json::json!({ "level": level, "max": max, "clear": clear }),
        ),
        Cli::NetworkLog {
            id,
            since_ms,
            url_pattern,
            max,
        } => (
            id,
            "diag_network_log".into(),
            serde_json::json!({ "since_ms": since_ms, "url_pattern": url_pattern, "max": max }),
        ),
        Cli::SwConsole {
            id,
            level,
            max,
            clear,
        } => (
            id,
            "diag_sw_console".into(),
            serde_json::json!({ "level": level, "max": max, "clear": clear }),
        ),
        Cli::ProfileStart { id, max_seconds } => (
            id,
            "diag_profile_start".into(),
            serde_json::json!({ "max_seconds": max_seconds }),
        ),
        Cli::ProfileStop { id, output } => (
            id,
            "diag_profile_stop".into(),
            serde_json::json!({ "output": output.map(|p| p.to_string_lossy().to_string()) }),
        ),
        Cli::HeapSnapshot { id, output } => (
            id,
            "diag_heap_snapshot".into(),
            serde_json::json!({ "output": output.map(|p| p.to_string_lossy().to_string()) }),
        ),
        Cli::AttachDevtools { id } => (id, "diag_attach_devtools".into(), serde_json::json!({})),
        Cli::ExecJsIsolated {
            id,
            script,
            timeout_ms,
        } => (
            id,
            "diag_exec_js_isolated".into(),
            serde_json::json!({ "script": script, "timeout_ms": timeout_ms }),
        ),
        Cli::WaitForCondition {
            id,
            script,
            timeout_ms,
            interval_ms,
        } => (
            id,
            "diag_wait_for_condition".into(),
            serde_json::json!({ "script": script, "timeout_ms": timeout_ms, "interval_ms": interval_ms }),
        ),
        Cli::DumpRecording {
            id,
            output,
            filter,
            since_ms,
            clear,
        } => (
            id,
            "dump_recording".into(),
            serde_json::json!({
                "output": output.map(|p| p.to_string_lossy().to_string()),
                "filter": filter,
                "since_ms": since_ms,
                "clear": clear,
            }),
        ),
        Cli::Trace {
            id,
            duration_ms,
            output,
            categories,
        } => (
            id,
            "diag_trace".into(),
            serde_json::json!({
                "duration_ms": duration_ms,
                "output": output.map(|p| p.to_string_lossy().to_string()),
                "categories": categories,
            }),
        ),
        Cli::DebuggerArm {
            id,
            strategy,
            url_pattern,
            uncaught_only,
            max_frames,
            sample_every,
        } => (
            id,
            "diag_debugger_arm".into(),
            serde_json::json!({
                "strategy": strategy,
                "url_pattern": url_pattern,
                "uncaught_only": uncaught_only,
                "max_frames": max_frames,
                "sample_every": sample_every,
            }),
        ),
        Cli::DebuggerSnapshot { id, clear } => (
            id,
            "diag_debugger_snapshot".into(),
            serde_json::json!({ "clear": clear }),
        ),
        Cli::DebuggerDisarm { id } => (id, "diag_debugger_disarm".into(), serde_json::json!({})),
        // These are handled in main() directly, not here
        Cli::Start { .. }
        | Cli::Stop { .. }
        | Cli::Kill { .. }
        | Cli::List
        | Cli::Daemon { .. }
        | Cli::Run { .. }
        | Cli::Install { .. }
        | Cli::Tools { .. }
        | Cli::Upgrade { .. } => {
            unreachable!("handled in main()")
        }
    }
}

const DALZZACK_DOCS: &str = r##"<!-- dalzzack-begin -->
## Browser Automation (dalzzack)

`dalzzack` is a browser automation CLI. It spawns a real browser window and controls it via commands.

### Workflow

```bash
dalzzack start --id b --width 1920 --height 1080                # FIRST TIME ONLY: start at this size
dalzzack list                                                   # check what's already running
dalzzack resize --id b --width 800 --height 600                 # ALREADY RUNNING: change size at runtime
dalzzack navigate -i b --url "https://example.com"              # open page
dalzzack describe -i b                                          # annotated screenshot + element list
dalzzack hover -i b --selector ".menu-trigger"                  # reveal a hover-only dropdown (macOS native :hover)
dalzzack click -i b --selector "button.submit"                  # click by CSS selector
dalzzack click -i b --text "Sign In"                            # click by visible text
dalzzack fill-form -i b --selector "#email" --value "test"      # fill single field
dalzzack wait-for -i b --selector ".result" --state added       # wait for new content
dalzzack console-logs -i b --level error                        # check for JS errors when something misbehaves
dalzzack back -i b                                              # history.back() with URL poll
dalzzack scroll -i b --to bottom --selector ".container"        # scroll to bottom
dalzzack save-image -i b --selector "img" --index -1 --output result.png  # save last image
dalzzack screenshot -i b                                        # quick visual check
dalzzack stop --id b                                            # graceful stop (use `kill --id b` if hung)
```

### Did the page really load? What failed? (always on — no `--record`)

```bash
dalzzack navigate -i b --url "https://site.example" --field document  # main document: status, every hop, challenge
dalzzack nav-log -i b                                           # every document chain: 302s, 403 cf-mitigated, reloads
dalzzack failed-requests -i b                                   # failed script/image/fetch/… WITH the URL
dalzzack cookies -i b                                           # cookie jar incl. HttpOnly + parent domains
dalzzack start --id c --engine chrome                           # a REAL Chrome (what Chrome sees, not WebView2)
dalzzack reset -i c                                             # first-visit state, same id (private/chrome profiles)
```

### When something hangs

If a tool times out or `navigate` never returns, the renderer is wedged. Diagnose from the outside instead of retrying:

```bash
dalzzack pause -i b                                             # WHERE is the renderer stuck right now?
dalzzack network-log -i b --since-ms 30000                      # any requests still pending?
dalzzack console-dump -i b --level error                        # any uncaught errors before the freeze?
dalzzack dump-recording -i b --filter network                   # (if started with --record) full event tape, hang-resistant
dalzzack kill --id b                                            # last resort: force-terminate the daemon
```

For investigations where you'll likely need to look at network/console events AFTER a freeze
(e.g. "which request stalled?"), start the daemon with `--record` (or `record on` on a live one —
events from before that moment are not on the tape). That subscribes to
browser-process events up front and buffers them in daemon memory — `dump-recording` then reads
the buffer directly, so it works even when every JS-/CDP-based tool would time out.

```bash
dalzzack start --id b --record                                  # arm capture BEFORE navigation
dalzzack navigate -i b --url "https://flaky-spa.example"
# … page freezes …
dalzzack dump-recording -i b --output /tmp/tape.json            # daemon-memory read, hang-resistant
```

### Window Sizing — when to use `start --width` vs `resize`

| Situation | Command |
|-----------|---------|
| First launch, want a non-default size | `start --id b --width 1920 --height 1080` |
| Daemon already running, need different size | `resize --id b --width 800 --height 600` |
| Want viewport size for responsive testing mid-session | `resize --id b --width 375 --height 812` |
| Re-run `start` on an existing id | **Does NOT resize** — returns `already_running`. Use `resize`. |

`start --width/--height` is **initial size only** and is ignored on subsequent `start` calls for the same id (the daemon is already up). To change a live window, always use `resize`. Run `dalzzack list` first if you're unsure whether an id is alive.

### All Commands

**Instance:**
- `start [--id ID] [--width PX] [--height PX] [--record] [--watch-hang] [--auto-dump-dir DIR] [--browser-arg FLAG] [--no-console-capture] [--console-capture] [--user-agent UA] [--raw-user-agent] [--enforce-csp] [--private-profile] [--engine webview|chrome] [--chrome-path PATH]` — start browser (default 1200x800; window is resizable). If the id already has an unresponsive daemon, auto-kills and respawns.
  - `--record` arms always-on background event capture (network/console/exceptions) so `dump-recording` works even when the renderer is wedged.
  - `--no-console-capture` collects no console output at all — no CDP subscription, no in-page hook. Rarely needed on Windows, where capture is already invisible to the page (see `console-logs`). `console-logs` then reports `console_transport: "disabled"` instead of an empty buffer.
  - `--console-capture` (**macOS only**) turns console capture ON, accepting that the page can see it. macOS has no invisible transport: WebKit's system-console setting makes WebKit stringify every logged argument twice, and `disable-devtool` counts those `toString` calls — two instead of one reads as "DevTools is open" and protected sites redirect away. So macOS captures nothing by default and `console-logs` reports `unavailable` with the reason. This flag installs the in-page hook instead, reported as `in_page_fallback`.
  - `--user-agent UA` overrides the User-Agent. Unlike `--browser-arg "--user-agent=…"` this needs no private user data folder, so cookies/logins stay shared. **The engine generates its own `Sec-CH-UA` headers and you cannot override those** — a UA whose major version disagrees with the engine is a *stronger* bot signal than the default. On Windows, WebView2 already sends Edge branding at the live runtime version, so the default is left alone; passing this flag also injects a matching `navigator.userAgentData` so the three agree.
  - `--enforce-csp` keeps the page's Content Security Policy **enforced**. dalzzack disables it by default (CDP `Page.setBypassCSP`) so cross-origin iframes can be inspected — which also means **nothing is ever blocked, so no violation can ever be reported** and `console-logs --source csp` stays empty regardless of the policy. Use this whenever the question *is* the policy; `list` reports `csp_bypassed`.
  - `--raw-user-agent` keeps the engine's own UA even where the default is a giveaway. macOS only in practice: the WKWebView default ends at `(KHTML, like Gecko)` with no browser product token — a plain embedded-browser signature — and is completed with the installed Safari's version by default.
  - `--watch-hang` runs a daemon-side watchdog (500 ms ping × 5 timeouts ⇒ "hang"). On hang, auto-dumps the recording buffer + a `hang.json` marker to `--auto-dump-dir` (default `<temp>/dalzzack-hangs`). Lets you walk away — post-mortem data is captured automatically.
  - `--browser-arg FLAG` (repeatable, Windows) passes a Chromium flag to the WebView2 browser process, e.g. `--browser-arg "--enable-heap-profiling" --browser-arg "--js-flags=--stack-trace-limit=0"`. WebView2 reads these only at environment creation, so they can't be changed on a live daemon. Such an instance gets its **own WebView2 user data folder** (so it does not share cookies/logins with other ids): WebView2 permits only one set of browser arguments per folder, and a second environment with different ones fails outright.
  - `--private-profile` gives the instance its **own browser profile** (cookies, storage, cache): empty at start, deleted by `stop`. Plain instances share ONE WebView2 profile — a login in one carries to the next, and `cookies clear` / `reset` in one would hit all of them — and every plain daemon started later — so on the shared profile those refuse with `SHARED_PROFILE` **even when no other daemon is running** (the profile outlives this one) unless `--force-shared`. `--dry-run` shows what they would delete.
  - `--ttl 10m` (`90s`, `2h`, `1h30m`, bare seconds): the daemon stops itself through the same path as `stop` (Chrome and a private-profile folder go with it); `start`/`list` report `ttl_s`, `expires_at_ms`. For short-lived comparison instances.
  - `--console-buffer N`: the console ring size (default 500, max 100000).
  - `--engine chrome [--chrome-path PATH]` drives a **real Google Chrome** instead of WebView2 — see *Chrome engine* below.
- `resize --id ID --width PX --height PX` — change size of an already-running daemon (min 100x100)
- `list` — show running instances (pings each; cleans up stale port/pid files). Reports each daemon's `renderer_ok` (**can this daemon run JS at all?** — a webview that failed to attach makes every tool time out as if the page were wedged), `version`, `record`, `recording` (`on` | `off` | `never` — the tape state), `recording_armed` (the capture transport came up; stays true after `record off`), `watch_hang`, `console_transport`, `user_agent`/`user_agent_source` and `csp_bypassed` (**`true` means no CSP is enforced, so `console-logs --source csp` cannot report anything**) — check this FIRST when a tape or a console read is empty: a daemon started without `--record` records nothing, `console_transport: "unavailable"` means console capture never came up (empty ≠ quiet page), and `version: null` means a pre-0.9.0 binary is still running.
  - Also `current_url` (the top document, from daemon memory — answers on a wedged renderer), `last_used` / `last_used_ms` / `last_tool` (tell an abandoned instance from one somebody is using), `started_at_ms`, `engine`, `browser_version`, `navigator_webdriver` (Chrome engine) and `profile` (`kind`: `shared` | `private` | `ephemeral`, plus its folder). Null on pre-0.18 daemons. From 0.19: `ttl_s` / `expires_at_ms` (`start --ttl`) and `console_buffer`.
- `stop --id ID` — graceful stop via __shutdown over TCP
- `kill --id ID` — force-kill by PID. Use when `stop` times out on a wedged daemon (bypasses TCP). On a Chrome-engine id it also ends Chrome (only if the recorded pid is still that same Chrome process — never a reused pid) and deletes its throw-away profile; on a `--private-profile` id it deletes that folder. `list` does the same for a Chrome-engine daemon whose process has died
- `reset --id ID [--keep-browsing-data] [--keep-rules] [--force-shared] [--dry-run] [--url URL]` — back to a first-visit state **without** stopping: the id, port, window and start flags stay. Clears the profile's browsing data (cookies, cache, storage, IndexedDB, service workers — WebView2 `ClearBrowsingDataAll`; on `--engine chrome` a brand-new Chrome on a new empty profile), intercept rules, injected scripts and dalzzack's buffers (console, nav-log, failed-requests, response headers, tape contents), then opens `--url` (default about:blank). The page is left for about:blank BEFORE anything is cleared, so the old document's unload handlers cannot write back into the emptied profile. `--keep-rules` on Chrome re-registers the injected scripts in the new browser. `status: "partial"` + `failed_steps` if any step (or a nested rule clear) failed. Refuses on the shared profile (`SHARED_PROFILE`) — even with no other daemon running — unless `--force-shared` or `--keep-browsing-data`; `--dry-run` reports each step's scope, the cookies per domain it would delete and whether it would be refused, and touches nothing
- `record on|off|status --id ID [--clear]` — switch the `--record` tape on a LIVE daemon. `on` captures from now (earlier events are not recoverable); `off` stops appending but keeps the tape readable by `dump-recording` (`--clear` drops it); `on` again resumes it, and `off_periods` lists the gaps. **Read `recording` (`on` | `off` | `never`) for the state** — `armed` only says the capture transport came up and stays `true` after `off`. Refused on a daemon whose startup capture self-test failed (a tape there would prove nothing); honours `--no-console-capture` (network events only)
- `tools` — print JSON schemas for every tool (for MCP / LLM discovery)
- `upgrade [--path DIR | --git URL] [--rev REV] [--keep-daemons] [--no-locked]` — self-update via `cargo install --force`. Stops running daemons first by default. Needs cargo: without it, it fails with `cargo_not_found` and a `download_url` for the prebuilt binary.

**Navigate:**
- `navigate --url URL [--timeout-ms 5000]` — also returns the page `title` and the **main document's** outcome: `document` (status, every redirect hop with its `location`/`cf-mitigated`/`server`/set-cookie names, `challenge`, whether it `committed`), `navigations_during_wait`, `later_navigations` and a `hint` — so a challenge that solves itself reads as "403 `cloudflare:challenge`, then a second navigation", not as a plain success. WebView engine: waits `--timeout-ms`; Chrome engine: returns at the top frame's `load` (`load_event_ms`), at most `--timeout-ms`
- `back --id ID` / `forward --id ID` — `history.back/forward()` with URL poll (up to 3 s); returns `before_url`, `after_url`, `elapsed_ms`, `changed`
- `reload --id ID [--hard]` — reload current page. `--hard` bypasses the HTTP cache (Windows: CDP `Page.reload {ignoreCache:true}`; macOS: `reloadFromOrigin`)
- `clear-site-data --id ID [--origin URL] [--types LIST]` — wipe an origin's storage out-of-band: service worker registrations, CacheStorage, localStorage, IndexedDB, cookies. Runs outside the page's JS world, so it works when a proxy/membrane or a policy hides `navigator.serviceWorker`. `--origin` defaults to the current page. Types: `appcache,cache_storage,cookies,file_systems,indexeddb,local_storage,service_workers,shader_cache,websql` or `all`. **macOS caveat: clears by HOST, not origin — scheme and port are not distinguished.**
- `get-url`, `get-html [--selector CSS] [--strip-noise] [--max-length N]`, `screenshot [--output PATH] [--selector CSS]` — with `--selector`, an element that is not fully visible is scrolled into view first (every engine captures only what is on screen)

**Interact:**
- `click --x X --y Y` | `click --selector CSS` | `click --text "..."` — click by coordinates, selector, or text
- `hover --selector CSS` / `hover --x X --y Y` — move the cursor (macOS uses native CGEvent so CSS `:hover` fires; Windows/Linux dispatch JS events only and set `native_hover: false`; the Chrome engine moves Chrome's own pointer, `native_hover: true`). Use this for dropdown menus and tooltips.
- `fill-form --selector CSS --value TEXT` — fill single field (shorthand)
- `fill-form --fields '[{"selector":"#a","value":"1"}]'` — fill multiple fields (JSON)
- `keyboard --text "hello"` | `keyboard --text "long" --paste` — type or paste text
- `keyboard --key Enter [--modifiers ctrl,shift]` — press special keys
- `scroll --to top|bottom [--selector CSS]` — absolute scroll
- `scroll [--direction down] [--amount 500] [--selector CSS]` — relative scroll
- `upload-file --selector "input[type=file]" --file PATH`
- `save-image --selector CSS --output PATH [--index -1]` — save DOM image (default: last match)

**Inspect:**
- `describe [--output PATH]` — annotated screenshot with numbered elements + JSON
- `check-visible --selector CSS` — instant visibility check
- `element-at --x X --y Y` — find element at coordinates
- `wait-for --selector CSS [--state present|added|removed] [--text "..."] [--timeout-ms 10000]`
- `wait --ms MS` — sleep
- `wait-navigation [--to SUBSTRING] [--timeout-ms 30000]` — wait until the document is REPLACED (form submit, click-through, redirect). A sentinel planted in the old document vanishes when the new one commits, so **same-URL navigations are detected too**. Trigger the click/submit first, then call this — if the navigation is already in flight and page JS no longer answers, that is treated as the event, not an error. `--to` is matched against the URL on **every** poll (and once more right before timing out), read from the browser process when the renderer is too busy for JS, so a destination that arrives late still counts. Proxied navigations can take 20–30 s — raise `--timeout-ms`. Timeout → `NAVIGATION_TIMEOUT` carrying `to`, `current_url`, `document_replaced`, `polls`/`probe_failures` so the reason is readable off the response. A completed wait also carries `document` — the newest top-frame chain from `nav-log` (status, hops, challenge).
- `nav-log [--since-ms N] [--url-pattern STR] [--frames] [--max 20] [--headers] [--no-redact]` — the **navigation timeline**: one entry per document request chain with EVERY hop (redirects included — their response never produces `responseReceived`, so nothing else records them), each hop's `status`, `key_headers` (`location`, `cf-mitigated`, `cf-ray`, `server`, `content-type`, …), `set_cookie_names`, a detected `challenge` (`cloudflare:<value>` from `cf-mitigated`), and whether the response `committed`. Top frame by default; `--frames` adds iframe documents. `--headers` adds the request headers **as sent on the wire** (`requestWillBeSentExtraInfo`: cookies, `sec-fetch-*`; cookie values masked to `name=…[len]`) and the full response headers per hop. Always on — no `--record`; daemon-memory read. WebView2 on Windows, or `--engine chrome`
- `failed-requests [--since-ms N] [--url-pattern STR] [--resource-type TYPE] [--no-http-errors] [--include-canceled] [--max 50] [--headers]` — every failed request of **any resource type** (document, script, image, font, fetch, …) **with its URL**, which CDP's `loadingFailed` never carries. `kind`: `blocked` (CSP / mixed content, with `blocked_reason`), `cors` (`cors_error`), `network_error` (`net::ERR_*`), `http_error` (status ≥ 400), `canceled` (hidden unless `--include-canceled` — navigating away and ad slots make plenty). Carries `initiator`/`initiator_url` and `frame_id`; `summary` counts by kind and type. Always on
- `response-headers [--url-pattern STR] [--resource-type TYPE] [--max 50] [--no-redact]` — response headers, **including the navigation document**. WebView2 on Windows or `--engine chrome`; no `--record` needed. **Redirect hops are entries too** (`redirect: true`, sharing the request id of the chain) — before 0.18 the `302` in front of a document was silently missing.
  - This is the only view of the document's headers. `get-requests` / `network-log` are an in-page `fetch`/XHR hook, and that hook does not exist yet when the document is fetched — the navigation is invisible to them by construction. `dump-recording` carries CDP network events but never stored headers.
  - `--resource-type document` is usually what you want: CSP, CORP/COEP, cache policy and cookies are all decided there. Documents live in a separate ring, so a page with hundreds of subresources cannot push the navigation out of the buffer.
  - Capture is armed at daemon start because `Network.responseReceived` fires **once**, at navigation time — there is no way to ask for it afterwards.
  - `set-cookie` values are masked by default; the cookie name and its attributes (`Path`/`Domain`/`HttpOnly`/`SameSite`/`Secure`) are kept because those are what you debug with. `--no-redact` for the raw value.
  - `raw_headers` appears when `Network.responseReceivedExtraInfo` arrived — headers exactly as they came off the wire, including repeated `set-cookie` lines.
  - Every entry carries `ts_ms` (wall-clock capture time) — when the same URL was loaded several times, that is how you tell the loads apart.
- `console-logs [--level log|info|warn|error|debug] [--source console|csp|security|network|deprecation|violation] [--frame top|INDEX|FRAME_ID] [--text SUBSTR] [--max 100] [--clear]` — read the page's captured console (log/info/warn/error/debug + uncaught errors + unhandled rejections) from the top document **and every frame, cross-site (out-of-process) ones included** — each line carries `frame_id`. Ring buffer (500 entries, `start --console-buffer N` for more), armed at daemon start; the oldest are dropped first and `dropped` counts them. `--frame` uses `exec-js --frame`'s vocabulary (`top` also keeps lines with no frame id); `matched` is the count before `--max`.
  - **CSP violations land here too** (`--source csp`), collected from CDP `Log.entryAdded` and `Audits.issueAdded`. The page's own `securitypolicyviolation` event does not fire in this environment, so this is the only way to see a policy block — without it a blocked load is completely silent. Each entry carries `source`; untagged entries are the page's own `console.*` calls.
  - **The page cannot tell it is being captured.** `console.log` is never wrapped: Windows subscribes to CDP `Runtime.consoleAPICalled` in the browser process, macOS turns on WebKit's own system-console logging and reads it back out of `log stream`. So `console.log.toString()` stays `[native code]`, and a getter-trapped object passed to `console.log` is never read — which is what anti-debug libraries like `disable-devtool` use to decide DevTools is open. (The old in-page hook serialized arguments with `JSON.stringify`, invoked those getters, and made every protected site redirect away.)
  - Because it reads daemon memory it **keeps answering while the renderer is wedged**, unlike the in-page capture it replaced.
  - Object arguments are rendered from the engine's own preview, so accessors show as `[getter]` rather than being resolved.
  - Every response carries `console_transport`: `cdp` | `in_page_fallback` | `disabled` | `unavailable`. `unavailable` means capture never came up — an empty buffer there means nothing at all.
  - Every response also carries `csp_bypassed` — read it in the same response before concluding anything from zero CSP violations ("nothing was blocked" on a bypassing daemon says nothing about the policy).
  - Page `console.*` lines and uncaught errors carry `context_id` + `frame_id` (Windows). On a proxy every frame's `url` is identical, so the frame id is the only thing that says *which* of 25 frames logged a line — pair it with `get-frames`.
- `cookies [list|clear] [--url URL | --domain D | --all] [--name N] [--no-redact] [--force-shared] [--dry-run]` — the browser's cookie jar through CDP, **HttpOnly cookies included** (invisible to `document.cookie`), and cookies on **parent domains** (`.example.com`: `_ga`, consent, cross-subdomain sessions) and third-party domains — which `clear-site-data` (one origin) leaves behind. Default scope: exactly what the browser would send to the current page; `--domain` = that domain, its subdomains and its parents; `--all` = the whole jar. Values masked unless `--no-redact`; each cookie reports `host_only`, `http_only`, `secure`, `same_site`, `expires`. `clear` deletes, then re-reads the jar and reports `remaining` (a CDP delete with a non-exact domain is otherwise a silent no-op). Refuses on the shared profile — even with no other daemon running — unless `--force-shared`; `--dry-run` lists what it would delete. Field names are snake_case (`http_only`, not CDP's `httpOnly`) — `--jq`/`--field` warn on stderr when a key is spelled the CDP way

**Advanced:**
- `exec-js --script "return document.title"` (alias: `--code`) or `exec-js --file probe.js` — prefer `--file` for anything beyond a one-liner: `--script "$(cat probe.js)"` breaks whenever the script contains quotes, backticks or `$`, `--file` never does. Results hard-capped at 5 MB. Runs in the page's **main world** (page globals ARE visible) but only in the **top frame** — a global set inside an iframe reads as `undefined` from here. Every result reports `world` / `frame` / `frame_id` / `frame_origin` / `context_id` — including the default untargeted call, so a context left over from a replaced document is visible rather than a guess.
- `exec-js --frame <top|INDEX|origin-substring|frame-id>` — run inside a specific frame (Windows). `INDEX` is the **`get-frames` index** — same DOM enumeration on both sides, so `get-frames` index N and `--frame N` are the same frame by construction; `frame-id` (reported by `get-frames` and `known_frames`) is the stable handle. Out-of-range indexes are rejected (`FRAME_INDEX_OUT_OF_RANGE` with `dom_frame_count`); a miss returns `known_frames`. A frame with no main-world context (srcless/about:blank frames may never announce one) is an honest `NO_MAIN_WORLD_CONTEXT` — use `--world isolated` there, it only needs the frame id.
  - **Cross-site (out-of-process) frames work too** — by index, frame id, name or URL substring, main or isolated world — through the frame's own CDP session; the result says `out_of_process: true`. A Cloudflare widget (`challenges.cloudflare.com`) is such a frame.
  - **`RENDERER_BUSY` ≠ `FRAME_NOT_FOUND`.** `RENDERER_BUSY`: the frame exists and got the evaluate, but its renderer did not answer within `--timeout-ms` (a page computing a challenge). `FRAME_NOT_FOUND`: no such frame, decided before anything is sent. While a frame is busy, `console-logs` and `nav-log` (daemon memory) keep answering.
- `exec-js --world isolated` — a real isolated world via `Page.createIsolatedWorld` (Windows): hidden from page hooks, and correspondingly blind to page globals. Use `main` (the default) when you need to read what the page created.
  - **CSP and the isolated world — measured, not assumed.** The world is created with `grantUniveralAccess`, which lifts the *same-origin policy* but not the page's CSP: on a page with `connect-src 'self'`, a request to a non-allowlisted origin is blocked in `--world isolated` exactly as in `--world main`, and each produces its own violation. So an isolated world is not a CSP bypass.
  - **What actually removes the wall is dalzzack itself.** By default the daemon calls `Page.setBypassCSP`, which disables enforcement for *every* world — so nothing is ever blocked, no violation is ever reported, and a load that a real browser would refuse succeeds here. If you are testing a policy, `start --enforce-csp` first; `list` reports `csp_bypassed`. To read the policy rather than test it, use `response-headers --resource-type document`.
- `exec-js --await --script "const r = await fetch('/x'); return r.status"` — resolve a promise instead of failing on it, so the script can use top-level `await` (Windows: CDP `Runtime.evaluate {awaitPromise:true}`; macOS: `callAsyncJavaScript`). Without `--await`, returning a promise fails with "Promise result not supported in sync mode".
- `exec-js --target sw [--script …]` — evaluate inside the page's **service worker** (its own isolate: `self`, `caches`, `clients`, the worker's globals — nothing page-side can reach them). `--target sw:<substring>` picks the worker whose script URL contains it; plain `sw` the most recently attached. `--await` works; `--frame`/`--world` don't apply. Results carry `target: "service_worker"`, `session_id`, `script_url`. `NO_SERVICE_WORKER` lists what is attached. WebView2 on Windows or `--engine chrome`
- `get-requests [--url-pattern STR] [--seconds 60] [--max-count N] [--include-headers] [--no-redact]` — captured XHR/fetch. **In-page hook**: script/CSS/image loads are invisible to it and it cannot say who initiated a request. For either, use `failed-requests` / `nav-log` (always on) or `dump-recording` — its CDP network events cover every request type and carry `initiator` (with a JS call stack for script-triggered loads). `--include-headers` adds each request's headers + timestamp (Authorization/Cookie redacted to a 12-char prefix; `--no-redact` for full values). On `--engine chrome` it is served from CDP instead (`source: "cdp"`, nothing injected; `bodyPreview` via `Network.getResponseBody`).
- `get-frames [--full]`, `get-frame-html [--selector CSS | --index N]` — iframe listing / HTML. Each frame carries its CDP `frame_id` (Windows) — the handle `exec-js --frame` accepts — and the listing index doubles as `--frame`'s index space. A cross-site frame is marked `out_of_process: true` with its current `url` (read from its own target — the parent cannot see a cross-site frame's location). `name` is truncated to 200 chars by default (`name_len`/`name_truncated` mark the cut — SafeFrame ad slots put multi-KB JSON there); `--full` returns complete values.
- `dismiss-modals [--max-attempts 3]` — close modals/popups (Vue/Element/Bootstrap)
- `captcha --input-file PATH [--output-file PATH]` — captcha image preprocessing

**Fault injection (client-side network interception):** break the network on purpose to reproduce race/timeout bugs — no server access needed. Rules are applied via document-start injection, so they beat the app bundle's own `fetch` capture, persist across navigations, and die with the daemon.
- `intercept --pattern "*/portfolio*" --action hang [--ms N]` — matching requests never resolve (black hole). Honors the app's own AbortController/timeout; `--ms` optionally auto-errors after N ms. Pending requests show up in `get-requests` with `intercepted:"hang"`.
- `intercept --pattern "*/markets*" --action delay --ms 8000` — hold matching requests N ms, then pass through normally.
- `intercept --pattern "*/orders" --action fail` — immediate TypeError('Failed to fetch') network error.
- `intercept --pattern "*/quote*" --action mock [--status 500] [--body '{"err":1}'] [--content-type application/json] [--ms N]` — respond locally without touching the network.
- `--method POST` on any rule restricts it to one HTTP method. Pattern without `*` = substring match; with `*` = glob over the full URL. First matching rule wins; re-adding the same pattern replaces it.
- `intercept-list` — active rules + the rule-set version the live page has applied.
- `intercept-clear [--pattern STR]` — remove all rules, or just one pattern.
- On `--engine chrome` the same rules are enforced by Chrome itself through CDP's `Fetch` domain (`sync.transport: "cdp_fetch"`) — nothing is injected, `fetch.toString()` stays native. Same scope: fetch and XHR, in every frame (cross-site frames are held at start until their rules are in place); a `mock` also answers the request's CORS preflight locally and echoes the request's `Origin` with credentials allowed, so credentialed cross-origin mocks work.

**Document-start injection:** run your own script *before any page script*, on every navigation. This is the only way to observe what happens before the page's own code runs — `exec-js` needs a loaded document, so by the time it arrives the first API call and the first DOM insertion are already history.
- `inject-script --name LABEL (--script JS | --file PATH) [--world isolated|main]` — register it. Takes effect from the **next** navigation.
- **The world is the important choice.** `isolated` (default) shares the DOM but **cannot see page globals** — it cannot wrap `window.fetch` or anything the page created, so a script written for that will run happily and observe nothing. Use `--world main` to pre-empt a page API. Measured: a `main`-world `Object.defineProperty` hook catches the page's *first* assignment; from `isolated` the same global reads `undefined` while the DOM is visible.
- The isolated world is the same one `exec-js --world isolated` uses, so values the injected script leaves behind are readable from there. Verified at document start: `readyState: "loading"`, `document.scripts.length: 0`, `document.body === null`.
- `inject-list` — what is registered, and the world each runs in.
- `inject-clear [--name LABEL]` — unregister one or all. **macOS cannot remove an individual `WKUserScript`**, so there the response says the script keeps running until the daemon restarts rather than implying a clean page.
- Scripts live in daemon memory and die with it. Windows uses CDP `Page.addScriptToEvaluateOnNewDocument`; macOS uses `WKUserScript` in a named `WKContentWorld`.
- Typical request-queue check: 4× `intercept --action hang` on distinct endpoints → drive the UI → `get-requests --include-headers` shows the 4 pending slots while the 5th request still completes; the app's 25s abort surfaces as `error:"aborted"`.
- Typical account-switch check: `intercept --action delay --ms 8000` to back up the queue → switch accounts → `get-requests --include-headers` and compare the Authorization prefix of requests fired after the switch.

**Diagnostics (hung / slow renderer):** when `exec-js` or other tools time out, the renderer is wedged. These look at it from the OUTSIDE.
- `pause --id ID [--duration-ms 1200]` — WHERE is it frozen. macOS: external `sample` of the WebContent process (works on a fully hung renderer). Windows: CDP JS stack PLUS a `MiniDumpWriteDump`-based minidump of every renderer process — driven by Windows kernel debug APIs, so it works on a fully wedged renderer (the kernel takes the dump, not V8). Open `.dmp` in WinDbg/Visual Studio. **Start here for any freeze.**
- `pause --id ID --symbolize [--download-symbols] [--symbol-cache DIR] [--all-threads]` — Windows: resolve native stacks to function names (dbghelp) instead of handing back a `.dmp` path. **Threads blocked in a syscall are excluded** and counted as `waiting_threads` — leaving them in puts "the process was idle" at the top of every histogram. Classification is by *symbol*, so a thread inside `NtAllocateVirtualMemory` counts as WORK (an allocation storm is exactly what you're usually hunting) while `NtWaitForSingleObject` counts as a wait. Module paths come from the dump itself, so a WebView2 auto-update can't break symbol loading. Without `--download-symbols` only exported symbols resolve and the rest falls back to `module+0xRVA` rather than vanishing.
- **`--download-symbols` needs `dbghelp.dll` + `symsrv.dll` next to `dalzzack.exe`.** The System32 dbghelp has no symbol-server support, and the loader binds it by base name at process start, so nothing can redirect it later. If those DLLs are missing, dalzzack copies them from the Windows Kits Debuggers folder and tells you to re-run. Check `dbghelp` (`external` = good, `system` = downloads impossible), `symsrv_loaded`, and `dbghelp_log` (dbghelp's own account of where it searched) in the response. First fetch of `msedge.dll.pdb` is ~630 MB.
- `pause --id ID --samples N [--interval-ms M]` — take N dumps of **one** process instead of one dump of every process. With `--symbolize` the response carries a frame histogram — the only way to tell "spinning here" from "happened to be here". The target picks itself: the first sample dumps everything, then sampling follows whichever process actually has busy threads (`sampled_pid` + `sample_target_reason` say which and why). `suggested_pid` is now reported in every mode, not just full-memory.
- `pause --id ID --full-memory [--only-pid PID] [--all-processes] [--max-bytes N]` — Windows: dump the **heap too** (`MiniDumpWithFullMemory` + full-memory-info + handle data), so page/JS strings and allocations are searchable in the `.dmp`. This is the answer to *"what is being allocated?"* — the default dump is stacks-only and contains no page strings at all. Size ≈ the target's working set, so without `--only-pid` only the **largest** WebView2 process is dumped (`--all-processes` overrides; that's GBs). Every run reports `candidate_processes` (pid + working set) — pick your `--only-pid` from there. `--max-bytes` warns, never fails.
- `profile-start --id ID` … reproduce the issue … `profile-stop --id ID [--output cpu.json]` — CPU sampling (busy-loop vs idle-wait).
- `heap-snapshot --id ID [--output PATH]` — memory report (macOS native memory; Windows JS heap usage).
- `console-dump --id ID [--level LVL] [--max N]` — same buffer as `console-logs`, plus a `source` field naming the transport that answered and explicit hung-renderer flagging. On `cdp` / `os_log` it is a daemon-memory read and works straight through a hang.
- `sw-console --id ID [--level LVL] [--max N] [--clear]` — the **Service Worker's own** console + uncaught exceptions (incl. a SW `fetch()` failing on CSP). A SW runs in its own isolate, so nothing it logs reaches `console-logs`/`console-dump`. WebView2 on Windows, or `--engine chrome` (where workers are held at start, so their boot logs are captured). Two transports, armed together: `Target.setAutoAttach` (full `console.*` + exceptions, but a SW target belongs to the *browser*, not this page session, so attach often isn't possible) and the `ServiceWorker` domain (`workerErrorReported` — errors only, reachable from the page session). A per-session `Runtime.enable` timeout is now degradation, not failure: worker errors already captured are still returned (`diagnostics.session_runtime_enable_error` flags it). `SW_ATTACH_UNAVAILABLE` fires only when NO transport is live and nothing was captured, and its `diagnostics` names the cause — no worker (`worker_versions_seen: 0`), attach refused (raw CDP error), or a runtime without `ICoreWebView2_11` (`session_cdp_available: false`).
- `network-log --id ID [--since-ms N] [--url-pattern STR] [--max N]` — fetch/XHR, flags requests still pending (a common hang trigger).
- `attach-devtools --id ID` — macOS: enable Safari Web Inspector; Windows: open the WebView2 DevTools window.
- `exec-js-isolated --id ID --script "..." [--timeout-ms 30000]` — run JS in a world isolated from page globals. Note it evaluates in the page's *default* context, not a real isolated world — `exec-js --world isolated` is the real thing.
- `wait-for-condition --id ID --script "return COND" [--timeout-ms 30000] [--interval-ms 250]` — poll a JS condition (detects wedged renderer instead of blocking forever).
- `dump-recording --id ID [--output PATH] [--filter all|network|console|exceptions] [--since-ms N] [--clear]` — snapshot the always-on recording buffer. **Pure daemon-memory read, no IPC into the page** — answers even when every other diagnostics tool would time out. Requires `start --record` or `record on` (a tape stopped by `record off` stays readable and says so in `note`). Reports `renderer_wedged_at_ms` so you can see exactly when capture stopped, and `armed`/`arm_error`/`arm_verified`: an empty tape now tells you whether the page was quiet or the capture transport never came up (it returns `RECORDING_NOT_ARMED` instead of a plausible-looking zero). `arm_verified` is a startup self-test — the daemon logs one console line on its own landing page, confirms it arrives, and removes it from the tape — so "events are flowing" is proven rather than assumed. `dalzzack_version` is included; if it's older than you expect, the daemon is a stale binary.
- `trace --id ID [--duration-ms 2000] [--output PATH] [--categories STR]` — capture a chrome-tracing-compatible timeline. Windows: real CDP `Tracing.*` over the browser process (hang-resistant, includes V8/blink internals). macOS: Performance API + paint/longtask via injection (degraded). Output opens in chrome://tracing or perfetto.
- `debugger-arm --id ID [--strategy first-script|exceptions|url] [--url-pattern REGEX]` / `debugger-snapshot --id ID [--clear]` / `debugger-disarm --id ID` — Windows-only: pre-arm V8 so `Debugger.paused` events fire, daemon buffers call frames and auto-resumes. Use when "what JS is running at startup?" matters.
- `debugger-snapshot` resolves each frame's `scriptId` to a URL by joining against buffered `Debugger.scriptParsed` — call frames carry a scriptId and an *empty* `url` for eval'd, bundled and inline scripts, which is exactly the code worth identifying. Each frame gets `url`, `url_source` (`call_frame` | `script_parsed` | `unknown`), `pageLineNumber` (inline scripts offset by the script's `startLine`) and `script_origin_stack` (who called `eval`). The `scripts[]` table lists the referenced scripts; `scripts_unresolved` counts ids we never saw parsed.
- `debugger-arm --strategy exceptions [--uncaught-only] [--max-frames N] [--sample-every N]` — pause on **every throw** (`Debugger.setPauseOnExceptions`). This is the only way to see engine-thrown errors: Proxy-invariant `TypeError`s, `Illegal invocation`, and friends never pass through the page's global `Error`/`TypeError` constructors, so wrapping those in-page always reports zero. Every throw pausing V8 is slow by construction — on a storm use `--sample-every N` (record every Nth) and `--max-frames N`. `debugger-snapshot` reports `seen_total` vs `dropped_by_sampling` so the sampled count is never mistaken for the real one, and it reads daemon memory, so it answers on a wedged renderer.

**Batch:** `run --steps workflow.json`

### Chrome engine (`start --engine chrome`)

WebView2 is Chromium but not Chrome, and the differences are what bot defences measure: the TLS ClientHello (measured: Chrome 154 sends 19 extensions, WebView2 18 — the missing one is `trust_anchors` 0xCA34, i.e. a different JA4), client hints, brand list. `--engine chrome` launches the installed Google Chrome on a **fresh throw-away profile** with only `--user-data-dir --remote-debugging-port=<free port> --no-first-run --no-default-browser-check` — no `--enable-automation`, no headless — and serves the same commands over CDP.
- `start` and `list` report `browser_version` and **`navigator_webdriver`** (measured at startup; must be `false`). Measured: `--remote-debugging-port=0` alone flips it to `true`, which is why dalzzack picks the port itself.
- Same tools, same output shapes. JS-based tools run through `Runtime.evaluate`; `click`/`hover`/`keyboard` use `Input.dispatch*Event`, so events are **trusted** (`isTrusted: true`, `trusted: true` in the result) and CSS `:hover` fires; `navigate` returns at `load`; `screenshot` uses `Page.captureScreenshot`.
- Frames: `get-frames` follows the top document's `<iframe>` order (same index space as WebView); cross-site iframes run out of process and are reached through their own session (`out_of_process: true`) — `exec-js --frame N` and `get-frame-html` work in them.
- CSP is **enforced** (`csp_bypassed: false`) — it is Chrome. `--user-agent` / `--enforce-csp` / `--private-profile` don't apply; `--browser-arg` passes Chrome flags; `--no-console-capture` skips `Runtime.enable` (frame main worlds then need `--world isolated`).
- `stop` closes Chrome and deletes the profile before it answers (its output carries `chrome.profile_removed`), so `stop && start` is safe; `kill` reaps both; closing the Chrome window (or Chrome dying) stops the daemon. `reset` starts a new Chrome on a new profile under the same id.
- Not available (WebView2 internals): `pause`, `profile-*`, `heap-snapshot`, `trace`, `debugger-*`, `exec-js-isolated` → `UNSUPPORTED_ON_ENGINE`. Use F12 in the Chrome window instead.
- **No prerendering** — the one measured difference from a user's Chrome: speculation-rules prerenders are refused while DevTools is attached (`PrerenderingDisabledByDevTools`); prefetch still happens. Don't read a page's missing prerender activation as the site's doing.

### Output filtering (every command that prints one JSON document)
- `--field PATH` (repeatable) — print one value: `--field status`, `--field document.hops[0].status`, `--field "instances[].id"` (`[]` fans out). Several → an object keyed by path. Missing → `null`. **A `status: "failed"` result is printed whole with exit code 1** — a filter never turns an error into a plausible `null`.
- `--jq EXPR` — real jq, built in (no `jq` binary needed): `--jq '.navigations[] | {status, final_url}'`. One output per line; sees failures too.
- `--raw` — strings without quotes (like `jq -r`); a fanned-out string list prints one per line.

### Rules
- `-i` may be omitted when exactly one instance is running — it is used automatically (noted on stderr). Zero or several instances, and every usage error, fail with one JSON line on **stdout**, so a runner with `2>/dev/null` can no longer turn a mistyped command into a silent no-op
- After a click/submit that replaces the document, use `wait-navigation` (not a sleep, not URL polling — it catches same-URL navigations). If the page had already landed before the call, it answers `already_at_target: true` within ~1.5 s instead of waiting out the timeout; to wait for a same-URL *re*-navigation, start it before the trigger
- **Git-Bash (MSYS2) rewrites any argument starting with `/` into a Windows path** — `--to "/zp/p/"` reaches the tool as `C:/Program Files/Git/zp/p/` and matches nothing. Prefix such commands with `MSYS_NO_PATHCONV=1` (or make the value not start with `/`). Affects `--to`, `--url-pattern`, `--pattern`; the response carries `hint_shell_pathconv` when a value has that shape
- Use `describe` first to discover elements, `screenshot` for quick checks
- Prefer `click --selector` or `--text` over coordinate clicks
- Use `hover` (not coordinate-only mouseover) to open dropdown menus on macOS; CSS `:hover` won't trigger from JS events alone
- Use `fill-form --selector --value` for single fields (no JSON needed)
- Use `wait-for --state added` instead of `sleep` to wait for new content
- Use `save-image --index -1` to grab the last matching image
- Use `scroll --to bottom` for SPA containers with dynamic content
- Use `back` / `forward` / `reload` for history navigation rather than re-navigating to a previous URL
- Use `console-logs` when JS-driven UI misbehaves — the page's own errors are usually the fastest debug signal. It no longer needs a live renderer, so reach for it during a hang too
- **To see what happened before the page's own code ran, `inject-script` is the only option** — `exec-js` always arrives too late. Pick `--world main` if you need to wrap or watch a page API; `isolated` cannot see page globals and will silently observe nothing
- **A tool that answers `status: "failed"` is telling you about itself, not the page.** Decode and parse failures used to be reported as empty results — `get-frames` said "No iframes on the page" for pages full of them. If you get a `JS_RESULT_*` error, the payload is in the message
- **When something is blocked and you don't know why, read the headers before theorising.** `response-headers --resource-type document` shows the CSP/CORP/cache policy the document was actually served with, and `console-logs --source csp` shows what that policy blocked. Both are on by default
- **Before concluding anything about a CSP, check `csp_bypassed` in `list`.** dalzzack disables CSP enforcement by default so cross-origin iframes can be inspected — which means nothing is ever blocked and `--source csp` sits at zero regardless of the policy. Restart with `start --enforce-csp` to make the browser a witness. Changing worlds does **not** work around this: the isolated world is subject to the same CSP as the main world (measured), so a load succeeding there is not evidence the policy allows it
- **Distinguish a CSP block from a CORS block.** They both surface as `Failed to fetch`. `console-logs --source csp` shows only the former; a CORS failure appears with `source: "javascript"` and says so
- **If a site redirects you away the moment it loads** (often to `google.com`), something dalzzack does is tripping `disable-devtool`. Check `list` for `console_transport`: `in_page_fallback` means the page-visible hook is installed — restart WITHOUT `--console-capture` (macOS) or with `--no-console-capture` (Windows). On macOS the default is already no capture, so a redirect there is not the console hook
- Don't reach for `--user-agent` to look "more like a browser". The engine's `Sec-CH-UA` headers cannot be overridden, so a mismatched UA scores *worse* than the default. Windows already sends a consistent Edge UA
- To reset ONE origin to a first-visit state, run `clear-site-data` then `reload --hard`; for everything (parent-domain and third-party cookies included) use `reset` — on a `--private-profile` or `--engine chrome` instance, since plain instances share one profile. Don't try to unregister a service worker from page JS — a proxy/membrane can hide `navigator.serviceWorker.controller` and a policy can block `getRegistrations()`, so it silently no-ops
- **"Did the site let us through?" is a question for `nav-log`, not the final URL.** A challenge that solves itself (`403` + `cf-mitigated: challenge`, then a script reload) ends on the right page and used to read as a plain success; `navigate`'s `document`/`challenge`/`later_navigations` and `nav-log` show both hops
- **"WebView2 gets challenged" is not evidence about the site.** WebView2's TLS ClientHello and client hints differ from Chrome's; answer "what does Chrome get?" with `start --engine chrome`, and check `navigator_webdriver: false` in its `start` output
- A script, image or iframe that silently failed to load: `failed-requests` (URL + `kind` + `error_text`), not `get-requests` (fetch/XHR only)
- When `cookies clear` or `reset` answers `SHARED_PROFILE`, this daemon is on the shared cookie jar — other sessions' daemons use it now or later, so "no other daemon is up" is not "nobody uses it". Restart with `--private-profile` rather than reaching for `--force-shared`, or `--dry-run` to see what is at stake
- A `--jq`/`--field` key that does not exist reads as `null`. If stderr says `no key "httpOnly" … did you mean "http_only"?`, the output is snake_case and the key was spelled the CDP way
- If the SW itself is the suspect (a panic or trace inside it), read `sw-console` — those logs never appear in `console-logs`
- Reach for `exec-js --await` instead of stashing a promise on `window.__x` and polling it with a second `exec-js` call
- When a tool times out (especially `exec-js` / `navigate`), run `pause` first to confirm the renderer is wedged; don't keep retrying the same call
- For sessions where post-mortem matters (which request stalled? which JS errored before the freeze?), launch with `start --record` so `dump-recording` has a hang-resistant tape to read
- **A tool timing out is not proof the page is wedged.** Check `list` → `renderer_ok` first: if it's false the webview itself never came up, and every timeout from that daemon is the tool's, not the page's
- `exec-js` sees page globals (main world) but only in the **top frame** — if something reads as `undefined`, try `--frame` before concluding your instrumentation failed. Count frames with `get-frames` and target with the same index or, better, the `frame_id` it reports — the two tools share one coordinate system
- A targeted exec-js failing as `CONTEXT_NOT_FOUND` means the frame's document was replaced since the context was recorded (iframe reload, ad-slot recycle) — re-run `get-frames` and target again; the payload carries `known_frames`
- On a wedge, prefer `pause --symbolize --samples 6` over a single `pause`: one sample can't distinguish spinning from passing through. For msedge-internal names (V8 GC/alloc), you need `--download-symbols`, which needs `dbghelp.dll`+`symsrv.dll` beside the exe — dalzzack installs them on first use and asks you to re-run once
- When `pause` tells you *where* the stack is but not *why* (GC / exception machinery in the RIP histogram), re-run it with `--full-memory --only-pid <the fat renderer>` — the stacks-only dump has no heap, so it cannot answer "what is allocated"
- Don't try to count engine-thrown exceptions by wrapping `window.Error`/`TypeError` — those never go through the JS constructors and you'll measure zero. Use `debugger-arm --strategy exceptions`
- Use `kill` only when `stop` fails — graceful shutdown gives the daemon a chance to clean up
- **`start` flags only apply to a NEW daemon.** Re-running `start --record` against a live id returns `already_running` and lists what it ignored in `ignored_flags` — the daemon keeps the flags it was born with. `stop` then `start` to change them
- If `dump-recording` looks empty, read `list` first: `record: false` means it was never recording (`record on` turns it on now — earlier events are gone), and an old `version` means `dalzzack upgrade` hasn't reached that daemon
- **Never re-run `start` to change window size** — it's a no-op if the id is alive and responsive. Use `resize` for live windows; `list` to check what's alive.
- `--width/--height` on `start` only set the *initial* size; they don't override an existing daemon's window.
<!-- dalzzack-end -->
"##;

const BLOCK_BEGIN: &str = "<!-- dalzzack-begin -->";
const BLOCK_END: &str = "<!-- dalzzack-end -->";

/// The text before and after the dalzzack block; `None` when there is no block.
///
/// Anything but one begin marker before one end marker is an error: without an
/// end marker, the text after the begin marker is the user's own.
fn split_around_block(text: &str) -> Result<Option<(&str, &str)>, String> {
    let (begins, ends) = (
        text.matches(BLOCK_BEGIN).count(),
        text.matches(BLOCK_END).count(),
    );
    if begins == 0 && ends == 0 {
        return Ok(None);
    }
    match (text.find(BLOCK_BEGIN), text.find(BLOCK_END)) {
        (Some(begin), Some(end)) if begins == 1 && ends == 1 && begin < end => {
            Ok(Some((&text[..begin], &text[end + BLOCK_END.len()..])))
        }
        _ => Err(format!(
            "expected one {BLOCK_BEGIN} before one {BLOCK_END}, found {begins} and {ends}. \
             Fix the markers by hand. The file was not changed."
        )),
    }
}

/// `existing` with its block replaced, or with a block added at the end.
fn with_docs(existing: &str) -> Result<String, String> {
    Ok(match split_around_block(existing)? {
        Some((before, after)) => match after.trim_start_matches(['\r', '\n']) {
            "" => format!("{before}{DALZZACK_DOCS}"),
            after => format!("{before}{DALZZACK_DOCS}\n{after}"),
        },
        None => match existing.trim_end() {
            "" => DALZZACK_DOCS.to_string(),
            kept => format!("{kept}\n\n{DALZZACK_DOCS}"),
        },
    })
}

fn inject_docs(file_path: &std::path::Path, label: &str) {
    use std::fs;

    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent).ok();
    }

    // Only a missing file counts as empty: writing after any other read error
    // would replace the user's file.
    let existing = match fs::read_to_string(file_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => fail_install(file_path, &format!("cannot read the file: {e}")),
    };
    let updated = with_docs(&existing).unwrap_or_else(|message| fail_install(file_path, &message));
    if existing.contains(BLOCK_BEGIN) {
        eprintln!("  Updated existing dalzzack section in {}", label);
    }

    fs::write(file_path, updated).unwrap_or_else(|e| {
        eprintln!("Error writing {}: {}", label, e);
        std::process::exit(1);
    });

    eprintln!("  Injected into {}", file_path.display());
}

fn fail_install(file: &std::path::Path, message: &str) -> ! {
    println!(
        "{}",
        serde_json::json!({
            "status": "failed",
            "error": "GUIDE_NOT_INSTALLED",
            "file": file.display().to_string(),
            "message": message,
        })
    );
    std::process::exit(1);
}

/// Rewrite the guide where it is already installed. `upgrade` runs this with
/// the new binary, so the guide always matches the commands. It creates nothing:
/// an agent the user did not set up stays that way.
fn refresh_docs() {
    let mut refreshed = Vec::new();
    let mut failed = Vec::new();
    for path in [
        home_dir().join(".claude").join("CLAUDE.md"),
        codex_home().join("AGENTS.md"),
    ] {
        let Ok(existing) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !existing.contains(BLOCK_BEGIN) && !existing.contains(BLOCK_END) {
            continue;
        }
        let written = with_docs(&existing)
            .and_then(|updated| std::fs::write(&path, updated).map_err(|e| e.to_string()));
        match written {
            Ok(()) => refreshed.push(path.display().to_string()),
            Err(message) => failed.push(serde_json::json!({
                "file": path.display().to_string(),
                "message": message,
            })),
        }
    }
    println!(
        "{}",
        serde_json::json!({
            "status": if failed.is_empty() { "completed" } else { "failed" },
            "refreshed": refreshed,
            "failed": failed,
        })
    );
}

/// Compile-time repository URL (from Cargo.toml `repository = "..."`).
/// Empty string when unset; `upgrade` falls back to an error in that case.
const BAKED_REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// The plain-binary archive that `release.yml` attaches for this platform.
#[cfg(target_os = "macos")]
const RELEASE_ASSET: Option<&str> = Some("dalzzack-macos-universal.tar.gz");
#[cfg(target_os = "windows")]
const RELEASE_ASSET: Option<&str> = Some("dalzzack-windows-x64.zip");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const RELEASE_ASSET: Option<&str> = None;

/// `<repo>/releases/latest/download/<asset>`: the newest release's archive,
/// whose name never changes. `None` without a repository URL or without a
/// release build for this platform.
fn release_download_url(repo: &str, asset: Option<&str>) -> Option<String> {
    let repo = repo.trim_end_matches('/');
    match asset {
        Some(asset) if !repo.is_empty() => Some(format!("{repo}/releases/latest/download/{asset}")),
        _ => None,
    }
}

fn cargo_on_path() -> bool {
    std::process::Command::new("cargo")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Same file, also when one path goes through a symlink or differs in case
/// on a case-insensitive file system.
fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// `dalzzack upgrade` — reinstall this binary via cargo, stopping any
/// running daemons first so they pick up the new code on next `start`.
fn upgrade(
    path: Option<PathBuf>,
    git_override: Option<String>,
    rev: Option<String>,
    keep_daemons: bool,
    no_locked: bool,
) {
    // 0. A binary installed from a release archive can run on a machine with
    //    no Rust. Point it at the archive before any daemon is stopped.
    if !cargo_on_path() {
        println!(
            "{}",
            serde_json::json!({
                "status": "failed",
                "error": "cargo_not_found",
                "message": "upgrade builds dalzzack from source with cargo, and cargo is not on PATH.",
                "download_url": release_download_url(BAKED_REPO_URL, RELEASE_ASSET),
                "current_exe": std::env::current_exe().ok().map(|p| p.display().to_string()),
                "hint": "Download the archive, stop the running daemons (`dalzzack list`, then \
                         `dalzzack stop --id <id>`), and replace current_exe with the binary \
                         inside. Or install Rust from https://rustup.rs and run upgrade again.",
            })
        );
        std::process::exit(1);
    }
    // Captured before step 3 renames the running image on Windows.
    let running_from = std::env::current_exe().ok();

    // 1. Discover and stop running daemons (unless --keep-daemons).
    let alive = list_running_instance_ids();
    if !alive.is_empty() {
        if keep_daemons {
            eprintln!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "daemons_running",
                    "ids": alive,
                    "message": "Refusing to upgrade with daemons running. Stop them or omit --keep-daemons.",
                })
            );
            std::process::exit(1);
        }
        eprintln!(
            "[upgrade] stopping {} running daemon(s): {:?}",
            alive.len(),
            alive
        );
        for id in &alive {
            stop_or_kill(id);
        }
    }

    // 2. Pick source.
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("install");
    if !no_locked {
        cmd.arg("--locked");
    }
    cmd.arg("--force");

    if let Some(p) = path {
        let p_str = p.to_string_lossy().to_string();
        eprintln!("[upgrade] cargo install --path {} --force", p_str);
        cmd.arg("--path").arg(p);
    } else {
        let url = git_override.unwrap_or_else(|| BAKED_REPO_URL.to_string());
        if url.is_empty() {
            eprintln!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "no_source",
                    "message": "No repository URL baked in. Pass --git <URL> or --path <DIR>.",
                })
            );
            std::process::exit(1);
        }
        eprintln!("[upgrade] cargo install --git {} --force", url);
        cmd.arg("--git").arg(&url);
        if let Some(r) = rev {
            cmd.arg("--rev").arg(r);
        }
    }

    // 3. Windows: get the running image out of cargo's way.
    //
    // Windows refuses to overwrite or delete an executable that is currently
    // running — and the process running `dalzzack upgrade` IS the installed
    // `dalzzack.exe`, so `cargo install` built fine and then failed on the
    // final move with os error 5, with no daemon left to blame. A running image
    // may be *renamed*, though: move it aside to `.old`, let cargo drop the new
    // file into place, and sweep the `.old` on the next start (the rustup /
    // cargo-update pattern).
    let install_target = cargo_install_target();
    let moved_aside = moved_aside_path(&install_target);
    #[cfg(target_os = "windows")]
    if install_target.exists() {
        let _ = std::fs::remove_file(&moved_aside);
        match std::fs::rename(&install_target, &moved_aside) {
            Ok(()) => eprintln!(
                "[upgrade] moved running image aside: {} -> {}",
                install_target.display(),
                moved_aside.display()
            ),
            Err(e) => eprintln!(
                "[upgrade] could not move {} aside ({e}); cargo may fail to replace it",
                install_target.display()
            ),
        }
    }

    // 4. Run cargo install.
    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "cargo_spawn_failed",
                    "message": format!("Failed to run cargo: {}. Is cargo on PATH?", e),
                })
            );
            std::process::exit(1);
        }
    };

    if !status.success() {
        // Put the previous binary back if cargo never produced a new one, so a
        // failed upgrade leaves a working `dalzzack` rather than none.
        let restored = if moved_aside.exists() && !install_target.exists() {
            std::fs::rename(&moved_aside, &install_target).is_ok()
        } else {
            false
        };
        println!(
            "{}",
            serde_json::json!({
                "status": "failed",
                "error": "cargo_install_failed",
                "exit_code": status.code(),
                "install_target": install_target.display().to_string(),
                "previous_binary_restored": restored,
                "hint": "Read cargo's own output above for the failing step. If it is the final \
                         move onto the install target with 'os error 5', something still has that \
                         file open — a daemon started from THIS binary (`dalzzack list`), or an \
                         antivirus scan; the running upgrade process itself is moved aside first \
                         and is not the cause.",
            })
        );
        std::process::exit(1);
    }
    // The old image is still mapped by this very process, so deleting it here
    // fails on Windows — it is swept on the next start (`sweep_moved_aside`).
    let _ = std::fs::remove_file(&moved_aside);

    // 5. macOS: ad-hoc re-codesign the new binary so Gatekeeper doesn't grump.
    //    The new binary is the one cargo installed, which is not this process's
    //    image when dalzzack was installed from a release archive.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&install_target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    let mut out = serde_json::json!({
        "status": "upgraded",
        "stopped_daemons": alive,
        "installed_to": install_target.display().to_string(),
    });
    // A binary from a release archive lives outside cargo's bin directory, so
    // the new build did not replace it and the old copy may still win on PATH.
    if let Some(exe) = running_from.filter(|exe| !same_path(exe, &install_target)) {
        out["note"] = format!(
            "This command ran from {}, which cargo did not replace. Delete that copy, \
             or put {} before it on PATH.",
            exe.display(),
            install_target
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )
        .into();
    }
    println!("{out}");
}

/// Option values that start with `/` are exactly what Git-Bash (MSYS2) rewrites
/// into a Windows path before the program ever sees them. Only the options
/// whose values are URL fragments — never real paths — are checked.
fn shell_pathconv_suspects(args: &serde_json::Value) -> Vec<String> {
    const URL_FRAGMENT_KEYS: &[&str] = &["to", "url_pattern", "pattern"];
    let Some(obj) = args.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .filter(|(k, _)| URL_FRAGMENT_KEYS.contains(&k.as_str()))
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s)))
        .filter(|(_, v)| looks_shell_path_converted(v))
        .map(|(k, v)| format!("--{}={v:?}", k.replace('_', "-")))
        .collect()
}

/// `C:/Program Files/Git/zp/p/` — a drive letter, then the MSYS root, then the
/// value the user actually typed. Legit URL fragments never look like this.
fn looks_shell_path_converted(v: &str) -> bool {
    let b = v.as_bytes();
    let has_drive = b.len() > 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b[2] == b'/' || b[2] == b'\\');
    has_drive
        && (v.contains("/Git/") || v.contains("\\Git\\") || v.to_ascii_lowercase().contains("msys"))
}

/// Where `cargo install` puts the binary, in cargo's own precedence:
/// `$CARGO_INSTALL_ROOT/bin`, else `$CARGO_HOME/bin`, else `~/.cargo/bin`.
fn cargo_install_target() -> PathBuf {
    let root = std::env::var_os("CARGO_INSTALL_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .unwrap_or_else(|| home_dir().join(".cargo"));
    root.join("bin")
        .join(format!("dalzzack{}", std::env::consts::EXE_SUFFIX))
}

/// The name a running image is renamed to while cargo replaces it.
fn moved_aside_path(target: &std::path::Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

/// Delete the `.old` image a previous `upgrade` left behind. The upgrade
/// itself cannot: on Windows that file was its own running image.
fn sweep_moved_aside() {
    // Both the configured install target and wherever this binary actually
    // lives — they differ when the exe was copied or a CARGO_INSTALL_ROOT
    // changed between the upgrade and now.
    let mut candidates = vec![moved_aside_path(&cargo_install_target())];
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(moved_aside_path(&exe));
    }
    candidates.dedup();
    for old in candidates {
        if old.exists() {
            match std::fs::remove_file(&old) {
                Ok(()) => tracing::debug!("[upgrade] swept {}", old.display()),
                // Still mapped by another process (an older daemon, say); next time.
                Err(e) => tracing::debug!("[upgrade] could not sweep {}: {e}", old.display()),
            }
        }
    }
}

/// Scan the temp dir for `dalzzack-*.port` files and return the instance IDs.
/// Doesn't ping — `stop_or_kill` will figure out whether a graceful shutdown works.
fn list_running_instance_ids() -> Vec<String> {
    let tmp_dir = std::env::temp_dir();
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("dalzzack-") {
                if let Some(id) = rest.strip_suffix(".port") {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

/// Try graceful shutdown; if it errors out (hung daemon), force-kill by PID.
/// Both calls are best-effort — we don't want a stuck daemon to block the upgrade.
fn stop_or_kill(id: &str) {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("dalzzack"));
    let stop = std::process::Command::new(&exe)
        .args(["stop", "--id", id])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if matches!(stop, Ok(s) if s.success()) {
        eprintln!("[upgrade]   stopped {}", id);
        return;
    }
    eprintln!("[upgrade]   stop failed for {} — force-killing", id);
    let _ = std::process::Command::new(&exe)
        .args(["kill", "--id", id])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn home_dir() -> std::path::PathBuf {
    // Windows: USERPROFILE (e.g. C:\Users\name)
    // macOS/Linux: HOME (e.g. /Users/name)
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            eprintln!("Error: Could not determine home directory (HOME / USERPROFILE not set)");
            std::process::exit(1);
        })
}

/// Where Codex keeps its global files: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
}

fn install_docs(target: &str) {
    match target {
        "claude-code" | "claude" => {
            let path = home_dir().join(".claude").join("CLAUDE.md");
            eprintln!("Installing for Claude Code (global)...");
            inject_docs(&path, "~/.claude/CLAUDE.md");
            println!(
                "{}",
                serde_json::json!({
                    "status": "completed",
                    "target": "claude-code",
                    "file": path.display().to_string(),
                    "message": "dalzzack docs injected into ~/.claude/CLAUDE.md (global, all projects)"
                })
            );
        }
        "codex" => {
            let path = codex_home().join("AGENTS.md");
            eprintln!("Installing for Codex (global)...");
            inject_docs(&path, &path.display().to_string());
            println!(
                "{}",
                serde_json::json!({
                    "status": "completed",
                    "target": "codex",
                    "file": path.display().to_string(),
                    "message": "dalzzack docs injected into the global Codex AGENTS.md (all projects)"
                })
            );
        }
        "codex-project" => {
            let path = std::path::PathBuf::from("AGENTS.md");
            eprintln!("Installing for Codex (current project)...");
            inject_docs(&path, "AGENTS.md");
            println!(
                "{}",
                serde_json::json!({
                    "status": "completed",
                    "target": "codex-project",
                    "file": dalzzack_lib::state::printable_path(&path),
                    "message": "dalzzack docs injected into ./AGENTS.md (current project)"
                })
            );
        }
        "all" => {
            let claude_path = home_dir().join(".claude").join("CLAUDE.md");
            let codex_path = codex_home().join("AGENTS.md");
            eprintln!("Installing for all agents...");
            inject_docs(&claude_path, "~/.claude/CLAUDE.md");
            inject_docs(&codex_path, &codex_path.display().to_string());
            println!(
                "{}",
                serde_json::json!({
                    "status": "completed",
                    "targets": ["claude-code", "codex"],
                    "files": [claude_path.display().to_string(), codex_path.display().to_string()],
                    "message": "dalzzack docs injected into the global Claude Code and Codex files (all projects)"
                })
            );
        }
        _ => {
            eprintln!(
                "Unknown target: '{}'. Use: claude-code, codex, codex-project, or all",
                target
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_prints_one_tool_or_only_the_names() {
        let defs = dalzzack_lib::tools::definitions::get_tool_definitions();
        let names = tools_output(&defs, None, true);
        assert_eq!(names.as_array().unwrap().len(), defs.len());
        assert!(names.to_string().len() < 2048);

        let one = tools_output(&defs, Some("get-html"), false);
        assert_eq!(one["name"], "get_html");
        assert_eq!(tools_output(&defs, Some("get_html"), false), one);
        assert!(one["inputSchema"].is_object());

        assert_eq!(tools_output(&defs, Some("nope"), false)["status"], "failed");
        assert_eq!(
            tools_output(&defs, None, false).as_array().unwrap().len(),
            defs.len()
        );
    }

    #[test]
    fn the_guide_replaces_its_own_block_and_leaves_the_rest() {
        let file = format!("# Mine\r\n\r\n{BLOCK_BEGIN}\nOLD GUIDE\n{BLOCK_END}\n\n# Also mine\n");
        let updated = with_docs(&file).unwrap();
        assert!(updated.starts_with("# Mine\r\n\r\n"));
        assert!(updated.ends_with("\n# Also mine\n"));
        assert!(updated.contains(DALZZACK_DOCS));
        assert!(!updated.contains("OLD GUIDE"));
        // Running it again changes nothing.
        assert_eq!(with_docs(&updated).unwrap(), updated);
    }

    #[test]
    fn the_guide_goes_at_the_end_of_a_file_without_a_block() {
        assert_eq!(with_docs("").unwrap(), DALZZACK_DOCS);
        assert_eq!(
            with_docs("# Mine\n\n").unwrap(),
            format!("# Mine\n\n{DALZZACK_DOCS}")
        );
    }

    #[test]
    fn a_broken_block_changes_nothing() {
        for file in [
            format!("{BLOCK_BEGIN}\nmy notes, with no end marker\n"),
            format!("{BLOCK_END}\nthe wrong order\n{BLOCK_BEGIN}\n"),
            format!("{BLOCK_BEGIN}\n{BLOCK_END}\n{BLOCK_BEGIN}\n{BLOCK_END}\n"),
            format!("only an end marker {BLOCK_END}\n"),
        ] {
            assert!(with_docs(&file).is_err(), "{file}");
        }
    }

    #[test]
    fn release_download_url_points_at_the_latest_fixed_name_asset() {
        assert_eq!(
            release_download_url(
                "https://github.com/gosuda/dalzzack/",
                Some("dalzzack-windows-x64.zip")
            )
            .as_deref(),
            Some("https://github.com/gosuda/dalzzack/releases/latest/download/dalzzack-windows-x64.zip")
        );
        // No repository baked in, or no release build for this platform.
        assert_eq!(
            release_download_url("", Some("dalzzack-windows-x64.zip")),
            None
        );
        assert_eq!(
            release_download_url("https://github.com/gosuda/dalzzack", None),
            None
        );
        // The baked-in values on the two release platforms.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert!(release_download_url(BAKED_REPO_URL, RELEASE_ASSET)
            .is_some_and(|u| u.starts_with("https://github.com/gosuda/dalzzack/releases/latest/")));
    }

    #[test]
    fn git_bash_path_conversion_is_recognised_and_plain_values_are_not() {
        // What MSYS2 makes of `--to "/zp/p/"`.
        assert!(looks_shell_path_converted("C:/Program Files/Git/zp/p/"));
        assert!(looks_shell_path_converted(r"C:\Program Files\Git\zp\p\"));
        assert!(looks_shell_path_converted("D:/msys64/api/v1"));
        // Real URL fragments and real paths that are not a conversion artefact.
        assert!(!looks_shell_path_converted("/zp/p/"));
        assert!(!looks_shell_path_converted("zp/p"));
        assert!(!looks_shell_path_converted("https://proxy.localhost/zp/p/"));
        assert!(!looks_shell_path_converted("C:/Users/me/probe.js"));
        assert!(!looks_shell_path_converted(""));
    }

    #[test]
    fn only_url_fragment_options_are_inspected() {
        let args = serde_json::json!({
            "to": "C:/Program Files/Git/zp/p/",
            "url_pattern": "/api/",
            "file": "C:/Program Files/Git/etc/probe.js",
        });
        let hits = shell_pathconv_suspects(&args);
        assert_eq!(
            hits,
            vec![r#"--to="C:/Program Files/Git/zp/p/""#.to_string()]
        );
    }

    #[test]
    fn moved_aside_name_appends_old_to_the_full_filename() {
        let p = moved_aside_path(std::path::Path::new("C:/x/bin/dalzzack.exe"));
        assert!(p.to_string_lossy().ends_with("dalzzack.exe.old"));
    }
}
