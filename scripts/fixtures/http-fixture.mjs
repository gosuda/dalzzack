#!/usr/bin/env node
/**
 * HTTP fixture for the 0.18 smoke tests. Runs as its OWN process: the test
 * runner drives the CLI with execSync, which blocks its event loop, so a server
 * living in the runner could never answer the navigations it is waiting for.
 *
 *   node scripts/fixtures/http-fixture.mjs <port>
 *
 * Routes:
 *   /            302 → /hop2 → 301 → /page     (redirect hops + set-cookie)
 *   /page        200, HttpOnly cookie, a 404 image, a script on an unsafe
 *                port (network error), a fetch that answers 500
 *   /challenge   403 `cf-mitigated: challenge` that reloads itself to ?solved=1
 *   /sw.html     registers /sw.js (a service worker with a global marker)
 *   /form        input + button recording event.isTrusted
 *   /frames      a cross-site (localhost) and a same-site iframe
 *   /hang        never answers
 *   /slow        answers after 3 s
 *   /links       a link to /r302 (302 → /page2), a form that POSTs to
 *                /submit (303 → /done), and a fragment link to #end
 */
import { createServer } from "http";

const port = Number(process.argv[2] || 18931);

createServer((req, res) => {
  const u = new URL(req.url, `http://127.0.0.1:${port}`);
  const p = u.pathname;
  const html = (body, headers = {}, status = 200) => {
    res.writeHead(status, { "Content-Type": "text/html", ...headers });
    res.end(body);
  };
  if (p === "/") {
    res.writeHead(302, { Location: "/hop2", "Set-Cookie": "first=1; Path=/" });
    return res.end();
  }
  if (p === "/hop2") {
    res.writeHead(301, { Location: "/page" });
    return res.end();
  }
  if (p === "/page") {
    return html(
      `<!doctype html><title>Fixture Page</title><h1>ok</h1>
<img src="/missing.png"><script src="http://127.0.0.1:1/nope.js"></script>
<script>fetch('/api/fail').catch(()=>{});</script>`,
      { "Set-Cookie": ["sid=secretvalue123; Path=/; HttpOnly", "pref=dark; Path=/"] },
    );
  }
  if (p === "/challenge") {
    if (u.searchParams.get("solved") !== "1") {
      return html(
        `<!doctype html><title>Just a moment...</title><script>setTimeout(()=>location.replace('/challenge?solved=1'),800)</script>`,
        { "cf-mitigated": "challenge", Server: "cloudflare" },
        403,
      );
    }
    return html(`<!doctype html><title>Through</title><p>through</p>`);
  }
  if (p === "/slow") {
    setTimeout(() => html(`<!doctype html><title>Slow</title><p>slow</p>`), 3000);
    return;
  }
  if (p === "/links") {
    return html(
      `<!doctype html><title>Links</title><a id=redirect href="/r302">redirect</a>
<form method=post action="/submit"><input name=q value=1><button id=post>post</button></form>
<a id=fragment href="#end">fragment</a><div style="height:3000px"></div><p id=end>end</p>`,
    );
  }
  if (p === "/r302") {
    res.writeHead(302, { Location: "/page2" });
    return res.end();
  }
  if (p === "/page2") {
    return html(`<!doctype html><title>Page 2</title><p>two</p>`);
  }
  if (p === "/submit") {
    res.writeHead(303, { Location: "/done" });
    return res.end();
  }
  if (p === "/done") {
    return html(`<!doctype html><title>Done</title><p>done</p>`);
  }
  if (p === "/hang") {
    // Never answers: what a stalled origin looks like to the browser.
    return;
  }
  if (p === "/api/fail") {
    res.writeHead(500, { "Content-Type": "application/json" });
    return res.end('{"err":1}');
  }
  if (p === "/sw.html") {
    return html(
      `<!doctype html><title>SW</title><script>navigator.serviceWorker.register('/sw.js').then(()=>document.title='SW registered')</script>`,
    );
  }
  if (p === "/sw.js") {
    res.writeHead(200, { "Content-Type": "text/javascript" });
    return res.end(
      `self.swMarker='hello-from-sw'; self.addEventListener('install',e=>self.skipWaiting()); self.addEventListener('activate',e=>e.waitUntil(clients.claim())); console.log('sw booted');`,
    );
  }
  if (p === "/form") {
    return html(
      `<!doctype html><title>Form</title><input id=q><button id=b onclick="window.__clicks.push(event.isTrusted)">Go</button><script>window.__clicks=[];</script>`,
    );
  }
  if (p === "/frames") {
    return html(
      `<!doctype html><title>Frames</title><iframe src="http://localhost:${port}/inner" name="xsite"></iframe><iframe src="/inner" name="same"></iframe>`,
    );
  }
  if (p === "/inner") {
    return html(`<!doctype html><title>inner</title><script>window.innerMarker=location.host</script><p>inner</p>`);
  }
  if (p === "/tall") {
    // A red box 2000px down: a viewport-relative clip on a scrolled page
    // must still capture red.
    return html(`<!doctype html><title>Tall</title><body style="margin:0"><div style="height:2000px"></div><div id=box style="width:200px;height:100px;background:rgb(255,0,0)"></div><div style="height:2000px"></div>`);
  }
  if (p === "/tabs") {
    return html(`<!doctype html><title>Tabs</title><input id=a><input id=b>`);
  }
  if (p === "/xframe") {
    // A cross-site frame that fetches and logs the moment it loads.
    return html(`<!doctype html><title>XFrame</title><iframe src="http://localhost:${port}/inner-fetch" name="xf"></iframe>`);
  }
  if (p === "/inner-fetch") {
    return html(`<!doctype html><title>inner-fetch</title><script>console.log('from-oopif');fetch('/api/first').then(r=>{window.firstStatus=r.status;document.title='done '+r.status})</script>`);
  }
  if (p === "/cors") {
    // A credentialed cross-origin fetch, for intercept mocks.
    return html(`<!doctype html><title>CORS</title><script>window.go=()=>fetch('http://localhost:${port}/api/xo',{credentials:'include',method:'POST',headers:{'content-type':'application/json'},body:'{}'}).then(r=>r.status,e=>String(e))</script>`);
  }
  if (p === "/xlog") {
    // The field-report repro: a localhost page with a cross-site (127.0.0.1)
    // child that floods the console, errors at once and again later.
    return html(`<!doctype html><title>XLog</title><script>for(let i=0;i<5;i++)console.log('parent-'+i)</script><iframe src="http://127.0.0.1:${port}/xlog-child" name="flood"></iframe>`);
  }
  if (p === "/xlog-child") {
    return html(`<!doctype html><title>flood</title><script>for(let i=0;i<700;i++)console.debug('child-'+i);console.error('child-error-early');setTimeout(()=>{for(let i=0;i<20;i++)console.debug('late-'+i);console.error('child-error-late')},1500)</script>`);
  }
  if (p === "/busy") {
    // A cross-site frame that logs, then spins its renderer for 15 s — what a
    // page computing a challenge looks like to an evaluate.
    return html(`<!doctype html><title>Busy</title><iframe src="http://localhost:${port}/spin" name="spin"></iframe>`);
  }
  if (p === "/spin") {
    return html(`<!doctype html><title>spin</title><script>console.log('spin-start');setTimeout(()=>{console.error('spin-now');const t=Date.now();while(Date.now()-t<15000);console.log('spin-end')},800)</script>`);
  }
  if (p === "/prerender") {
    return html(`<!doctype html><title>Prerender</title><script type="speculationrules">{"prerender":[{"source":"list","urls":["/inner"]}]}</script><a id=go href="/inner">go</a>`);
  }
  res.writeHead(404, { "Content-Type": "text/plain" });
  res.end("nope");
}).listen(port, "127.0.0.1", () => console.log(`fixture listening on ${port}`));
