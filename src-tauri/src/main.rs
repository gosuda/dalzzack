//! dalzzack — Browser automation CLI for AI agents
//!
//! Daemon mode: `dalzzack start --id hn` → persistent browser instance
//! Individual tools: `dalzzack navigate --id hn --url "..."`
//! Batch mode: `dalzzack run --id hn --steps file.json`

// NOTE: windows_subsystem = "windows" intentionally NOT set.
// dalzzack is a CLI tool — it needs console output (stdout/stderr).
// A GUI-only build would need it; we use the console subsystem instead.

use clap::{Parser, Subcommand};
use dalzzack_lib::guide_file::{codex_home, global_files, with_docs, BLOCK_BEGIN, BLOCK_END};
use dalzzack_lib::state::home_dir;
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
    /// A failure and the output of --field or --jq are always printed whole.
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

    /// Get the visible text of the page or one element. This is the cheapest way
    /// to read a page.
    #[command(name = "get-text")]
    GetText {
        /// Instance ID
        #[arg(long, short, default_value = "")]
        id: String,
        /// CSS selector (default: full page)
        #[arg(long)]
        selector: Option<String>,
        /// Cut the text to N bytes
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

    /// Check the latest release and replace this binary if it is older.
    /// Stops running daemons so they pick up the new version, then refreshes the
    /// installed guides. --path, --git, --rev, and --no-locked build from source
    /// with cargo instead.
    #[command(visible_alias = "update")]
    Upgrade {
        /// Print the current and latest versions. Change nothing.
        #[arg(long, conflicts_with_all = ["force", "tag", "path", "git", "rev", "no_locked"])]
        check: bool,
        /// Install the release even if this version is the same.
        #[arg(long, conflicts_with_all = ["path", "git", "rev", "no_locked"])]
        force: bool,
        /// Install this release tag and not the latest: a release candidate or a rollback.
        #[arg(long, conflicts_with_all = ["path", "git", "rev", "no_locked"])]
        tag: Option<String>,
        /// Build from this folder with `cargo install --path`: the `src-tauri`
        /// folder of a clone.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Build from this repository with `cargo install --git`.
        #[arg(long)]
        git: Option<String>,
        /// Git revision (branch / tag / sha) to build. Uses --git or this project's repository.
        #[arg(long)]
        rev: Option<String>,
        /// Don't try to stop running daemons — fail if any are alive.
        #[arg(long)]
        keep_daemons: bool,
        /// Build from source without the `--locked` flag of cargo install.
        #[arg(long)]
        no_locked: bool,
    },

    /// Run several steps in one call. With the id of a live instance, the steps run
    /// there and one result is printed. Otherwise this starts a browser, runs the
    /// steps, prints one line per step, and exits. A failed step stops the run.
    Run {
        /// Instance ID (auto-generates if omitted)
        #[arg(long, short)]
        id: Option<String>,
        /// JSON steps file, or - for stdin: a list of commands such as
        /// [["click", "--text", "OK"], ["wait-for", "--selector", ".done"]], or
        /// {"steps": [{"tool": "click_element", "args": {"text": "OK"}}]}
        #[arg(long)]
        steps: PathBuf,
    },

    /// Print a part of the usage guide. With no topic, list the topics.
    Guide {
        /// The topic to print
        topic: Option<String>,
        /// Print every topic
        #[arg(long, conflicts_with = "topic")]
        all: bool,
    },

    /// Inject dalzzack usage docs into AI agent config files.
    /// claude-code → ~/.claude/CLAUDE.md (global, all projects)
    /// codex → ~/.codex/AGENTS.md or $CODEX_HOME/AGENTS.md (global, all projects)
    /// codex-project → ./AGENTS.md (current project)
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
        println!("{}", show(v, filter));
        eprintln!("(result is a failure — printed whole instead of --field)");
        return 1;
    }
    match dalzzack_lib::cli::output::render(v, filter) {
        Ok(text) => {
            println!("{text}");
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

/// The first line of a clap error, which says what is wrong.
fn usage_message(e: &clap::Error) -> String {
    let text = e.render().to_string();
    text.lines().next().unwrap_or("invalid usage").to_string()
}

fn main() {
    // Usage errors must reach stdout too: clap prints them to stderr and a
    // runner with `2>/dev/null` sees a successful-looking empty result.
    let (cli, filter, full_flag) = match Args::try_parse() {
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
                a.full,
            )
        }
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                e.exit();
            }
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "USAGE",
                    "message": usage_message(&e),
                })
            );
            // Full clap rendering still goes to stderr for humans.
            e.exit();
        }
    };

    // A previous `upgrade` may have left its own (then-running) image behind
    // as `dalzzack.exe.old`; this is the first moment it can be deleted.
    dalzzack_lib::upgrade::sweep_moved_aside();

    // These print progress or several documents; a filter has nothing single
    // to apply to, and silently ignoring it would be worse than saying so.
    if filter.is_active()
        && matches!(
            cli,
            Cli::Run { .. } | Cli::Guide { .. } | Cli::Install { .. } | Cli::Daemon { .. }
        )
    {
        println!(
            "{}",
            serde_json::json!({
                "status": "failed",
                "error": "USAGE",
                "message": "--field/--jq apply to commands that print one JSON document; run, \
                            guide, install and daemon do not",
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
            let steps = load_steps(&steps).unwrap_or_else(|message| {
                println!(
                    "{}",
                    serde_json::json!({ "status": "failed", "error": "USAGE", "message": message })
                );
                std::process::exit(2);
            });
            let live = id
                .as_deref()
                .filter(|id| dalzzack_lib::daemon::ping_info(id).is_some());
            match live {
                Some(id) => std::process::exit(run_on_instance(id, steps, &filter)),
                None => {
                    let instance_id = id.unwrap_or_else(|| format!("run-{}", std::process::id()));
                    dalzzack_lib::cli::run_steps(&instance_id, steps);
                }
            }
        }
        Cli::Guide { topic, all } => print_guide(topic.as_deref(), all),
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
            check,
            force,
            tag,
            path,
            git,
            rev,
            keep_daemons,
            no_locked,
        } => {
            let (report, code) = match dalzzack_lib::upgrade::run(dalzzack_lib::upgrade::Options {
                check,
                force,
                tag,
                keep_daemons,
                path,
                git,
                rev,
                no_locked,
            }) {
                Ok(report) => (report, 0),
                Err(report) => (report, 1),
            };
            emit(&report, &filter, "", &serde_json::Value::Null);
            std::process::exit(code);
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

/// All steps of a steps file. They are read first, so that a mistake in the last
/// one does not leave the first ones done.
fn load_steps(path: &std::path::Path) -> Result<Vec<dalzzack_lib::cli::Step>, String> {
    match dalzzack_lib::cli::read_steps(path)? {
        serde_json::Value::Array(lines) => lines
            .iter()
            .enumerate()
            .map(|(i, line)| command_step(line).map_err(|e| format!("step {}: {e}", i + 1)))
            .collect(),
        document => dalzzack_lib::cli::object_steps(document),
    }
}

/// One `["click", "--text", "OK"]` step, read with the CLI's own definitions.
fn command_step(line: &serde_json::Value) -> Result<dalzzack_lib::cli::Step, String> {
    let words: Option<Vec<&str>> = line
        .as_array()
        .map(|words| words.iter().map(|word| word.as_str()).collect())
        .unwrap_or_default();
    let words = words.ok_or("expected a list of strings")?;
    let parsed = Args::try_parse_from(std::iter::once("dalzzack").chain(words))
        .map_err(|e| usage_message(&e))?;
    if matches!(
        parsed.cmd,
        Cli::Start { .. }
            | Cli::Stop { .. }
            | Cli::Kill { .. }
            | Cli::List
            | Cli::Daemon { .. }
            | Cli::Run { .. }
            | Cli::Guide { .. }
            | Cli::Install { .. }
            | Cli::Tools { .. }
            | Cli::Upgrade { .. }
    ) {
        return Err("only commands that act on a page can be a step".to_string());
    }
    let (id, tool, args) = cli_to_tool_call(parsed.cmd);
    if !id.is_empty() {
        return Err("a step cannot name an instance: `run --id` chooses it".to_string());
    }
    Ok(dalzzack_lib::cli::Step {
        tool,
        args: Some(args),
    })
}

/// Run steps on a live instance and print one result: the last step's, or the
/// failed step's. Returns the exit code.
fn run_on_instance(
    id: &str,
    steps: Vec<dalzzack_lib::cli::Step>,
    filter: &dalzzack_lib::cli::output::OutputFilter,
) -> i32 {
    let total = steps.len();
    let mut last = serde_json::Value::Null;
    for (i, step) in steps.into_iter().enumerate() {
        let args = step.args.unwrap_or_else(|| serde_json::json!({}));
        let result = dalzzack_lib::daemon::send_command(id, &step.tool, args.clone())
            .unwrap_or_else(|e| serde_json::json!({ "status": "failed", "error": e.to_string() }));
        if result["status"] == "failed" {
            let failed = serde_json::json!({
                "status": "failed",
                "steps_run": i + 1,
                "steps_total": total,
                "failed_step": i + 1,
                "tool": step.tool,
                "result": result,
            });
            emit(&failed, filter, "", &serde_json::Value::Null);
            return 1;
        }
        last = if filter.full {
            result
        } else {
            dalzzack_lib::cli::brief::brief(&step.tool, &args, &result)
        };
    }
    let done = serde_json::json!({
        "status": "completed",
        "steps_run": total,
        "steps_total": total,
        "last": last,
    });
    emit(&done, filter, "", &serde_json::Value::Null)
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
        Cli::GetText {
            id,
            selector,
            max_length,
        } => (
            id,
            "get_text".into(),
            serde_json::json!({ "selector": selector, "max_length": max_length }),
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
        | Cli::Guide { .. }
        | Cli::Install { .. }
        | Cli::Tools { .. }
        | Cli::Upgrade { .. } => {
            unreachable!("handled in main()")
        }
    }
}

/// The parts of the guide that `install` leaves out, so that an agent pays for
/// them only when it needs them: name, what it covers, text.
const GUIDE_TOPICS: [(&str, &str, &str); 11] = [
    (
        "instance",
        "start, stop, list, reset, resize, upgrade, install",
        include_str!("../guide/instance.md"),
    ),
    (
        "navigate",
        "navigate, reload, get-text, get-html, and whether a page really loaded",
        include_str!("../guide/navigate.md"),
    ),
    (
        "interact",
        "click, hover, fill-form, keyboard, scroll, upload-file, save-image",
        include_str!("../guide/interact.md"),
    ),
    (
        "inspect",
        "describe, wait-for, nav-log, failed-requests, response-headers, console-logs, cookies",
        include_str!("../guide/inspect.md"),
    ),
    (
        "advanced",
        "exec-js, service workers, get-requests, frames, run",
        include_str!("../guide/advanced.md"),
    ),
    (
        "intercept",
        "make requests hang, wait, fail, or return a mock",
        include_str!("../guide/intercept.md"),
    ),
    (
        "inject",
        "run a script before the page's own code",
        include_str!("../guide/inject.md"),
    ),
    (
        "diagnostics",
        "what to do when a page or a command hangs",
        include_str!("../guide/diagnostics.md"),
    ),
    (
        "chrome",
        "drive a real Google Chrome with --engine chrome",
        include_str!("../guide/chrome.md"),
    ),
    (
        "output",
        "--field, --jq, --full, --max-output, and where files go",
        include_str!("../guide/output.md"),
    ),
    (
        "rules",
        "every usage rule",
        include_str!("../guide/rules.md"),
    ),
];

/// `guide`: one topic, all of them, or the list of topics.
fn print_guide(topic: Option<&str>, all: bool) {
    if all {
        for (_, _, text) in GUIDE_TOPICS {
            println!("{text}");
        }
        return;
    }
    let Some(topic) = topic else {
        println!("dalzzack guide <topic>\n");
        for (name, about, _) in GUIDE_TOPICS {
            println!("  {name:<12} {about}");
        }
        return;
    };
    match GUIDE_TOPICS.iter().find(|(name, _, _)| *name == topic) {
        Some((_, _, text)) => print!("{text}"),
        None => {
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "error": "UNKNOWN_TOPIC",
                    "message": format!("No topic is named {topic}. `dalzzack guide` lists them."),
                })
            );
            std::process::exit(2);
        }
    }
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
    for path in global_files() {
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
    use dalzzack_lib::guide_file::DALZZACK_DOCS;

    /// A debug build needs more stack to parse a command line than a test thread
    /// has. `main` has this much too.
    fn with_main_stack(test: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(test)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn a_step_is_a_command_line() {
        use serde_json::json;
        with_main_stack(|| {
            let step = command_step(&json!(["click", "--text", "Sign In"])).unwrap();
            assert_eq!(step.tool, "click_element");
            assert_eq!(step.args.unwrap()["text"], "Sign In");

            let step = command_step(&json!(["get-html", "--selector", "h1"])).unwrap();
            assert_eq!(step.tool, "get_html");
        });
    }

    #[test]
    fn a_step_that_is_not_a_page_command_is_refused() {
        use serde_json::json;
        with_main_stack(|| {
            for line in [
                json!(["stop", "--id", "b"]),
                json!(["run", "--steps", "x.json"]),
                json!(["guide"]),
                json!(["upgrade"]),
                json!(["get-url", "-i", "other"]),
                json!(["no-such-command"]),
                json!(["click", 5]),
                json!("click"),
            ] {
                assert!(command_step(&line).is_err(), "{line}");
            }
        });
    }

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
    fn the_installed_guide_stays_small() {
        // Every session of every agent pays for these bytes, also a session
        // that never opens a browser. The rest belongs in a topic.
        let size = DALZZACK_DOCS.len();
        assert!(size <= 4096, "the installed guide is {size} bytes");
    }

    #[test]
    fn agents_md_is_the_installed_guide() {
        assert_eq!(
            include_str!("../../AGENTS.md").replace("\r\n", "\n"),
            DALZZACK_DOCS.replace("\r\n", "\n"),
            "run `dalzzack install codex-project` in the repo root"
        );
    }

    #[test]
    fn the_installed_guide_names_every_topic() {
        for (name, about, text) in GUIDE_TOPICS {
            assert!(DALZZACK_DOCS.contains(name), "{name}");
            assert!(!about.is_empty() && !text.trim().is_empty(), "{name}");
        }
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
}
