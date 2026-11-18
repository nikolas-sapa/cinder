# cinder — landing site

Static site, no build step. `index.html` + `style.css` + `main.js` + `assets/`.

## Deploy

Serve the directory as-is on any static host — the repo root is `site/`:

- **Vercel/Netlify/Cloudflare Pages**: set the project root to `site/`; no build command, output dir `site/` itself.
- **nginx/caddy**: point the docroot at this directory.
- **Local check**: `python3 -m http.server -d site 8000` → http://localhost:8000

## Notes

- `demo.gif` is a placeholder — the terminal demo section renders a styled
  static TUI mock until `demo.gif` is dropped into this directory (the `<img>`
  overlays it and removes itself on 404, so a missing gif degrades cleanly).
- External requests: Geist Sans/Mono woff2 via fontsource on jsDelivr, and
  GitHub links. Nothing else.
- GitHub URLs currently point at `github.com/nikolas-sapa/cinder` — update if
  the repo moves.
