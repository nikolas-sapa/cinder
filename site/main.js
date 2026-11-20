(function(){

/* ====================== PORT ======================
 * ctrl-v4 · install strip + copy keycap
 *
 * LIFT markup : one .ctl4-strip subtree — .ctl4-well (.ctl4-rail>.ctl4-led,
 *               .ctl4-body>.ctl4-prompt+.ctl4-cmd>.ctl4-caret + .ctl4-ghost),
 *               button.ctl4-key (.ctl4-key-cap>.ctl4-key-tex canvas +
 *               .ctl4-key-l label), and sibling .ctl4-sr live region.
 *               Optional: the .ctl4-echo block (drop it to skip feature 4).
 * LIFT css    : all rules between "LIFT FROM HERE / LIFT TO HERE" markers
 *               incl. @keyframes ctl4ledblip/ctl4pp/ctl4blink/ctl4pop,
 *               the --stack/media rules, and the PRM block's .ctl4-led and
 *               .ctl4-ghost entries.
 * LIFT js     : this whole IIFE — [P1] cap texture, [P2] copy handler,
 *               [P3] echo typer. All selectors are per-strip scoped; N
 *               strips on one page are supported (querySelectorAll).
 *
 * RENAME MAP — prod collisions avoided (prod names in index.html/style.css):
 *   .install      -> .ctl4-strip     .install-body -> .ctl4-well+.ctl4-body
 *   .prompt       -> .ctl4-prompt    .install-cmd   -> .ctl4-cmd
 *   .caret        -> .ctl4-caret     .copychip      -> .ctl4-key
 *   .cc-l         -> .ctl4-key-l     .copied        -> .is-copied
 *   .done         -> .is-done        #install-cmd   -> .ctl4-cmd (class)
 *   #copy-btn     -> .ctl4-key       new: .ctl4-rail .ctl4-led .ctl4-ghost
 *   new: .ctl4-key-cap .ctl4-key-tex .ctl4-key-l .ctl4-sr .ctl4-echo-*
 *   state classes are only ever combined as .ctl4-*.is-* — no bare globals.
 *
 * BEHAVIOR PARITY: clipboard write + execCommand fallback, click-to-select,
 * [ ⧉ copy ]<->[ ✓ copied ] swap, 1.5s window — identical to prod
 * main.js L1-33. Label width locked at 13ch so the swap never reflows.
 * No <input>/<textarea> in markup (fallback textarea is transient) — no
 * data-1p-ignore attributes needed. Echo line is aria-hidden decoration.
 * ====================== /PORT ====================== */
(()=>{
'use strict';
const RM=matchMedia('(prefers-reduced-motion: reduce)');
const B8=[0,32,8,40,2,34,10,42,48,16,56,24,50,18,58,26,
          12,44,4,36,14,46,6,38,60,28,52,20,62,30,54,22,
          3,35,11,43,1,33,9,41,51,19,59,27,49,17,57,25,
          15,47,7,39,13,45,5,37,63,31,55,23,61,29,53,21];

function h2(x,y,s){
  let n=(x*374761393+y*668265263+(s|0)*2246822519)>>>0;
  n=Math.imul(n^(n>>>13),1274126177)>>>0;
  return ((n^(n>>>16))>>>0)/4294967296;
}

/* [P1] keycap face: ordered-Bayer sheen + sparse ember grain */
function paintTex(cv){
  const d=Math.min(devicePixelRatio||1,2);
  const w=cv.clientWidth,h=cv.clientHeight;
  if(!w||!h)return;
  const W=Math.max(1,Math.round(w*d)),H=Math.max(1,Math.round(h*d));
  if(cv.width!==W||cv.height!==H){cv.width=W;cv.height=H;}
  const g=cv.getContext('2d');
  g.clearRect(0,0,W,H);
  const cell=2*d,cols=Math.ceil(W/cell),rows=Math.ceil(H/cell);
  for(let y=0;y<rows;y++)for(let x=0;x<cols;x++){
    const n=h2(x,y,3);
    const sheen=.62-(y/rows)*.55+n*.18;
    const t=(B8[(y&7)*8+(x&7)]+.5)/64;
    if(sheen>t*1.5){
      g.fillStyle='rgba(255,255,255,.045)';
      g.fillRect(x*cell,y*cell,cell,cell);
    }else if(n>.97){
      g.fillStyle='rgba(255,178,36,.09)';
      g.fillRect(x*cell,y*cell,cell,cell);
    }
  }
}
const texs=document.querySelectorAll('.ctl4-key-tex');
texs.forEach(paintTex);
if(document.fonts&&document.fonts.ready)
  document.fonts.ready.then(()=>texs.forEach(paintTex));

/* [P2] copy key: clipboard + label swap + done flash + aria-live */
document.querySelectorAll('.ctl4-strip').forEach(strip=>{
  const key=strip.querySelector('.ctl4-key');
  const lbl=strip.querySelector('.ctl4-key-l');
  const cmd=strip.querySelector('.ctl4-cmd');
  const live=strip.querySelector('.ctl4-sr');
  if(!key||!lbl||!cmd)return;
  let t=null;

  key.addEventListener('click',async()=>{
    const text=cmd.textContent.trim();
    try{
      await navigator.clipboard.writeText(text);
    }catch{
      const ta=document.createElement('textarea');
      ta.value=text;document.body.appendChild(ta);ta.select();
      document.execCommand('copy');ta.remove();
    }
    clearTimeout(t);
    key.classList.add('is-copied');
    strip.classList.add('is-done');
    lbl.textContent='[ ✓ copied ]';
    if(live)live.textContent='install command copied to clipboard';
    t=setTimeout(()=>{
      key.classList.remove('is-copied');
      strip.classList.remove('is-done');
      lbl.textContent='[ ⧉ copy ]';
      if(live)live.textContent='';
    },1500);
  });

  strip.addEventListener('click',e=>{
    if(e.target.closest('.ctl4-key'))return;
    const r=document.createRange();
    r.selectNodeContents(cmd);
    const sel=getSelection();
    sel.removeAllRanges();sel.addRange(r);
  });
});

/* [P3] echo line: $ cinder id -> peer hash, types once on scroll-in */
const EC='cinder id',EO='peer: 4d2e…f18c';
const wait=ms=>new Promise(r=>setTimeout(r,ms));
document.querySelectorAll('.ctl4-echo').forEach(el=>{
  const t1=el.querySelector('.ctl4-echo-t1');
  const t2=el.querySelector('.ctl4-echo-t2');
  const cr=el.querySelector('.ctl4-echo-caret');
  const out=el.querySelector('.ctl4-echo-out');
  if(!t1||!t2)return;
  const showAll=()=>{
    t1.textContent=EC;t2.textContent=EO;
    if(cr&&out)out.appendChild(cr);
    el.classList.add('is-static');
  };
  const type=async()=>{
    if(RM.matches){showAll();return;}
    for(const ch of EC){t1.textContent+=ch;await wait(46+Math.random()*54);}
    await wait(340);
    if(cr&&out)out.appendChild(cr);
    for(const ch of EO){t2.textContent+=ch;await wait(26+Math.random()*30);}
    el.classList.add('is-static');
  };
  if(RM.matches||!('IntersectionObserver' in window)){showAll();return;}
  const io=new IntersectionObserver(es=>{
    es.forEach(e=>{if(e.isIntersecting){io.disconnect();type();}});
  },{threshold:.4});
  io.observe(el);
});
})();

})();

/* ---- nav: scrolled state ---- */
const navEl = document.querySelector(".nav");
if (navEl) {
  const onScroll = () => navEl.classList.toggle("scrolled", scrollY > 8);
  addEventListener("scroll", onScroll, { passive: true });
  onScroll();
}

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


/* ---- fieldLoop: shared canvas lifecycle (IO+visibility+PRM+resize) ---- */
function fieldLoop(cv,api){
  let running=false,inView=false,raf=0,last=0,t=6+Math.random()*30;
  function frame(now){
    raf=0;
    const nt=now/1000,dt=Math.min(.05,Math.max(0,nt-last));
    last=nt;t+=dt;api.tick(t,dt);
    if(running)raf=requestAnimationFrame(frame);
  }
  function want(){return !PRM.matches&&!document.hidden&&inView;}
  function sync(){
    if(want()&&!running){running=true;last=performance.now()/1000;
      raf=requestAnimationFrame(frame);}
    else if(!want()&&running){running=false;
      if(raf){cancelAnimationFrame(raf);raf=0;}}
  }
  new IntersectionObserver(es=>{
    inView=es[0].isIntersecting;sync();
  },{rootMargin:'60px 0px'}).observe(cv);
  document.addEventListener('visibilitychange',sync);
  if(PRM.addEventListener)PRM.addEventListener('change',()=>{
    sync();if(PRM.matches)api.still();
  });
  let rt;addEventListener('resize',()=>{
    clearTimeout(rt);rt=setTimeout(()=>{
      api.resize();if(PRM.matches)api.still();},140);
  });
  api.resize();
  api.still();            /* first paint for everyone; PRM keeps just this */
}

/* ---- ember field v2 ---- */
(function(){
  const cv=document.getElementById('emberCanvas');if(!cv)return;
  const CELL=4,MAX_POCKETS=4,MAX_SPARKS=26;
  /* charcoal -> ember-dim -> ember -> pale ramp (ground = unpainted) */
  const RAMP=['#2A1D08','#3E2B0C','#573B10','#735011','#916517',
              '#B87A18','#D9961D','#FFB224','#FFC85C','#FFE3A1','#FFF4D8'];
  let cols=0,rows=0,heat=null,tmp=null;
  let t=0,bL=1,bR=1,nextPocket=0,nextSpark=0;
  const pockets=[],sparks=[];

  function resize(){
    cols=Math.max(1,Math.ceil(cv.clientWidth/CELL));
    rows=Math.max(1,Math.ceil(cv.clientHeight/CELL));
    heat=new Float32Array(cols*rows);
    tmp=new Float32Array(cols*rows);
  }

  /* twin-front smolder floor — prod envelope, compressed sines */
  function base(x,y){
    const mx=x/(cols-1)*2-1,a=mx<0?-mx:mx;
    let e=a*a*(3-2*a);e=e*e*(3-2*e);
    e=(.30+.70*e)*(mx<0?bL:bR);                 /* 0.30 floor = centre smolder */
    const q=y/(rows-1),vp=q*q;
    const warp=4.5*Math.sin(y*.13+t*.27)+2*Math.sin(y*.29-t*.19);
    const wx=x+warp;
    let v=.42*Math.sin(wx*.030+t*.50+y*.110)
         +.26*Math.sin(wx*.073-t*.38+y*.190+1.7)
         +.16*Math.sin(wx*.160+t*.72-y*.090+4.1)
         +.10*Math.sin(wx*.310-t*1.15+y*.240+2.3);
    v=v*.68+.5;
    v=v*e*vp*1.42+.006+.010*h2d(x,y,4);
    v=v<0?0:v>1?1:v;
    return v*v*(3-2*v)*.45+v*.55;
  }

  /* cheap blur/decay: each cell relaxes toward its neighbour mean at
     rate D while cooling at rate K — energy is lost every pass, so
     flares bleed a molten halo then die back to the smolder floor. */
  function stepHeat(dt){
    const D=Math.min(1,dt*6.5),K=Math.min(1,dt*1.9);
    for(let y=0;y<rows;y++){
      const yr=y*cols,up=y>0?-cols:0,dn=y<rows-1?cols:0;
      for(let x=0;x<cols;x++){
        const i=yr+x,h0=heat[i];
        const n=(x>0?heat[i-1]:0)+(x<cols-1?heat[i+1]:0)
               +(up?heat[i+up]:0)+(dn?heat[i+dn]:0);
        let nh=h0*(1-K)+(n*.25-h0)*D;
        tmp[i]=nh<0?0:nh>1.2?1.2:nh;
      }
    }
    const sw=heat;heat=tmp;tmp=sw;
  }

  /* stamp a soft heat disc — pocket cores & spark heads */
  function stamp(cx,cy,r,v){
    const x0=Math.max(0,(cx-r)|0),x1=Math.min(cols-1,Math.ceil(cx+r)),
          y0=Math.max(0,(cy-r)|0),y1=Math.min(rows-1,Math.ceil(cy+r));
    for(let y=y0;y<=y1;y++)for(let x=x0;x<=x1;x++){
      const dx=x-cx,dy=(y-cy)*1.15,d=Math.sqrt(dx*dx+dy*dy);
      if(d>=r)continue;
      const f=1-d/r,i=y*cols+x,hv=v*f*(1.4-.4*f);
      if(heat[i]<hv)heat[i]=hv;
    }
  }

  function spawnPocket(){
    pockets.push({
      x:cols*(.12+.76*Math.random()),
      y:rows*(.45+.52*Math.random()),
      r:2.5+Math.random()*3.5,
      atk:.22+Math.random()*.30,           /* flare-up seconds */
      dec:1.9+Math.random()*2.6,           /* relax seconds */
      pk:.62+Math.random()*.50,t0:t
    });
  }
  function spawnSpark(){
    let x,y;
    const r=Math.random();
    if(r<.5){                              /* edge fronts, like prod */
      x=Math.random()<.5?2+Math.random()*cols*.22:cols-3-Math.random()*cols*.22;
      y=rows*(.6+.36*Math.random());
    }else if(r<.8&&pockets.length){        /* pocket cores spit embers */
      const p=pockets[(Math.random()*pockets.length)|0];
      x=p.x;y=p.y;
    }else{
      x=2+Math.random()*(cols-4);y=rows*(.7+.26*Math.random());
    }
    sparks.push({x,y,vx:(Math.random()-.5)*1.6,vy:4.5+Math.random()*5,
      amp:.5+Math.random()*1.6,wf:1.4+Math.random()*2.4,
      ph:Math.random()*6.28,t0:t,ttl:2.6+Math.random()*2.4});
  }

  function vfn(x,y){
    const v=base(x,y)+heat[y*cols+x]*.9;
    return v>1.15?1.15:v;
  }
  function cfn(v,th,x,y){
    const tt=th*.60+h2d(x,y,9)*.40;        /* hybrid bayer+hash threshold */
    if(v<=tt)return h2d(x,y,2)<.028?'#1B1E23':null;
    const h=v*.82+(v-tt)*.55,hi=h>1?1:h;   /* palette ~∝ v, like prod */
    return RAMP[Math.min(RAMP.length-1,(hi*(RAMP.length-1))|0)];
  }

  function tick(nt,dt){
    t=nt;
    bL=.90+.10*Math.sin(t*.33);
    bR=.90+.10*Math.sin(t*.33+2.3);
    if(t>=nextPocket&&pockets.length<MAX_POCKETS){
      spawnPocket();nextPocket=t+1.6+Math.random()*2.6;
    }
    if(t>=nextSpark&&sparks.length<MAX_SPARKS){
      spawnSpark();nextSpark=t+.22+Math.random()*.4;
    }
    for(let i=pockets.length-1;i>=0;i--){
      const p=pockets[i],age=t-p.t0;
      let env;
      if(age<p.atk){const u=age/p.atk;env=u*u*(3-2*u);}
      else env=1-(age-p.atk)/p.dec;
      if(env<=0){pockets.splice(i,1);continue;}
      stamp(p.x,p.y,p.r*(0.8+.3*env),p.pk*env);
    }
    for(let i=sparks.length-1;i>=0;i--){
      const s=sparks[i],age=t-s.t0;
      if(age>s.ttl){sparks.splice(i,1);continue;}
      const fade=1-age/s.ttl;
      const sx=s.x+s.vx*age+s.amp*Math.sin(t*s.wf+s.ph),
            sy=s.y-s.vy*age;
      stamp(sx,sy,1.1,.55+.45*fade);
      stamp(sx,sy+2,.8,.3*fade);
    }
    stepHeat(dt);
    paint(cv,CELL,vfn,cfn);
  }

  /* one static frame: pockets mid-flare, spark trails, heat pre-bled */
  function still(){
    heat.fill(0);pockets.length=0;sparks.length=0;t=6;
    bL=.98;bR=.94;
    const sd=[[.22,.72,4.2,.95],[.55,.62,3.2,.8],[.78,.8,5,.9]];
    for(const s of sd)stamp(s[0]*cols,s[1]*rows,s[2],s[3]);
    for(let i=0;i<22;i++)stepHeat(.05);     /* pre-diffuse */
    for(const s of sd)stamp(s[0]*cols,s[1]*rows,s[2]*.7,s[3]*.8);
    stamp(cols*.34,rows*.55,1.1,.9);stamp(cols*.34,rows*.48,.8,.5);
    stamp(cols*.86,rows*.6,1.1,.8);
    paint(cv,CELL,vfn,cfn);
  }

  fieldLoop(cv,{resize,tick,still});
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
        `${D}22:10</span>  ${D}— key change detected · sends blocked until /verify —`,
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

  let peerGen = 0;
  function loadPeer(name) {
    const g = ++peerGen;
    peer = name;
    replyIdx = 0;
    transcript.innerHTML = "";
    peerName.textContent = name;
    peerStatus.textContent = PEERS[name].status;
    PEERS[name].lines.forEach((l, i) =>
      setTimeout(() => { if (g === peerGen) line(l); }, 120 + i * 160));
  }

  document.querySelectorAll(".peer[data-peer]").forEach((el) => {
    const pick = () => {
      document.querySelectorAll(".peer").forEach((x) => { x.classList.remove("active"); x.setAttribute("aria-selected", "false"); });
      el.classList.add("active");
      el.setAttribute("aria-selected", "true");
      loadPeer(el.dataset.peer);
    };
    el.addEventListener("click", pick);
    el.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(); } });
  });

  const CMDS = {
    "/help": () => sys("commands: /help · /finger · /verify · /msg &lt;text&gt; · /invite · /contacts · /deny · /wipe"),
    "/finger": () => sys(`peer ${PEERS[peer].fp} · X25519 · ${peer === "operator" ? "UNVERIFIED — run /verify" : "verified · session pinned"}`),
    "/verify": () => {
      sys("sas: 041 · 229 · 088 · 611 · 204 · 773 · 512");
      setTimeout(() => sys("— read aloud out-of-band · if it matches, session pinned —"), 350);
      if (peer === "operator") setTimeout(() => { sys("— operator verified · sends unblocked —"); peerStatus.textContent = "· verified"; }, 800);
    },
    "/invite": () => sys("alias for /add — consumes a cinder:// link · minting is invite.create (rpc) or cinder id"),
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
  document.querySelectorAll(".kc[data-cmd]").forEach((kc) => {
    const press = () => {
      input.value = kc.dataset.cmd;
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
      input.focus();
    };
    kc.addEventListener("click", press);
  });
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
  let reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const _prmM = matchMedia('(prefers-reduced-motion: reduce)');
  (_prmM.addEventListener || _prmM.addListener).call(_prmM, 'change', e => { reduced = e.matches; });
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
    W = document.documentElement.clientWidth; H = document.documentElement.clientHeight;
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
paintBurns();

/* ---- dither-flood buttons: left sweep + click-point ignite ---- */
function attachFlood(btn){
  const cv=btn.querySelector('canvas');
  if(!cv)return;
  const fill=btn.dataset.fill||'#FFB224',halo=btn.dataset.halo||'#5A4114';
  let s=0,tgt=0;            /* hover sweep */
  let rs=0,rtgt=0;          /* ignite ripple 0..1 */
  let ix=0,iy=0,maxD=1;     /* ignite origin (cell coords) + radius */
  let raf=null;
  function draw(){
    paint(cv,3,(x,y,c)=>{
      const E=s*(c+18)-9;
      let v=(E-x)/10;
      if(rs>0){
        const fr=rs*maxD;
        const dx=x-ix,dy=(y-iy)*1.5;
        const vR=(fr-Math.sqrt(dx*dx+dy*dy))/9;
        if(vR>v)v=vR;
      }
      return Math.min(1,Math.max(0,v));
    },(v,t,x,y)=>{
      if(v>t)return fill;
      if(v*.45>t)return h2d(x,y,5)<.14?'#FFB224':halo;
      if(h2d(x,y)<.045)return '#26292F';
      return null;
    });
  }
  function step(){
    s+=(tgt-s)*.18;
    if(Math.abs(tgt-s)<.004)s=tgt;
    rs+=(rtgt-rs)*.24;
    if(Math.abs(rtgt-rs)<.004)rs=rtgt;
    draw();
    if(s!==tgt||rs!==rtgt)raf=requestAnimationFrame(step);else raf=null;
  }
  function kick(){if(!raf)raf=requestAnimationFrame(step);}
  function go(v){tgt=v;if(PRM.matches){s=v;draw();return;}kick();}
  function rgo(v){rtgt=v;if(PRM.matches){rs=v;draw();return;}kick();}
  const hot=()=>btn.matches(':hover')||document.activeElement===btn;
  function igniteAt(px,py){
    const r=btn.getBoundingClientRect();
    ix=px/3;iy=py/3;
    const w=r.width/3,h=r.height/3;
    const dxm=Math.max(ix,w-ix),dym=Math.max(iy,h-iy)*1.5;
    maxD=Math.sqrt(dxm*dxm+dym*dym)+8;
    rgo(1);
  }
  function igniteCenter(){
    const r=btn.getBoundingClientRect();
    igniteAt(r.width/2,r.height/2);
  }
  btn.addEventListener('mouseenter',()=>go(1));
  btn.addEventListener('mouseleave',()=>{go(0);rgo(0);});
  btn.addEventListener('focus',()=>go(1));
  btn.addEventListener('blur',()=>{go(0);rgo(0);});
  btn.addEventListener('pointerdown',e=>{
    const r=btn.getBoundingClientRect();
    go(.85);
    igniteAt(e.clientX-r.left,e.clientY-r.top);
  });
  btn.addEventListener('pointerup',()=>{go(hot()?1:0);rgo(0);});
  btn.addEventListener('pointercancel',()=>{rgo(0);});
  btn.addEventListener('keydown',e=>{
    if(e.repeat)return;
    if(e.key===' '||e.key==='Enter'){
      if(e.key===' ')e.preventDefault();
      go(.85);igniteCenter();
    }
  });
  btn.addEventListener('keyup',e=>{
    if(e.key===' '||e.key==='Enter'){
      go(hot()?1:0);rgo(0);
      if(e.key===' ')btn.click();
    }
  });
  draw();
}
document.querySelectorAll('.btn-primary[data-fill]').forEach(attachFlood);

/* ---- Space parity on link-styled controls (a.scroll fix) ---- */
document.querySelectorAll('.btn-ghost,.vtag').forEach(el=>{
  el.addEventListener('keydown',e=>{if(e.key===' ')e.preventDefault();});
  el.addEventListener('keyup',e=>{if(e.key===' ')el.click();});
});

/* ---- page-edge dissolve v2 ---- */
(function(){
  const CELL=4,MAX_CREEPS=2;
  const insts=[];
  document.querySelectorAll('.dissolve-cv').forEach(cv=>{
    insts.push({cv,dir:cv.dataset.dir==='up'?'up':'down',
      cols:0,rows:0,t:0,nextSpawn:4+Math.random()*6,creeps:[],dirty:true});
  });
  if(!insts.length)return;

  function resizeAll(){
    for(const st of insts){
      st.cols=Math.max(1,Math.ceil(st.cv.clientWidth/CELL));
      st.rows=Math.max(1,Math.ceil(st.cv.clientHeight/CELL));
      st.creeps.length=0;st.dirty=true;
    }
  }

  /* creep depth (cells) contributed at column x — smoothstep taper
     across its width + ragged hash edge = corrosion, not a rectangle */
  function creepDepth(st,c,x){
    let dp=0;
    for(const k of st.creeps){
      const u=1-Math.abs(x-k.x0)/k.w;
      if(u<=0)continue;
      const shape=u*u*(3-2*u),
            d=k.depth*k.env*shape+(h2d(x,k.seed,23)-.5)*2.6;
      if(d>dp)dp=d;
    }
    return dp;
  }

  function makeVfn(st){
    return function(x,y,c,r){
      const din=st.dir==='up'?y:(r-1-y);       /* distance from edge */
      let v=Math.pow(1-din/r,1.5);             /* prod base, unchanged */
      const dp=creepDepth(st,c,x);
      if(dp>din){
        const cv2=(dp-din)/2.4;                /* ~2-cell dithered front */
        const cc=cv2>1?1:cv2;
        if(cc*.92>v)v=cc*.92;
      }
      return v;
    };
  }
  function makeCfn(st){
    return function(v,th,x,y){
      if(v<=th)return null;
      const din=st.dir==='up'?y:(st.rows-1-y);
      const s=h2d(x,y,3);
      /* advancing creep front glints warm */
      const dpv=st.creeps.length?creepDepth(st,st.cols,x):0;
      if(dpv>0&&Math.abs(dpv-din)<1.4){
        if(s<.08)return '#FFB224';
        if(s<.30)return '#B87A18';
      }
      if(s<.02)return s<.006?'#FFB224':'#B87A18';
      return v>.82?'#565D68':v>.5?'#3D434B':'#292D33';
    };
  }

  function spawnCreep(st){
    st.creeps.push({
      x0:st.cols*(.1+.8*Math.random()),
      w:5+Math.random()*8,
      depth:4+Math.random()*5,                 /* cells inward */
      grow:5+Math.random()*2,hold:2+Math.random()*1.5,
      fade:6+Math.random()*2.5,
      seed:(Math.random()*97)|0,env:0,t0:st.t
    });
  }

  function tick(nt){
    for(const st of insts){
      st.t=nt;
      if(nt>=st.nextSpawn&&st.creeps.length<MAX_CREEPS){
        if(Math.random()>.45)spawnCreep(st);   /* sparse: ~55% of windows skip */
        st.nextSpawn=nt+7+Math.random()*5;
      }
      let live=false;
      for(let i=st.creeps.length-1;i>=0;i--){
        const k=st.creeps[i],age=nt-k.t0;
        if(age<k.grow){const u=age/k.grow;k.env=u*u*(3-2*u);}
        else if(age<k.grow+k.hold)k.env=1;
        else k.env=1-(age-k.grow-k.hold)/k.fade;
        if(k.env<=0&&age>k.grow+k.hold){st.creeps.splice(i,1);continue;}
        live=true;
      }
      if(live||st.dirty){
        paint(st.cv,CELL,makeVfn(st),makeCfn(st));
        st.dirty=live;                          /* one more paint after last creep */
      }
    }
  }

  function stillAll(){
    for(const st of insts){
      st.t=6;
      st.creeps=[{x0:st.cols*.63,w:9,depth:6.5,env:1,seed:11}];
      paint(st.cv,CELL,makeVfn(st),makeCfn(st));
      st.creeps=[];st.dirty=false;
    }
  }

  /* one loop instance drives every .dissolve-cv canvas */
  const driver=document.querySelector('.dissolve-cv');
  fieldLoop(driver,{resize:resizeAll,tick,still:stillAll});
})();

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

/* ---- molten field v2 ---- */
(function(){
  const cv=document.getElementById('moltenCanvas');if(!cv)return;
  const CELL=8,SKY=.22;
  const T_CYC=9.5,T_FILL=5.0,T_HOLD=1.4,T_REL=1.4;   /* charge cycle, s */
  const FILL=['#8A5F17','#B87A18','#DB9A1F','#FFB224','#FFCE66','#FFE7AC'];
  const BED=['#241A0B','#33230D','#452F10','#5C3F12','#755213',
             '#8F6516','#B87A18','#D9961D','#FFB224','#FFD073'];
  let t=0,cols=0,rows=0,skyRows=0,P=0,REL=0;

  function resize(){
    cols=Math.max(1,Math.ceil(cv.clientWidth/CELL));
    rows=Math.max(1,Math.ceil(cv.clientHeight/CELL));
    skyRows=Math.max(1,Math.round(rows*SKY));
  }

  /* charge fill value per cell: ragged front, ordered blocks.
     P = fill progress 0..1, REL = dissolve progress 0..1 */
  function charge(x,y){
    if(P<=0)return 0;
    const fx=P*(cols+8)-4+(h2d(x,3,13)-.5)*3.6;   /* per-column front jitter */
    let fv=(fx-x)/2.6;
    fv=fv<0?0:fv>1?1:fv;
    if(REL>0)fv-=REL*1.3;                         /* blocks dissolve in B8 order */
    if(fv>0)fv*=.72+.28*h2d(x,y,21+((t*5)|0));    /* interior shimmer */
    return fv<0?0:fv;
  }

  /* molten bed — bottom-anchored noise + rising pulses */
  function molten(x,y){
    const fy=(y-skyRows)/(rows-skyRows);
    let n=.5+.5*Math.sin(x*.055+t*.9+y*.11)
        +.3*Math.sin(x*.11-t*.6-y*.04+1.9)
        +.2*Math.sin(x*.21+t*1.7-y*.13+4.2);
    n*=.5;
    let v=(fy*fy*.85+fy*.35)*(.4+.95*n);
    v+=.18*fy*fy*Math.max(0,Math.sin(x*.09-t*.7)+Math.sin(x*.031+t*.43+2))*.5;
    const ex=x/cols;
    v*=Math.min(1,ex*14)*Math.min(1,(1-ex)*14);
    return v<0?0:v>1?1:v;
  }

  function vfn(x,y){
    if(y<skyRows)return 0;
    const f=charge(x,y),m=molten(x,y);
    return f>m?f:m;
  }
  function cfn(v,th,x,y){
    if(y<skyRows){                                /* star sliver */
      const s=h2d(x,y,31);
      if(s<.05&&Math.sin(t*(1+s*9)+s*40)>.2)
        return s<.012?'#FFE9C0':'#8A6A2A';
      return null;
    }
    const tt=th*.62+h2d(x,y,9)*.38;
    const f=charge(x,y);
    if(f>tt){                                     /* charge blocks lead */
      const h=.32+(1-f)*.56+h2d(x,y,27)*.2;       /* fresh front cells burn palest */
      return FILL[Math.min(FILL.length-1,(h*(FILL.length-1))|0)];
    }
    if(v>tt){                                     /* molten bed */
      const h=Math.min(1,v*.8+(v-tt)*.5);
      return BED[Math.min(BED.length-1,(h*(BED.length-1))|0)];
    }
    return v>.3&&h2d(x,y,5)<.4?'#1C130A':null;    /* warm ground haze */
  }

  function tick(nt){
    t=nt;
    const ph=t%T_CYC;
    if(ph<T_FILL){const u=ph/T_FILL;P=u<.12?u*u/.12:u;REL=0;}
    else if(ph<T_FILL+T_HOLD){P=1;REL=0;}
    else if(ph<T_FILL+T_HOLD+T_REL){P=1;REL=(ph-T_FILL-T_HOLD)/T_REL;}
    else{P=0;REL=0;}
    paint(cv,CELL,vfn,cfn);
  }

  function still(){t=6;P=.62;REL=0;paint(cv,CELL,vfn,cfn);}

  fieldLoop(cv,{resize,tick,still});
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
    var t = e.target;
    if (t && (/^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) || t.isContentEditable)) return;
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

/* ---- page-edge dither frame: bayer vignette on all 4 viewport edges ---- */
(function(){
  var base = document.getElementById('fx-base');
  var fx = document.getElementById('fx-ember');
  if (!base || !fx) return;
  var RES = 2, CELL = 6, DOT = 2.9;
  var EDGE_L = 104, EDGE_R = 104, EDGE_T = 38, EDGE_B = 38, MAX_IGN = 24;
  var FAINT = [61,66,73], DIMB = [90,65,20];
  var TH8 = new Float32Array([
     0,32, 8,40, 2,34,10,42, 48,16,56,24,50,18,58,26,
    12,44, 4,36,14,46, 6,38, 60,28,52,20,62,30,54,22,
     3,35,11,43, 1,33, 9,41, 51,19,59,27,49,17,57,25,
    15,47, 7,39,13,45, 5,37, 63,31,55,23,61,29,53,21]);
  for (var i=0; i<64; i++) TH8[i] = (TH8[i]+0.5)/64;
  function ign(x, y){
    var f = (0.06711056*x + 0.00583715*y) % 1;
    return (52.9829189*f) % 1;
  }
  function density(x, y, w, h, o){
    var d = 0;
    if (x < o.l)       d = 1 - x/o.l;
    if (w - x < o.r)   d = Math.max(d, 1 - (w - x)/o.r);
    if (y < o.t)       d = Math.max(d, 1 - y/o.t);
    if (h - y < o.b)   d = Math.max(d, 1 - (h - y)/o.b);
    return Math.pow(d, 1.45);
  }
  function paintFrame(ctx, w, h, o){
    var cellT = CELL/RES;
    var cols = Math.ceil(w/cellT), rows = Math.ceil(h/cellT);
    var img = ctx.createImageData(w, h);
    var d = img.data, lit = [];
    for (var p0=0; p0<d.length; p0+=4){
      d[p0]=11; d[p0+1]=12; d[p0+2]=14; d[p0+3]=255;
    }
    for (var gy=0; gy<rows; gy++){
      var cy = gy*cellT + cellT/2;
      var cssY = cy*RES;
      var brow = (gy&7)<<3;
      for (var gx=0; gx<cols; gx++){
        var cx = gx*cellT + cellT/2;
        var cssX = cx*RES;
        var dens = density(cssX, cssY, w*RES, h*RES, o);
        if (dens <= 0) continue;
        var th = TH8[brow|(gx&7)]*0.62 + ign(gx,gy)*0.38;
        if (dens <= th) continue;
        var beat = (dens - th)/(1 - th);
        var rad = (DOT/RES) * (0.38 + 0.62*Math.min(1, beat*1.5));
        var r2 = rad*rad;
        var warm = Math.pow(dens, 3)*0.85;
        var cr = (FAINT[0] + (DIMB[0]-FAINT[0])*warm)|0;
        var cg = (FAINT[1] + (DIMB[1]-FAINT[1])*warm)|0;
        var cb = (FAINT[2] + (DIMB[2]-FAINT[2])*warm)|0;
        var x0 = Math.max(0, Math.floor(cx-rad));
        var x1 = Math.min(w-1, Math.ceil(cx+rad));
        var y0 = Math.max(0, Math.floor(cy-rad));
        var y1 = Math.min(h-1, Math.ceil(cy+rad));
        for (var ty=y0; ty<=y1; ty++){
          for (var tx=x0; tx<=x1; tx++){
            var dx = tx+0.5-cx, dy = ty+0.5-cy;
            if (dx*dx+dy*dy > r2) continue;
            var p = (ty*w+tx)*4;
            d[p]=cr; d[p+1]=cg; d[p+2]=cb; d[p+3]=255;
          }
        }
        lit.push({x:cx, y:cy, r:rad});
      }
    }
    ctx.putImageData(img, 0, 0);
    return lit;
  }
  function drawIgn(ctx, x, y, r, a){
    var gr = r*3.4;
    var g = ctx.createRadialGradient(x, y, 0, x, y, gr);
    g.addColorStop(0, 'rgba(255,178,36,'+(0.28*a).toFixed(3)+')');
    g.addColorStop(1, 'rgba(255,178,36,0)');
    ctx.fillStyle = g;
    ctx.fillRect(x-gr, y-gr, gr*2, gr*2);
    ctx.fillStyle = 'rgba(255,178,36,'+(0.95*a).toFixed(3)+')';
    ctx.fillRect(x-r, y-r, r*2, r*2);
    ctx.fillStyle = 'rgba(255,235,190,'+(0.55*a).toFixed(3)+')';
    ctx.fillRect(x-r*0.45, y-r*0.45, r*0.9, r*0.9);
  }
  var mq = window.matchMedia('(prefers-reduced-motion: reduce)');
  var reduced = mq.matches;
  var bctx = base.getContext('2d', {alpha:false});
  var fctx = fx.getContext('2d');
  var lit = [], igns = [], raf = 0, running = false;
  var nextSpawn = 0;
  function build(){
    var w = document.documentElement.clientWidth, h = document.documentElement.clientHeight;
    base.width = fx.width = Math.ceil(w/RES);
    base.height = fx.height = Math.ceil(h/RES);
    var o = {l:EDGE_L, r:EDGE_R, t:EDGE_T, b:EDGE_B};
    lit = paintFrame(bctx, base.width, base.height, o);
    fctx.clearRect(0, 0, fx.width, fx.height);
    igns = [];
    if (reduced && lit.length){
      for (var k=0; k<9; k++){
        var c = lit[(Math.random()*lit.length)|0];
        drawIgn(fctx, c.x, c.y, c.r*1.4, 0.5 + Math.random()*0.4);
      }
    }
  }
  function loop(now){
    raf = 0;
    var t = now/1000;
    fctx.clearRect(0, 0, fx.width, fx.height);
    if (t >= nextSpawn && igns.length < MAX_IGN && lit.length){
      var n = Math.random() < 0.35 ? 2 : 1;
      for (var k=0; k<n; k++){
        var c = lit[(Math.random()*lit.length)|0];
        igns.push({x:c.x, y:c.y, r:c.r*1.4, t0:t,
                   ttl:1.1 + Math.random()*1.6});
      }
      nextSpawn = t + 0.5 + Math.random()*1.4;
    }
    for (var i=igns.length-1; i>=0; i--){
      var s = igns[i], p = (t - s.t0)/s.ttl;
      if (p >= 1){ igns.splice(i,1); continue; }
      drawIgn(fctx, s.x, s.y, s.r, Math.sin(p*Math.PI));
    }
    if (running) raf = requestAnimationFrame(loop);
  }
  function start(){
    if (!running && !reduced && !document.hidden){
      running = true;
      if (!raf) raf = requestAnimationFrame(loop);
    }
  }
  function stop(){ running = false; if (raf){ cancelAnimationFrame(raf); raf=0; } }
  document.addEventListener('visibilitychange', function(){
    document.hidden ? stop() : start();
  });
  if (mq.addEventListener) mq.addEventListener('change', function(e){
    reduced = e.matches; build();
    reduced ? stop() : start();
  });
  var rt; window.addEventListener('resize', function(){
    clearTimeout(rt); rt = setTimeout(build, 150);
  });
  build();
  start();
})();

/* ---- relay instrument panel: scope + ppm + throughput, all dithered ---- */
(function(){
'use strict';
var panelEl = document.getElementById('panel');
if (!panelEl) return;

var RM = matchMedia('(prefers-reduced-motion: reduce)');
var reduced = RM.matches;
var STATIC_T = 8.73;

var LEVELS, PAL = [
  [ 11, 12, 14],[ 20, 21, 25],[ 33, 35, 41],[ 50, 47, 38],
  [ 74, 55, 22],[106, 71, 20],[146, 96, 24],[196,127, 27],
  [232,152, 30],[255,178, 36],[255,190, 52],[255,214,110],[255,240,196]
];
LEVELS = PAL.length - 1;

var TH8 = new Float32Array([
   0,32, 8,40, 2,34,10,42, 48,16,56,24,50,18,58,26,
  12,44, 4,36,14,46, 6,38, 60,28,52,20,62,30,54,22,
   3,35,11,43, 1,33, 9,41, 51,19,59,27,49,17,57,25,
  15,47, 7,39,13,45, 5,37, 63,31,55,23,61,29,53,21]);
for (var i=0; i<64; i++) TH8[i] = (TH8[i]+0.5)/64;

function ign(x, y){
  var f = (0.06711056*x + 0.00583715*y) % 1;
  return (52.9829189*f) % 1;
}
function el(id){ return document.getElementById(id); }
function clamp01(v){ return v<0 ? 0 : (v>1 ? 1 : v); }

function makeRaster(cv, cell){
  var r = {cv:cv, ctx:cv.getContext('2d',{alpha:false}), cell:cell,
           cols:0, rows:0, img:null, V:null};
  r.fit = function(){
    r.cols = Math.max(16, Math.round(cv.clientWidth/cell));
    r.rows = Math.max(8,  Math.round(cv.clientHeight/cell));
    cv.width = r.cols; cv.height = r.rows;
    r.img = r.ctx.createImageData(r.cols, r.rows);
    r.V = new Float32Array(r.cols*r.rows);
  };
  return r;
}

function quantize(r, vals){
  var d = r.img.data, cols = r.cols, rows = r.rows, p = 0, o = 0;
  for (var y=0; y<rows; y++){
    var brow = (y&7)<<3;
    for (var x=0; x<cols; x++){
      var v = vals[p];
      var th = TH8[brow|(x&7)]*0.62 + ign(x,y)*0.38;
      var f = v*LEVELS, l = f|0;
      if (f-l > th) l++;
      if (l<0) l = 0; else if (l>LEVELS) l = LEVELS;
      var c = PAL[l];
      d[o]=c[0]; d[o+1]=c[1]; d[o+2]=c[2]; d[o+3]=255;
      p++; o+=4;
    }
  }
  r.ctx.putImageData(r.img, 0, 0);
}

var sc  = makeRaster(el('cv-scope'), 3);
var scP = null, scS = null, scR = null, scA = null;
var scCx = 0, scCy = 0, scRmax = 1;
var TAU = Math.PI*2;

function scopeFit(){
  sc.fit();
  var n = sc.cols*sc.rows;
  scP = new Float32Array(n);
  scS = new Float32Array(n);
  scR = new Float32Array(n);
  scA = new Float32Array(n);
  scCx = (sc.cols-1)/2; scCy = (sc.rows-1)/2;
  scRmax = Math.min(scCx, scCy);
  for (var y=0, i=0; y<sc.rows; y++)
    for (var x=0; x<sc.cols; x++, i++){
      var dx = (x-scCx)/scRmax, dy = (y-scCy)/scRmax;
      scR[i] = Math.sqrt(dx*dx+dy*dy);
      scA[i] = Math.atan2(dy, dx);
    }
}

function drawScope(t){
  var sig = 0.55 + 0.28*Math.sin(t*0.9)
                 + 0.10*Math.sin(t*2.17+1.7)
                 + 0.05*Math.sin(t*5.3+0.6);
  sig = clamp01(sig); if (sig<0.08) sig = 0.08;
  var R = 0.34 + 0.52*sig, iR2 = 1.9/(R*R);
  var arm = t*1.05;
  var cols = sc.cols, rows = sc.rows;
  var rhw = 0.6/scRmax;

  for (var y=0, i=0; y<rows; y++)
    for (var x=0; x<cols; x++, i++){
      var r = scR[i], a = scA[i];
      var back = (arm - a) % TAU; if (back<0) back += TAU;
      var trail = 1 - back/1.5;
      if (trail<0) trail = 0; trail *= trail;

      var v = 0.014 + 0.010*ign(x+11, y+47);
      if (Math.abs(r-0.45)<rhw || Math.abs(r-0.85)<rhw)
        v += 0.045;
      if (Math.abs(x-scCx)<0.6 || Math.abs(y-scCy)<0.6)
        v += 0.035;

      var blob = Math.exp(-r*r*iR2);
      v += blob * (0.30+0.85*sig) * (0.40+0.85*trail);
      v += 0.09*Math.sin(r*30 - t*4.5)*Math.exp(-r*1.9)
           * (0.3+0.7*trail) * sig;

      var dArm = Math.min(back, TAU-back);
      if (dArm<0.05 && r<1.04)
        v += (1 - dArm/0.05) * (0.9 - 0.45*r);

      var p = scP[i]*0.90; if (v>p) p = v;
      scP[i] = p;
      scS[i] = p * (0.96 + 0.04*Math.sin(t*23 + r*31 + x*0.7));
    }
  quantize(sc, scS);
}

var vu = makeRaster(el('cv-vu'), 4);
var CH = 8, M_W = 3;
var vuDisp = new Float32Array(CH);
var vuPk   = new Float32Array(CH);
var vuHold = new Float32Array(CH);

function chSrc(c, t){
  switch(c){
    case 0: {
      var g = Math.sin(t*0.53+1) > 0 ? 1 : 0.15;
      return g*(0.45 + 0.35*Math.abs(Math.sin(t*4.7))
                      + 0.20*Math.abs(Math.sin(t*7.9+2)));
    }
    case 1: return 0.35 + 0.30*Math.sin(t*1.3+0.8) + 0.15*Math.abs(Math.sin(t*5.1));
    case 2: return 0.22 + 0.10*Math.sin(t*0.8)
                  + 0.35*Math.max(0, Math.sin(t*0.31+2)*Math.sin(t*2.9));
    case 3: return 0.50 + 0.28*Math.sin(t*2.6+4) + 0.18*Math.sin(t*11+1);
    case 4: return 0.30 + 0.18*Math.sin(t*0.42+3.4) + 0.08*Math.sin(t*1.8);
    case 5: return 0.40 + 0.30*Math.abs(Math.sin(t*3.3+1.2))*Math.sin(t*0.9);
    case 6: return 0.15 + 0.12*Math.sin(t*1.1+5) + 0.20*Math.abs(Math.sin(t*6.7));
    default:return Math.pow(Math.max(0, Math.sin(t*0.37+0.6)), 18);
  }
}

function drawVU(t, dt){
  var cols = vu.cols, rows = vu.rows;
  var dotRows = Math.floor(rows/2);
  var gap = Math.max(2, Math.floor((cols - CH*M_W)/(CH+1)));

  var ATT = 1 - Math.exp(-dt/0.02), REL = 1 - Math.exp(-dt/0.35);
  for (var c=0; c<CH; c++){
    var s = clamp01(chSrc(c, t)), d = vuDisp[c];
    d += (s>d ? ATT : REL) * (s-d);
    vuDisp[c] = clamp01(d);
    if (s >= vuPk[c]){ vuPk[c] = s; vuHold[c] = 1.1; }
    else {
      vuHold[c] -= dt;
      if (vuHold[c] < 0){ vuPk[c] -= dt*0.55; if (vuPk[c] < d) vuPk[c] = d; }
    }
  }

  var V = vu.V; V.fill(0);
  var y, x, i, yb;
  for (y=0; y<rows; y++){
    yb = rows-1-y;
    if (yb & 1) continue;
    for (x=0; x<2; x++) V[y*cols+x] = 0.10;
    for (x=cols-2; x<cols; x++) V[y*cols+x] = 0.10;
  }

  for (c=0; c<CH; c++){
    var x0 = gap + c*(M_W+gap);
    var pkRow = Math.round(vuPk[c]*(dotRows-1));
    var fillRows = vuDisp[c]*dotRows;
    for (var di=0; di<dotRows; di++){
      y = rows-1 - di*2;
      var cov = fillRows - di;
      var lit = 0;
      if (cov > 0)
        lit = Math.min(1, cov) * (0.55 + 0.45*(di+1)/dotRows) * 1.05;
      for (var k=0; k<M_W; k++){
        i = y*cols + x0+k;
        if (V[i] < 0.14) V[i] = 0.14;
        if (lit > V[i]) V[i] = lit;
        if (di === pkRow && vuPk[c] > 0.03) V[i] = 1.2;
      }
    }
  }
  quantize(vu, V);
}

var sp = makeRaster(el('cv-spark'), 3);
var TICK = 0.12;
var hist = null, lastTick = 0;

function sample(t){
  var v = 0.42 + 0.24*Math.sin(t*0.57)
               + 0.16*Math.sin(t*1.63+2.4)
               + 0.10*Math.sin(t*3.7+0.9)
               + (Math.random()-0.5)*0.14;
  if (Math.random() < 0.05) v += 0.30;
  return clamp01(v);
}
function seedHist(t){
  hist = new Float32Array(sp.cols+1);
  for (var i=0; i<=sp.cols; i++){
    var tt = t - (sp.cols-i)*TICK;
    hist[i] = clamp01(0.42 + 0.24*Math.sin(tt*0.57)
                            + 0.16*Math.sin(tt*1.63+2.4)
                            + 0.10*Math.sin(tt*3.7+0.9)
                            + 0.06*Math.sin(tt*13.7+i));
  }
  lastTick = t;
}
function drawSpark(t){
  var cols = sp.cols, rows = sp.rows;
  while (t - lastTick >= TICK){
    lastTick += TICK;
    hist.copyWithin(0, 1);
    hist[cols] = sample(lastTick);
  }
  var frac = (t - lastTick)/TICK;
  if (frac>1) frac = 1; else if (frac<0) frac = 0;

  var V = sp.V;
  for (var x=0; x<cols; x++){
    var s = hist[x] + (hist[x+1]-hist[x])*frac;
    var h = s*rows;
    for (var y=0; y<rows; y++){
      var yb = rows-1-y;
      var v = 0.012 + 0.008*ign(x*3+y, y*7+x);
      if (yb === 0) v = 0.12;
      if (yb < h){
        var dep = (h - yb)/h;
        var cov = Math.min(1, h - yb);
        var vv = cov * (1.08 - 0.62*dep);
        if (vv > v) v = vv;
      }
      V[y*cols+x] = v;
    }
  }
  quantize(sp, V);
}

var raf = 0, running = false, inView = false, last = 0;

function drawAll(t, dt){
  drawScope(t);
  drawVU(t, dt);
  drawSpark(t);
}

function warmup(tEnd, n){
  seedHist(tEnd);
  for (var i=n; i>0; i--){
    var t = tEnd - i/60;
    drawAll(t, 1/60);
  }
  drawAll(tEnd, 1/60);
}

function loop(now){
  raf = 0;
  var t = now/1000, dt = t - last;
  if (dt > 0.05) dt = 0.05;
  if (dt < 0) dt = 0;
  drawAll(t, dt);
  last = t;
  if (running) raf = requestAnimationFrame(loop);
}

function start(){
  if (!running && !reduced && !document.hidden && inView){
    running = true;
    last = performance.now()/1000;
    if (!raf) raf = requestAnimationFrame(loop);
  }
}
function stop(){
  running = false;
  if (raf){ cancelAnimationFrame(raf); raf = 0; }
}

function fitAll(){
  scopeFit();
  vu.fit();
  sp.fit();
  seedHist(performance.now()/1000);
  if (reduced) warmup(STATIC_T, 30);
}

document.addEventListener('visibilitychange', function(){
  document.hidden ? stop() : start();
});
new IntersectionObserver(function(e){
  inView = e[0].isIntersecting;
  inView ? start() : stop();
}).observe(panelEl);

if (RM.addEventListener) RM.addEventListener('change', function(e){
  reduced = e.matches;
  if (reduced){ stop(); warmup(STATIC_T, 30); } else { start(); }
});

var rt; window.addEventListener('resize', function(){
  clearTimeout(rt); rt = setTimeout(fitAll, 150);
});

fitAll();
if (reduced) warmup(STATIC_T, 30);
start();
})();

/* ---------- ident-ticket v2: fp matrix + tear ---------- */
(function(){

(function(){
'use strict';

/* ================================================================
   ident-ticket-v2 — one IIFE:
     1. Matrix() fingerprint factory (prod #cv-fp code, parameterized
        by seed + optional single-cell diff for the MITM compare)
     2. hex fingerprint text (selectable, one forged group on claimed)
     3. compare toggle
     4. ticket tear + aria-live announce + reset
     5. ASCII column-width assertions
   ================================================================ */
var RM = matchMedia('(prefers-reduced-motion: reduce)');
var reduced = RM.matches;
var STATIC_T = 4.61;          // frozen frame: scanline mid-field, flare dying

function el(id){ return document.getElementById(id); }
function clamp01(v){ return v<0 ? 0 : (v>1 ? 1 : v); }

/* cyrb53 — small deterministic string hash (prod verbatim) */
function hash53(str, seed){
  var h1 = 0xdeadbeef ^ (seed||0), h2 = 0x41c6ce57 ^ (seed||0);
  for (var i=0; i<str.length; i++){
    var ch = str.charCodeAt(i);
    h1 = Math.imul(h1 ^ ch, 2654435761);
    h2 = Math.imul(h2 ^ ch, 1597334677);
  }
  h1 = Math.imul(h1 ^ (h1>>>16), 2246822507) ^ Math.imul(h2 ^ (h2>>>13), 3266489909);
  h2 = Math.imul(h2 ^ (h2>>>16), 2246822507) ^ Math.imul(h1 ^ (h1>>>13), 3266489909);
  return (h2>>>0) * 4294967296 + (h1>>>0);
}

/* shared ordered-dither recipe (prod verbatim) */
var PAL = [
  [ 11, 11, 13],[ 20, 21, 25],[ 33, 35, 41],[ 50, 47, 38],[ 74, 55, 22],
  [106, 71, 20],[146, 96, 24],[196,127, 27],[232,152, 30],[255,178, 36],
  [255,190, 52],[255,214,110],[255,240,196]
];
var LEVELS = PAL.length - 1;
var TH8 = new Float32Array([
   0,32, 8,40, 2,34,10,42, 48,16,56,24,50,18,58,26,
  12,44, 4,36,14,46, 6,38, 60,28,52,20,62,30,54,22,
   3,35,11,43, 1,33, 9,41, 51,19,59,27,49,17,57,25,
  15,47, 7,39,13,45, 5,37, 63,31,55,23,61,29,53,21]);
for (var i=0; i<64; i++) TH8[i] = (TH8[i]+0.5)/64;
function ign(x, y){
  var f = (0.06711056*x + 0.00583715*y) % 1;
  return (52.9829189*f) % 1;
}

/* ================= fingerprint matrix factory =================
   prod's #cv-fp singleton becomes Matrix(cvEl, seed, diff):
   16×16 cells, mirrored L→R; Gaussian feature field + per-cell detail
   seeded by `seed`; diff={x,y} overrides ONE full-grid cell on the
   right half so the claimed glyph differs in a single cell-block. */
var GRID = 16, HALF = 8;

function Matrix(cvEl, seed, diff){
  var cx2d = cvEl.getContext('2d', {alpha:false});
  var DPR = Math.min(window.devicePixelRatio || 1, 2);
  var PITCH = 9 * DPR;
  var BUF = GRID * PITCH + 1, IMG = null, V = null;
  var CELL = new Float32Array(HALF * GRID);
  var ENERGY = new Float32Array(GRID * GRID);
  var FEAT = [], nextIgn = 0;

  (function seedField(){
    var s = seed + ':feat';
    for (var f=0; f<7; f++){
      FEAT.push({
        x: 0.5 + (hash53(s+':x'+f, f) % 1000)/1000 * (HALF-1),
        y:      (hash53(s+':y'+f, f) % 1000)/1000 * (GRID-1),
        r: 1.6 + (hash53(s+':r'+f, f) % 1000)/1000 * 3.4,
        a: 0.45 + (hash53(s+':a'+f, f) % 1000)/1000 * 0.55
      });
    }
  })();

  function cellField(hx, y){
    var base = 0;
    for (var f=0; f<FEAT.length; f++){
      var g = FEAT[f], dx = hx-g.x, dy = y-g.y;
      var b = g.a * Math.exp(-(dx*dx+dy*dy)/(g.r*g.r));
      if (b > base) base = b;
    }
    var det = (hash53(seed+':c:'+hx+':'+y, 7) % 10000)/10000;
    return clamp01(base*(0.50 + 0.62*det) + 0.16*det*det*det*det - 0.02);
  }
  for (var y0=0; y0<GRID; y0++)
    for (var hx0=0; hx0<HALF; hx0++)
      CELL[y0*HALF+hx0] = cellField(hx0, y0);

  /* forge one cell-block: keep it clearly different from the real cell */
  if (diff){
    var orig = CELL[diff.y*HALF + (GRID-1-diff.x)];   // mirrored half-cell src
    diff.v = 0.30 + (hash53(seed+':fv', 11) % 560)/1000;
    if (Math.abs(diff.v - orig) < 0.30)
      diff.v = diff.v > orig ? Math.max(0.12, orig-0.34)
                             : Math.min(0.95, orig+0.34);
  }

  function cellAt(cx, cy){
    if (diff && cx===diff.x && cy===diff.y) return diff.v;
    var hx = cx < HALF ? cx : GRID-1-cx;
    return CELL[cy*HALF+hx];
  }

  function fit(){
    DPR = Math.min(window.devicePixelRatio || 1, 2);
    PITCH = 9 * DPR;
    BUF = GRID * PITCH + 1;
    cvEl.width = BUF; cvEl.height = BUF;
    IMG = cx2d.createImageData(BUF, BUF);
    V = new Float32Array(BUF * BUF);
  }

  function quantize(){
    var d = IMG.data, p = 0, o = 0;
    for (var y=0; y<BUF; y++){
      var brow = (y&7)<<3;
      for (var x=0; x<BUF; x++){
        var v = V[p];
        var th = TH8[brow|(x&7)]*0.62 + ign(x,y)*0.38;
        var f = v*LEVELS, l = f|0;
        if (f-l > th) l++;
        if (l<0) l = 0; else if (l>LEVELS) l = LEVELS;
        var c = PAL[l];
        d[o]=c[0]; d[o+1]=c[1]; d[o+2]=c[2]; d[o+3]=255;
        p++; o+=4;
      }
    }
    cx2d.putImageData(IMG, 0, 0);
  }

  function ignite(t, dt){
    if (t >= nextIgn){
      ENERGY[(Math.random()*GRID*GRID)|0] = 1;
      nextIgn = t + 0.7 + Math.random()*1.9;
    }
    var decay = Math.exp(-dt*2.4);
    for (var i=0; i<ENERGY.length; i++)
      if (ENERGY[i] > 0.002) ENERGY[i] *= decay; else ENERGY[i] = 0;
  }

  function draw(t, dt){
    ignite(t, dt);
    var margin = 8*DPR, span = BUF + margin*2;
    var sy = ((t*0.145) % 1) * span - margin;
    var sHW = 5*DPR;
    for (var y=0; y<BUF; y++){
      var cy = (y/PITCH)|0; if (cy > GRID-1) cy = GRID-1;
      var gY = (y % PITCH) < DPR;
      var scan = 1 - Math.abs(y-sy)/sHW;
      if (scan < 0) scan = 0;
      var brow = (y&7)<<3;
      for (var x=0; x<BUF; x++){
        var o = y*BUF + x, v;
        var cx = (x/PITCH)|0; if (cx > GRID-1) cx = GRID-1;
        if (gY || (x % PITCH) < DPR){
          v = 0.055 + 0.05*scan;
        } else {
          var ci = cy*GRID + cx;
          var cv0 = cellAt(cx, cy);
          var e = ENERGY[ci];
          var dens = cv0*(0.25 + 0.72*cv0) + 0.38*e;
          if (dens > 1) dens = 1;
          var th = TH8[brow|(x&7)]*0.70 + ign(x+31, y+57)*0.30;
          if (th < dens){
            v = 0.30 + 0.62*cv0 + 0.55*e;
          } else {
            v = 0.012 + 0.010*ign(x*3+y, y*7+x);
          }
          v += 0.13*scan*(0.35 + 0.65*cv0);
        }
        V[o] = v > 1 ? 1 : v;
      }
    }
    quantize();
  }

  function warmup(tEnd, n){
    for (var i=0; i<ENERGY.length; i++) ENERGY[i] = 0;
    nextIgn = 0;
    for (i=n; i>0; i--) draw(tEnd - i/60, 1/60);
    draw(tEnd, 1/60);
  }

  fit();
  return { draw:draw, warmup:warmup, fit:fit };
}

/* ---- three instances: solo, compare-you, compare-claimed ---- */
var SEED = '9c1f7bd24e88a1c3f50d6b4e9a2d7710c86be412f9aa07';
var DC = 9 + hash53(SEED+':dc', 3) % 5;     // diff cell: right half, interior
var DR = 3 + hash53(SEED+':dr', 5) % 10;    // (mirror keeps it a single block)

var mats = [];
function addM(id, diff){ var c = el(id); if (c) mats.push(Matrix(c, SEED, diff)); }
addM('cv-fp',   null);
addM('cv-fp-l', null);
addM('cv-fp-r', {x:DC, y:DR});

var fpCard = el('fpCard');
var raf = 0, running = false, inView = false, last = 0;

function loop(now){
  raf = 0;
  var t = now/1000, dt = t - last;
  if (dt > 0.05) dt = 0.05;
  if (dt < 0) dt = 0;
  for (var i=0; i<mats.length; i++) mats[i].draw(t, dt);
  last = t;
  if (running) raf = requestAnimationFrame(loop);
}
function start(){
  if (!running && !reduced && !document.hidden && inView){
    running = true;
    last = performance.now()/1000;
    if (!raf) raf = requestAnimationFrame(loop);
  }
}
function stop(){
  running = false;
  if (raf){ cancelAnimationFrame(raf); raf = 0; }
}
function fitAll(){
  for (var i=0; i<mats.length; i++){
    mats[i].fit();
    if (reduced) mats[i].warmup(STATIC_T, 90);
  }
}
document.addEventListener('visibilitychange', function(){
  document.hidden ? stop() : start();
});
new IntersectionObserver(function(e){
  inView = e[0].isIntersecting;
  inView ? start() : stop();
}).observe(fpCard);
if (RM.addEventListener) RM.addEventListener('change', function(e){
  reduced = e.matches;
  if (reduced){ stop(); for (var i=0;i<mats.length;i++) mats[i].warmup(STATIC_T,90); }
  else start();
});
var rt; window.addEventListener('resize', function(){
  clearTimeout(rt); rt = setTimeout(fitAll, 150);
});
if (reduced) for (var j=0; j<mats.length; j++) mats[j].warmup(STATIC_T, 90);
start();

/* ================= fingerprint hex (selectable) ================= */
function chunks(s, n){ var a=[],i; for(i=0;i<s.length;i+=n) a.push(s.slice(i,i+n)); return a; }
var GROUPS = chunks(SEED, 4);                 // 14 groups · 56 hex chars
var FH = ('0000000' + (hash53(SEED+':fg',13)>>>0).toString(16)).slice(-8);
var FORGE = FH.slice(0,4);
if (FORGE === GROUPS[4]) FORGE = 'dead';
var CGROUPS = GROUPS.slice(); CGROUPS[4] = FORGE;   // claimed: group 05 forged

function hexLine(groups, markCls){
  return groups.map(function(g,i){
    return i===4 ? '<b class="'+markCls+'">'+g+'</b>' : g;
  }).join(' ');
}
var hx;
if (hx = el('hexSolo')) hx.innerHTML = hexLine(GROUPS, 'ref');
if (hx = el('hexA'))    hx.innerHTML = hexLine(GROUPS, 'ref');
if (hx = el('hexB'))    hx.innerHTML = hexLine(CGROUPS, 'bad');

/* diff-cell markers + verdict */
function hex2(n){ return ('0'+n.toString(16)).slice(-2).toUpperCase(); }
['dmA','dmB'].forEach(function(id){
  var d = el(id); if (!d) return;
  d.style.left = (DC*6.25)+'%';
  d.style.top  = (DR*6.25)+'%';
});
var vd;
if (vd = el('vdText'))
  vd.textContent = '≠ MISMATCH — cell ('+hex2(DC)+'·'+hex2(DR)+') + hex group 05 differ — DO NOT TRUST';
if (vd = el('vdStat')) vd.textContent = '255/256 CELLS EQUAL';

/* ================= compare toggle ================= */
var cmpBtn = el('cmpBtn'), modeEl = el('fpMode');
var opts = cmpBtn ? cmpBtn.querySelectorAll('.opt') : [];
function setCmp(on){
  fpCard.classList.toggle('cmp', on);
  cmpBtn.setAttribute('aria-pressed', String(on));
  el('fpSolo').hidden = on;
  el('fpCmp').hidden = !on;
  for (var i=0; i<opts.length; i++)
    opts[i].classList.toggle('on', (i===1) === on);
  if (modeEl) modeEl.textContent = on ? 'compare' : 'solo';
}
if (cmpBtn){
  cmpBtn.addEventListener('click', function(){ setCmp(el('fpCmp').hidden); });
  document.addEventListener('keydown', function(e){
    if (e.key !== 'c' && e.key !== 'C') return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    var t = e.target;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
    setCmp(el('fpCmp').hidden);
  });
}

/* ================= ticket tear ================= */
var tk = el('tk'), keysTxt = el('tkKeysTxt'),
    live = el('tkLive'), reset = el('tkReset');
var KEYS_HTML = keysTxt ? keysTxt.innerHTML : '';

function redeem(){
  if (tk.classList.contains('torn')) return;
  tk.classList.add('torn');
  tk.setAttribute('aria-disabled', 'true');
  keysTxt.innerHTML = 'link consumed · <b>replay rejected</b> — stub detached';
  live.textContent = 'ticket redeemed — link consumed · replay rejected';
  if (reset) reset.hidden = false;
}
if (tk){
  tk.addEventListener('click', function(e){ e.preventDefault(); redeem(); });
  tk.addEventListener('keydown', function(e){
    if (e.key === 'Enter' || e.key === ' '){ e.preventDefault(); redeem(); }
  });
}
if (reset){
  reset.addEventListener('click', function(){
    tk.classList.remove('torn');
    tk.removeAttribute('aria-disabled');
    keysTxt.innerHTML = KEYS_HTML;
    live.textContent = '';
    reset.hidden = true;
  });
}

/* ---- ASCII column audit --------------------------------------------
   verified widths:
     .tk-main  58 cols × 16 rows  (prod desktop pre, cols 0–57, keeps ┊ perf)
     .tk-stub  15 cols × 16 rows  (prod desktop pre, cols 58–72)
     .tk-mtop  50 cols × 13 rows  (prod mobile pre, through the ├┄┤ row)
     .tk-mstub 50 cols ×  5 rows  (blank / SINGLE USE / serial / blank / └┘)
     .tk-void  14 cols ×  4 rows  (stamp overlay)
   combined desktop width = 73 cols — identical to prod's single pre.
   ------------------------------------------------------------------- */

})();

})();

/* ---------- voice call card ---------- */
(function(){
'use strict';
var RM = matchMedia('(prefers-reduced-motion: reduce)');
var reduced = RM.matches;
var STATIC_T = 14.31;

var LEVELS, PAL = [
  [ 11, 12, 14],[ 20, 21, 25],[ 33, 35, 41],[ 50, 47, 38],[ 74, 55, 22],
  [106, 71, 20],[146, 96, 24],[196,127, 27],[232,152, 30],[255,178, 36],
  [255,190, 52],[255,214,110],[255,240,196]
];
LEVELS = PAL.length - 1;

var TH8 = new Float32Array([
   0,32, 8,40, 2,34,10,42, 48,16,56,24,50,18,58,26,
  12,44, 4,36,14,46, 6,38, 60,28,52,20,62,30,54,22,
   3,35,11,43, 1,33, 9,41, 51,19,59,27,49,17,57,25,
  15,47, 7,39,13,45, 5,37, 63,31,55,23,61,29,53,21]);
for (var i=0; i<64; i++) TH8[i] = (TH8[i]+0.5)/64;

function ign(x, y){
  var f = (0.06711056*x + 0.00583715*y) % 1;
  return (52.9829189*f) % 1;
}
function el(id){ return document.getElementById(id); }
function clamp01(v){ return v<0 ? 0 : (v>1 ? 1 : v); }
function sstep(a, b, x){
  x = (x-a)/(b-a);
  return x<=0 ? 0 : (x>=1 ? 1 : x*x*(3-2*x));
}

var DPR = Math.min(window.devicePixelRatio || 1, 2);
var CELL = 4;

var cv = el('cv-wave');
if (!cv) return;
var ctx = cv.getContext('2d', {alpha:false});
var cols = 0, rows = 0, img = null, V = null, P = null;
var laneTX = {y0:0, y1:0, cy:0, hh:0};
var laneRX = {y0:0, y1:0, cy:0, hh:0};

function fit(){
  cols = Math.max(48, Math.round(cv.clientWidth * DPR / CELL));
  rows = Math.max(24, Math.round(cv.clientHeight * DPR / CELL));
  cv.width = cols; cv.height = rows;
  img = ctx.createImageData(cols, rows);
  V = new Float32Array(cols*rows);
  P = new Float32Array(cols*rows);
  var mid = rows>>1;
  laneTX.y0 = 0;        laneTX.y1 = mid-1;
  laneTX.cy = mid*0.5;  laneTX.hh = mid*0.5 - 2;
  laneRX.y0 = mid+1;    laneRX.y1 = rows-1;
  laneRX.cy = (mid+1 + rows-1)*0.5;
  laneRX.hh = (rows-1 - mid-1)*0.5 - 2;
}

function quantize(){
  var d = img.data, p = 0, o = 0;
  for (var y=0; y<rows; y++){
    var brow = (y&7)<<3;
    for (var x=0; x<cols; x++){
      var v = V[p];
      var th = TH8[brow|(x&7)]*0.62 + ign(x,y)*0.38;
      var f = v*LEVELS, l = f|0;
      if (f-l > th) l++;
      if (l<0) l = 0; else if (l>LEVELS) l = LEVELS;
      var c = PAL[l];
      d[o]=c[0]; d[o+1]=c[1]; d[o+2]=c[2]; d[o+3]=255;
      p++; o+=4;
    }
  }
  ctx.putImageData(img, 0, 0);
}

var SEC_PER_COL = 0.011;
var CORE_W = 1.15;
var GLOW_W = 4.6;
var iCore = 1/(2*CORE_W*CORE_W);
var iGlow = 1/(2*GLOW_W*GLOW_W);

function envTX(u){
  var g = sstep(0.30, 0.62, 0.5 + 0.5*Math.sin(u*0.43 + 0.9));
  var syl = 0.55 + 0.45*Math.sin(u*3.1 + Math.sin(u*0.9)*2.0);
  return clamp01(g * (0.28 + 0.72*syl) + 0.03);
}
function envRX(u){
  var g = sstep(0.25, 0.58, 0.5 + 0.5*Math.sin(u*0.43 + 0.9 + 2.9));
  var syl = 0.50 + 0.50*Math.sin(u*2.7 + 1.6 + Math.sin(u*0.63 + 3.0)*2.3);
  return clamp01(g * (0.30 + 0.70*syl) + 0.03);
}
function osc(u){
  return 0.55*Math.sin(u*57.0)
       + 0.28*Math.sin(u*103.0 + 1.4)
       + 0.17*Math.sin(u*151.0 + 4.2)*Math.sin(u*8.3);
}

var txGain = 1, rxGain = 1;
var txEnvNow = 0, rxEnvNow = 0;

function laneGain(base){
  if (ended) return 0.0;
  if (held)  return 0.05;
  return base;
}

function drawLane(lane, envFn, gain, t){
  var y0 = lane.y0, y1 = lane.y1, cy = lane.cy, hh = lane.hh;
  for (var x=0; x<cols; x++){
    var u = t - (cols-1-x)*SEC_PER_COL;
    var env = envFn(u) * gain;
    var tr = cy + osc(u)*env*hh;
    var bright = 0.16 + 0.84*Math.min(1, env*1.6);
    var edge = (x >= cols-2) ? 1.35 : 1.0;
    for (var y=y0; y<=y1; y++){
      var dd = y - tr;
      var v = 0.010 + 0.008*ign(x*5+y, y*11+x*3);
      v += Math.exp(-dd*dd*iCore)*0.92*bright*edge;
      v += Math.exp(-dd*dd*iGlow)*0.30*bright;
      var ax = Math.abs(y - cy);
      if (ax < 0.6) v += 0.085;
      else if (ax < 1.6) v += 0.03;
      if (y === y0 || y === y1) v += 0.06;
      var i = y*cols + x;
      var pp = P[i]*0.90; if (v > pp) pp = v;
      P[i] = pp;
      V[i] = pp * (0.965 + 0.035*Math.sin(t*21 + x*0.6 + y*0.13));
    }
  }
}

function drawDivider(t){
  var mid = rows>>1;
  for (var x=0; x<cols; x++){
    var v = 0.13 + 0.02*Math.sin(x*0.9 + t*0.7);
    var i = mid*cols + x;
    var pp = P[i]*0.90; if (v > pp) pp = v;
    P[i] = pp;
    V[i] = pp;
  }
}

var BASE_S = 4*60 + 17;
var elapsed = 0, shownSec = -1;
var muted = false, held = false, ended = false;
var nextHud = 0;

function fmt(s){
  s = s|0;
  var h = (s/3600)|0, m = ((s%3600)/60)|0, ss = s%60;
  return (h<10?'0':'')+h + ':' + (m<10?'0':'')+m + ':' + (ss<10?'0':'')+ss;
}
function tickChrome(t, dt){
  if (!held && !ended) elapsed += dt;
  var s = (BASE_S + elapsed)|0;
  if (s !== shownSec){
    shownSec = s;
    el('cd-time').textContent = fmt(s);
  }
  if (t >= nextHud){
    nextHud = t + 0.24;
    var rtt = 41 + 6*Math.sin(t*0.53) + 3*Math.sin(t*1.9+1.2);
    var jit = 3 + 1.6*Math.sin(t*0.8+2.0) + 0.8*Math.sin(t*2.6);
    el('cd-rtt').textContent = Math.max(18, Math.round(rtt));
    el('cd-jit').textContent = '±' + Math.max(1, Math.round(jit));
    var tdb = 20*Math.log10(txEnvNow*txGain + 0.001);
    var rdb = 20*Math.log10(rxEnvNow*rxGain + 0.001);
    el('ro-txdb').textContent = (tdb<=-60 ? '−∞' :
      '−' + Math.max(0, -tdb).toFixed(1));
    el('ro-rxdb').textContent = (rdb<=-60 ? '−∞' :
      '−' + Math.max(0, -rdb).toFixed(1));
  }
}

var led = el('led'), txState = el('tx-state'), callState = el('call-state');
var bMute = el('btn-mute'), bHold = el('btn-hold'), bEnd = el('btn-end');

function refreshChips(){
  bMute.classList.toggle('on', muted);
  bMute.setAttribute('aria-pressed', muted ? 'true' : 'false');
  bMute.textContent = muted ? '[·mute·]' : '[ mute ]';
  bHold.classList.toggle('on', held);
  bHold.setAttribute('aria-pressed', held ? 'true' : 'false');
  bHold.textContent = held ? '[·hold·]' : '[ hold ]';
  txState.textContent = ended ? 'TX · END' : held ? 'TX · HELD'
                       : muted ? 'TX · MUTED' : 'TX · LIVE';
  txState.className = 'ptt' + (ended||held ? ' held' : muted ? ' muted' : '');
  callState.textContent = ended ? 'ENDED' : held ? 'HOLD' : 'LIVE';
  callState.className = ended ? 'dead' : held ? 'held' : '';
  led.className = 'led' + (ended ? ' dead' : held ? ' held' : '');
  if (ended){
    bMute.disabled = true; bHold.disabled = true; bEnd.disabled = true;
    bEnd.textContent = '[·end·]';
  }
}
function setMute(v){ if (!ended){ muted = v; refreshChips(); } }
function setHold(v){ if (!ended){ held = v; refreshChips(); } }
function endCall(){
  if (ended) return;
  ended = true;
  refreshChips();
  stop();
  for (var i=0; i<24; i++) drawAll(lastT, 1/60, true);
}

bMute.addEventListener('click', function(){ setMute(!muted); });
bHold.addEventListener('click', function(){ setHold(!held); });
bEnd.addEventListener('click', endCall);
document.addEventListener('keydown', function(e){
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  var tgt = e.target;
  if (tgt && (/^(INPUT|TEXTAREA|SELECT)$/.test(tgt.tagName) || tgt.isContentEditable)) return;
  var k = e.key.toLowerCase();
  if (k === 'm') setMute(!muted);
  else if (k === 'h') setHold(!held);
  else if (k === 'e') endCall();
});

var panel = el('callPanel');
var raf = 0, running = false, inView = false, last = 0, lastT = STATIC_T;

function drawAll(t, dt, silent){
  txEnvNow = envTX(t); rxEnvNow = envRX(t);
  var k = Math.min(1, dt*7);
  txGain += (laneGain(muted ? 0.04 : 1) - txGain)*k;
  rxGain += (laneGain(1) - rxGain)*k;
  drawLane(laneTX, envTX, txGain, t);
  drawLane(laneRX, envRX, rxGain*0.82, t);
  drawDivider(t);
  quantize();
  if (!silent) tickChrome(t, dt);
}

function warmup(tEnd, n){
  for (var i=n; i>0; i--){
    var t = tEnd - i/60;
    drawAll(t, 1/60, true);
  }
  drawAll(tEnd, 1/60, true);
}

function loop(now){
  raf = 0;
  var t = now/1000, dt = t - last;
  if (dt > 0.05) dt = 0.05;
  if (dt < 0) dt = 0;
  lastT = t;
  drawAll(t, dt);
  last = t;
  if (running) raf = requestAnimationFrame(loop);
}

function start(){
  if (!running && !reduced && !ended && !document.hidden && inView){
    running = true;
    last = performance.now()/1000;
    if (!raf) raf = requestAnimationFrame(loop);
  }
}
function stop(){
  running = false;
  if (raf){ cancelAnimationFrame(raf); raf = 0; }
}

document.addEventListener('visibilitychange', function(){
  document.hidden ? stop() : start();
});
new IntersectionObserver(function(e){
  inView = e[0].isIntersecting;
  inView ? start() : stop();
}).observe(panel);

if (RM.addEventListener) RM.addEventListener('change', function(e){
  reduced = e.matches;
  if (reduced){ stop(); warmup(STATIC_T, 45); } else { start(); }
});

var rt; window.addEventListener('resize', function(){
  clearTimeout(rt); rt = setTimeout(function(){
    fit();
    if (reduced) warmup(STATIC_T, 45);
  }, 150);
});

if (reduced){
  var redraw = function(){ warmup(STATIC_T, 8); };
  bMute.addEventListener('click', redraw);
  bHold.addEventListener('click', redraw);
}

fit();
refreshChips();
if (reduced) warmup(STATIC_T, 45);
start();
})();

/* ---------- dithered event stream ---------- */
(function(){
'use strict';
var RM = matchMedia('(prefers-reduced-motion: reduce)');
var reduced = RM.matches;

var LEVELS, PAL = [
  [ 11, 12, 14],[ 20, 21, 25],[ 33, 35, 41],[ 50, 47, 38],[ 74, 55, 22],
  [106, 71, 20],[146, 96, 24],[196,127, 27],[232,152, 30],[255,178, 36],
  [255,190, 52],[255,214,110],[255,240,196]
];
LEVELS = PAL.length - 1;

var TH8 = new Float32Array([
   0,32, 8,40, 2,34,10,42, 48,16,56,24,50,18,58,26,
  12,44, 4,36,14,46, 6,38, 60,28,52,20,62,30,54,22,
   3,35,11,43, 1,33, 9,41, 51,19,59,27,49,17,57,25,
  15,47, 7,39,13,45, 5,37, 63,31,55,23,61,29,53,21]);
for (var i=0; i<64; i++) TH8[i] = (TH8[i]+0.5)/64;

function ign(x, y){
  var f = (0.06711056*x + 0.00583715*y) % 1;
  return (52.9829189*f) % 1;
}
function el(id){ return document.getElementById(id); }
function clamp01(v){ return v<0 ? 0 : (v>1 ? 1 : v); }

var LATEST0 = 1180;
var seq = LATEST0;

var SCRIPT = [
  {d: 650, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"msg:9c4f"'],['dir','"in"'],['bytes','42']]},
  {d: 380, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9c50"'],['dir','"in"'],['bytes','17']]},
  {d:1250, m:'file_state',       sev:'warn',   p:[['file_id','"file:2b11"'],['msg_id','"msg:9c4f"'],['chat_id','"chat:77aa01"'],['dir','"in"'],['state','"offered"'],['bytes_done','0'],['bytes_total','184220']]},
  {d: 900, m:'message.received', sev:'info',   p:[['chat_id','"chat:4d08be"'],['msg_id','"m:71c2"'],['dir','"out"'],['bytes','96'],['acked','true']]},
  {d:1700, m:'contact.denied',   sev:'danger', p:[['contact_id','"contact:7f44ae"'],['peer_id','"peer:umbra-9"']]},
  {d: 700, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9c55"'],['dir','"in"'],['bytes','31']]},
  {d: 600, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9c56"'],['dir','"in"'],['bytes','58'],['kind','"react"']]},
  {d:1600, gap:23},
  {d: 850, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9d01"'],['dir','"in"'],['bytes','22'],['replayed','true']]},
  {d: 480, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9d02"'],['dir','"in"'],['bytes','44'],['replayed','true']]},
  {d:2200, m:'chat.wiped',       sev:'warn',   p:[['chat_id','"chat:4d08be"'],['by','"local"']]},
  {d: 750, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9d0a"'],['dir','"out"'],['bytes','64'],['acked','true']]},
  {d:1100, m:'file_state',       sev:'warn',   p:[['file_id','"file:2b1c"'],['msg_id','"msg:e01d7c"'],['chat_id','"chat:77aa01"'],['dir','"in"'],['state','"offered"'],['bytes_done','0'],['bytes_total','92415']]},
  {d: 520, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9d10"'],['dir','"in"'],['bytes','19']]},
  {d: 420, m:'message.received', sev:'info',   p:[['chat_id','"chat:77aa01"'],['msg_id','"m:9d11"'],['dir','"in"'],['bytes','33']]},
  {d:2600, m:'contact.denied',   sev:'danger', p:[['contact_id','"contact:b02e51"'],['peer_id','"peer:fern-old"']]}
];

var rowsEl = el('evRows');
var seqRead = el('seqRead');
if (!rowsEl || !seqRead) return;
var MAX_ROWS = 64;
var staggerIdx = 0;

function paramsStr(p){
  var s = '';
  for (var i=0; i<p.length; i++)
    s += '"' + p[i][0] + '":' + p[i][1] + (i<p.length-1 ? ',' : '');
  return s;
}

function makeFrame(ev, seqN){
  var p = paramsStr(ev.p);
  var full = '{"method":"' + ev.m + '","params":{"seq":' + seqN + ',' + p + '}}';
  var r = document.createElement('div');
  r.className = 'ev-row ' + ev.sev;
  r.innerHTML =
    '<i class="ev-dot" aria-hidden="true"></i>' +
    '<code class="frame mono"><span class="arr">←</span> ' +
    '<span>{"method":"</span><span class="m">' + ev.m +
    '</span><span>","params":{"seq":</span><span class="s">' + seqN +
    '</span><span>,' + p + '}}</span></code>';
  r.querySelector('.frame').title = full;
  return r;
}

function makeGap(from, to){
  var r = document.createElement('div');
  r.className = 'ev-row gap danger';
  r.innerHTML =
    '<i class="ev-dot" aria-hidden="true"></i>' +
    '<span class="gp mono">— gap: seq ' + from + '→' + to + ' · resyncing —</span>';
  return r;
}

function appendRow(r, animate){
  if (!animate) r.classList.add('noanim');
  else if (staggerIdx > 0)
    r.style.animationDelay = (staggerIdx*90) + 'ms';
  rowsEl.appendChild(r);
  while (rowsEl.children.length > MAX_ROWS)
    rowsEl.removeChild(rowsEl.firstChild);
  rowsEl.scrollTop = rowsEl.scrollHeight;
}

function emit(ev, animate){
  if (ev.gap){
    var from = seq + 1, to = seq + ev.gap;
    seq = to;
    appendRow(makeGap(from, to), animate);
  } else {
    seq += 1;
    appendRow(makeFrame(ev, seq), animate);
  }
  seqRead.textContent = seq;
  seqTarget = ((seq - LATEST0) % SPAN) / SPAN;
}

var CELL = 4;
var SPAN = 48;
var seqTarget = 0, seqDisp = 0;

var cv = el('cv-seq'), ctx = cv.getContext('2d', {alpha:false});
var cols = 0, rows = 0, img = null;

function fitSeq(){
  var w = cv.clientWidth, h = cv.clientHeight;
  cols = Math.max(16, Math.round(w/CELL));
  rows = Math.max(8,  Math.round(h/CELL));
  cv.width = cols; cv.height = rows;
  img = ctx.createImageData(cols, rows);
}

function quantize(vals){
  var d = img.data, p = 0, o = 0;
  for (var y=0; y<rows; y++){
    var brow = (y&7)<<3;
    for (var x=0; x<cols; x++){
      var v = vals[p];
      var th = TH8[brow|(x&7)]*0.62 + ign(x,y)*0.38;
      var f = v*LEVELS, l = f|0;
      if (f-l > th) l++;
      if (l<0) l = 0; else if (l>LEVELS) l = LEVELS;
      var c = PAL[l];
      d[o]=c[0]; d[o+1]=c[1]; d[o+2]=c[2]; d[o+3]=255;
      p++; o+=4;
    }
  }
  ctx.putImageData(img, 0, 0);
}

var V = null;

function drawSeq(){
  if (!V || V.length !== cols*rows) V = new Float32Array(cols*rows);
  else V.fill(0);
  var period = Math.max(6, (rows/6)|0);
  for (var y=0; y<rows; y++){
    var rule = (y % period) === period-1;
    for (var x=0; x<cols; x++){
      var i = y*cols + x;
      V[i] = 0.010 + 0.008*ign(x*3+y, y*7+x);
      if (rule && V[i] < 0.05) V[i] = 0.05;
    }
  }
  var x0 = cols-9, x1 = cols-3;
  var top = 2, fillable = rows-4;
  var fill = seqDisp*fillable;
  var pkRow = Math.round(seqDisp*(fillable-1));
  for (var yy=0; yy<rows; yy++){
    var yb = rows-1-yy;
    for (var xx=x0; xx<=x1; xx++){
      var ii = yy*cols + xx;
      if (yb === 0){ if (V[ii] < 0.14) V[ii] = 0.14; continue; }
      if (yb > 0 && V[ii] < 0.09) V[ii] = 0.09;
      if (yb >= top && yb < top+fillable){
        var cov = fill - (yb - top);
        if (cov > 0){
          var lit = Math.min(1, cov) * (0.42 + 0.58*(yb-top)/fillable) * 1.15;
          if (lit > V[ii]) V[ii] = lit;
          if (yb - top === pkRow && seqDisp > 0.02) V[ii] = 1.2;
        }
      }
    }
  }
  quantize(V);
}

var panel = el('evtPanel');
var raf = 0, running = false, inView = false;
var last = 0, nextAt = 0, idx = 0;

function loop(now){
  raf = 0;
  var t = now/1000;
  var dt = t - last; if (dt > 0.05) dt = 0.05; if (dt < 0) dt = 0;
  last = t;
  if (t - nextAt > 2) nextAt = t + 0.4;
  staggerIdx = 0;
  while (t >= nextAt){
    emit(SCRIPT[idx], true);
    nextAt += SCRIPT[idx].d/1000;
    idx = (idx + 1) % SCRIPT.length;
    staggerIdx++;
  }
  seqDisp += (seqTarget - seqDisp) * (1 - Math.exp(-dt/0.28));
  drawSeq();
  if (running) raf = requestAnimationFrame(loop);
}

function start(){
  if (!running && !reduced && !document.hidden && inView){
    running = true;
    last = performance.now()/1000;
    if (!nextAt) nextAt = last + 0.5;
    if (!raf) raf = requestAnimationFrame(loop);
  }
}
function stop(){
  running = false;
  if (raf){ cancelAnimationFrame(raf); raf = 0; }
}

document.addEventListener('visibilitychange', function(){
  document.hidden ? stop() : start();
});
new IntersectionObserver(function(e){
  inView = e[0].isIntersecting;
  inView ? start() : stop();
}).observe(panel);

if (RM.addEventListener) RM.addEventListener('change', function(e){
  reduced = e.matches;
  if (reduced){ stop(); renderStatic(); }
  else { start(); }
});

var rt; window.addEventListener('resize', function(){
  clearTimeout(rt); rt = setTimeout(function(){
    fitSeq(); drawSeq();
    if (reduced) drawSeq();
  }, 150);
});

fitSeq();

if (reduced){
  renderStatic();
} else {
  var SEED = 5;
  for (var k=0; k<SEED; k++){
    emit(SCRIPT[idx], false);
    idx = (idx + 1) % SCRIPT.length;
  }
  seqDisp = seqTarget;
  drawSeq();
  start();
}

function renderStatic(){
  rowsEl.innerHTML = '';
  idx = 0;
  for (var k=0; k<SCRIPT.length; k++)
    emit(SCRIPT[k], false);
  seqDisp = seqTarget;
  drawSeq();
}
})();

/* ---------- diagrams v3: board + fsm ---------- */
(function(){

(() => {
  "use strict";
  const $  = (s, r = document) => [...r.querySelectorAll(s)];
  const byId = id => document.getElementById(id);
  const prm  = matchMedia('(prefers-reduced-motion: reduce)');
  const wide = matchMedia('(min-width: 44rem)');

  /* ============ ascii width verification ============
     textContent of each <pre> == rendered columns (buttons inline;
     .pk/.tr/.mbx-tick live outside <pre>). board rows must equal
     data-cols exactly; fsm/rail are ragged → max ≤ data-cols. */
  

  /* ================= board v3 ================= */
  (function board(){
    const board = byId('board'), btn = byId('viewBtn'), pbtn = byId('peerBtn');
    if (!board || !btn) return;
    const pres = $$ => [...board.querySelectorAll('pre[data-v]')];
    const PRES = pres();
    const viewName = byId('viewName'), peerName = byId('peerName');
    let pov = 't', peerOn = true, inView = false, hovered = false;

    /* ---- pov + peer toggles ---- */
    const vt = btn.querySelector('.vt'), vr = btn.querySelector('.vr');
    function setPov(v){
      pov = v; board.dataset.pov = v;
      PRES.forEach(p => p.hidden = p.dataset.v !== v);
      vt.classList.toggle('on', v==='t'); vr.classList.toggle('on', v==='r');
      if (viewName) viewName.textContent = v==='t' ? 'transport' : 'relay';
      btn.setAttribute('aria-pressed', String(v==='r'));
      gateAll();
    }
    const pon = pbtn.querySelector('.pon'), poff = pbtn.querySelector('.poff');
    function setPeer(on){
      peerOn = on; board.dataset.peer = on ? 'on' : 'off';
      pon.classList.toggle('on', on); poff.classList.toggle('on', !on);
      if (peerName) peerName.textContent = on ? 'online' : 'offline';
      pbtn.setAttribute('aria-pressed', String(!on));
      for (const el of board.querySelectorAll('.j-peer')){
        el.textContent = on ? '●' : '○';
        el.className = (on ? 'e' : 'f') + ' j-peer';
      }
      gateAll();
    }
    btn.addEventListener('click', () => setPov(pov==='t' ? 'r' : 't'));
    pbtn.addEventListener('click', () => setPeer(!peerOn));
    document.addEventListener('keydown', e => {
      const t = e.target;
      if (t && (t.tagName==='INPUT' || t.tagName==='TEXTAREA' || t.isContentEditable)) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      if (e.key === 'v') setPov(pov==='t' ? 'r' : 't');
      if (e.key === 'o') setPeer(!peerOn);
    });

    /* ---- mailbox queue ---- */
    const QUEUE_MAX = 6;               /* display saturates: 3+6 → 9/64 */
    let queue = 0;
    const tickEl = byId('mbxTick');
    function setTxt(sel, txt){ for (const el of board.querySelectorAll(sel)) el.textContent = txt; }
    function paintMbx(){
      const n = Math.min(3 + queue, 9);               /* single col, 92-safe */
      setTxt('.j-mbq', String(n));
      const df = Math.min(3 + queue, 12), mf = Math.min(3 + queue, 7);
      for (const pre of PRES){
        const desk = pre.classList.contains('m-desktop');
        const f = desk ? df : mf, cap = desk ? 12 : 7;
        const bf = pre.querySelector('.j-mbf'), be = pre.querySelector('.j-mbe');
        if (bf) bf.textContent = '▓'.repeat(f);
        if (be) be.textContent = '░'.repeat(cap - f);
      }
    }
    let tickT = null;
    function flashTick(txt, cls){
      if (!tickEl) return;
      tickEl.textContent = txt;
      tickEl.className = 'mbx-tick' + (cls ? ' ' + cls : '');
      void tickEl.offsetWidth;                         /* restart animation */
      tickEl.classList.add('on');
      clearTimeout(tickT);
      tickT = setTimeout(() => tickEl.classList.remove('on'), 950);
    }
    function enqueue(){
      if (queue >= QUEUE_MAX) { flashTick('FULL', 'full'); return; }
      queue++; paintMbx(); flashTick('+1');
    }
    function dequeue(){
      if (queue <= 0) return;
      queue--; paintMbx(); flashTick('−1', 'minus');
    }

    /* ---- packet engine: ▮ head + ▓▒░· trail from cell history ----
       geometry (desktop rows @1.5em, prod-verified):
         fwd wire row7 y=10.5em : you→relay c18-34 · relay→peer c59-74
         rev wire row8 y=12em   : peer→relay c74-60 · relay→you c34-19
         queue pipe col46 r9→14 : relay bottom ┬ → mailbox bar row14  */
    const ROW = 1.5;
    const h = (r, a, b) => { const c = []; for (let x = a; x !== b + Math.sign(b - a); x += Math.sign(b - a)) c.push({ x, r }); return c; };
    const v = (c, a, b) => { const p = []; for (let y = a; y !== b + Math.sign(b - a); y += Math.sign(b - a)) p.push({ x: c, r: y }); return p; };
    const STEP = 55, REST = 1500;

    function Pkt(ids){
      this.els = ids.map(byId);
      this.timer = null; this.i = 0; this.cells = [];
    }
    Pkt.prototype.show = function(on){
      this.els.forEach(el => el.classList.toggle('on', on));
    };
    Pkt.prototype.place = function(){
      const c = this.cells[this.i];
      const hd = this.els[0];
      hd.style.left = c.x + 'ch';
      hd.style.top  = (c.r * ROW) + 'em';
      hd.classList.toggle('hold', !!c.hold);
      /* trail: previous 4 cells, only while contiguous in same leg   */
      for (let k = 1; k <= 4; k++){
        const t = this.els[k], j = this.i - k, p = this.cells[j];
        const ok = !c.hold && p && p.leg === c.leg && !p.hold;
        t.classList.toggle('on', !!ok);
        if (ok){ t.style.left = p.x + 'ch'; t.style.top = (p.r * ROW) + 'em'; }
      }
    };
    Pkt.prototype.step = function(){
      const c = this.cells[this.i];
      if (!c) { this.finish(); return; }
      if (c.fork) for (const el of board.querySelectorAll('.j-fork')) el.classList.add('lit');
      else        for (const el of board.querySelectorAll('.j-fork')) el.classList.remove('lit');
      this.place();
      if (c.hold){
        if (c.spark) this.spark();
        this.timer = setTimeout(() => { this.i++; this.step(); }, c.hold);
        return;
      }
      if (c.act) c.act();
      this.i++;
      this.timer = setTimeout(() => this.step(), STEP);
    };
    Pkt.prototype.spark = function(){
      /* relay-view fork: shed a faint · down the queue pipe while held */
      const s = byId('fkS');
      if (!s || pov !== 'r') return;
      let r = 9;
      s.classList.add('on');
      const drop = () => {
        if (r > 12 || !s.classList.contains('on')) { s.classList.remove('on'); return; }
        s.style.left = '46ch'; s.style.top = (r * ROW) + 'em'; r++;
        setTimeout(drop, 130);
      };
      drop();
      setTimeout(() => s.classList.remove('on'), 700);
    };
    Pkt.prototype.run = function(cells, done){
      this.cells = cells; this.i = 0; this.onDone = done;
      this.show(true); this.step();
    };
    Pkt.prototype.finish = function(){
      this.show(false); this.running = false;
      for (const el of board.querySelectorAll('.j-fork')) el.classList.remove('lit');
      if (this.onDone) this.onDone();
    };
    Pkt.prototype.stop = function(){
      clearTimeout(this.timer); this.show(false); this.running = false;
      for (const el of board.querySelectorAll('.j-fork')) el.classList.remove('lit');
    };

    const pkF = new Pkt(['pkH','pkT1','pkT2','pkT3','pkT4']);
    const pkR = new Pkt(['rkH','rkT1','rkT2','rkT3','rkT4']);
    const pkD = new Pkt(['dkH','dkT1','dkT2','dkT3','dkT4']);

    /* paths — leg ids keep the trail from smearing across teleports */
    function leg(cells, id){ cells.forEach(c => c.leg = id); return cells; }
    function fwdCells(){
      const hold = { x:34, r:7, hold: (pov==='r' ? 850 : 450), fork:true, spark:true, leg:'h' };
      const a = leg(h(7, 18, 34), 'a');
      if (peerOn) return [...a, hold, ...leg(h(7, 59, 74), 'b')];
      const drop = leg(v(46, 9, 14), 'q');
      drop[drop.length - 1].act = enqueue;
      return [...a, hold, ...drop];
    }
    function revCells(){
      const hold = { x:60, r:8, hold: (pov==='r' ? 700 : 380), leg:'h' };
      return [...leg(h(8, 74, 60), 'a'), hold, ...leg(h(8, 34, 19), 'b')];
    }
    function drainCells(){
      const a = leg(v(46, 14, 9), 'r');
      a[0].act = dequeue;
      return [...a, { x:46, r:9, hold: 300, leg:'h' }, ...leg(h(7, 59, 74), 'b')];
    }

    /* cycle drivers — fwd loops forever while gated on */
    let fwdT = null, revT = null, drainT = null;
    function fwdCycle(){
      if (pkF.running) return;
      pkF.running = true;
      pkF.run(fwdCells(), () => { fwdT = setTimeout(fwdCycle, REST); });
    }
    function revCycle(){
      if (pkR.running) return;
      if (!peerOn){ revT = setTimeout(revCycle, 900); return; }  /* no replies while offline */
      pkR.running = true;
      pkR.run(revCells(), () => { revT = setTimeout(revCycle, REST + 700); });
    }
    function drainCheck(){
      if (peerOn && queue > 0 && !pkD.running){
        pkD.running = true;
        pkD.run(drainCells(), () => { drainT = setTimeout(drainCheck, 350); });
      } else drainT = setTimeout(drainCheck, 1200);
    }

    /* ---- instrument readouts (demo) ---- */
    const S = { rtt:24, q:3, rx:1.0, tx:0.4, up:41 };
    const fmtUp = s => { const m = Math.floor(s/60), ss = s%60;
      return (m<10?'0':'')+m+':'+(ss<10?'0':'')+ss; };
    const clamp = (v,a,b) => Math.min(b, Math.max(a,v));
    function tickReadouts(){
      S.rtt = clamp(Math.round(S.rtt + (Math.random()-.5)*5), 17, 39);
      S.q   = clamp(S.q + (Math.random()<.3 ? (Math.random()<.5?-1:1) : 0), 2, 6);
      const floor = peerOn ? .6 : .0, cap = peerOn ? 1.5 : .05;
      S.rx  = clamp(S.rx + (Math.random()-.5)*.3, floor, cap);
      S.tx  = clamp(S.tx + (Math.random()-.5)*.2, peerOn?.2:.0, peerOn?.7:.05);
      S.up += 1;
      setTxt('.j-rtt', String(S.rtt)); setTxt('.j-q', String(S.q));
      setTxt('.j-rx', S.rx.toFixed(1)); setTxt('.j-tx', S.tx.toFixed(1));
      setTxt('.j-upt', fmtUp(S.up));
    }
    let ledOn = true;
    function tickLed(){
      ledOn = !ledOn;
      for (const el of board.querySelectorAll('.j-led'))
        el.className = (ledOn ? 'e' : 'd') + ' j-led';
    }

    /* ---- master gate ---- */
    let roT = null, ledT = null;
    const animated = () => inView && !document.hidden && !prm.matches;
    function gateAll(){
      const run = animated();
      const pkRun = run && wide.matches && !hovered;
      if (run){
        if (!roT)  roT  = setInterval(tickReadouts, 1400);
        if (!ledT) ledT = setInterval(tickLed, 900);
      } else {
        clearInterval(roT);  roT  = null;
        clearInterval(ledT); ledT = null;
      }
      if (pkRun){
        fwdCycle(); revCycle();
        if (!drainT) drainT = setTimeout(drainCheck, 800);
      } else {
        pkF.stop(); pkR.stop(); pkD.stop();
        clearTimeout(fwdT); fwdT = null;
        clearTimeout(revT); revT = null;
        clearTimeout(drainT); drainT = null;
      }
    }

    new IntersectionObserver(es => { inView = es[0].isIntersecting; gateAll(); },
      { threshold: .15 }).observe(board);
    board.addEventListener('mouseenter', () => { hovered = true;  gateAll(); });
    board.addEventListener('mouseleave', () => { hovered = false; gateAll(); });
    document.addEventListener('visibilitychange', gateAll);
    (prm.addEventListener || prm.addListener).call(prm, 'change', gateAll);
    (wide.addEventListener || wide.addListener).call(wide, 'change', gateAll);
    paintMbx(); setPeer(true); gateAll();
  })();

  /* ================= fsm v3 ================= */
  (function fsm(){
    const fwrap = byId('fwrap');
    if (!fwrap) return;
    const logBody = byId('log-body');
    let tickN = 0;
    const appendLog = (cls, tag, html) => {
      if (!logBody) return;
      const div = document.createElement('div');
      div.className = 'll ' + cls;
      div.innerHTML = `<span class="t">${tag}</span>${html}`;
      logBody.appendChild(div);
      while (logBody.children.length > 24) logBody.firstChild.remove();   /* capped register */
      logBody.scrollTop = logBody.scrollHeight;
    };
    const stepLog = (a, ar, b, n) =>
      appendLog('step', 't+' + String(++tickN).padStart(2, '0'),
        `${a} <span class="a">${ar}</span> ${b} <span class="x">· ${n}</span>`);

    const EDGES = {
      invite:['link','deny'],  handshake:['noise','deny'],
      sealed:['rekey','sas','keychg','deny'], unverified:['sasback','deny'],
      verified:['wipe','deny'], wiped:['deny'], denied:[]
    };
    const OUT = {                                    /* arrow, to, via */
      link:['──▶','handshake','cinder:// link'],
      noise:['══▶','sealed','noise_xx · 3 flights'],
      rekey:['──▶','sealed','rekey · self-loop'],
      sas:['──▶','verified','/verify match'],
      keychg:['──▶','unverified','key change · sends blocked'],
      sasback:['──▶','verified','/verify match'],
      wipe:['──▶','wiped','chat.wipe'],
      deny:['──▶','denied','contact.deny · terminal']
    };

    /* ---- edge trace paths: [row,col] cells on the 128-col canvas ----
       measured against prod art: button row r4; labels r3; drop tees
       r5; deny bus r15; denied box r18-20. deny is per-state: own drop
       col → bus → junction c30 → down.                               */
    const hr = (r, a, b) => { const c = []; for (let x=a;x<=b;x++) c.push([r,x]); return c; };
    const vr = (c, a, b) => { const p = []; for (let y=a;y<=b;y++) p.push([y,c]); return p; };
    const TRACE = {
      link:    hr(4, 20, 34),
      noise:   hr(4, 54, 74),
      sas:     hr(4, 94, 108),
      keychg:  vr(88, 5, 9),
      wipe:    vr(120, 5, 9),
      rekey:   [[3,80],[2,80],[1,80], ...hr(1, 81, 91), [2,91],[3,91]],
      sasback: [[11,98],[11,99],[11,100],[10,100],[9,100],[8,100],
                ...hr(7, 100, 115), [6,115],[5,115]]
    };
    const DENY_DROP = { invite:[12,6], handshake:[44,6], sealed:[78,6],
      unverified:[86,14], verified:[126,6], wiped:[120,13] };
    function denyPath(state){
      const d = DENY_DROP[state]; if (!d) return [];
      const [col, r0] = d, cells = vr(col, r0, 15);         /* own drop → bus */
      const dir = col < 30 ? 1 : -1;
      for (let c = col + dir; c !== 30 + dir; c += dir) cells.push([15, c]);
      cells.push([16,30],[17,30]);                          /* → DENIED */
      return cells;
    }

    /* ---- tracer pool: one · per lit edge, loops its path ---- */
    const tracers = [];
    function mkTr(){
      const el = document.createElement('span');
      el.className = 'tr'; el.textContent = '·'; el.setAttribute('aria-hidden','true');
      fwrap.appendChild(el); return el;
    }
    function stopTraces(){
      for (const t of tracers){ clearInterval(t.iv); clearTimeout(t.st); t.el.remove(); }
      tracers.length = 0;
    }
    function startTraces(state){
      stopTraces();
      if (prm.matches) return;
      (EDGES[state] || []).forEach((e, idx) => {
        const cells = e === 'deny' ? denyPath(state) : TRACE[e];
        if (!cells || !cells.length) return;
        const el = mkTr();
        if (e === 'deny') el.classList.add('den');
        el.classList.add('on');
        let i = 0;
        const place = () => {
          const [r, c] = cells[i % cells.length];
          el.style.top = (r * 1.55) + 'em'; el.style.left = c + 'ch';
        };
        tracers.push({ el,
          st: setTimeout(() => {
            place();
            tracers.find(t => t.el === el).iv =
              setInterval(() => { i++; place(); }, 58);
          }, idx * 170),                                       /* stagger edges */
          iv: 0 });
      });
    }

    /* ---- readout: typed 'state → event → state' + sr truth ---- */
    const roState = byId('ro-state'), roN = byId('ro-n'),
          roAnim  = byId('ro-anim'),  roSr = byId('ro-sr');
    let typeIv = null;
    function phrase(s){
      const es = EDGES[s] || [];
      if (!es.length) return `${s} ── terminal · no outgoing edges`;
      return es.map(e => { const [,to,via] = OUT[e];
        return `${s} → ${via} → ${to}`; }).join('  │  ');
    }
    function typeReadout(str){
      clearInterval(typeIv);
      roSr.textContent = str;                    /* aria-live truth, instant */
      if (prm.matches){ roAnim.textContent = str; return; }
      let i = 0;
      typeIv = setInterval(() => {
        i++;
        roAnim.innerHTML = str.slice(0, i).replace(/</g,'&lt;') +
          (i < str.length ? '<span class="cur">▌</span>' : '');
        if (i >= str.length){ clearInterval(typeIv); typeIv = null; }
      }, 16);
      roAnim.textContent = '';
    }
    function renderReadout(){
      if (!sel){
        roState.textContent = '· · ·'; roN.textContent = 'edges';
        typeReadout('click a state · enter/space inspects · esc resets');
      } else {
        const n = (EDGES[sel] || []).length;
        roState.textContent = sel; roN.textContent = `edges ${n}`;
        typeReadout(phrase(sel));
      }
    }

    /* ---- selection (prod behavior + traces) ---- */
    let sel = null;
    const applySel = () => {
      for (const el of $('[data-edge].hot')) el.classList.remove('hot');
      for (const el of $('[data-state].sel')) el.classList.remove('sel');
      if (!sel) return;
      for (const el of $(`[data-state="${sel}"]`)) el.classList.add('sel');
      for (const e of EDGES[sel] || [])
        for (const el of $(`[data-edge="${e}"]`)) el.classList.add('hot');
    };
    const select = s => {
      sel = (sel === s) ? null : s;
      applySel();
      for (const b of $('.st'))
        b.setAttribute('aria-pressed', String(b.dataset.state === sel));
      renderReadout();
      if (sel){
        startTraces(sel);
        appendLog('trc', 'trc',
          `inspect <b>${sel}</b> <span class="x">· ${EDGES[sel].length} out-edges · tracing</span>`);
      } else stopTraces();
    };
    const reset = () => {
      if (!sel) return;
      sel = null; stopTraces(); applySel();
      for (const b of $('.st')) b.setAttribute('aria-pressed', 'false');
      renderReadout();
    };
    for (const pre of $('pre.fsm, pre.rail'))
      pre.addEventListener('click', e => {
        const t = e.target.closest('[data-state]');
        t ? select(t.dataset.state) : reset();
      });
    document.addEventListener('keydown', e => { if (e.key === 'Escape' && !/^(INPUT|TEXTAREA)$/.test(e.target.tagName)) reset(); });

    for (const li of $('.spec li[data-edge]')) {
      li.addEventListener('mouseenter', () => {
        for (const el of $(`[data-edge="${li.dataset.edge}"]`)) el.classList.add('hot');
      });
      li.addEventListener('mouseleave', applySel);
    }

    /* ---- marker stepping (prod led cycle) ---- */
    const leds = $('.led[data-i]');
    const PATH = [
      ['invite','──▶','handshake','cinder:// link'],
      ['handshake','══▶','sealed','noise_xx · 3 flights'],
      ['sealed','──▶','verified','/verify match']
    ];
    let cur = 0, prev = -1, timer = null, visible = false;
    const paint = () => {
      for (const l of leds){
        const on = +l.dataset.i === cur;
        l.classList.toggle('on', on);
        l.textContent = on ? '●' : '○';
      }
    };
    const step = () => {
      if (prev >= 0){
        if (cur === 0 && prev === 3)
          appendLog('rst','···','<span class="x">cycle reset · re-enter invite</span>');
        else { const [a,ar,b,n] = PATH[prev]; stepLog(a, ar, b, n); }
      }
      paint(); prev = cur; cur = (cur + 1) % 4;
    };
    const park = () => { cur = 3; prev = -1; paint(); };
    const start = () => {
      if (timer || prm.matches || !visible || document.hidden) return;
      timer = setInterval(step, 1100);
    };
    const stop = () => { clearInterval(timer); timer = null; };

    if ('IntersectionObserver' in window){
      const io = new IntersectionObserver(es => {
        visible = es.some(x => x.isIntersecting);
        visible ? start() : stop();
      }, { threshold: .1 });
      for (const id of ['fsm','rail']){ const el = byId(id); if (el) io.observe(el); }
    } else visible = true;

    document.addEventListener('visibilitychange',
      () => document.hidden ? stop() : start());
    const onPrm = () => {
      if (prm.matches){
        stop(); stopTraces(); park(); renderReadout();
        appendLog('prm','prm','<span class="x">marker parked · prefers-reduced-motion</span>');
      } else { start(); if (sel) startTraces(sel); renderReadout(); }
    };
    prm.addEventListener ? prm.addEventListener('change', onPrm)
                       : prm.addListener(onPrm);

    renderReadout();
    if (prm.matches){
      park();
      appendLog('prm','prm','<span class="x">marker parked · prefers-reduced-motion</span>');
    } else { step(); start(); }
  })();
})();

})();

/* ---------- mega footer v3 ---------- */

(() => {
  "use strict";
  const footer = document.querySelector(".fm2");
  const mq = window.matchMedia("(prefers-reduced-motion: reduce)");

  /* ---- uptime counters (fake boot offset, labelled demo) ---- */
  const upEls = ["upt", "upt2"].map(id => document.getElementById(id)).filter(Boolean);
  const upBase = 4*3600 + 17*60 + 33;
  const t0 = Date.now();
  const p2 = n => String(n).padStart(2, "0");
  function tickUptime(){
    const s = upBase + Math.floor((Date.now() - t0) / 1000);
    const txt = p2(Math.floor(s/3600)) + ":" + p2(Math.floor(s/60)%60) + ":" + p2(s%60);
    upEls.forEach(el => { el.textContent = txt; });
  }

  /* ---- starfield: two pre layers, rows rotated left ---- */
  const host = document.getElementById("sf");
  const farEl = document.getElementById("sfFar");
  const nearEl = document.getElementById("sfNear");
  const ROWS = 5;
  function makeRows(cols, density, chars){
    const rows = [];
    for(let r = 0; r < ROWS; r++){
      let s = "";
      for(let c = 0; c < cols; c++){
        s += Math.random() < density
          ? chars[Math.floor(Math.random() * chars.length)] : " ";
      }
      rows.push(s);
    }
    return rows;
  }
  let far = [], near = [], farAcc = 0, nearAcc = 0;
  function buildStars(){
    const fs = parseFloat(getComputedStyle(farEl).fontSize);
    const cols = Math.ceil(host.clientWidth / (fs * 0.6)) + 30;
    far  = makeRows(cols, 0.06,  "······.+");
    near = makeRows(cols, 0.016, "*+");
    farEl.textContent  = far.join("\n");
    nearEl.textContent = near.join("\n");
  }
  const rot = rows => rows.map(s => s.slice(1) + s[0]);
  function drift(){
    farAcc += 0.16; nearAcc += 0.55;
    let dirty = false;
    while(farAcc >= 1){ far = rot(far); farAcc -= 1; dirty = true; }
    if(dirty){ farEl.textContent = far.join("\n"); dirty = false; }
    while(nearAcc >= 1){ near = rot(near); nearAcc -= 1; dirty = true; }
    if(dirty) nearEl.textContent = near.join("\n");
  }

  /* ---- signal sweep: clone the wordmark into .fm2-echo, then drive
     --sx (-0.35 → 1.35) over ~2.3s every ~8s. The gradient + bayer
     mask on .fm2-echo does the rest; base glyphs never reflow. ---- */
  const mark = footer.querySelector(".fm2-mark");
  const word = footer.querySelector(".fm2-word");
  const echo = document.getElementById("echo");
  if(echo && word) echo.appendChild(word.cloneNode(true));
  let sxRaf = 0, sweepTimer = 0;
  const SWEEP_MS = 2300, SX_MIN = -0.35, SX_SPAN = 1.70;
  function runSweep(){
    const t0 = performance.now();
    function step(t){
      const k = Math.min(1, (t - t0) / SWEEP_MS);
      mark.style.setProperty("--sx", (SX_MIN + k * SX_SPAN).toFixed(4));
      if(k < 1 && live) sxRaf = requestAnimationFrame(step);
    }
    sxRaf = requestAnimationFrame(step);
  }
  function scheduleSweep(d){
    sweepTimer = setTimeout(() => { runSweep(); scheduleSweep(); },
      d !== undefined ? d : 5600 + Math.random() * 1400);
  }

  /* ---- meteors: coin-flip direction, random top, ~15s cadence ---- */
  const metL = document.getElementById("metL");
  const metR = document.getElementById("metR");
  let metTimer = 0;
  function spawnMeteor(){
    const el = Math.random() < 0.6 ? metL : metR;
    el.style.top = (4 + Math.random() * 42) + "%";
    el.classList.remove("go");
    void el.offsetWidth;            /* restart the css animation */
    el.classList.add("go");
  }
  [metL, metR].forEach(el => el &&
    el.addEventListener("animationend", () => el.classList.remove("go")));
  function scheduleMeteor(d){
    metTimer = setTimeout(() => { spawnMeteor(); scheduleMeteor(); },
      d !== undefined ? d : 13000 + Math.random() * 5000);
  }

  /* ---- gate: intersecting && !hidden && motion-ok → .fm-live + timers ---- */
  let inView = false, live = false, timers = [];
  function start(){
    if(live) return;
    live = true;
    footer.classList.add("fm-live");
    timers.push(setInterval(drift, 140), setInterval(tickUptime, 1000));
    tickUptime();
    scheduleSweep(1400);      /* first sweep early so it's discoverable */
    scheduleMeteor(5200);
  }
  function stop(){
    if(!live) return;
    live = false;
    footer.classList.remove("fm-live");
    timers.forEach(clearInterval);
    timers = [];
    clearTimeout(sweepTimer); clearTimeout(metTimer);
    cancelAnimationFrame(sxRaf);
    mark.style.setProperty("--sx", SX_MIN);
    metL.classList.remove("go"); metR.classList.remove("go");
  }
  function gate(){
    (inView && !document.hidden && !mq.matches) ? start() : stop();
  }
  if("IntersectionObserver" in window){
    new IntersectionObserver(es => {
      inView = es[0].isIntersecting; gate();
    }, {threshold: 0.02}).observe(footer);
  } else { inView = true; }
  document.addEventListener("visibilitychange", gate);
  if(mq.addEventListener) mq.addEventListener("change", gate);
  else mq.addListener(gate);

  buildStars();
  tickUptime();
  let rsz;
  window.addEventListener("resize", () => {
    clearTimeout(rsz);
    rsz = setTimeout(buildStars, 200);
  });
})();


/* ---------- spec ledger v3 — sync-highlight panes ---------- */
/* ================================================================
   spec-panes v3 — IIFE, ES5 to match prod main.js
   ---- PORT ----
   Replaces the "spec ledger v2" IIFE in main.js.
   API surface identical: .head button toggles .open on li.row,
   aria-expanded/aria-controls preserved, exclusive-open (opening a
   row closes siblings + resets #allBtn), #allBtn expand/collapse all.
   Deltas vs prod:
     - setRow() writes aria-hidden on .body instead of body.hidden;
       init clears the markup `hidden` attr once (CSS grid-rows owns
       the collapse so it can transition).
     - sync pairing block: pointerover/pointerleave + focusin/focusout
       per .pair. pointer classes .lit/.on/.src; focus classes
       .litf/.onf/.srcf (kept separate so pointer+focus don't clear
       each other's state).
     - .pcap .v[data-flash] chips flash .ln[data-sync~="key"] in the
       row's .side.agent for ~1.5s (.on+.zap).
   ================================================================ */
(function(){
  "use strict";
  var root=document.querySelector(".ledger").parentElement;
  if(!root)return;
  var rows=[].slice.call(root.querySelectorAll(".row"));
  var allBtn=root.querySelector("#allBtn");

  function setRow(row,open){
    var head=row.querySelector(".head");
    var body=row.querySelector(".body");
    row.classList.toggle("open",open);
    head.setAttribute("aria-expanded",open?"true":"false");
    body.setAttribute("aria-hidden",open?"false":"true");
  }

  rows.forEach(function(row){
    /* markup ships `hidden` as a no-JS fallback; CSS collapse owns it now */
    row.querySelector(".body").hidden=false;
    row.querySelector(".head").addEventListener("click",function(){
      var opening=!row.classList.contains("open");
      if(opening){
        rows.forEach(function(r){if(r!==row)setRow(r,false);});
        if(allBtn){
          allBtn.setAttribute("aria-pressed","false");
          allBtn.textContent="[+] all";
        }
      }
      setRow(row,opening);
    });
  });

  if(allBtn)allBtn.addEventListener("click",function(){
    var open=allBtn.getAttribute("aria-pressed")!=="true";
    allBtn.setAttribute("aria-pressed",open?"true":"false");
    allBtn.setAttribute("aria-label",
      (open?"collapse":"expand")+" all ledger entries");
    allBtn.textContent=(open?"[-]":"[+]")+" all";
    rows.forEach(function(r){setRow(r,open);});
  });

  /* ---- sync pairing ----
     data-sync="k1 k2" pairs every .ln sharing a key across the two
     .side panes of the same .pair. Hover/focus of a paired line:
     - sibling pane gets .lit/.litf (reveal: all its paired lines
       show a faint ▶)
     - matching .ln in the sibling get .on/.onf (ember ▶ + band)
     - source line gets .src/.srcf */
  function clearLn(pair,names){
    names.forEach(function(n){
      [].slice.call(pair.querySelectorAll(".ln."+n))
        .forEach(function(el){el.classList.remove(n);});
    });
  }
  function markTargets(pair,ln,cls){
    var sides=[].slice.call(pair.querySelectorAll(".side"));
    var side=ln.closest(".side");
    if(!side||sides.length<2)return;
    var other=sides[0]===side?sides[1]:sides[0];
    (ln.getAttribute("data-sync")||"").split(/\s+/)
      .forEach(function(k){
        if(!k)return;
        [].slice.call(other.querySelectorAll('.ln[data-sync~="'+k+'"]'))
          .forEach(function(el){el.classList.add(cls);});
      });
  }

  [].slice.call(root.querySelectorAll(".pair")).forEach(function(pair){
    var sides=[].slice.call(pair.querySelectorAll(".side"));
    if(sides.length<2)return;
    var a=sides[0],b=sides[1];
    function otherOf(side){return side===a?b:a;}
    function litOff(){a.classList.remove("lit");b.classList.remove("lit");}
    function litfOff(){a.classList.remove("litf");b.classList.remove("litf");}

    pair.addEventListener("pointerover",function(e){
      var t=e.target;
      var side=t&&t.closest?t.closest(".side"):null;
      litOff();
      if(!side||!pair.contains(side))return;
      otherOf(side).classList.add("lit");
      clearLn(pair,["on","src"]);
      var ln=t.closest?t.closest(".ln[data-sync]"):null;
      if(ln&&side.contains(ln)){
        ln.classList.add("src");
        markTargets(pair,ln,"on");
      }
    });
    pair.addEventListener("pointerleave",function(){
      litOff();
      clearLn(pair,["on","src"]);
    });

    pair.addEventListener("focusin",function(e){
      var t=e.target;
      var side=t&&t.closest?t.closest(".side"):null;
      litfOff();
      if(!side)return;
      otherOf(side).classList.add("litf");
      clearLn(pair,["onf","srcf"]);
      var ln=t.closest?t.closest(".ln[data-sync]"):null;
      if(ln){
        ln.classList.add("srcf");
        markTargets(pair,ln,"onf");
      }
    });
    pair.addEventListener("focusout",function(){
      setTimeout(function(){
        if(!pair.contains(document.activeElement)){
          litfOff();
          clearLn(pair,["onf","srcf"]);
        }
      },0);
    });
  });

  /* ---- pcap chips: flash paired agent-pane lines ---- */
  [].slice.call(root.querySelectorAll(".pcap .v[data-flash]"))
    .forEach(function(chip){
      chip.addEventListener("click",function(){
        var body=chip.closest(".body");
        var agent=body?body.querySelector(".side.agent"):null;
        if(!agent)return;
        var hits=[];
        (chip.getAttribute("data-flash")||"").split(/\s+/)
          .forEach(function(k){
            if(!k)return;
            hits=hits.concat([].slice.call(
              agent.querySelectorAll('.ln[data-sync~="'+k+'"]')));
          });
        hits.forEach(function(el){
          el.classList.remove("zap");
          void el.offsetWidth;   /* restart the animation on re-click */
          el.classList.add("on","zap");
        });
        clearTimeout(agent._zt);
        agent._zt=setTimeout(function(){
          hits.forEach(function(el){el.classList.remove("on","zap");});
        },1500);
      });
    });
})();

/* ---------- spec arch v2 — schematic col badges ---------- */

(function(){
  "use strict";
  document.querySelectorAll("pre.schematic").forEach(function(pre){
    var lines=pre.textContent.replace(/\s+$/,"").split("\n"),max=0;
    lines.forEach(function(l){
      var n=Array.from(l).length;
      if(n>max)max=n;
    });
    var tag=pre.closest(".arch").querySelector(".cols");
    if(tag){
      tag.textContent=max+" cols · ";
      tag.classList.toggle("bad",max>54);
    }
  });
})();


/* ---------- dvf — block-fill dividers ---------- */

/* ---- PORT ---------------------------------------------------------------
 * Mount: drop `<div class="dvf [dvf--label|dvf--tick]" data-label="nn"
 *        data-tick="fig·nn" role="separator"></div>` inside .wrap between a
 *        .sec-head and the next section (replaces .dither-div / bare space).
 * Lift:  this whole IIFE + the marked .dvf css block. No exports; it finds
 *        and drives every .dvf node present at DOMContentLoaded (call the
 *        internal build path again for late-inserted nodes if ever needed).
 * Notes: cells are real text spans (selectable/printable); each cell is
 *        pinned to the measured advance width of its settle glyph so the
 *        rule stays edge-to-edge even if a glyph falls back to another font.
 *        prefers-reduced-motion renders the finished rule immediately and
 *        disables the observer, loops and the ember seed.
 * ------------------------------------------------------------------------ */
(function(){
'use strict';

var MQ = window.matchMedia('(prefers-reduced-motion: reduce)');
var D_FILL   = 600;    /* total sweep, ms                                */
var SEED_MS  = 12000;  /* ember flash cadence                            */
var LOOP_GAP = 1500;   /* pause between looped demo fills                */
var MAXC     = 420;    /* cell-count safety cap                          */
var GLYPH  = { blocks:'▁▂▃▄▅▆▇█', thin:'·░─' };
var RAMPMS = { blocks:190, thin:130 };  /* per-cell glyph ramp window     */

var items = [];

function rm(){ return MQ.matches; }
function slice(nl){ return Array.prototype.slice.call(nl); }
function kindOf(el){
  return el.classList.contains('dvf--label') ? 'label'
       : el.classList.contains('dvf--tick')  ? 'tick' : 'ramp';
}
function el(tag, cls, txt, hidden){
  var s = document.createElement('span');
  s.className = cls;
  if (hidden) s.setAttribute('aria-hidden','true');
  if (txt != null) s.textContent = txt;
  return s;
}
/* measure the advance width of one settle glyph in the live font */
function probe(host, ch){
  var s = el('span','dvf-probe', new Array(21).join(ch), true);
  host.appendChild(s);
  var w = s.offsetWidth / 20;
  host.removeChild(s);
  return w > 0 ? w : 8;
}
function makeRule(n){
  var r = el('span','dvf-rule', null, true);
  var h = '';
  for (var i=0;i<n;i++) h += '<span class="dvf-c"> </span>';
  r.innerHTML = h;
  return r;
}

function build(it){
  var e = it.el, k = it.kind;
  e.innerHTML = '';                              /* drops .dvf-fb fallback */
  var gset  = (k === 'tick') ? 'thin' : 'blocks';
  it.seq    = GLYPH[gset].split('');             /* transient … final      */
  it.settle = it.seq[it.seq.length-1];
  var cw = probe(e, it.settle), W = e.clientWidth;
  it.cells = []; it.steps = []; it.x = [];

  if (k === 'label'){
    var tag = el('span','dvf-x dvf-tag', null, true);
    var i1 = el('i', null, '['), b = el('b', null, e.getAttribute('data-label') || '00'), i2 = el('i', null, ']');
    tag.appendChild(i1); tag.appendChild(document.createTextNode(' '));
    tag.appendChild(b);  tag.appendChild(document.createTextNode(' '));
    tag.appendChild(i2);
    e.appendChild(tag);
    var n = Math.max(3, Math.min(MAXC,
              Math.floor((W - tag.offsetWidth) / (cw * 2))));
    var r1 = makeRule(n), r2 = makeRule(n);
    e.insertBefore(r1, tag); e.appendChild(r2);
    it.cells = slice(r1.children).concat(slice(r2.children));
    it.steps = slice(r1.children).concat([tag], slice(r2.children));
    it.x = [tag];
  } else if (k === 'tick'){
    var b1  = el('span','dvf-x dvf-br','[', true);
    var fig = el('span','dvf-x dvf-fig', e.getAttribute('data-tick') || 'fig·00', true);
    var b2  = el('span','dvf-x dvf-br',']', true);
    e.appendChild(b1); e.appendChild(fig); e.appendChild(b2);
    var fixed = b1.offsetWidth + fig.offsetWidth + b2.offsetWidth;
    var nt = Math.max(6, Math.min(MAXC, Math.floor((W - fixed) / cw)));
    var rt = makeRule(nt);
    e.insertBefore(rt, fig);
    it.cells = slice(rt.children);
    it.steps = [b1].concat(it.cells, [fig, b2]);
    it.x = [b1, fig, b2];
  } else {
    var nr = Math.max(4, Math.min(MAXC, Math.floor(W / cw)));
    var rr = makeRule(nr);
    e.appendChild(rr);
    it.cells = slice(rr.children);
    it.steps = it.cells.slice();
  }
  it.sig = k + ':' + it.cells.length;
  it.cw = cw; it._w = W;
  it.cells.forEach(function(c){ c.style.width = cw + 'px'; c._k = -1; });
  var w = e.closest('.slot') && e.closest('.slot').querySelector('[data-w]');
  if (w) w.textContent = Math.round(W) + 'px · ' + it.cells.length + ' cells';
}

function arm(it){                                 /* blank, ready to fill  */
  it.gen++; it.busy = false;                      /* abort in-flight rAF   */
  it.armed = true; it.done = false;
  it.cells.forEach(function(c){
    c.textContent = ' '; c._k = -1;
    c.classList.remove('is-fresh','is-hot');
  });
  it.x.forEach(function(x){
    x._on = false;
    x.style.opacity = '0';
    x.style.transform = 'scaleY(.4)';
  });
  it.el.classList.remove('is-done');
}
function settle(it){                              /* finished rule         */
  it.armed = false; it.done = true; it.busy = false; it.gen++;
  it.cells.forEach(function(c){ c.textContent = it.settle; c._k = it.seq.length-1; });
  it.x.forEach(function(x){ x.style.opacity = '1'; x.style.transform = 'scaleY(1)'; });
  it.el.classList.add('is-done');
}

function fill(it, cb){
  if (it.busy || it.done) return;
  it.busy = true; it.armed = false;
  var g = ++it.gen;
  var steps = it.steps, n = steps.length, seq = it.seq, L = seq.length;
  var ramp = RAMPMS[it.kind === 'tick' ? 'thin' : 'blocks'];
  var stepms = ramp / (L - 1);
  var S = n > 1 ? (D_FILL - ramp) / (n - 1) : 0;
  var t0 = performance.now();
  function fr(now){
    if (g !== it.gen) return;                     /* aborted by rebuild    */
    var t = now - t0, all = true;
    for (var i = 0; i < n; i++){
      var st = steps[i], p = t - i * S;
      if (p < 0){ all = false; continue; }
      if (st.classList.contains('dvf-x')){
        if (!st._on){
          st._on = true;
          st.style.opacity = '1';
          st.style.transform = 'scaleY(1)';
        }
        continue;
      }
      var k = Math.min(L - 1, Math.floor(p / stepms));
      if (k !== st._k){
        st._k = k;
        st.textContent = seq[k];
        if (k === L - 1){
          st.classList.add('is-fresh');
          (function(c){ setTimeout(function(){ c.classList.remove('is-fresh'); }, 340); })(st);
        }
      }
      if (k < L - 1) all = false;
    }
    if (!all){ requestAnimationFrame(fr); return; }
    it.busy = false; it.done = true;
    it.el.classList.add('is-done');
    if (cb) cb();
  }
  requestAnimationFrame(fr);
}

var io = new IntersectionObserver(function(es){
  es.forEach(function(en){
    var it = en.target._dvf; if (!it) return;
    it.vis = en.isIntersecting;
    if (rm() || it.loop || it.stat) return;
    if (en.isIntersecting){ if (!it.done && !it.busy) fill(it); }
    else if (it.done || it.busy){ arm(it); }      /* left view → re-arm    */
  });
}, { threshold: .6 });

function loop(it){                                /* matrix "filling" col  */
  if (it._looping) return;
  it._looping = true;
  (function go(){
    fill(it, function(){
      setTimeout(function(){
        if (rm()){ settle(it); it._looping = false; return; }
        arm(it);
        setTimeout(go, 80);
      }, LOOP_GAP);
    });
  })();
}

function rebuild(it){
  var g = ++it.gen; it.busy = false;
  build(it);
  if (g !== it.gen) return;
  if (rm() || it.stat){ settle(it); }
  else if (it.done){ settle(it); }
  else {
    arm(it);
    if (it.loop){ it._looping = false; loop(it); }
    else if (it.vis) fill(it);
  }
}

var ro = ('ResizeObserver' in window) ? new ResizeObserver(function(es){
  es.forEach(function(en){
    var it = en.target._dvf; if (!it) return;
    /* rebuild only when the width moved by ~one cell or more */
    var W = en.target.clientWidth;
    if (it._w != null && Math.abs(W - it._w) < (it.cw || 8) * .9) return;
    rebuild(it);
  });
}) : null;

function seed(){
  if (rm()) return;
  var pool = items.filter(function(it){
    return it.vis && it.done && !it.busy && it.cells.length;
  });
  if (!pool.length) return;
  var it = pool[Math.floor(Math.random() * pool.length)];
  var c  = it.cells[Math.floor(Math.random() * it.cells.length)];
  if (c._k !== it.seq.length - 1) return;
  c.classList.add('is-hot');
  setTimeout(function(){ c.classList.remove('is-hot'); }, 560);
}

function init(){
  slice(document.querySelectorAll('.dvf')).forEach(function(e){
    var it = {
      el: e, kind: kindOf(e), gen: 0,
      loop: e.hasAttribute('data-dvf-loop'),
      stat: e.hasAttribute('data-dvf-static'),
      armed: false, done: false, busy: false, vis: false
    };
    e._dvf = it;
    build(it);
    items.push(it);
    if (ro) ro.observe(e);
    if (rm() || it.stat){
      settle(it);
    } else {
      arm(it);
      io.observe(e);
      if (it.loop) loop(it);
    }
  });
  if (document.fonts && document.fonts.ready){
    document.fonts.ready.then(function(){ items.forEach(rebuild); });
  }
}

MQ.addEventListener ? MQ.addEventListener('change', onMQ)
                    : MQ.addListener(onMQ);
function onMQ(){
  items.forEach(function(it){
    it.gen++; it.busy = false;
    if (rm()){ it._looping = false; settle(it); }
    else if (it.stat){ settle(it); }
    else { io.observe(it.el); arm(it); if (it.loop) loop(it); else if (it.vis) fill(it); }
  });
  meter();
}

function meter(){
  var m = document.getElementById('motionState');
  if (m) m.textContent = rm() ? 'reduced — static rules' : 'full — sweep enabled';
}

setInterval(seed, SEED_MS);
meter();
if (document.readyState === 'loading'){
  document.addEventListener('DOMContentLoaded', init);
} else { init(); }
})();


/* ---- nav scrollspy ---- */
(function(){
  const links=[...document.querySelectorAll('.nav-links a[href^="#"]')];
  if(!links.length||!('IntersectionObserver' in window))return;
  const map=new Map(links.map(a=>[a.getAttribute('href').slice(1),a]));
  const io=new IntersectionObserver(es=>{
    es.forEach(e=>{if(e.isIntersecting){
      links.forEach(a=>a.classList.remove('on'));
      const a=map.get(e.target.id);a&&a.classList.add('on');
    }});
  },{rootMargin:'-30% 0px -55% 0px'});
  map.forEach((a,id)=>{const s=document.getElementById(id);s&&io.observe(s);});
})();
