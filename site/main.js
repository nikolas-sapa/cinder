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
