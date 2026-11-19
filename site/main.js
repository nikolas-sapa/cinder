const copyBtn = document.getElementById("copy-btn");
const installCmd = document.getElementById("install-cmd");

copyBtn.addEventListener("click", async () => {
  const text = installCmd.textContent.trim();
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
  copyBtn.classList.add("copied");
  copyBtn.querySelector(".copy-label").textContent = "copied";
  setTimeout(() => {
    copyBtn.classList.remove("copied");
    copyBtn.querySelector(".copy-label").textContent = "copy";
  }, 1600);
});

const io = new IntersectionObserver(
  (entries) => {
    for (const e of entries) {
      if (e.isIntersecting) {
        e.target.classList.add("in");
        io.unobserve(e.target);
      }
    }
  },
  { threshold: 0.12, rootMargin: "0px 0px -8% 0px" }
);

for (const el of document.querySelectorAll("[data-reveal]")) io.observe(el);

/* ---- ember field: twin firefronts burn in from the page edges.
   8x8 Bayer + interleaved gradient noise hybrid threshold, 13-level
   charcoal->ember->white ramp, sparks rise from the fronts. ---- */
(function(){
  const cv = document.getElementById("emberCanvas");
  if (!cv) return;
  const CELL = 4, ROWS = 50, MAX_COLS = 480, STAR_TOP = 13, STATIC_T = 6.0;
  const PAL = [
    [ 11, 12, 14],[ 20, 21, 25],[ 33, 35, 41],[ 50, 47, 38],
    [ 74, 55, 22],[106, 71, 20],[146, 96, 24],[196,127, 27],
    [232,152, 30],[255,178, 36],[255,190, 52],[255,214,110],[255,240,196]
  ];
  const LEVELS = PAL.length - 1;
  const TH8 = new Float32Array([
     0,32, 8,40, 2,34,10,42, 48,16,56,24,50,18,58,26,
    12,44, 4,36,14,46, 6,38, 60,28,52,20,62,30,54,22,
     3,35,11,43, 1,33, 9,41, 51,19,59,27,49,17,57,25,
    15,47, 7,39,13,45, 5,37, 63,31,55,23,61,29,53,21]);
  for (let i = 0; i < 64; i++) TH8[i] = (TH8[i] + 0.5) / 64;
  const ign = (x, y) => {
    const f = (0.06711056 * x + 0.00583715 * y) % 1;
    return (52.9829189 * f) % 1;
  };
  const ctx = cv.getContext("2d", { alpha: false });
  const mq = matchMedia("(prefers-reduced-motion: reduce)");
  let reduced = mq.matches;
  let cols = 0, img = null, lvls = null, raf = 0, running = false, inView = true;
  let last = 0, nextSpawn = 0;
  const stars = [], sparks = [];
  for (let i = 0; i < 16; i++)
    stars.push([Math.random(), Math.random(), 0.4 + Math.random() * 1.8, Math.random() * 6.28]);

  function resize() {
    cols = Math.min(MAX_COLS, Math.max(80, Math.ceil(cv.clientWidth / CELL)));
    cv.width = cols; cv.height = ROWS;
    img = ctx.createImageData(cols, ROWS);
    lvls = new Uint8Array(cols * ROWS);
    if (reduced) { seedStatic(); draw(STATIC_T); }
  }
  function spawn(t) {
    const left = Math.random() < 0.5;
    return {
      x0: left ? 2 + Math.random() * cols * 0.26 : cols - 3 - Math.random() * cols * 0.26,
      y0: ROWS * 0.60 + Math.random() * ROWS * 0.36,
      vx: (left ? 1 : -1) * (0.4 + Math.random() * 1.4),
      vy: 4.5 + Math.random() * 5.5,
      amp: 0.6 + Math.random() * 1.8, wf: 1.2 + Math.random() * 2.6,
      ph: Math.random() * 6.283, t0: t, ttl: 3.2 + Math.random() * 3.0,
      big: Math.random() < 0.12
    };
  }
  function seedStatic() {
    sparks.length = 0;
    for (let k = 0; k < 14; k++) {
      const s = spawn(STATIC_T - 0.4 - k * 0.31);
      s.ttl = 99; sparks.push(s);
    }
  }
  function put(x, y, lvl) {
    if (x < 0 || x >= cols || y < 0 || y >= ROWS) return;
    const p = y * cols + x;
    if (lvls[p] >= lvl) return;
    lvls[p] = lvl;
    const o = p * 4, c = PAL[lvl];
    img.data[o] = c[0]; img.data[o+1] = c[1]; img.data[o+2] = c[2]; img.data[o+3] = 255;
  }
  function draw(t) {
    const d = img.data;
    const bL = 0.90 + 0.10 * Math.sin(t * 0.33);
    const bR = 0.90 + 0.10 * Math.sin(t * 0.33 + 2.3);
    const inv = 1 / (cols - 1);
    for (let y = 0; y < ROWS; y++) {
      const q = y / (ROWS - 1);
      const vp = q * q;
      const warp = 7.0 * Math.sin(y * 0.09 + t * 0.27)
                 + 3.0 * Math.sin(y * 0.23 - t * 0.19)
                 + 1.2 * Math.sin(t * 0.11 + y * 0.05);
      const yrow = y * cols, brow = (y & 7) << 3;
      for (let x = 0; x < cols; x++) {
        const mx = x * inv * 2 - 1;
        const a = mx < 0 ? -mx : mx;
        let e = a * a * (3 - 2 * a); e = e * e * (3 - 2 * e);
        e *= (mx < 0 ? bL : bR);
        const wx = x + warp;
        let v = 0.42 * Math.sin(wx * 0.030 + t * 0.50 + y * 0.110)
              + 0.26 * Math.sin(wx * 0.073 - t * 0.38 + y * 0.190 + 1.7)
              + 0.16 * Math.sin(wx * 0.160 + t * 0.72 - y * 0.090 + 4.1)
              + 0.10 * Math.sin(wx * 0.310 - t * 1.15 + y * 0.240 + 2.3)
              + 0.06 * Math.sin(wx * 0.520 + t * 1.70 - y * 0.380 + 5.6);
        v = v * 0.68 + 0.5;
        v = v * e * vp * 1.6;
        v += 0.004 + 0.007 * ign(x + 31, y + 57);
        v = v < 0 ? 0 : (v > 1 ? 1 : v);
        v = v * v * (3 - 2 * v) * 0.45 + v * 0.55;
        const th = TH8[brow | (x & 7)] * 0.62 + ign(x, y) * 0.38;
        const f = v * LEVELS;
        let lvl = f | 0;
        if (f - lvl > th) lvl++;
        if (lvl > LEVELS) lvl = LEVELS;
        const p = yrow + x;
        lvls[p] = lvl;
        const o = p * 4, c = PAL[lvl];
        d[o] = c[0]; d[o+1] = c[1]; d[o+2] = c[2]; d[o+3] = 255;
      }
    }
    for (const st of stars) {
      if (0.5 + 0.5 * Math.sin(t * st[2] + st[3]) > 0.74)
        put((st[0] * cols) | 0, (st[1] * STAR_TOP) | 0, 3);
    }
    for (let k = sparks.length - 1; k >= 0; k--) {
      const s = sparks[k], age = t - s.t0;
      if (age > s.ttl) { sparks.splice(k, 1); continue; }
      const sx = (s.x0 + s.vx * age + s.amp * Math.sin(t * s.wf + s.ph)) | 0;
      const sy = (s.y0 - s.vy * age) | 0;
      const fade = 1 - age / s.ttl;
      let head = fade > 0.55 ? 12 : (fade > 0.3 ? 10 : 8);
      if (Math.sin(t * 27 + s.ph) > 0.7) head = Math.min(head + 1, LEVELS);
      put(sx, sy, head);
      if (s.big) put(sx + 1, sy, head - 1);
      put(sx, sy + 1, Math.max(head - 3, 4));
      put(sx, sy + 2, Math.max(head - 6, 4));
      put(sx, sy + 3, 4);
    }
    ctx.putImageData(img, 0, 0);
  }
  function loop(now) {
    raf = 0;
    const t = now / 1000;
    if (t - last > 0.05) last = t - 0.05;
    if (t >= nextSpawn && sparks.length < 36) {
      sparks.push(spawn(t));
      nextSpawn = t + 0.18 + Math.random() * 0.35;
    }
    draw(t);
    last = t;
    if (running) raf = requestAnimationFrame(loop);
  }
  function start() {
    if (!running && !reduced && !document.hidden && inView) {
      running = true;
      if (!raf) raf = requestAnimationFrame(loop);
    }
  }
  function stop() { running = false; if (raf) { cancelAnimationFrame(raf); raf = 0; } }
  document.addEventListener("visibilitychange", () => document.hidden ? stop() : start());
  new IntersectionObserver((e) => {
    inView = e[0].isIntersecting; inView ? start() : stop();
  }).observe(cv);
  if (mq.addEventListener) mq.addEventListener("change", (e) => {
    reduced = e.matches;
    if (reduced) { stop(); seedStatic(); draw(STATIC_T); } else start();
  });
  let rt; addEventListener("resize", () => { clearTimeout(rt); rt = setTimeout(resize, 150); });
  resize(); start();
})();

/* ---- interactive terminal demo ---- */
const transcript = document.getElementById("transcript");
const input = document.getElementById("term-input");
if (transcript && input) {
  const D = '<span class="dim">';
  const A = '<span class="amber">';
  const PEERS = {
    eidolon: { status: "· verified", fp: "9c1f…aa07",
      lines: [
        `${D}13:04</span>  ${A}eidolon</span>  copy that`,
        `${D}13:04</span>  ${A}eidolon</span>  wheels up in ten`,
        `${D}13:07</span>  ${A}agent</span>   msg.send → id:ccfd7c · deduped:false`,
        `${D}13:12</span>  — sas verified · numeric match · session pinned —`,
        `${D}13:13</span>  <span class="you">you</span>  rendezvous at the NEM port, 0400`,
      ],
      replies: ["solid copy", "ack — wheels up", "rendezvous locked"] },
    "k-9": { status: "· verified", fp: "4b7e…1d90",
      lines: [
        `${D}09:41</span>  ${A}k-9</span>  relay lagging on eu-west, switching`,
        `${D}09:44</span>  <span class="you">you</span>  mailbox had 2 queued, all drained`,
        `${D}09:44</span>  ${A}agent</span>   file.send → 2.1MB · chunk 0/17 offered`,
        `${D}09:51</span>  — file verified · sha256 match · saved —`,
      ],
      replies: ["roger", "queued, draining now", "chunk 17/17 confirmed"] },
    operator: { status: "· unverified", fp: "77aa…01c3",
      lines: [
        `${D}22:10</span>  ${A}operator</span>  new key — verify me before anything`,
        `${D}22:10</span>  ${D}— key change detected · sends blocked until /sas —`,
      ],
      replies: ["verify first, then we talk", "still waiting on sas"] },
  };
  const peerName = document.getElementById("peer-name");
  const peerStatus = document.getElementById("peer-status");
  let peer = "eidolon";
  let replyIdx = 0;
  const line = (html, cls = "msg") => {
    const p = document.createElement("p");
    p.className = cls + " typed";
    p.innerHTML = html;
    transcript.appendChild(p);
    transcript.scrollTop = transcript.scrollHeight;
    return p;
  };
  const sys = (html) => line(`${D}${html}</span>`, "msg dim");
  const stamp = () => "13:" + String(14 + Math.floor(Math.random() * 40)).padStart(2, "0");

  function loadPeer(name) {
    peer = name;
    replyIdx = 0;
    transcript.innerHTML = "";
    peerName.textContent = name;
    peerStatus.textContent = PEERS[name].status;
    PEERS[name].lines.forEach((l, i) =>
      setTimeout(() => line(l), 120 + i * 160));
  }

  document.querySelectorAll(".peer[data-peer]").forEach((el) => {
    const pick = () => {
      document.querySelectorAll(".peer").forEach((x) => x.classList.remove("active"));
      el.classList.add("active");
      loadPeer(el.dataset.peer);
    };
    el.addEventListener("click", pick);
    el.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(); } });
  });

  const CMDS = {
    "/help": () => sys("commands: /help · /finger · /sas · /msg &lt;text&gt; · /invite · /contacts · /deny · /wipe"),
    "/finger": () => sys(`peer ${PEERS[peer].fp} · X25519 · ${peer === "operator" ? "UNVERIFIED — run /sas" : "verified · session pinned"}`),
    "/sas": () => {
      sys("sas: 041 · 229 · 088 · 611 · 204 · 773 · 512");
      setTimeout(() => sys("— read aloud out-of-band · if it matches, session pinned —"), 350);
      if (peer === "operator") setTimeout(() => { sys("— operator verified · sends unblocked —"); peerStatus.textContent = "· verified"; }, 800);
    },
    "/invite": () => sys(`cinder://${PEERS[peer].fp.replace("…", "")}…@relay:8443?rendezvous=8f2a · one-time, expires on use`),
    "/contacts": () => sys("eidolon ✓ · k-9 ✓ · operator ⚠ unverified · denied: 0"),
    "/deny": () => { sys(`— ${peer} denied · sessions torn down · grants revoked · token rotated —`); sys("no un-deny via rpc — manual db surgery only"); },
    "/wipe": () => { transcript.innerHTML = ""; sys(`— chat wiped · 214 events · 3 files · dedup rows —`); sys("authenticated ChatWipe sent · peer's client destroys its copy"); },
  };

  input.addEventListener("keydown", (e) => {
    if (e.key !== "Enter") return;
    const raw = input.value.trim();
    input.value = "";
    if (!raw) return;
    line(`${D}${stamp()}</span>  <span class="prompt">&gt;</span> ${raw.replace(/</g, "&lt;")}`, "msg dim");
    if (raw.startsWith("/")) {
      const cmd = raw.split(/\s+/)[0].toLowerCase();
      if (CMDS[cmd]) return CMDS[cmd]();
      if (cmd === "/msg") {
        const body = raw.slice(5).trim() || "…";
        line(`${D}${stamp()}</span>  <span class="you">you</span>  ${body.replace(/</g, "&lt;")}`, "msg out-msg");
        const replies = PEERS[peer].replies;
        return setTimeout(() =>
          line(`${D}${stamp()}</span>  ${A}${peer}</span>  ${replies[replyIdx++ % replies.length]}`), 700);
      }
      return sys(`unknown command ${cmd.replace(/</g, "&lt;")} — /help`);
    }
    line(`${D}${stamp()}</span>  <span class="you">you</span>  ${raw.replace(/</g, "&lt;")}`, "msg out-msg");
    const replies = PEERS[peer].replies;
    setTimeout(() => line(`${D}${stamp()}</span>  ${A}${peer}</span>  ${replies[replyIdx++ % replies.length]}`), 700);
  });

  document.querySelector(".term-body").addEventListener("click", () => input.focus());
  loadPeer("eidolon");
}

/* ---- background glyph field: drifting embers, cursor ignites ---- */
(function(){
  const canvas = document.getElementById("bgField");
  if (!canvas) return;
  const CELL = 42, MAX = 850, RADIUS = 130;
  const DRIFT = 0.09, COOL = 0.955;
  const GLYPHS = '···········::::***++x~~';
  const SIZE = 14;
  const ctx = canvas.getContext('2d');
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  let W = 0, H = 0, raf = 0, running = false, t = 0;
  const parts = [];
  const mouse = { x: -9999, y: -9999, active: false };
  function rng(seed){ return function(){ seed|=0; seed=seed+0x6D2B79F5|0;
    let r=Math.imul(seed^seed>>>15,1|seed); r=r+Math.imul(r^r>>>7,61|r)^r;
    return ((r^r>>>14)>>>0)/4294967296; }; }
  function seed(){
    const rand = rng(0xC1D3E);
    parts.length = 0;
    const cols = Math.ceil(W/CELL), rows = Math.ceil(H/CELL);
    for (let gy=0; gy<rows; gy++) for (let gx=0; gx<cols; gx++){
      if (parts.length >= MAX) break;
      if (rand() < 0.28) continue;
      parts.push({
        x: gx*CELL + rand()*CELL, y: gy*CELL + rand()*CELL,
        g: GLYPHS[(rand()*GLYPHS.length)|0],
        phase: rand()*6.283, speed: 0.6 + rand()*0.9,
        base: 0.20 + rand()*0.30, heat: 0, ox: 0, oy: 0
      });
    }
  }
  function resize(){
    const dpr = Math.min(devicePixelRatio||1, 2);
    W = innerWidth; H = innerHeight;
    canvas.width = W*dpr; canvas.height = H*dpr;
    canvas.style.width = W+'px'; canvas.style.height = H+'px';
    ctx.setTransform(dpr,0,0,dpr,0,0);
    ctx.font = '500 '+SIZE+'px "JetBrains Mono",monospace';
    ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
    seed();
    if (reduced) draw();
  }
  const lerp = (a,b,k) => a+(b-a)*k;
  function draw(){
    ctx.clearRect(0,0,W,H);
    for (const p of parts){
      const tw = 0.75 + 0.25*Math.sin(t*0.02*p.speed + p.phase);
      const a = Math.min(1, p.base*tw + p.heat*0.85);
      const x = p.x + p.ox, y = p.y + p.oy;
      if (p.heat > 0.02){
        ctx.fillStyle = 'rgba(90,65,20,'+(p.heat*0.5).toFixed(3)+')';
        ctx.fillText(p.g, x, y);
        const r=lerp(155,255,p.heat)|0, g=lerp(160,178,p.heat)|0, b=lerp(166,36,p.heat)|0;
        ctx.fillStyle = 'rgba('+r+','+g+','+b+','+a.toFixed(3)+')';
      } else {
        ctx.fillStyle = 'rgba(155,160,166,'+a.toFixed(3)+')';
      }
      ctx.fillText(p.g, x, y);
    }
  }
  function tick(){
    t++;
    const r2 = RADIUS*RADIUS;
    for (const p of parts){
      p.y -= DRIFT*p.speed;
      p.x += Math.sin(t*0.008*p.speed + p.phase)*0.12;
      if (p.y < -20) p.y = H+20;
      if (p.x < -20) p.x = W+20; else if (p.x > W+20) p.x = -20;
      if (mouse.active){
        const dx = p.x - mouse.x, dy = p.y - mouse.y, d2 = dx*dx+dy*dy;
        if (d2 < r2){
          const d = Math.sqrt(d2)||1, f = 1 - d/RADIUS;
          p.ox += (dx/d)*f*2.4; p.oy += (dy/d)*f*2.4;
          p.heat = Math.min(1, p.heat + f*0.3);
        }
      }
      p.ox *= 0.9; p.oy *= 0.9; p.heat *= COOL;
    }
    draw();
    if (running) raf = requestAnimationFrame(tick);
  }
  function start(){ if (!running && !reduced){ running = true; raf = requestAnimationFrame(tick); } }
  function stop(){ running = false; cancelAnimationFrame(raf); }
  document.addEventListener('visibilitychange', () => document.hidden ? stop() : start());
  addEventListener('pointermove', (e) => {
    mouse.x = e.clientX; mouse.y = e.clientY; mouse.active = true;
  }, {passive:true});
  addEventListener('pointerleave', () => { mouse.active = false; });
  let rt; addEventListener('resize', () => { clearTimeout(rt); rt = setTimeout(resize, 150); });
  resize(); start();
})();

/* ---- hero ascii logotype: hand-set CINDER letterforms + ember shimmer ---- */
(function(){
  const pre = document.getElementById("heroAscii");
  if (!pre) return;
  const LETTERS = {
    C: [" ▄██████▄ ","██▀    ▀▀ ","██        ","██        ","██▄    ▄▄ "," ▀██████▀ "],
    I: ["████████"," ▀▀██▀▀ ","   ██   ","   ██   "," ▄▄██▄▄ ","████████"],
    N: ["███    ██","████   ██","██▀██  ██","██ ▀██ ██","██  ▀████","██   ▀███"],
    D: ["████████▄ ","██     ▀██","██      ██","██      ██","██     ▄██","████████▀ "],
    E: ["█████████","██▀▀▀▀▀▀▀","█████▀   ","██       ","██▄▄▄▄▄▄▄","█████████"],
    R: ["████████▄ ","██     ▀██","██     ▄██","████████▀ ","██   ▀██▄ ","██    ▀███"]
  };
  const GAP = "  ", order = ["C","I","N","D","E","R"], rows = [];
  for (let r = 0; r < 6; r++) {
    rows.push(order.map(k => {
      const letter = LETTERS[k];
      const w = Math.max(...letter.map(s => s.length));
      return letter[r].padEnd(w);
    }).join(GAP));
  }
  const HOT = [[1,7],[1,8],[4,8],[3,25],[2,50],[0,64],[5,62]];
  const cells = new Map();
  rows.forEach((row, r) => {
    for (let c = 0; c < row.length; c++) {
      const ch = row[c];
      if (ch === " ") { pre.appendChild(document.createTextNode(" ")); continue; }
      const s = document.createElement("span");
      s.className = "cell"; s.textContent = ch;
      pre.appendChild(s);
      cells.set(r + "," + c, s);
    }
    pre.appendChild(document.createTextNode("\n"));
  });
  HOT.forEach(rc => {
    const s = cells.get(rc[0] + "," + rc[1]);
    if (s) s.classList.add("hot");
  });
  const mq = matchMedia("(prefers-reduced-motion: reduce)");
  const all = Array.from(cells.values());
  let timer = null;
  function tick() {
    const s = all[(Math.random() * all.length) | 0];
    if (s && !s.classList.contains("hot") && !s.classList.contains("glow")) {
      s.classList.add("glow");
      setTimeout(() => s.classList.remove("glow"), 1200 + Math.random() * 800);
    }
    timer = setTimeout(tick, 300 + Math.random() * 800);
  }
  function setMotion() {
    clearTimeout(timer);
    if (mq.matches) all.forEach(s => s.classList.remove("glow"));
    else timer = setTimeout(tick, 500);
  }
  if (mq.addEventListener) mq.addEventListener("change", setMotion);
  setMotion();
})();

/* ---- shared ordered-dither painter (Bayer 8x8) ---- */
const B8=[0,32,8,40,2,34,10,42,48,16,56,24,50,18,58,26,
          12,44,4,36,14,46,6,38,60,28,52,20,62,30,54,22,
          3,35,11,43,1,33,9,41,51,19,59,27,49,17,57,25,
          15,47,7,39,13,45,5,37,63,31,55,23,61,29,53,21];
const PRM = matchMedia('(prefers-reduced-motion: reduce)');
function h2d(x,y,s){
  let n=(x*374761393+y*668265263+(s|0)*2246822519)>>>0;
  n=Math.imul(n^(n>>>13),1274126177)>>>0;
  return ((n^(n>>>16))>>>0)/4294967296;
}
function paint(cv,cell,vfn,cfn){
  const dpr=Math.min(devicePixelRatio||1,2);
  const w=cv.clientWidth,h=cv.clientHeight;
  if(!w||!h)return;
  const W=Math.round(w*dpr),H=Math.round(h*dpr);
  if(cv.width!==W||cv.height!==H){cv.width=W;cv.height=H;}
  const g=cv.getContext('2d');
  g.clearRect(0,0,W,H);
  const s=cell*dpr,cols=Math.ceil(w/cell),rows=Math.ceil(h/cell);
  for(let y=0;y<rows;y++)for(let x=0;x<cols;x++){
    const v=vfn(x,y,cols,rows);
    if(v<=0)continue;
    const t=(B8[(y&7)*8+(x&7)]+.5)/64;
    const c=cfn(v,t,x,y);
    if(!c)continue;
    g.fillStyle=c;
    g.fillRect(Math.round(x*s),Math.round(y*s),Math.ceil(s),Math.ceil(s));
  }
}

/* ---- card burn: the terminal's top edge dithers out ---- */
function paintBurns(){
  document.querySelectorAll('.burn').forEach(cv=>{
    paint(cv,2,(x,y,c,r)=>{
      const p=x/c;
      const edge=p<.5?1:Math.max(0,1-(p-.5)/.42);
      if(y===0)return edge;
      const bleed=Math.min(1,4*edge*(1-edge));
      return bleed*Math.pow(Math.max(0,1-y/23),1.35);
    },(v,t,x,y)=>{
      if(v<=t)return null;
      if(v<.85&&h2d(x,y,7)<.08)return '#FFB224';
      return y===0?'#23262D':'#3D4249';
    });
  });
}

/* ---- dither-flood buttons ---- */
function attachFlood(btn){
  const cv=btn.querySelector('canvas');
  if(!cv)return;
  const fill=btn.dataset.fill||'#FFB224',halo=btn.dataset.halo||'#5A4114';
  let s=0,tgt=0,raf=null;
  function draw(){
    paint(cv,3,(x,y,c)=>{
      const E=s*(c+18)-9;
      return Math.min(1,Math.max(0,(E-x)/10));
    },(v,t,x,y)=>{
      if(v>t)return fill;
      if(v*.45>t)return halo;
      if(h2d(x,y)<.05)return '#26292F';
      return null;
    });
  }
  function step(){
    s+=(tgt-s)*.14;
    if(Math.abs(tgt-s)<.004)s=tgt;
    draw();
    if(s!==tgt)raf=requestAnimationFrame(step);else raf=null;
  }
  function go(v){
    tgt=v;
    if(PRM.matches){s=v;draw();return;}
    if(!raf)raf=requestAnimationFrame(step);
  }
  btn.addEventListener('mouseenter',()=>go(1));
  btn.addEventListener('mouseleave',()=>go(0));
  btn.addEventListener('focus',()=>go(1));
  btn.addEventListener('blur',()=>go(0));
  draw();
}
document.querySelectorAll('.btn-primary[data-fill]').forEach(attachFlood);

/* ---- page-edge dissolve: floor erodes upward into noise ---- */
function paintDissolve(){
  document.querySelectorAll('.dissolve-cv').forEach(cv=>{
    paint(cv,3,(x,y,c,r)=>Math.pow(y/r,1.5),(v,t,x,y)=>{
      if(v<=t)return null;
      const s=h2d(x,y,3);
      if(s<.018)return '#5A4114';
      if(s<.022)return '#FFB224';
      return v>.8?'#3D4249':v>.45?'#2E3238':'#22252B';
    });
  });
}
function paintStatics(){paintBurns();paintDissolve();}
let statRT=null;
addEventListener('resize',()=>{clearTimeout(statRT);statRT=setTimeout(paintStatics,120);});
paintStatics();

/* ---- ascii glyph-dither band: text-ramp halftone, bayer 8x8 ---- */
(function(){
  const canvas = document.getElementById('asciiCanvas');
  if (!canvas) return;
  const ROWS = 14, COL_W = 8, FONT_PX = 13;
  const RAMP = ' .·:+*#%@', LVL = RAMP.length;
  const BAY = [
     0,32, 8,40, 2,34,10,42, 48,16,56,24,50,18,58,26,
    12,44, 4,36,14,46, 6,38, 60,28,52,20,62,30,54,22,
     3,35,11,43, 1,33, 9,41, 51,19,59,27,49,17,57,25,
    15,47, 7,39,13,45, 5,37, 63,31,55,23,61,29,53,21];
  const SPARK_MAX = 12, SPARK_P = 0.045, SPARK_LIFE = 1.5, SPARK_ATT = 0.12;
  const T_STATIC = 8.0;
  const LUT = [];
  (function(){
    const stops = [[90,65,20],[255,178,36],[255,233,196]];
    for (let i = 0; i < 256; i++){
      const t = i/255, lo = t < 0.5, k = lo ? t*2 : (t-0.5)*2;
      const a = stops[lo?0:1], b = stops[lo?1:2];
      LUT[i] = 'rgb(' + Math.round(a[0]+(b[0]-a[0])*k) + ','
                      + Math.round(a[1]+(b[1]-a[1])*k) + ','
                      + Math.round(a[2]+(b[2]-a[2])*k) + ')';
    }
  })();
  const ctx = canvas.getContext('2d');
  const mq = matchMedia('(prefers-reduced-motion: reduce)');
  let reduced = mq.matches;
  let W = 0, H = 0, cols = 0, cellH = 0, charW = 0, glyphDx = 0;
  let heat = null, raf = 0, running = false, inView = true;
  const sparks = [];
  function lum(gx, gy, t){
    const nx = gx*0.14, ny = gy*0.34;
    const v = Math.sin(nx + t*0.55)
          + Math.sin(ny*0.9 - t*0.38 + nx*0.6)
          + Math.sin((nx*0.7 + ny)*0.9 + t*0.8)
          + 0.45*Math.sin(nx*2.3 - ny*1.7 + t*1.15);
    return v*0.24 + 0.46;
  }
  function resize(){
    const dpr = Math.min(devicePixelRatio||1, 2);
    W = canvas.clientWidth; H = canvas.clientHeight;
    canvas.width = W*dpr; canvas.height = H*dpr;
    ctx.setTransform(dpr,0,0,dpr,0,0);
    ctx.font = '500 ' + FONT_PX + 'px "JetBrains Mono",monospace';
    ctx.textAlign = 'left'; ctx.textBaseline = 'middle';
    charW = ctx.measureText('M').width;
    glyphDx = Math.max(0, (COL_W - charW)*0.5);
    cols = Math.ceil(W/COL_W); cellH = H/ROWS;
    heat = new Float32Array(cols*ROWS);
    if (reduced){ seedStatic(); drawFrame(T_STATIC); }
  }
  function seedStatic(){
    sparks.length = 0;
    const pts = [[0.22,0.36],[0.55,0.62],[0.81,0.30]];
    for (let i = 0; i < pts.length; i++)
      sparks.push({x:(cols*pts[i][0])|0, y:(ROWS*pts[i][1])|0,
                   t0:T_STATIC - 0.3 - i*0.18, seed:i*3.1});
  }
  function drawFrame(now){
    ctx.clearRect(0,0,W,H);
    heat.fill(0);
    for (const sp of sparks){
      const age = now - sp.t0;
      const env = age < SPARK_ATT ? age/SPARK_ATT
                                  : 1 - (age-SPARK_ATT)/(SPARK_LIFE-SPARK_ATT);
      if (env <= 0) continue;
      heat[sp.y*cols + sp.x] = 1.18 * env * (0.7 + 0.3*Math.sin(age*22 + sp.seed));
    }
    let last = '';
    const midY = (ROWS-1)*0.5;
    for (let gy = 0; gy < ROWS; gy++){
      const envY = 1 - Math.abs(gy-midY)/ROWS*0.85;
      const yPos = gy*cellH + cellH*0.5;
      for (let gx = 0; gx < cols; gx++){
        const v = lum(gx,gy,now)*envY + heat[gy*cols+gx];
        if (v <= 0) continue;
        const f = v*(LVL-1);
        let base = f|0; if (base < 0) base = 0;
        let lvl = base + ((f-base) > BAY[(gy&7)*8 + (gx&7)]*0.015625 ? 1 : 0);
        if (lvl <= 0) continue;
        if (lvl >= LVL) lvl = LVL-1;
        const c = LUT[v > 1 ? 255 : (v*255)|0];
        if (c !== last){ ctx.fillStyle = c; last = c; }
        ctx.fillText(RAMP[lvl], gx*COL_W + glyphDx, yPos);
      }
    }
  }
  function tick(ts){
    const now = ts/1000;
    if (sparks.length < SPARK_MAX && Math.random() < SPARK_P)
      sparks.push({x:(Math.random()*cols)|0, y:(Math.random()*ROWS)|0,
                   t0:now, seed:Math.random()*9});
    for (let i = sparks.length-1; i >= 0; i--)
      if (now - sparks[i].t0 > SPARK_LIFE) sparks.splice(i,1);
    drawFrame(now);
    if (running) raf = requestAnimationFrame(tick);
  }
  function sync(){
    if (!reduced && inView && !document.hidden && !running){
      running = true; raf = requestAnimationFrame(tick);
    } else if ((reduced || !inView || document.hidden) && running){
      running = false; cancelAnimationFrame(raf);
    }
  }
  new IntersectionObserver(function(e){
    inView = e[0].isIntersecting; sync();
  }).observe(canvas);
  document.addEventListener('visibilitychange', sync);
  if (mq.addEventListener) mq.addEventListener('change', function(){
    reduced = mq.matches; sync();
    if (reduced){ seedStatic(); drawFrame(T_STATIC); }
  });
  let rt; addEventListener('resize', () => { clearTimeout(rt); rt = setTimeout(resize, 150); });
  resize(); sync();
})();

/* ---- molten field: webgl fragment-shader bayer dither (2d fallback) ---- */
(function(){
  const canvas = document.getElementById("moltenCanvas");
  if (!canvas) return;
  const CELL_PX = 8, DPR_CAP = 2, SKY = 0.22;
  const FSCALE = 0.055, DRIFT = 0.14, WARP = 1.55, RISE = 0.22;
  const STAR_TARGET = 30, T0 = 40.0;
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const dpr = () => Math.min(devicePixelRatio || 1, DPR_CAP);
  let raf = 0, running = false, inView = true, pageVisible = !document.hidden;
  let simT = T0, lastNow = 0;
  function wantFrames() { return !reduced && inView && pageVisible; }
  function pump() {
    if (wantFrames() && !running) {
      running = true; lastNow = performance.now();
      raf = requestAnimationFrame(tick);
    } else if (!wantFrames() && running) {
      running = false; cancelAnimationFrame(raf);
    }
  }
  function tick(now) {
    const dt = Math.min(0.1, (now - lastNow) / 1000);
    lastNow = now; simT += dt;
    draw(simT);
    if (running) raf = requestAnimationFrame(tick);
  }
  const VERT = "attribute vec2 aP;void main(){gl_Position=vec4(aP,0.,1.);}";
  const FRAG = `
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform vec2 uRes;
uniform float uT;
uniform float uStarD;
const float SKY=${SKY.toFixed(4)};
const float FSCALE=${FSCALE.toFixed(4)};
const float DRIFT=${DRIFT.toFixed(4)};
const float WARP=${WARP.toFixed(3)};
const float RISE=${RISE.toFixed(3)};
float hash(vec2 p){return fract(sin(dot(p,vec2(127.1,311.7)))*43758.5453123);}
float vnoise(vec2 p){
  vec2 i=floor(p),f=fract(p);
  vec2 u=f*f*(3.-2.*f);
  return mix(mix(hash(i),hash(i+vec2(1.,0.)),u.x),
             mix(hash(i+vec2(0.,1.)),hash(i+vec2(1.,1.)),u.x),u.y);
}
float fbm(vec2 p){
  float v=0.,a=.5;
  for(int i=0;i<4;i++){v+=a*vnoise(p);p=p*2.03+vec2(1.7,9.2);a*=.5;}
  return v;
}
float bayer2(vec2 a){a=floor(a);return fract(a.x*.5+a.y*a.y*.75);}
float bayer4(vec2 a){return bayer2(a*.5)*.25+bayer2(a);}
float bayer8(vec2 a){return bayer4(a*.5)*.25+bayer2(a);}
void main(){
  vec2 cell=floor(gl_FragCoord.xy);
  vec3 col=vec3(0.043,0.047,0.055);
  float skyBase=uRes.y*(1.-SKY);
  if(cell.y>skyBase){
    float h=hash(cell);
    if(h<uStarD){
      float tw=.5+.5*sin(uT*(.7+h*40.)+h*217.);
      float s=smoothstep(.25,.95,tw);
      col+=vec3(1.,.80,.30)*s*(.55+.45*h/uStarD);
      if(h<uStarD*.18) col+=vec3(.9,.85,.7)*s*.6;
    }
  }else{
    float fy=(skyBase-cell.y)/skyBase;
    vec2 p=cell*FSCALE;
    p.x+=uT*DRIFT;
    vec2 q=vec2(fbm(p+vec2(0.,uT*.05)),
                fbm(p+vec2(5.2,1.3)-uT*.035));
    float f=fbm(p+WARP*q+vec2(0.,uT*.025));
    float v=(fy*fy*.75+fy*.35)*(.30+.95*f);
    v+=.06*sin(cell.x*.13-uT*.9)*fy;
    float rise=vnoise(vec2(cell.x*.18,cell.y*.06-uT*1.1));
    v+=RISE*smoothstep(.72,.95,rise)*fy*fy;
    vec2 e=cell/uRes;
    v*=smoothstep(0.,.05,e.x)*smoothstep(1.,.95,e.x);
    v=clamp(v,0.,1.);
    float b=bayer8(cell);
    if(v>b){
      float heat=clamp((v-b)*2.6,0.,1.)*.6+v*.4;
      vec3 em=mix(vec3(.353,.255,.078),vec3(1.,.698,.141),smoothstep(0.,.85,heat));
      em=mix(em,vec3(1.,.90,.60),smoothstep(.80,1.,heat));
      em=mix(em,vec3(1.,.42,.37),smoothstep(.93,1.,heat)*.35);
      col=em;
    }else{
      col+=vec3(.09,.045,.012)*smoothstep(.35,1.,v);
    }
  }
  gl_FragColor=vec4(col,1.);
}`;
  let gl = canvas.getContext("webgl", { alpha: false, antialias: false, depth: false,
    stencil: false, powerPreference: "low-power", preserveDrawingBuffer: false })
    || canvas.getContext("experimental-webgl");
  let uRes, uT, uStarD;
  function glInit() {
    const compile = (type, src) => {
      const s = gl.createShader(type);
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS))
        throw new Error(gl.getShaderInfoLog(s) || "shader compile failed");
      return s;
    };
    const prog = gl.createProgram();
    gl.attachShader(prog, compile(gl.VERTEX_SHADER, VERT));
    gl.attachShader(prog, compile(gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS))
      throw new Error(gl.getProgramInfoLog(prog) || "link failed");
    gl.useProgram(prog);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const aP = gl.getAttribLocation(prog, "aP");
    gl.enableVertexAttribArray(aP);
    gl.vertexAttribPointer(aP, 2, gl.FLOAT, false, 0, 0);
    uRes = gl.getUniformLocation(prog, "uRes");
    uT = gl.getUniformLocation(prog, "uT");
    uStarD = gl.getUniformLocation(prog, "uStarD");
  }
  function glResize() {
    const w = Math.max(16, Math.round(canvas.clientWidth * dpr() / CELL_PX));
    const h = Math.max(8, Math.round(canvas.clientHeight * dpr() / CELL_PX));
    if (canvas.width === w && canvas.height === h) return;
    canvas.width = w; canvas.height = h;
    gl.viewport(0, 0, w, h);
    const skyCells = Math.max(1, Math.round(h * SKY));
    gl.uniform1f(uStarD, Math.min(0.06, STAR_TARGET / (w * skyCells)));
    gl.uniform2f(uRes, w, h);
  }
  function drawGL(t) {
    gl.uniform1f(uT, t);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }
  function start2D(cv) {
    const ctx = cv.getContext("2d");
    if (!ctx) { draw = () => {}; return; }
    const B4 = [[0,8,2,10],[12,4,14,6],[3,11,1,9],[15,7,13,5]];
    let W = 0, H = 0;
    const stars = [];
    for (let i = 0; i < STAR_TARGET; i++)
      stars.push([Math.random(), Math.random(), .6 + Math.random() * 2.4, Math.random() * 6.28]);
    function rs() {
      const w = Math.max(16, Math.round(cv.clientWidth * dpr() / CELL_PX));
      const h = Math.max(8, Math.round(cv.clientHeight * dpr() / CELL_PX));
      if (w === W && h === H) return;
      W = w; H = h; cv.width = w; cv.height = h;
    }
    function fr(t) {
      const im = ctx.createImageData(W, H), d = im.data;
      const skyH = Math.max(1, Math.round(H * SKY));
      for (let y = 0; y < H; y++) {
        const inSky = y < skyH;
        const fy = inSky ? 0 : (y - skyH) / (H - skyH);
        for (let x = 0; x < W; x++) {
          let v = 0;
          if (!inSky) {
            v = (fy * fy * .75 + fy * .35) * .8
              + .13 * Math.sin(x * .055 + t * 1.3 + y * .10)
              + .10 * Math.sin(x * .11 - t * .8 + y * .05)
              + .05 * Math.sin(x * .23 + t * 2.0 - y * .09);
            if (x < 3 || x > W - 4) v *= .4;
            v = Math.max(0, Math.min(1, v));
          }
          const th = (B4[y & 3][x & 3] + .5) / 16;
          const i = (y * W + x) * 4;
          if (v > th) {
            const heat = Math.min(1, (v - th) * 2.4 + v * .4);
            d[i] = 255;
            d[i+1] = Math.round(65 + heat * 115 + (heat > .8 ? (heat - .8) * 60 : 0));
            d[i+2] = Math.round(20 + heat * 40 + (heat > .85 ? (heat - .85) * 120 : 0));
            d[i+3] = 255;
          } else if (!inSky && v > .35) {
            d[i] = 28; d[i+1] = 14; d[i+2] = 4; d[i+3] = 255;
          }
        }
      }
      for (const [sx, sy, sp, ph] of stars) {
        if (.5 + .5 * Math.sin(t * sp + ph) > .55) {
          const i = (((sy * skyH) | 0) * W + ((sx * W) | 0)) * 4;
          d[i] = 255; d[i+1] = 204; d[i+2] = 77; d[i+3] = 255;
        }
      }
      ctx.putImageData(im, 0, 0);
    }
    draw = fr; resize = rs;
    rs(); fr(simT);
  }
  let draw, resize, cur = canvas;
  try {
    if (!gl) throw new Error("webgl unavailable");
    glInit();
    draw = drawGL; resize = glResize;
    canvas.addEventListener("webglcontextlost", (e) => {
      e.preventDefault();
      running = false; cancelAnimationFrame(raf);
      const cv = canvas.cloneNode();
      canvas.replaceWith(cv);
      io.unobserve(cur); ro.unobserve(cur);
      cur = cv;
      io.observe(cv); ro.observe(cv);
      start2D(cv); pump();
    });
  } catch (err) {
    start2D(canvas);
  }
  const io = new IntersectionObserver(
    (en) => { inView = en[0].isIntersecting; pump(); },
    { threshold: 0.01 }
  );
  const ro = new ResizeObserver(() => {
    resize();
    if (!running) draw(simT);
  });
  io.observe(canvas); ro.observe(canvas);
  document.addEventListener("visibilitychange", () => {
    pageVisible = !document.hidden; pump();
  });
  resize(); draw(simT); pump();
})();

/* ---- boot panel: cinderd types its startup, then the banner ---- */
(function(){
  const term = document.getElementById("bootTerm");
  const boot = document.getElementById("bootBody");
  if (!term || !boot) return;
  const CHAR_MS = 14, JITTER = 8, LINE_MS = 130, PRE_BANNER = 380, BANNER_MS = 110;
  const BOOT = [
    { segs:[{t:"cinderd v0.1.0", c:"ink"}] },
    { segs:[{t:"[ok]", c:"ok"}, {t:" identity loaded · peer-id 9c1f…aa07"}] },
    { segs:[{t:"[ok]", c:"ok"}, {t:" noise XX · session 7f3a"}] },
    { segs:[{t:"[ok]", c:"ok"}, {t:" relay connected · mailbox drained: 3"}] },
    { segs:[{t:"[..]", c:"pend"}, {t:" listening :8420"}] }
  ];
  const GLYPHS = [
    [" ██████╗ ","██╔════╝ ","██║      ","██║      ","╚██████╗ "," ╚═════╝ "],
    ["██╗","██║","██║","██║","██║","╚═╝"],
    ["███╗   ██╗","████╗  ██║","██╔██╗ ██║","██║╚██╗██║","██║ ╚████║","╚═╝  ╚═══╝"],
    ["██████╗ ","██╔══██╗","██║  ██║","██║  ██║","██████╔╝","╚═════╝ "],
    ["███████╗","██╔════╝","█████╗  ","██╔══╝  ","███████╗","╚══════╝"],
    ["██████╗ ","██╔══██╗","██████╔╝","██╔══██╗","██║  ██║","╚═╝  ╚═╝"]
  ];
  const BANNER = [];
  for (let r = 0; r < 6; r++) {
    let row = "";
    for (let g = 0; g < GLYPHS.length; g++) row += (g ? "  " : "") + GLYPHS[g][r];
    BANNER.push(row);
  }
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
  let started = false, finished = false, paused = false, skip = false;
  const el = (tag, cls, txt) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (txt !== undefined) e.textContent = txt;
    return e;
  };
  function sleep(ms) {
    return new Promise(res => {
      let waited = 0, last = performance.now();
      (function tick(){
        if (skip) return res();
        const now = performance.now();
        if (!paused) waited += now - last;
        last = now;
        if (waited >= ms) return res();
        setTimeout(tick, 16);
      })();
    });
  }
  function lineEl(line, lit) {
    const l = el("div", "line" + (lit ? " lit" : " typing"));
    line.segs.forEach(s => {
      const sp = el("span", s.c || "");
      if (lit) sp.textContent = s.t;
      l.appendChild(sp);
    });
    return l;
  }
  const bannerEl = row => el("div", "banner-line", row);
  function promptEl(){
    const l = el("div", "line bprompt");
    l.appendChild(el("span", "gt", ">"));
    l.appendChild(el("span", "bcaret", "_"));
    return l;
  }
  function renderInstant(){
    if (finished) return;
    finished = true;
    boot.textContent = "";
    BOOT.forEach(l => boot.appendChild(lineEl(l, true)));
    boot.appendChild(el("div", "gap"));
    BANNER.forEach(r => boot.appendChild(bannerEl(r)));
    boot.appendChild(el("div", "gap"));
    boot.appendChild(promptEl());
  }
  async function run(){
    for (const line of BOOT) {
      if (skip) return renderInstant();
      const l = lineEl(line, false);
      boot.appendChild(l);
      for (let s = 0; s < line.segs.length; s++) {
        const sp = l.children[s], txt = line.segs[s].t;
        for (let k = 0; k < txt.length; k++) {
          if (skip) return renderInstant();
          sp.textContent += txt[k];
          await sleep(CHAR_MS + Math.random() * JITTER);
        }
      }
      l.classList.remove("typing");
      l.classList.add("lit");
      await sleep(LINE_MS);
    }
    await sleep(PRE_BANNER);
    if (skip) return renderInstant();
    boot.appendChild(el("div", "gap"));
    for (const r of BANNER) {
      if (skip) return renderInstant();
      boot.appendChild(bannerEl(r));
      await sleep(BANNER_MS);
    }
    boot.appendChild(el("div", "gap"));
    boot.appendChild(promptEl());
    finished = true;
  }
  term.addEventListener("click", () => {
    if (!started || finished) return;
    paused = !paused;
    term.classList.toggle("paused", paused);
  });
  addEventListener("keydown", (e) => {
    if ((e.key === "s" || e.key === "S") && started && !finished) {
      skip = true;
      term.classList.remove("paused");
      paused = false;
    }
  });
  if (reduced) {
    renderInstant();
  } else {
    const io = new IntersectionObserver(entries => {
      entries.forEach(e => {
        if (e.isIntersecting && !started) {
          started = true;
          io.disconnect();
          run();
        }
      });
    }, { threshold: 0.3 });
    io.observe(term);
  }
})();
