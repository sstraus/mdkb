// Pause SMIL diagrams while they are off screen, and keep them still for
// visitors who ask for reduced motion.
const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
const diagrams = document.querySelectorAll(".fig svg");

if (reduced) {
  diagrams.forEach((svg) => svg.pauseAnimations());
} else {
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) {
      if (e.isIntersecting) e.target.unpauseAnimations();
      else e.target.pauseAnimations();
    }
  });
  diagrams.forEach((svg) => io.observe(svg));
}

document.querySelectorAll("[data-copy]").forEach((btn) => {
  btn.addEventListener("click", async () => {
    const text = document.getElementById(btn.dataset.copy)?.textContent ?? "";
    try {
      await navigator.clipboard.writeText(text);
      btn.textContent = "Copied";
    } catch {
      btn.textContent = "Select";
    }
    setTimeout(() => (btn.textContent = "Copy"), 1600);
  });
});
