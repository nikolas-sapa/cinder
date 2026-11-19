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

/* ---- ember field: live 4x4 ordered-dither shader (canvas at cell res, upscaled) ---- */
const cv = document.getElementById("emberCanvas");
if (cv) {
  const ctx = cv.getContext("2d");
  const BAYER = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
  ];
  const STAR_ROWS = 9;
  const rows = 42;
  let cols = 0;
  const stars = [];
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;

  for (let i = 0; i < 26; i++)
    stars.push([Math.random(), Math.random(), 0.5 + Math.random() * 2.2, Math.random() * 6.28]);

  function resize() {
    cols = Math.max(80, Math.floor(cv.clientWidth / 4));
    cv.width = cols;
    cv.height = rows;
  }

  function frame(ms) {
    const t = ms / 1000;
    const im = ctx.createImageData(cols, rows);
    const d = im.data;
    for (let y = 0; y < rows; y++) {
      const inSky = y < STAR_ROWS;
      const ramp = inSky ? 0 : (y - STAR_ROWS) / (rows - STAR_ROWS);
      for (let x = 0; x < cols; x++) {
        let v = 0.04;
        if (!inSky) {
          v = ramp * 0.92
            + 0.14 * Math.sin(x * 0.045 + t * 1.4 + y * 0.12)
            + 0.10 * Math.sin(x * 0.11 - t * 0.8 + y * 0.05)
            + 0.06 * Math.sin(x * 0.21 + t * 2.1 - y * 0.09);
        }
        const th = (BAYER[y & 3][x & 3] + 0.5) / 16;
        if (v > th) {
          const i = (y * cols + x) * 4;
          const heat = inSky ? 0.7 : 0.35 + ramp * 0.65;
          d[i] = 255;
          d[i + 1] = Math.round(178 * heat + 30);
          d[i + 2] = Math.round(36 * heat);
          d[i + 3] = 255;
        }
      }
    }
    for (const [sx, sy, sp, ph] of stars) {
      if (0.6 + 0.4 * Math.sin(t * sp + ph) > 0.6) {
        const x = (sx * cols) | 0;
        const y = (sy * STAR_ROWS) | 0;
        const i = (y * cols + x) * 4;
        d[i] = 255; d[i + 1] = 200; d[i + 2] = 70; d[i + 3] = 255;
      }
    }
    ctx.putImageData(im, 0, 0);
    if (!reduce) requestAnimationFrame(frame);
  }

  addEventListener("resize", () => { resize(); if (reduce) frame(performance.now()); });
  resize();
  requestAnimationFrame(frame);
}

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
