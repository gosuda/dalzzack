### Did the page really load? What failed? (always on — no `--record`)

```bash
dalzzack navigate -i b --url "https://site.example" --field document  # main document: status, every hop, challenge
dalzzack nav-log -i b                                           # every document chain: 302s, 403 cf-mitigated, reloads
dalzzack failed-requests -i b                                   # failed script/image/fetch/… WITH the URL
dalzzack cookies -i b                                           # cookie jar incl. HttpOnly + parent domains
dalzzack start --id c --engine chrome                           # a REAL Chrome (what Chrome sees, not WebView2)
dalzzack reset -i c                                             # first-visit state, same id (private/chrome profiles)
```

**Navigate:**
- `navigate --url URL [--timeout-ms 5000]` — also returns the page `title` and the **main document's** outcome: `document` (status, every redirect hop with its `location`/`cf-mitigated`/`server`/set-cookie names, `challenge`, whether it `committed`), `navigations_during_wait`, `later_navigations` and a `hint` — so a challenge that solves itself reads as "403 `cloudflare:challenge`, then a second navigation", not as a plain success. WebView engine: waits `--timeout-ms`; Chrome engine: returns at the top frame's `load` (`load_event_ms`), at most `--timeout-ms`. In a pipe, a normal load prints only `status`, `url`, `title`, and `http`, and `document` stays for a redirect, HTTP 400 or more, a challenge, a second navigation, or a response that did not commit. `--full` and `--field document...` always read it
- `back --id ID` / `forward --id ID` — `history.back/forward()` with URL poll (up to 3 s); returns `before_url`, `after_url`, `elapsed_ms`, `changed`
- `reload --id ID [--hard]` — reload current page. `--hard` bypasses the HTTP cache (Windows: CDP `Page.reload {ignoreCache:true}`; macOS: `reloadFromOrigin`)
- `clear-site-data --id ID [--origin URL] [--types LIST]` — wipe an origin's storage out-of-band: service worker registrations, CacheStorage, localStorage, IndexedDB, cookies. Runs outside the page's JS world, so it works when a proxy/membrane or a policy hides `navigator.serviceWorker`. `--origin` defaults to the current page. Types: `appcache,cache_storage,cookies,file_systems,indexeddb,local_storage,service_workers,shader_cache,websql` or `all`. **macOS caveat: clears by HOST, not origin — scheme and port are not distinguished.**
- `get-text [--selector CSS] [--max-length N]` — the visible text of the page or one element, with empty space removed. This is the cheapest way to read a page
- `get-url`, `get-html [--selector CSS] [--no-strip-noise] [--max-length N]`, `screenshot [--output PATH] [--selector CSS]` — with `--selector`, an element that is not fully visible is scrolled into view first (every engine captures only what is on screen)
