//! The HTMX shell — server-rendered, zero JS toolchain (HTMX vendored in W2
//! when the first swap arrives; the W1 shell is static HTML).

use axum::response::Html;

/// The cockpit shell: the tab strip and content region the later rungs swap
/// into. W1 renders the empty state — no agents, no tabs.
pub(crate) async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>newt-web</title>
<style>
  :root { color-scheme: light dark; }
  body { font-family: ui-monospace, monospace; margin: 0; }
  header { padding: 0.5rem 1rem; border-bottom: 1px solid color-mix(in srgb, currentColor 25%, transparent); }
  header h1 { font-size: 1rem; margin: 0; }
  #tabs { display: flex; gap: 0.5rem; padding: 0.5rem 1rem; }
  #content { padding: 1rem; }
  .empty { opacity: 0.7; }
</style>
</head>
<body>
<header><h1>newt-web</h1></header>
<nav id="tabs"></nav>
<main id="content">
  <p class="empty">No agents yet. Tabs appear here as agents spawn or announce.</p>
</main>
</body>
</html>
"#,
    )
}
