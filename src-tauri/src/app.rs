//! Tauri application setup and entry points.

#[cfg(target_os = "linux")]
use crate::platform::disable_web_security;
use crate::state::{APP_HANDLE, INSTANCE_ID, WINDOW_TITLE_CACHE};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use tauri::{WebviewUrl, WebviewWindowBuilder};

/// Inline landing page rendered when a daemon starts.
/// Embedded as a `data:` URL so it works regardless of where the binary is
/// installed (the embedded asset bundle is unreliable for standalone CLI installs).
const LANDING_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>dalzzack</title>
<style>
:root{
  color-scheme:light;
  --ink:#2b1b4e; --ink2:#43316c; --muted:#56437e;
  --round:ui-rounded,"SF Pro Rounded","Arial Rounded MT Bold","Segoe UI Black","Segoe UI Variable Display",system-ui,sans-serif;
  --sans:system-ui,-apple-system,"Segoe UI Variable Text","Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif;
  --mono:ui-monospace,"SF Mono","Cascadia Mono","Cascadia Code",Menlo,Consolas,monospace;
  --o:clamp(170px,35vmin,400px);
}
*{box-sizing:border-box}
html,body{margin:0}
body{font-family:var(--sans);color:var(--ink);-webkit-font-smoothing:antialiased;overflow-x:hidden;min-height:100vh}
code{font-family:var(--mono)}

/* ---------- sky ---------- */
.sky{position:fixed;top:0;left:0;right:0;bottom:0;z-index:-1;overflow:hidden;pointer-events:none;
  background:
    radial-gradient(55% 65% at 0% 0%,#ffd7ee 0%,rgba(255,215,238,0) 70%),
    radial-gradient(50% 60% at 100% 100%,#c4acf5 0%,rgba(196,172,245,0) 70%),
    radial-gradient(38% 48% at 72% 42%,rgba(255,255,255,.6) 0%,rgba(255,255,255,0) 70%),
    linear-gradient(112deg,#f6c9e6 0%,#ecd2f8 36%,#dcc7f8 68%,#cdb6f3 100%)}
.spk{position:absolute;width:18px;height:18px;fill:#fff8d2;filter:drop-shadow(0 0 5px rgba(255,240,170,.95))}
.spk.s{width:11px;height:11px}
.spk.w{fill:#fff;filter:drop-shadow(0 0 4px rgba(255,255,255,.95))}

/* ---------- puffy lettering ---------- */
.puffy{position:relative;display:inline-block;margin:0;font-family:var(--round);font-weight:900;line-height:1.08;letter-spacing:.025em;white-space:nowrap;padding:0 .08em}
.puffy::before{content:attr(data-text);content:attr(data-text)/"";position:absolute;left:.08em;top:0;color:#fff4fa;
  text-shadow:
    .04em 0 0 #fff4fa,-.04em 0 0 #fff4fa,0 .04em 0 #fff4fa,0 -.04em 0 #fff4fa,
    .028em .028em 0 #fff4fa,-.028em .028em 0 #fff4fa,.028em -.028em 0 #fff4fa,-.028em -.028em 0 #fff4fa,
    .075em 0 .012em #f2a0cd,-.075em 0 .012em #f2a0cd,0 .075em .012em #f2a0cd,0 -.075em .012em #f2a0cd,
    .053em .053em .012em #f2a0cd,-.053em .053em .012em #f2a0cd,.053em -.053em .012em #f2a0cd,-.053em -.053em .012em #f2a0cd,
    0 .125em .012em #d97fbf,.05em .105em .012em #d97fbf,-.05em .105em .012em #d97fbf,
    0 .2em .24em rgba(128,52,150,.42),0 0 .55em rgba(255,255,255,.75)}
.puffy>span{position:relative;background:linear-gradient(180deg,#fff 0%,#fff 22%,#ffe2f0 52%,#ffb6d9 100%);-webkit-background-clip:text;background-clip:text;color:transparent;-webkit-text-fill-color:transparent}
.puffy::after{content:attr(data-text);content:attr(data-text)/"";position:absolute;left:.08em;top:0;pointer-events:none;
  background:linear-gradient(180deg,rgba(255,255,255,0) 20%,rgba(255,255,255,.95) 27%,rgba(255,255,255,0) 40%);
  -webkit-background-clip:text;background-clip:text;color:transparent;-webkit-text-fill-color:transparent}

/* ---------- glass ---------- */
.glass{background:linear-gradient(180deg,rgba(255,255,255,.78),rgba(255,255,255,.56));
  border:1px solid rgba(255,255,255,.9);
  box-shadow:inset 0 1px 0 #fff,inset 0 -10px 24px rgba(196,160,246,.18),0 14px 34px rgba(132,84,196,.2);
  -webkit-backdrop-filter:blur(16px) saturate(150%);backdrop-filter:blur(16px) saturate(150%)}

/* ---------- hero ---------- */
.hero{position:relative;min-height:100vh;max-width:1760px;margin:0 auto;
  display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1.3fr);grid-template-rows:auto auto auto;align-content:center;
  grid-template-areas:"copy stage" "try try" "hint hint";
  padding:clamp(16px,3.5vh,48px) clamp(20px,4.5vw,88px) 10px;column-gap:clamp(8px,2vw,40px)}
.copy{grid-area:copy;align-self:end;position:relative;z-index:2;padding-bottom:calc(var(--o)*.06)}
.brand{font-size:clamp(56px,8.2vw,134px)}
.catch{font-size:clamp(24px,3.3vw,54px);margin-top:.12em}
.tagline{margin:clamp(14px,2.6vh,30px) 0 0;font-size:clamp(16px,1.45vw,22px);font-weight:600;color:var(--ink2);letter-spacing:.005em}

.status{display:inline-flex;align-items:center;gap:10px;margin-top:clamp(14px,2.4vh,26px);padding:.6em 1.07em .6em .87em;border-radius:999px;font-size:clamp(15px,1.05vw,20px);max-width:100%}
.dot{position:relative;flex:none;width:.8em;height:.8em;border-radius:50%;background:radial-gradient(circle at 35% 30%,#d9ffef 0,#5fe3aa 45%,#21b77a 100%);box-shadow:0 0 0 3px rgba(80,220,160,.22),0 0 10px rgba(60,210,150,.7)}
.ready{font-weight:800;color:#13704a;letter-spacing:.01em}
.sep{width:1px;height:1.1em;background:rgba(90,60,140,.22)}
.idl{font-size:.8em;font-weight:700;letter-spacing:.08em;text-transform:uppercase;color:var(--muted)}
.id{font-size:1em;font-weight:700;color:var(--ink);overflow-wrap:anywhere}

.try{grid-area:try;justify-self:start;display:flex;align-items:center;gap:12px;max-width:100%;margin-top:6px;padding:10px 20px 10px 10px;border-radius:18px;position:relative;z-index:2}
.try-l{flex:none;font:800 clamp(12px,.8vw,15px)/1 var(--sans);letter-spacing:.1em;text-transform:uppercase;color:#7a1f5d;background:linear-gradient(180deg,#ffe9f4,#ffd2e9);border:1px solid #fff;padding:8px 11px;border-radius:11px;box-shadow:0 2px 8px rgba(230,120,180,.25)}
.try code{font-size:clamp(13px,1.15vw,20px);color:var(--ink);overflow-wrap:anywhere;line-height:1.5}
.try .p{color:#a03a7c;font-weight:700;margin-right:.6em;-webkit-user-select:none;user-select:none}

.hint{grid-area:hint;justify-self:center;margin-top:clamp(10px,2vh,22px);display:flex;flex-direction:column;align-items:center;gap:2px;font-size:13px;font-weight:600;color:var(--muted);letter-spacing:.04em}
.hint svg{width:16px;height:16px;stroke:var(--muted);fill:none;stroke-width:2.4;stroke-linecap:round;stroke-linejoin:round}

/* ---------- stage ---------- */
.stage{grid-area:stage;position:relative;align-self:center;height:calc(var(--o)*1.62);min-height:0}
.stage>*{position:absolute}
.orbits{left:50%;top:calc(var(--o)*.68);width:calc(var(--o)*2.5);transform:translate(-50%,-50%);overflow:visible;filter:drop-shadow(0 0 4px rgba(255,250,220,.9))}
.beam{left:50%;top:calc(var(--o)*.72);width:calc(var(--o)*.7);height:calc(var(--o)*.66);transform:translateX(-50%);
  background:radial-gradient(42% 100% at 50% 100%,rgba(255,246,206,.95) 0%,rgba(255,240,214,.5) 45%,rgba(255,236,246,0) 100%)}
.pedestal{left:50%;top:calc(var(--o)*1.12);width:calc(var(--o)*1.34);transform:translateX(-50%)}
.orb{left:50%;top:calc(var(--o)*.18);width:var(--o);height:var(--o);margin-left:calc(var(--o)/-2);border-radius:50%;
  background:
    radial-gradient(circle at 31% 25%,rgba(255,255,255,.95) 0,rgba(255,255,255,.55) 5%,rgba(255,255,255,0) 17%),
    radial-gradient(circle at 50% 52%,rgba(255,240,250,.28) 0,rgba(236,200,252,.34) 52%,rgba(255,255,255,.5) 66%,rgba(204,168,250,.5) 85%,rgba(255,255,255,.85) 100%);
  box-shadow:0 0 0 2px rgba(255,255,255,.6),inset 0 0 34px rgba(255,255,255,.75),inset 0 -26px 54px rgba(170,122,238,.35),0 0 70px 14px rgba(255,212,240,.65),0 34px 70px rgba(130,84,206,.28);
  -webkit-backdrop-filter:blur(2px) saturate(130%);backdrop-filter:blur(2px) saturate(130%)}
.orb::after{content:"";position:absolute;top:5%;left:5%;right:5%;bottom:5%;border-radius:50%;border:3px solid transparent;border-top-color:rgba(255,255,255,.85);border-left-color:rgba(255,255,255,.35);transform:rotate(-24deg)}
.orb::before{content:"";position:absolute;right:16%;bottom:12%;width:16%;height:8%;border-radius:50%;background:rgba(255,255,255,.55);transform:rotate(-38deg);filter:blur(1px)}
.mascot{position:absolute;left:14%;top:12%;width:72%;height:72%;filter:drop-shadow(0 6px 10px rgba(220,110,170,.25))}

/* floating glass windows + tiles */
.win,.tile,.bubble{font-size:calc(var(--o)*.05);background:linear-gradient(160deg,rgba(255,255,255,.86),rgba(246,232,255,.58));border:1px solid rgba(255,255,255,.95);
  box-shadow:inset 0 1px 0 #fff,inset 0 -.5em 1em rgba(186,146,240,.25),0 .7em 1.8em rgba(140,92,210,.24),0 0 1.4em rgba(255,255,255,.55);
  -webkit-backdrop-filter:blur(10px);backdrop-filter:blur(10px)}
.win{width:calc(var(--o)*.48);padding:.5em .55em .6em;border-radius:1.05em}
.bar{display:flex;gap:.3em;margin:0 0 .5em .15em}
.bar i{width:.6em;height:.6em;border-radius:50%;background:#ff7f9f;box-shadow:inset 0 -1px 2px rgba(0,0,0,.12)}
.bar i:nth-child(2){background:#ffc85c}.bar i:nth-child(3){background:#a98af4}
.pane{position:relative;height:4.4em;border-radius:.7em;background:linear-gradient(180deg,rgba(236,222,255,.9),rgba(246,236,255,.8));box-shadow:inset 0 1px 3px rgba(140,100,210,.25)}
.btn{position:absolute;left:12%;top:36%;width:58%;height:1.25em;border-radius:1em;background:linear-gradient(180deg,#ffb3cd,#ff86ad);box-shadow:inset 0 2px 2px rgba(255,255,255,.6),0 .2em .5em rgba(240,100,150,.35)}
.cursor{position:absolute;left:58%;top:44%;width:2.3em;height:2.3em}
.burst{position:absolute;left:62%;top:6%;width:1.8em;height:1.6em}
.field{position:absolute;left:9%;right:9%;top:30%;height:1.6em;border-radius:1em;background:#fff;box-shadow:inset 0 1px 2px rgba(140,100,210,.3),0 0 0 .14em rgba(196,168,250,.6)}
.field b{position:absolute;left:.35em;top:.22em;width:1.15em;height:1.15em;border-radius:.3em;background:#e6dbff;color:#7b5cc4;font:800 .8em/1.45 var(--mono);text-align:center}
.field i{position:absolute;left:1.9em;top:.35em;width:.12em;height:.9em;background:#9b80dc}
.pic{position:absolute;left:10%;top:12%;width:46%;height:76%}
.lines{position:absolute;left:62%;right:10%;top:22%}
.lines i{display:block;height:.38em;margin-bottom:.45em;border-radius:1em;background:rgba(156,124,220,.35)}
.lines i:nth-child(2){width:70%}
.okc{position:absolute;left:50%;top:50%;width:2.6em;height:2.6em;margin:-1.3em 0 0 -1.3em}
.tile{width:calc(var(--o)*.21);height:calc(var(--o)*.21);border-radius:1em;display:flex;align-items:center;justify-content:center}
.tile svg{width:62%;height:62%}
.tile.list{width:calc(var(--o)*.34);height:calc(var(--o)*.27)}
.tile.list svg{width:78%;height:78%}
.bubble{width:calc(var(--o)*.19);height:calc(var(--o)*.19);border-radius:50%;display:flex;align-items:center;justify-content:center}
.bubble svg{width:64%;height:64%}

.w1{left:calc(50% - var(--o)*1.16);top:calc(var(--o)*.02);rotate:-5deg}
.w2{left:calc(50% + var(--o)*.5);top:calc(var(--o)*-.02);rotate:3deg}
.w3{left:calc(50% + var(--o)*.74);top:calc(var(--o)*.5);rotate:5deg}
.w4{left:calc(50% + var(--o)*.66);top:calc(var(--o)*1.06);rotate:-4deg}
.t1{left:calc(50% - var(--o)*.98);top:calc(var(--o)*.56);rotate:-6deg}
.t2{left:calc(50% + var(--o)*.36);top:calc(var(--o)*.3);rotate:8deg}
.t3{left:calc(50% + var(--o)*.42);top:calc(var(--o)*.92);rotate:-5deg}
.t4{left:calc(50% - var(--o)*1.12);top:calc(var(--o)*1.12);rotate:4deg}
.b1{left:calc(50% - var(--o)*.74);top:calc(var(--o)*.9)}
.sp1{left:calc(50% + var(--o)*.3);top:calc(var(--o)*.06)}
.sp2{left:calc(50% - var(--o)*.62);top:calc(var(--o)*.3)}
.sp3{left:calc(50% + var(--o)*.62);top:calc(var(--o)*.86)}
.sp4{left:calc(50% - var(--o)*.5);top:calc(var(--o)*1.34)}

/* ---------- commands ---------- */
.cmds{max-width:1280px;margin:8px auto 0;padding:30px clamp(20px,3vw,40px) 30px;border-radius:32px;width:calc(100% - 2*clamp(16px,4.5vw,88px))}
.cmds-h{display:flex;align-items:baseline;gap:16px;flex-wrap:wrap;margin:0 0 22px}
.cmds-h .puffy{font-size:clamp(30px,3vw,42px)}
.cmds-h p{margin:0;color:var(--muted);font-size:14px}
.groups{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:20px}
.g{padding:10px 10px 6px;border-radius:20px;background:linear-gradient(170deg,rgba(255,255,255,.82),rgba(250,243,255,.62));border:1px solid #fff;
  box-shadow:inset 0 -8px 18px rgba(190,150,240,.14),0 10px 24px rgba(140,92,210,.14)}
.gbar{display:flex;align-items:center;gap:5px;padding:0 0 10px 4px}
.gbar>i{flex:none;width:9px;height:9px;border-radius:50%;background:#ff7f9f}
.gbar>i:nth-child(2){background:#ffc85c}.gbar>i:nth-child(3){background:#a98af4}
.g h3{flex:1;display:flex;align-items:center;gap:8px;margin:0 0 0 7px;padding:7px 11px;border-radius:999px;background:rgba(238,228,255,.9);box-shadow:inset 0 1px 2px rgba(140,100,210,.2);font:800 12px/1 var(--sans);letter-spacing:.11em;text-transform:uppercase;color:var(--ink2)}
.g h3 b{flex:none;width:11px;height:11px;border-radius:50%;background:radial-gradient(circle at 35% 30%,#fff 0,var(--c) 60%);box-shadow:0 1px 4px var(--c)}
.g ul{list-style:none;margin:0;padding:2px 8px 0}
.g li{margin:0 0 10px}
.g code{display:block;font-size:14px;font-weight:700;color:var(--ink)}
.g li span{display:block;margin-top:1px;font-size:13px;line-height:1.35;color:var(--muted)}
.sticker{position:relative;min-height:180px;border-radius:20px;border:1.5px dashed rgba(255,255,255,.9);display:flex;align-items:center;justify-content:center}
.mini{width:62%;max-width:190px;aspect-ratio:1;border-radius:50%;display:flex;align-items:center;justify-content:center;
  background:radial-gradient(circle at 31% 25%,rgba(255,255,255,.95) 0,rgba(255,255,255,0) 18%),radial-gradient(circle,rgba(255,240,250,.3) 0,rgba(236,200,252,.35) 55%,rgba(255,255,255,.7) 100%);
  box-shadow:0 0 0 2px rgba(255,255,255,.6),inset 0 -14px 30px rgba(170,122,238,.3),0 0 40px rgba(255,212,240,.7)}
.mini svg{width:74%;height:74%}
.c1{--c:#a98af4}.c2{--c:#7fb8ff}.c3{--c:#ff8fbf}.c4{--c:#ffab76}.c5{--c:#f5c445}.c6{--c:#4fd3a0}.c7{--c:#c57ef0}

footer{text-align:center;padding:22px 20px 34px;font-size:14px;color:var(--muted)}
footer code{font-size:13px;color:var(--ink);background:rgba(255,255,255,.7);border:1px solid #fff;padding:2px 7px;border-radius:7px}

/* ---------- responsive ---------- */
@media (max-width:1100px){ .groups{grid-template-columns:repeat(3,minmax(0,1fr))} }
@media (max-width:760px){ .groups{grid-template-columns:repeat(2,minmax(0,1fr))} }
@media (max-width:860px){
  .hero{grid-template-columns:minmax(0,1fr) minmax(0,1fr)}
  :root{--o:clamp(150px,31vmin,300px)}
  .w1,.t4{display:none}
}
@media (max-width:620px){
  .hero{grid-template-columns:1fr;grid-template-areas:"copy" "try" "stage" "hint"}
  .stage{margin-top:20px}
}
@media (max-height:640px){ .hint{display:none} }

/* ---------- motion ---------- */
@media (prefers-reduced-motion:no-preference){
  .mascot{animation:float 5.5s ease-in-out infinite}
  .win,.tile,.bubble{animation:bob 6s ease-in-out infinite}
  .w2{animation-delay:-1.5s}.w3{animation-delay:-3s}.w4{animation-delay:-4.5s}
  .t1{animation-delay:-2s}.t2{animation-delay:-.8s}.t3{animation-delay:-3.6s}.t4{animation-delay:-5s}.b1{animation-delay:-2.7s}
  .spk{animation:tw 3.4s ease-in-out infinite}
  .spk:nth-of-type(2n){animation-delay:-1.1s}.spk:nth-of-type(3n){animation-delay:-2.3s}
  .dot::after{content:"";position:absolute;top:-4px;left:-4px;right:-4px;bottom:-4px;border-radius:50%;border:2px solid rgba(60,210,150,.55);animation:ping 2.4s ease-out infinite}
  .hint svg{animation:nudge 2.2s ease-in-out infinite}
  @keyframes float{0%,100%{transform:translateY(0)}50%{transform:translateY(-4%)}}
  @keyframes bob{0%,100%{translate:0 0}50%{translate:0 -7px}}
  @keyframes tw{0%,100%{opacity:.35;transform:scale(.7)}50%{opacity:1;transform:scale(1)}}
  @keyframes ping{0%{opacity:.9;transform:scale(.6)}80%,100%{opacity:0;transform:scale(1.6)}}
  @keyframes nudge{0%,100%{transform:translateY(0)}50%{transform:translateY(4px)}}
}
</style>
</head>
<body>
<svg width="0" height="0" style="position:absolute" aria-hidden="true">
  <defs>
    <linearGradient id="gm" x1="0" y1="0" x2=".25" y2="1"><stop offset="0" stop-color="#fff8d4"/><stop offset=".5" stop-color="#ffe390"/><stop offset=".84" stop-color="#ffc6b4"/><stop offset="1" stop-color="#ffaac8"/></linearGradient>
    <linearGradient id="gs" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#ffb8d0"/><stop offset="1" stop-color="#f079a6"/></linearGradient>
    <radialGradient id="gc" cx=".5" cy=".4" r=".7"><stop offset="0" stop-color="#fffbf2"/><stop offset="1" stop-color="#ffe8cc"/></radialGradient>
  </defs>
  <symbol id="sp" viewBox="-10 -10 20 20"><path d="M0-10C1-3 3-1 10 0 3 1 1 3 0 10-1 3-3 1-10 0-3-1-1-3 0-10Z"/></symbol>
  <symbol id="mascot" viewBox="0 0 200 200">
    <path d="M42 152c0-14 25-20 58-20s58 6 58 20c0 16-26 26-58 26s-58-10-58-26z" fill="url(#gs)"/>
    <ellipse cx="100" cy="146" rx="58" ry="12" fill="url(#gc)" stroke="#fff4e4" stroke-width="8" stroke-linecap="round" stroke-dasharray="0 10.6"/>
    <path d="M45 132c0-12 25-18 55-18s55 6 55 18c0 9-25 14-55 14s-55-5-55-14z" fill="url(#gs)"/>
    <path d="M89.7 13.9A60 60 0 1 0 158.7 85.5 50 50 0 0 1 89.7 13.9z" fill="url(#gm)" stroke="url(#gm)" stroke-width="12" stroke-linejoin="round"/>
    <path d="M51 90c-3-17 2-34 13-46" fill="none" stroke="#fff" stroke-width="7" stroke-linecap="round" opacity=".85"/>
    <circle cx="69" cy="31" r="3.4" fill="#fff" opacity=".9"/>
    <g fill="#fff4b8"><path d="M146 30c1.4 8 4 10.6 12 12-8 1.4-10.6 4-12 12-1.4-8-4-10.6-12-12 8-1.4 10.6-4 12-12z"/><path d="M170 62c.8 4.6 2.3 6.1 6.9 6.9-4.6.8-6.1 2.3-6.9 6.9-.8-4.6-2.3-6.1-6.9-6.9 4.6-.8 6.1-2.3 6.9-6.9z"/></g>
  </symbol>
</svg>

<div class="sky" aria-hidden="true">
  <svg class="spk" style="left:6%;top:12%"><use href="#sp"/></svg>
  <svg class="spk s w" style="left:31%;top:7%"><use href="#sp"/></svg>
  <svg class="spk s" style="left:44%;top:84%"><use href="#sp"/></svg>
  <svg class="spk w" style="left:3%;top:78%"><use href="#sp"/></svg>
  <svg class="spk s w" style="left:95%;top:22%"><use href="#sp"/></svg>
  <svg class="spk s" style="left:91%;top:62%"><use href="#sp"/></svg>
  <svg class="spk s w" style="left:22%;top:94%"><use href="#sp"/></svg>
</div>

<section class="hero">
  <div class="copy">
    <h1 class="puffy brand" data-text="dalzzack"><span>dalzzack</span></h1><br>
    <p class="puffy catch" data-text="with Agent Together"><span>with Agent Together</span></p>
    <p class="tagline">Browser automation, ready for your agent.</p>
    <div class="status glass"><span class="dot"></span><span class="ready">ready</span><span class="sep"></span><span class="idl">id</span><code class="id">__INSTANCE_ID__</code></div>
  </div>

  <div class="stage" aria-hidden="true">
    <svg class="orbits" viewBox="0 0 600 360">
      <defs>
        <linearGradient id="ol" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#fff" stop-opacity=".2"/><stop offset=".3" stop-color="#fff6d8" stop-opacity=".95"/><stop offset=".7" stop-color="#fff" stop-opacity=".9"/><stop offset="1" stop-color="#ffe9f6" stop-opacity=".25"/></linearGradient>
        <radialGradient id="pe" cx=".35" cy=".3" r=".7"><stop offset="0" stop-color="#fff"/><stop offset=".6" stop-color="#ffe3f2"/><stop offset="1" stop-color="#e2c8ff"/></radialGradient>
      </defs>
      <g fill="none" stroke="url(#ol)" stroke-width="2.4">
        <ellipse cx="300" cy="180" rx="286" ry="86" transform="rotate(-11 300 180)"/>
        <ellipse cx="300" cy="180" rx="236" ry="150" transform="rotate(16 300 180)" stroke-width="1.8"/>
      </g>
      <g fill="url(#pe)">
        <g transform="rotate(-11 300 180)"><circle cx="34" cy="148" r="8"/><circle cx="566" cy="212" r="7"/><circle cx="160" cy="105" r="6"/><circle cx="448" cy="257" r="7"/></g>
        <g transform="rotate(16 300 180)"><circle cx="120" cy="278" r="7"/><circle cx="480" cy="82" r="6"/><circle cx="300" cy="330" r="5"/></g>
      </g>
    </svg>
    <div class="beam"></div>
    <svg class="pedestal" viewBox="0 0 300 100">
      <defs>
        <linearGradient id="ps" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#efe3ff"/><stop offset="1" stop-color="#c1a4f2"/></linearGradient>
        <radialGradient id="pt" cx=".5" cy=".45" r=".62"><stop offset="0" stop-color="#fffbe8"/><stop offset=".38" stop-color="#fde4f4"/><stop offset="1" stop-color="#dbc5fb"/></radialGradient>
        <radialGradient id="pg" cx=".5" cy=".5" r=".5"><stop offset="0" stop-color="#fff3cc"/><stop offset="1" stop-color="#fff3cc" stop-opacity="0"/></radialGradient>
      </defs>
      <ellipse cx="150" cy="66" rx="150" ry="34" fill="url(#pg)"/>
      <path d="M22 50v14c0 14 57 25 128 25s128-11 128-25V50z" fill="url(#ps)"/>
      <ellipse cx="150" cy="50" rx="128" ry="23" fill="url(#pt)"/>
      <path d="M64 33v10c0 9 38 15 86 15s86-6 86-15V33z" fill="url(#ps)"/>
      <ellipse cx="150" cy="33" rx="86" ry="14" fill="url(#pt)"/>
      <ellipse cx="150" cy="33" rx="58" ry="8.5" fill="none" stroke="#fff4cf" stroke-width="3"/>
      <ellipse cx="150" cy="33" rx="46" ry="6" fill="url(#pg)"/>
    </svg>
    <div class="orb">
      <svg class="mascot" viewBox="0 0 200 200"><use href="#mascot"/></svg>
    </div>

    <div class="win w1"><div class="bar"><i></i><i></i><i></i></div><div class="pane">
      <span class="btn"></span>
      <svg class="burst" viewBox="0 0 36 32" fill="none" stroke="#ffcf4d" stroke-width="3.4" stroke-linecap="round"><path d="M6 20 2 16M14 12l-2-8M24 13l5-7M27 22l6-1"/></svg>
      <svg class="cursor" viewBox="0 0 40 40"><path d="M8 4l24 15-10 2.6L27 33l-5 2.4-5-11.4-7.6 7.2z" fill="#f4eeff" stroke="#9a7ae0" stroke-width="2.6" stroke-linejoin="round"/></svg>
    </div></div>
    <div class="win w2"><div class="bar"><i></i><i></i><i></i></div><div class="pane"><span class="field"><b>I</b><i></i></span></div></div>
    <div class="win w3"><div class="bar"><i></i><i></i><i></i></div><div class="pane">
      <svg class="pic" viewBox="0 0 46 34"><rect x="1" y="1" width="44" height="32" rx="5" fill="#f6efff" stroke="#c8b2f6" stroke-width="2"/><circle cx="14" cy="11" r="4" fill="#c7b0f7"/><path d="M4 30l12-12 9 9 6-6 11 9z" fill="#b597f2"/></svg>
      <div class="lines"><i></i><i></i><i></i></div>
    </div></div>
    <div class="win w4"><div class="bar"><i></i><i></i><i></i></div><div class="pane">
      <svg class="okc" viewBox="0 0 52 52"><g stroke="#ffd25e" stroke-width="3" stroke-linecap="round"><path d="M26 1v5M47 9l-4 4M5 9l4 4M51 30h-5M1 30h5"/></g><circle cx="26" cy="30" r="15" fill="#7fe0a6" stroke="#fff" stroke-width="2.5"/><path d="M19 30l5 5 9-10" fill="none" stroke="#fff" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/></svg>
    </div></div>
    <div class="tile t1"><svg viewBox="0 0 40 40" fill="none" stroke="#8d6ad8" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"><path d="M13 12 5 20l8 8M27 12l8 8-8 8M23 9l-6 22"/></svg></div>
    <div class="tile t2"><svg viewBox="0 0 40 40"><path d="M13 8.5v23c0 1.6 1.7 2.5 3 1.7l17-11.5c1.2-.8 1.2-2.6 0-3.4L16 6.8c-1.3-.8-3 .1-3 1.7z" fill="#ffc35c"/></svg></div>
    <div class="tile t3"><svg viewBox="0 0 40 40"><rect x="6" y="22" width="6" height="12" rx="3" fill="#ffc35c"/><rect x="17" y="15" width="6" height="19" rx="3" fill="#ff9fc4"/><rect x="28" y="7" width="6" height="27" rx="3" fill="#ffd36e"/></svg></div>
    <div class="tile list t4"><svg viewBox="0 0 60 44" fill="none" stroke-linecap="round" stroke-linejoin="round"><g stroke="#9b7be0" stroke-width="3"><path d="M5 9l3 3 6-7M5 23l3 3 6-7M5 37l3 3 6-7"/></g><g stroke="#cbb7f5" stroke-width="4"><path d="M22 9h32M22 23h28M22 37h30"/></g></svg></div>
    <div class="bubble b1"><svg viewBox="0 0 40 40"><circle cx="20" cy="20" r="17" fill="#86e3ad"/><path d="M12 20.5l5.5 5.5L28.5 14" fill="none" stroke="#fff" stroke-width="4.4" stroke-linecap="round" stroke-linejoin="round"/></svg></div>
    <svg class="spk sp1"><use href="#sp"/></svg>
    <svg class="spk s w sp2"><use href="#sp"/></svg>
    <svg class="spk s sp3"><use href="#sp"/></svg>
    <svg class="spk w sp4"><use href="#sp"/></svg>
  </div>

  <div class="try glass"><span class="try-l">try</span><code><span class="p">$</span>dalzzack navigate -i __INSTANCE_ID__ --url "https://example.com"</code></div>
  <div class="hint">commands<svg viewBox="0 0 24 24"><path d="M6 9l6 6 6-6"/></svg></div>
</section>

<section class="cmds glass">
  <div class="cmds-h"><h2 class="puffy" data-text="commands"><span>commands</span></h2></div>
  <div class="groups">
    <div class="g c1"><div class="gbar"><i></i><i></i><i></i><h3><b></b>Instance</h3></div><ul>
      <li><code>start</code><span>Open a browser window</span></li>
      <li><code>list</code><span>Show running instances</span></li>
      <li><code>stop</code><span>Close an instance</span></li>
      <li><code>kill</code><span>Force-stop a stuck daemon</span></li>
      <li><code>resize</code><span>Change the window size</span></li>
      <li><code>reset</code><span>Back to a first-visit state</span></li>
      <li><code>record</code><span>Turn the event tape on/off</span></li>
    </ul></div>
    <div class="g c2"><div class="gbar"><i></i><i></i><i></i><h3><b></b>Navigate</h3></div><ul>
      <li><code>navigate</code><span>Open a URL and wait</span></li>
      <li><code>back</code><span>Go back in history</span></li>
      <li><code>forward</code><span>Go forward in history</span></li>
      <li><code>reload</code><span>Reload, optionally hard</span></li>
      <li><code>get-url</code><span>Read the current URL</span></li>
      <li><code>get-html</code><span>Page or element HTML</span></li>
      <li><code>clear-site-data</code><span>Wipe one origin's storage</span></li>
    </ul></div>
    <div class="g c3"><div class="gbar"><i></i><i></i><i></i><h3><b></b>Look</h3></div><ul>
      <li><code>describe</code><span>Annotated screenshot + JSON</span></li>
      <li><code>screenshot</code><span>Capture the viewport</span></li>
      <li><code>check-visible</code><span>Is a selector visible?</span></li>
      <li><code>element-at</code><span>Element at a point</span></li>
      <li><code>get-frames</code><span>List iframes and frame ids</span></li>
      <li><code>get-frame-html</code><span>HTML inside an iframe</span></li>
    </ul></div>
    <div class="g c4"><div class="gbar"><i></i><i></i><i></i><h3><b></b>Act</h3></div><ul>
      <li><code>click</code><span>Selector, text or point</span></li>
      <li><code>hover</code><span>Hover to open menus</span></li>
      <li><code>fill-form</code><span>Fill one or many fields</span></li>
      <li><code>keyboard</code><span>Type text or press keys</span></li>
      <li><code>scroll</code><span>Scroll page or container</span></li>
      <li><code>upload-file</code><span>Attach a file to an input</span></li>
      <li><code>save-image</code><span>Save an image to disk</span></li>
      <li><code>dismiss-modals</code><span>Close popups and modals</span></li>
    </ul></div>
    <div class="g c5"><div class="gbar"><i></i><i></i><i></i><h3><b></b>Wait</h3></div><ul>
      <li><code>wait-for</code><span>Until a selector appears</span></li>
      <li><code>wait-navigation</code><span>Until the document changes</span></li>
      <li><code>wait</code><span>Sleep for milliseconds</span></li>
    </ul></div>
    <div class="g c6"><div class="gbar"><i></i><i></i><i></i><h3><b></b>Network</h3></div><ul>
      <li><code>nav-log</code><span>Redirects and challenges</span></li>
      <li><code>failed-requests</code><span>Failed loads with URLs</span></li>
      <li><code>response-headers</code><span>Headers, document included</span></li>
      <li><code>get-requests</code><span>Captured fetch and XHR</span></li>
      <li><code>cookies</code><span>List or clear the jar</span></li>
      <li><code>intercept</code><span>Hang, delay, fail or mock</span></li>
    </ul></div>
    <div class="g c7"><div class="gbar"><i></i><i></i><i></i><h3><b></b>Debug</h3></div><ul>
      <li><code>console-logs</code><span>Console output and CSP</span></li>
      <li><code>exec-js</code><span>Run JavaScript in the page</span></li>
      <li><code>inject-script</code><span>Run before page scripts</span></li>
      <li><code>pause</code><span>Inspect a frozen renderer</span></li>
      <li><code>dump-recording</code><span>Read the event tape</span></li>
      <li><code>trace</code><span>Chrome-tracing timeline</span></li>
    </ul></div>
    <div class="sticker" aria-hidden="true">
      <div class="mini"><svg viewBox="0 0 200 200"><use href="#mascot"/></svg></div>
      <svg class="spk" style="left:22%;top:20%"><use href="#sp"/></svg>
      <svg class="spk s w" style="left:74%;top:16%"><use href="#sp"/></svg>
      <svg class="spk s" style="left:80%;top:70%"><use href="#sp"/></svg>
      <svg class="spk s w" style="left:16%;top:74%"><use href="#sp"/></svg>
    </div>
  </div>
</section>

<footer>run <code>dalzzack tools</code> for JSON schemas · <code>dalzzack --help</code> for all options</footer>
</body>
</html>"##;

fn landing_data_url(instance_id: &str) -> String {
    let html = LANDING_HTML.replace("__INSTANCE_ID__", instance_id);
    format!("data:text/html;charset=utf-8;base64,{}", B64.encode(html))
}

/// Everything the WebView needs at construction time.
///
/// These are all one-shot: WebView2 reads browser args only at environment
/// creation, the User-Agent is a builder parameter, and the injected script must
/// be registered before the first document. None of it can be changed on a live
/// daemon, which is why `start` reports ignored flags rather than pretending.
#[derive(Debug, Clone, Default)]
pub struct AppOptions {
    pub instance_id: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Extra Chromium command-line flags (Windows/WebView2 only).
    pub browser_args: Vec<String>,
    /// `None` keeps the engine's own UA — see [`crate::ua`] for when that is
    /// the right answer.
    pub user_agent: Option<String>,
    /// Install the detectable in-page console hook (platforms with no
    /// out-of-band console transport).
    pub console_fallback: bool,
    /// Engine build number for client-hint consistency, when a UA override
    /// makes that necessary.
    pub ua_full_version: Option<String>,
    /// WebView2 user data folder. `None` = the folder every plain daemon
    /// shares. Must be `Some` whenever `browser_args` is non-empty (see the
    /// note above `run_with_instance_id`).
    pub data_directory: Option<std::path::PathBuf>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    run_with_instance_id(AppOptions::default());
}

/// wry's own default browser args. Passing `additional_browser_args` replaces
/// them wholesale, so anything we add has to carry these along or the webview
/// silently changes behaviour (OOUI popouts, SmartScreen) as a side effect of
/// an unrelated flag.
#[cfg(target_os = "windows")]
const WRY_DEFAULT_BROWSER_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

// Why `--browser-arg` always gets a private WebView2 user data folder:
//
// WebView2 allows exactly one set of `AdditionalBrowserArguments` per user data
// folder. A second environment over the same folder with *different* arguments
// fails outright with `ERROR_INVALID_STATE` (0x8007139F) — and Tauri logs that
// failure and carries on with a window that has no webview attached, so the
// daemon looks alive while every JS call times out. Since all dalzzack
// daemons share one folder by default, ANY `--browser-arg` daemon started
// alongside a plain one used to come up silently broken.
//
// Instances without custom arguments keep sharing the default folder (and with
// it cookies and logins across ids); only the ones that must differ — or that
// asked for `--private-profile` — get their own
// (`crate::profile::private_webview_dir`, chosen by `run_daemon`).

/// macOS: show the dalzzack icon in the Dock and the app switcher.
///
/// The icon of a macOS app comes from its `.app` bundle, and the release is a
/// bare binary (`tauri build --no-bundle`). Tauri sets the Dock icon itself only
/// in dev builds, so a release showed the generic executable icon. Must run on
/// the main thread, as the `setup` hook does.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    const ICON: &[u8] = include_bytes!("../icons/icon.icns");
    unsafe {
        let data = objc2_foundation::NSData::with_bytes(ICON);
        let image: *mut AnyObject = msg_send![class!(NSImage), alloc];
        let image: *mut AnyObject = msg_send![image, initWithData: &*data];
        if image.is_null() {
            tracing::warn!("[macOS] the embedded icon.icns is not an image; Dock icon unchanged");
            return;
        }
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
    tracing::info!("[macOS] Dock icon set from the embedded icon.icns");
}

/// Run with an instance ID for parallel execution support.
pub fn run_with_instance_id(opts: AppOptions) {
    let AppOptions {
        instance_id,
        width,
        height,
        browser_args,
        user_agent,
        console_fallback,
        ua_full_version,
        data_directory,
    } = opts;

    // Build the document-start script now: it has to be registered before the
    // first document, so every decision it depends on is already made.
    let init_script =
        crate::injection::initialization_script(&crate::injection::InjectionOptions {
            console_fallback,
            stealth_script: crate::ua::consistency_script(
                user_agent.is_some(),
                ua_full_version.as_deref(),
            )
            .unwrap_or_default(),
        });

    // Windows: WebView2 security is handled via CDP (Page.setBypassCSP)
    // after WebView creation, not via browser arguments.
    // See setup_cdp_for_webview() in webview/frame_injection.rs
    #[cfg(not(target_os = "windows"))]
    if !browser_args.is_empty() {
        tracing::warn!(
            "--browser-arg is Windows/WebView2 only; ignoring {} argument(s)",
            browser_args.len()
        );
    }

    let window_title = match &instance_id {
        Some(id) => format!("dalzzack-{}", id),
        None => "dalzzack".to_string(),
    };

    // Update title cache for spawn_blocking access
    {
        let mut cache = WINDOW_TITLE_CACHE.lock().unwrap();
        *cache = window_title.clone();
    }

    let instance_id_clone = instance_id.clone();

    let win_width = width.unwrap_or(1200) as f64;
    let win_height = height.unwrap_or(800) as f64;

    // Inline landing page as a data: URL so the daemon always shows something
    // on start, regardless of whether the Tauri asset bundle is reachable from
    // the installed binary location (e.g. ~/.cargo/bin standalone installs).
    let landing_id = instance_id.clone().unwrap_or_else(|| "default".to_string());
    let landing_url_str = landing_data_url(&landing_id);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![])
        .setup(move |app| {
            let handle = app.handle().clone();

            tracing::info!(
                "Creating main window with title: {} ({}x{})",
                window_title,
                win_width,
                win_height
            );

            let landing =
                url::Url::parse(&landing_url_str).expect("inline landing data URL must be valid");

            let close_id = instance_id_clone.clone();
            #[allow(unused_mut)]
            let mut builder =
                WebviewWindowBuilder::new(&handle, "main", WebviewUrl::External(landing));

            // Chromium flags (`--enable-heap-profiling`, `--js-flags=…`, …).
            // WebView2 only reads these at environment creation, so there is no
            // way to set them after the fact — they have to come from `start`.
            #[cfg(target_os = "windows")]
            if !browser_args.is_empty() {
                let joined = format!("{} {}", WRY_DEFAULT_BROWSER_ARGS, browser_args.join(" "));
                tracing::info!("Additional WebView2 browser args: {}", joined);
                builder = builder.additional_browser_args(&joined);
            }
            // A private folder for `--private-profile`, and always for
            // `--browser-arg` — see the note above `run_with_instance_id` for
            // why a shared folder is fatal there.
            #[cfg(target_os = "windows")]
            if let Some(dir) = data_directory.clone() {
                tracing::info!("WebView2 user data folder: {}", dir.display());
                builder = builder.data_directory(dir);
            }
            #[cfg(not(target_os = "windows"))]
            let _ = &data_directory;

            // Only set when we have something better than the engine's own UA —
            // overriding a UA that already agrees with the engine's Sec-CH-UA
            // headers can only introduce drift. See `crate::ua`.
            if let Some(ua) = user_agent.as_deref() {
                tracing::info!("Applying User-Agent override: {}", ua);
                builder = builder.user_agent(ua);
            }

            let window = builder
                .title(&window_title)
                .inner_size(win_width, win_height)
                .min_inner_size(400.0, 300.0)
                .resizable(true)
                .center()
                .visible(true)
                .focused(true)
                .initialization_script(&init_script)
                // Enable F12 / right-click "Inspect" / attach-devtools. Effective
                // only when the `tauri/devtools` feature is on (see Cargo.toml —
                // enabled by default), otherwise a harmless no-op.
                .devtools(true)
                .build()
                .unwrap_or_else(|e| {
                    tracing::error!("!!! Failed to create main window: {} !!!", e);
                    panic!("Failed to create main window: {}", e);
                });

            // When the user closes the window, shut the whole daemon down.
            // Otherwise the TCP server keeps running with no WebView attached,
            // and subsequent CLI calls fail silently against a stale port file.
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    if let Some(id) = close_id.as_ref() {
                        let _ = std::fs::remove_file(crate::daemon::port_file_path(id));
                        let _ = std::fs::remove_file(crate::daemon::pid_file_path(id));
                        tracing::info!("Window closed — daemon shutting down (id: {})", id);
                    }
                    std::process::exit(0);
                }
            });

            // Defensive: ensure the window is visible and foregrounded on start.
            // Daemon-spawned processes (esp. on macOS) don't always auto-activate.
            let _ = window.show();
            let _ = window.set_focus();

            #[cfg(target_os = "macos")]
            set_dock_icon();

            tracing::info!("Main window created successfully");

            // Linux: Disable Same-Origin Policy for cross-origin iframe access
            // Windows: uses CDP Page.setBypassCSP (setup_cdp_for_webview)
            // macOS: uses the WKScriptMessageHandler native bridge (setup_cdp_for_webview)
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = disable_web_security(&window) {
                    tracing::warn!("Web Security disable failed (ignored): {}", e);
                }
            }
            let _ = window; // suppress unused variable warning

            let instance_id_inner = instance_id_clone.clone();
            tauri::async_runtime::spawn(async move {
                let mut app_handle = APP_HANDLE.write().await;
                *app_handle = Some(handle);

                if let Some(id) = instance_id_inner {
                    let mut iid = INSTANCE_ID.write().await;
                    *iid = Some(id);
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            tracing::error!("!!! Tauri application error: {} !!!", e);
            panic!("error while running tauri application: {}", e);
        });

    tracing::info!("=== dalzzack Shutting Down ===");
}
