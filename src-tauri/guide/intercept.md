**Fault injection (client-side network interception):** break the network on purpose to reproduce race/timeout bugs — no server access needed. Rules are applied via document-start injection, so they beat the app bundle's own `fetch` capture, persist across navigations, and die with the daemon.
- `intercept --pattern "*/portfolio*" --action hang [--ms N]` — matching requests never resolve (black hole). Honors the app's own AbortController/timeout; `--ms` optionally auto-errors after N ms. Pending requests show up in `get-requests` with `intercepted:"hang"`.
- `intercept --pattern "*/markets*" --action delay --ms 8000` — hold matching requests N ms, then pass through normally.
- `intercept --pattern "*/orders" --action fail` — immediate TypeError('Failed to fetch') network error.
- `intercept --pattern "*/quote*" --action mock [--status 500] [--body '{"err":1}'] [--content-type application/json] [--ms N]` — respond locally without touching the network.
- `--method POST` on any rule restricts it to one HTTP method. Pattern without `*` = substring match; with `*` = glob over the full URL. First matching rule wins; re-adding the same pattern replaces it.
- `intercept-list` — active rules + the rule-set version the live page has applied.
- `intercept-clear [--pattern STR]` — remove all rules, or just one pattern.
- On `--engine chrome` the same rules are enforced by Chrome itself through CDP's `Fetch` domain (`sync.transport: "cdp_fetch"`) — nothing is injected, `fetch.toString()` stays native. Same scope: fetch and XHR, in every frame (cross-site frames are held at start until their rules are in place); a `mock` also answers the request's CORS preflight locally and echoes the request's `Origin` with credentials allowed, so credentialed cross-origin mocks work.
