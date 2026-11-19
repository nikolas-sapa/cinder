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

/* ---- terminal demo: self-typing prompt ---- */
const typedEl = document.getElementById("typed");
if (typedEl && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
  const cmds = ["/finger", "/sas verify", "/msg wheels up in ten?"];
  let ci = 0;
  const typeIn = (s, i, done) =>
    i <= s.length
      ? (typedEl.textContent = s.slice(0, i), setTimeout(() => typeIn(s, i + 1, done), 55 + Math.random() * 80))
      : setTimeout(done, 2600);
  const erase = (done) =>
    typedEl.textContent.length
      ? (typedEl.textContent = typedEl.textContent.slice(0, -1), setTimeout(() => erase(done), 28))
      : setTimeout(done, 500);
  (function loop() {
    typeIn(cmds[ci], 0, () => erase(() => { ci = (ci + 1) % cmds.length; loop(); }));
  })();
} else if (typedEl) {
  typedEl.textContent = "/finger";
}
