//! JavaScript injected at document start on every page load.
//!
//! Everything here runs inside the page, so everything here is observable by
//! the page. Two rules follow from that and are not negotiable:
//!
//! 1. **Never write to the page's console.** Anti-debug scripts monitor it, and
//!    a `[dalzzack] …` line is a free fingerprint.
//! 2. **Never read a property off a value the page handed us.** Reading invokes
//!    accessors, and libraries like `disable-devtool` detect DevTools precisely
//!    by passing a getter-trapped object to `console.log` and checking whether
//!    the getter ran. That false positive is what made protected sites redirect
//!    away from dalzzack.
//!
//! State lives under a single per-daemon randomized global (see [`crate::pagens`])
//! rather than a spray of `__TW_*` names.

use crate::pagens;

/// What to include in the document-start script.
#[derive(Debug, Clone, Default)]
pub struct InjectionOptions {
    /// Install the in-page console capture fallback.
    ///
    /// Only for platforms/situations where the out-of-band transport (CDP on
    /// Windows, WebKit's system-console logging on macOS) is unavailable. It is
    /// strictly more detectable than those, so it is opt-in from the daemon and
    /// always reported as `console_transport: "in_page_fallback"`.
    pub console_fallback: bool,
    /// Pre-rendered UA/fingerprint consistency block, or empty for none.
    pub stealth_script: String,
}

/// Build the document-start script for this daemon.
pub fn initialization_script(opts: &InjectionOptions) -> String {
    let mut out = String::with_capacity(24 * 1024);
    out.push_str(CORE_BLOCK);
    out.push_str(INTERCEPTOR_BLOCK);
    if opts.console_fallback {
        out.push_str(CONSOLE_FALLBACK_BLOCK);
    }
    if !opts.stealth_script.is_empty() {
        out.push('\n');
        out.push_str(&opts.stealth_script);
    }
    pagens::render(&out)
}

/// Namespace bootstrap + dialog auto-dismiss.
///
/// The dialog overrides stay because a modal blocks every automation call, but
/// they are silent and carry a native-looking `toString`.
const CORE_BLOCK: &str = r#"
(function() {
    var NS = window.NS_PLACEHOLDER = window.NS_PLACEHOLDER || {};
    if (NS.init) return;
    NS.init = true;

    // Engines disagree on how a native function stringifies: JSC returns
    // "function log() {\n    [native code]\n}", V8 returns it on one line. A
    // hardcoded guess is a fingerprint — comparing our string against any real
    // builtin's is a one-liner. So derive the shape from a builtin we never
    // touch and only swap the name in.
    var NATIVE_TPL = (function() {
        try {
            var s = Function.prototype.toString.call(Object.prototype.hasOwnProperty);
            if (s.indexOf('hasOwnProperty') !== -1 && s.indexOf('[native code]') !== -1) {
                // NUL cannot appear in the engine's own source text, so it is
                // an unambiguous slot; a space would collide with 'function '.
                return s.replace('hasOwnProperty', '\u0000');
            }
        } catch(e) {}
        return 'function \u0000() { [native code] }';
    })();
    // Published on NS: CONSOLE_FALLBACK_BLOCK is a separate IIFE and would
    // otherwise have to re-derive this (or hardcode it, which is the bug).
    NS.nt = NATIVE_TPL;

    function nativeish(fn, name) {
        try {
            Object.defineProperty(fn, 'name', { value: name, configurable: true });
            var src = NATIVE_TPL.replace('\u0000', name);
            Object.defineProperty(fn, 'toString', {
                value: function() { return src; },
                configurable: true, writable: true
            });
        } catch(e) {}
        return fn;
    }

    // Auto-dismiss modal dialogs. Silent: writing to the page console would
    // both pollute the site's logs and identify us.
    window.alert = nativeish(function() {}, 'alert');
    window.confirm = nativeish(function() { return true; }, 'confirm');
    window.prompt = nativeish(function() { return ''; }, 'prompt');
})();
"#;

/// XHR/fetch capture (180s TTL ring buffer) + fault injection.
///
/// Rules live in `NS.rules`, populated by the daemon via a runtime-added
/// document-start script (survives navigation) and via exec-js for the
/// already-loaded page. Rules are consulted at call time, so they can arrive
/// before or after this script runs.
const INTERCEPTOR_BLOCK: &str = r#"
(function(){
    var NS = window.NS_PLACEHOLDER = window.NS_PLACEHOLDER || {};
    if(NS.ni) return;
    NS.ni = true;
    NS.cr = [];
    NS.rules = NS.rules || [];
    NS.v = NS.v || 0;
    var id = 0;
    var MAX = 30000;
    var TTL = 180000;
    var HMAX = 50;
    var HVMAX = 4096;
    var EXT = /\.(js|css|png|jpg|jpeg|gif|svg|ico|woff2?|ttf|eot|map|webp|mp4|mp3)(\?|$)/i;

    function ok(u) {
        return u && !u.startsWith('data:') && !u.startsWith('blob:')
            && !EXT.test(u) && !u.includes('tauri://');
    }
    function tr(s, n) {
        return (!s || s.length <= n) ? s : s.substring(0, n) + '...[TRUNCATED]';
    }
    function sb(b) {
        if (!b) return null;
        if (typeof b === 'string') return tr(b, MAX);
        if (b instanceof URLSearchParams) return b.toString();
        try { return tr(JSON.stringify(b), MAX); } catch(e) { return null; }
    }
    // Push an entry into the ring buffer. Idempotent per entry so intercepted
    // requests can be recorded while still pending and mutated in place later.
    function gc(entry) {
        var cutoff = Date.now() - TTL;
        while (NS.cr.length > 0 && NS.cr[0].ts < cutoff) {
            NS.cr.shift();
        }
        if (entry.__p) return;
        entry.__p = true;
        NS.cr.push(entry);
    }

    function globRe(p) {
        return new RegExp('^' + p.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*') + '$');
    }
    // First matching rule wins. Patterns without '*' are substring matches;
    // patterns with '*' are globs against the full URL.
    function matchRule(u, m) {
        var rules = NS.rules || [];
        for (var k = 0; k < rules.length; k++) {
            var r = rules[k];
            if (!r || !r.pattern || !r.action) continue;
            if (r.method && m && r.method.toUpperCase() !== m.toUpperCase()) continue;
            if (r.pattern.indexOf('*') === -1) {
                if (u.indexOf(r.pattern) !== -1) return r;
            } else {
                if (!r.__re || r.__reSrc !== r.pattern) {
                    try { r.__re = globRe(r.pattern); r.__reSrc = r.pattern; } catch(e) { continue; }
                }
                if (r.__re.test(u)) return r;
            }
        }
        return null;
    }
    function addHdr(h, k, v) {
        if (Object.keys(h).length >= HMAX) return;
        h[String(k).toLowerCase()] = tr(String(v), HVMAX);
    }
    function hdrsOf(input, init) {
        var h = {};
        try {
            if (input instanceof Request && input.headers) {
                input.headers.forEach(function(v, k) { addHdr(h, k, v); });
            }
            var ih = init && init.headers;
            if (ih) {
                if (Array.isArray(ih)) {
                    ih.forEach(function(p) { if (p && p.length >= 2) addHdr(h, p[0], p[1]); });
                } else if (typeof ih.forEach === 'function' && typeof ih.get === 'function') {
                    ih.forEach(function(v, k) { addHdr(h, k, v); });
                } else if (typeof ih === 'object') {
                    Object.keys(ih).forEach(function(k) { addHdr(h, k, ih[k]); });
                }
            }
        } catch(e) {}
        return h;
    }
    function abortReason(sig) {
        if (sig && sig.reason) return sig.reason;
        try { return new DOMException('The operation was aborted.', 'AbortError'); }
        catch(e) { var err = new Error('The operation was aborted.'); err.name = 'AbortError'; return err; }
    }
    function fireXhrError(x) {
        try {
            x.dispatchEvent(new ProgressEvent('error'));
            x.dispatchEvent(new ProgressEvent('loadend'));
        } catch(e) {}
    }

    var xo = XMLHttpRequest.prototype.open,
        xs = XMLHttpRequest.prototype.send,
        xh = XMLHttpRequest.prototype.setRequestHeader;
    XMLHttpRequest.prototype.open = function(m, u) {
        this.__i = { id: ++id, method: m, url: u, ts: Date.now(), type: 'xhr' };
        xo.apply(this, arguments);
    };
    XMLHttpRequest.prototype.setRequestHeader = function(k, v) {
        if (this.__i) {
            this.__i.hdrs = this.__i.hdrs || {};
            addHdr(this.__i.hdrs, k, v);
        }
        return xh.apply(this, arguments);
    };
    XMLHttpRequest.prototype.send = function(b) {
        var self = this;
        if (this.__i && ok(this.__i.url)) {
            var i = this.__i;
            i.reqBody = sb(b);
            var t = performance.now();
            this.addEventListener('loadend', function() {
                i.dur = Math.round(performance.now() - t);
                i.status = this.status;
                i.ct = this.getResponseHeader('content-type') || '';
                try {
                    if (i.ct.includes('json') || i.method === 'POST') {
                        i.resBody = tr(this.responseText, MAX);
                    }
                    i.resPreview = this.responseText ? this.responseText.substring(0, 100) : null;
                } catch(e) {}
                gc(i);
            });

            var rule = matchRule(i.url, i.method);
            if (rule) {
                i.intercepted = rule.action;
                if (rule.action === 'hang') {
                    gc(i);
                    if (rule.ms) {
                        setTimeout(function() { i.error = 'dalzzack: hang timeout'; fireXhrError(self); }, rule.ms);
                    }
                    return; // swallow the send — request never leaves the page
                }
                if (rule.action === 'fail') {
                    i.error = 'dalzzack: injected network error';
                    gc(i);
                    setTimeout(function() { fireXhrError(self); }, 0);
                    return;
                }
                if (rule.action === 'mock') {
                    gc(i);
                    var mst = rule.status || 200, mbd = rule.body || '';
                    setTimeout(function() {
                        i.status = mst;
                        i.ct = rule.content_type || 'application/json';
                        i.resBody = tr(mbd, MAX);
                        i.resPreview = mbd.substring(0, 100);
                        try {
                            Object.defineProperty(self, 'readyState', { value: 4, configurable: true });
                            Object.defineProperty(self, 'status', { value: mst, configurable: true });
                            Object.defineProperty(self, 'responseText', { value: mbd, configurable: true });
                            Object.defineProperty(self, 'response', { value: mbd, configurable: true });
                            self.dispatchEvent(new Event('readystatechange'));
                            self.dispatchEvent(new ProgressEvent('load'));
                            self.dispatchEvent(new ProgressEvent('loadend'));
                        } catch(e) {}
                    }, rule.ms || 0);
                    return;
                }
                if (rule.action === 'delay') {
                    gc(i);
                    var dArgs = arguments;
                    setTimeout(function() { xs.apply(self, dArgs); }, rule.ms || 0);
                    return;
                }
            }
        }
        xs.apply(this, arguments);
    };

    var of = window.fetch;
    window.fetch = function(input, init) {
        var u = (input instanceof Request) ? input.url : String(input);
        var m = ((init && init.method) || (input instanceof Request && input.method) || 'GET').toUpperCase();
        if (!ok(u)) return of.apply(this, arguments);
        var i = { id: ++id, method: m, url: u, reqBody: sb(init && init.body), hdrs: hdrsOf(input, init), ts: Date.now(), type: 'fetch' };
        var t = performance.now();
        var fSelf = this, fArgs = arguments;
        var sig = (init && init.signal) || (input instanceof Request ? input.signal : null);

        function dispatch() {
            return of.apply(fSelf, fArgs).then(function(r) {
                i.dur = Math.round(performance.now() - t);
                i.status = r.status;
                i.ct = r.headers.get('content-type') || '';
                var c = r.clone();
                c.text().then(function(txt) {
                    if (i.ct.includes('json') || m === 'POST') {
                        i.resBody = tr(txt, MAX);
                    }
                    i.resPreview = txt ? txt.substring(0, 100) : null;
                    gc(i);
                }).catch(function() { gc(i); });
                return r;
            }).catch(function(e) { i.error = e.message; i.dur = Math.round(performance.now() - t); gc(i); throw e; });
        }

        var rule = matchRule(u, m);
        if (!rule) return dispatch();
        i.intercepted = rule.action;

        if (rule.action === 'hang') {
            gc(i); // record immediately so the pending request is visible in get-requests
            return new Promise(function(resolve, reject) {
                var tm = null;
                if (rule.ms) {
                    tm = setTimeout(function() {
                        i.error = 'dalzzack: hang timeout';
                        i.dur = Math.round(performance.now() - t);
                        reject(new TypeError('Failed to fetch'));
                    }, rule.ms);
                }
                if (sig) {
                    var onAbort = function() {
                        if (tm) clearTimeout(tm);
                        i.error = 'aborted';
                        i.dur = Math.round(performance.now() - t);
                        reject(abortReason(sig));
                    };
                    if (sig.aborted) return onAbort();
                    sig.addEventListener('abort', onAbort);
                }
            });
        }
        if (rule.action === 'fail') {
            i.error = 'dalzzack: injected network error';
            i.dur = 0;
            gc(i);
            return Promise.reject(new TypeError('Failed to fetch'));
        }
        if (rule.action === 'mock') {
            return new Promise(function(resolve, reject) {
                setTimeout(function() {
                    var st = rule.status || 200, bd = rule.body || '';
                    i.status = st;
                    i.ct = rule.content_type || 'application/json';
                    i.resBody = tr(bd, MAX);
                    i.resPreview = bd.substring(0, 100);
                    i.dur = Math.round(performance.now() - t);
                    gc(i);
                    try {
                        // 204/205/304 must have a null body per the Response constructor
                        var noBody = (st === 204 || st === 205 || st === 304);
                        resolve(new Response(noBody ? null : bd, {
                            status: st,
                            headers: { 'Content-Type': rule.content_type || 'application/json' }
                        }));
                    } catch(e) { reject(e); }
                }, rule.ms || 0);
            });
        }
        // delay: hold the request, then pass through (abort honored during the wait)
        gc(i);
        return new Promise(function(resolve, reject) {
            var done = false;
            if (sig) {
                var onAbort = function() {
                    if (done) return;
                    done = true;
                    i.error = 'aborted';
                    reject(abortReason(sig));
                };
                if (sig.aborted) return onAbort();
                sig.addEventListener('abort', onAbort);
            }
            setTimeout(function() {
                if (done) return;
                done = true;
                resolve();
            }, rule.ms || 0);
        }).then(dispatch);
    };
})();
"#;

/// In-page console capture — **fallback only**.
///
/// Used when neither CDP (Windows) nor WebKit's system-console logging (macOS)
/// is available. Unlike the original implementation this never reads a property
/// off a logged value: it walks own property *descriptors* and takes only
/// `descriptor.value`, so an accessor is reported as `[getter]` without being
/// invoked.
///
/// Deliberately absent: `JSON.stringify` (walks the graph, calls every getter
/// and `toJSON`), `String(obj)` / template concat (calls `toString` /
/// `valueOf` / `Symbol.toPrimitive`), `instanceof` (calls `Symbol.hasInstance`),
/// and `Object.prototype.toString.call` (reads `Symbol.toStringTag`, which can
/// itself be an accessor).
const CONSOLE_FALLBACK_BLOCK: &str = r#"
(function() {
    var NS = window.NS_PLACEHOLDER = window.NS_PLACEHOLDER || {};
    if (NS.con) return;
    NS.con = true;
    var MAX = 500;
    var MAXLEN = 2000;
    NS.logs = [];

    // Capture natives before page script can replace them.
    var _gOPD = Object.getOwnPropertyDescriptor;
    var _gOPN = Object.getOwnPropertyNames;
    var _isArray = Array.isArray;
    var _defineProp = Object.defineProperty;
    var _now = Date.now;

    // Read a property WITHOUT triggering an accessor. Returns undefined for
    // accessors and for anything that throws.
    function dataValue(obj, key) {
        try {
            var d = _gOPD(obj, key);
            if (d && 'value' in d) return d.value;
        } catch(e) {}
        return undefined;
    }

    function describe(v, depth) {
        var t = typeof v;
        if (v === null) return 'null';
        if (t === 'undefined') return 'undefined';
        if (t === 'string') return v;
        if (t === 'number' || t === 'boolean') return '' + v;
        if (t === 'bigint') return '' + v + 'n';
        if (t === 'symbol') return '[symbol]';
        if (t === 'function') {
            var fname = dataValue(v, 'name');
            return 'function ' + (typeof fname === 'string' ? fname : '') + '()';
        }
        if (depth <= 0) return '[object]';

        var parts = [];
        var arr = false;
        try { arr = _isArray(v); } catch(e) {}

        var names;
        try { names = _gOPN(v); } catch(e) { return arr ? '[array]' : '[object]'; }

        for (var k = 0; k < names.length && parts.length < 20; k++) {
            var key = names[k];
            if (arr && key === 'length') continue;
            var d;
            try { d = _gOPD(v, key); } catch(e) { continue; }
            if (!d) continue;
            if (!d.enumerable && !arr) continue;
            var rendered;
            if ('value' in d) {
                rendered = describe(d.value, depth - 1);
            } else {
                rendered = '[getter]';   // never invokes d.get
            }
            parts.push(arr ? rendered : key + ': ' + rendered);
        }
        var body = parts.join(', ');
        if (arr) return '[' + body + ']';
        return '{' + body + '}';
    }

    function fmt(args) {
        var out = [];
        for (var i = 0; i < args.length; i++) {
            try { out.push(describe(args[i], 2)); }
            catch(e) { out.push('[unserializable]'); }
        }
        return out.join(' ');
    }

    function push(level, args) {
        try {
            if (NS.logs.length >= MAX) NS.logs.shift();
            NS.logs.push({ level: level, message: fmt(args).substring(0, MAXLEN), ts: _now() });
        } catch(e) {}
    }

    ['log', 'info', 'warn', 'error', 'debug'].forEach(function(level) {
        var orig = console[level];
        var wrapper = function() { push(level, arguments); return orig.apply(console, arguments); };
        // Cheap cover for `console.log.toString()` checks. The string is taken
        // from a real builtin (see NATIVE_TPL) rather than hardcoded: JSC prints
        // "function log() {\n    [native code]\n}" and a one-line guess differs
        // from every genuine builtin on the page, which is itself the tell.
        // Note this still cannot defeat
        // `Function.prototype.toString.call(console.log)`, nor
        // `console.log.hasOwnProperty('toString')`; patching those would be a
        // larger detection surface than the one it hides, so the honest escape
        // hatch is `start --no-console-capture`.
        try {
            _defineProp(wrapper, 'name', { value: level, configurable: true });
            _defineProp(wrapper, 'length', { value: 0, configurable: true });
            var wsrc = (NS.nt || 'function \u0000() { [native code] }').replace('\u0000', level);
            _defineProp(wrapper, 'toString', {
                value: function() { return wsrc; },
                configurable: true, writable: true
            });
        } catch(e) {}
        console[level] = wrapper;
    });

    window.addEventListener('error', function(e) {
        var msg = dataValue(e, 'message');
        push('error', ['[uncaught] ' + (typeof msg === 'string' ? msg : '(error)')]);
    });
    window.addEventListener('unhandledrejection', function() {
        push('error', ['[unhandledrejection]']);
    });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn script(fallback: bool) -> String {
        initialization_script(&InjectionOptions {
            console_fallback: fallback,
            stealth_script: String::new(),
        })
    }

    #[test]
    fn no_placeholder_survives_rendering() {
        assert!(!script(true).contains(pagens::NS_TOKEN));
    }

    #[test]
    fn no_brand_names_leak_into_the_page() {
        // The literal strings an anti-bot script would grep the page for.
        // The fault-injection error strings are the one deliberate exception:
        // they surface to the user as request errors, not as page globals.
        let s = script(true)
            .replace("'dalzzack: hang timeout'", "")
            .replace("'dalzzack: injected network error'", "");
        for needle in ["dalzzack", "Dalzzack", "DALZZACK", "__TW_", "__tw_"] {
            assert!(!s.contains(needle), "leaked {needle} into the page script");
        }
    }

    #[test]
    fn nothing_writes_to_the_page_console() {
        let s = script(true);
        for forbidden in [
            "console.log(",
            "console.warn(",
            "console.error(",
            "console.info(",
        ] {
            assert!(
                !s.contains(forbidden),
                "injected script writes to the page console via {forbidden}"
            );
        }
    }

    #[test]
    fn console_fallback_is_opt_in() {
        assert!(!script(false).contains("NS.logs"));
        assert!(script(true).contains("NS.logs"));
    }

    #[test]
    fn fallback_serializer_never_calls_a_getter() {
        let s = script(true);
        // The exact constructs that invoked accessors in the original `fmt`.
        assert!(!s.contains("JSON.stringify(a)"));
        assert!(!s.contains("instanceof Error"));
        assert!(s.contains("'[getter]'"));
        assert!(s.contains("'value' in d"));
    }

    #[test]
    fn stealth_block_is_appended_when_present() {
        let s = initialization_script(&InjectionOptions {
            console_fallback: false,
            stealth_script: "/*STEALTH_MARKER*/".to_string(),
        });
        assert!(s.contains("/*STEALTH_MARKER*/"));
    }

    #[test]
    fn interceptor_state_lives_under_the_namespace() {
        let s = script(false);
        let ns = pagens::namespace();
        assert!(s.contains(&format!("window.{ns}")));
        assert!(!s.contains("window.__cr"));
        assert!(!s.contains("window.__ni"));
    }
}
