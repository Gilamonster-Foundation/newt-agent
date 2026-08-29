# Vendored browser assets

newt-web serves browser dependencies locally; production rendering must not
depend on a CDN.

- `htmx.min.js` — the existing vendored HTMX runtime.
- `panel.js` — live-transcript attachment and prompt-box reset.

The vendored Mermaid runtime and its Markdown adapter were **deleted in E0b
(#1869)**. They could not draw under the strict CSP C3b shipped — no
`cspNonce`, a theme stylesheet scoped to a per-render id that no hash can
admit, and a blocked theme rendering black-on-black — so diagrams are now
rendered server-side by `newt_core::markup::extension::flowchart`, in SVG
presentation attributes that no `style-src*` directive governs.
