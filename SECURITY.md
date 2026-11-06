# Security Policy

## Reporting a vulnerability

Please **do not** open public GitHub issues for security vulnerabilities.

Report it privately through GitHub: open the repository's **Security** tab and choose **Report a vulnerability** (<https://github.com/gosuda/dalzzack/security/advisories/new>). Include:

- A description of the issue and where it lives in the code
- A minimal reproduction (steps, repo link, or proof-of-concept)
- The impact you anticipate (data exposure, code execution, privilege escalation, etc.)
- Your suggested fix, if you have one

You'll get an acknowledgement within 5 business days. Coordinated disclosure timelines are negotiable — the default is 90 days from triage to public fix.

## Supported versions

| Version | Supported |
|---------|-----------|
| 1.0.x   | Yes (current) |
| < 1.0   | No |

Versions before 1.0 were never released publicly. They are not patched.

---

## Threat model and known relaxations

dalzzack drives a real WebView on behalf of an AI agent. Several browser security guarantees are intentionally relaxed so that the agent can interact with cross-origin iframes, modal dialogs, and dynamic content. **Operators should understand these before pointing the daemon at untrusted URLs.**

### Same-Origin Policy is relaxed

- **macOS** — `WKPreferencesSetWebSecurityEnabled` is set to `false` via a private WebKit API ([`src-tauri/src/platform/security.rs`](src-tauri/src/platform/security.rs)). This is *not* App Store compatible and is enabled unconditionally at daemon startup.
- **Linux** — WebKitGTK is configured with `allow-universal-access-from-file-urls` and `allow-file-access-from-file-urls`, plus `enable-write-console-messages-to-stdout`.
- **Windows** — SOP is *not* disabled. Cross-origin iframe access is routed through CDP (Page.setBypassCSP + Runtime execution contexts), which is more constrained but still permissive.

**What this means in practice.** Any page the agent navigates to can read content from any cross-origin iframe it embeds. Malicious sites can bypass standard CORS and SOP protections to read auth tokens, cross-site cookies (within the daemon's WebView), and iframe contents that a normal browser would isolate.

**Mitigations available today**

- Run each agent task in a daemon with its own profile: `dalzzack start --id <fresh> --private-profile` → ... → `dalzzack stop`. A private profile starts empty and `stop` deletes it, so leaked state stays inside that session. Plain instances share one persistent WebView2 profile, so cookies and logins from one session are available in the next.
- Avoid pointing daemons at sites you do not trust enough to log into manually. On the shared profile, every login made earlier is available to every page the agent opens. On a private profile there is no login to abuse, but the agent's own state can still be exfiltrated through SOP-bypassing iframes.
- Do not embed user-controlled URLs as direct `navigate` targets without a vetting step.

A future release should gate SOP-disabling behind an explicit `--allow-cross-origin` flag rather than enabling it by default. Track this in the issue tracker; PRs welcome.

### Dialog auto-dismiss

`alert()`, `confirm()`, and `prompt()` are overridden in the page context so the agent never blocks on a native dialog ([`src-tauri/src/injection.rs`](src-tauri/src/injection.rs)). `confirm()` returns `true`; `prompt()` returns `""`. Sites that depend on these for safety checks (e.g. "Are you sure you want to delete?") will be auto-confirmed. This is intentional but worth knowing.

### Network interception

XHR and `fetch` requests are wrapped to capture URL, method, status, content type, and limited body previews (180-second TTL, stored in WebView memory only). Bodies are truncated to 200 chars. Captured data is exposed via `get-requests` and is **not** transmitted anywhere by dalzzack — but be aware that if the agent uploads this data to an LLM, request bodies (potentially including credentials) go with it.

### Execution context

`exec-js` runs in the page's main frame with full DOM access. There is no sandboxing. The agent driving dalzzack can read and modify any page state, including form data and cookies belonging to whatever site is currently loaded. Treat the daemon's WebView as a trusted execution environment for the agent.

### File system access

`save-image`, `screenshot`, `get-html` (its `save_to` argument), and `upload-file` all touch the local filesystem with the daemon process's permissions. There is no path restriction. Run the daemon as a non-privileged user.

---

### Self-update

`dalzzack upgrade` downloads a binary from the GitHub releases of this project and runs it. It checks the SHA-256 hash from the `.sha256` file of the same release, which detects a damaged download but not a compromised release. Use `--check` to see what would change, and `--tag` to choose a version.

## Hardening recommendations for operators

If you ship dalzzack as part of an agent product:

1. Run the daemon in a dedicated user account or container.
2. Restrict outbound network egress to the sites the agent actually needs.
3. Sanitize any URL before passing it to `navigate`.
4. Tear down the daemon between unrelated tasks rather than reusing a long-lived instance.
5. Treat the WebView's process memory as containing potentially sensitive data — don't core-dump it into shared logging.
