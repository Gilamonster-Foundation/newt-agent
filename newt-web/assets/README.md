# Vendored browser assets

newt-web serves browser dependencies locally; production rendering must not
depend on a CDN.

- `htmx.min.js` — the existing vendored HTMX runtime.
- `mermaid.min.js` — Mermaid 11.15.0, downloaded from the pinned npm/jsDelivr
  distribution path
  `https://cdn.jsdelivr.net/npm/mermaid@11.15.0/dist/mermaid.min.js`.
  SHA-256: `70137e77bb273bb2ef972b86e8b0400cca8be53cb25bfc45911a186dc98665de`.
  Upstream license: `mermaid.LICENSE`.
- `markdown.js` — newt-web's source-controlled progressive-enhancement adapter.
