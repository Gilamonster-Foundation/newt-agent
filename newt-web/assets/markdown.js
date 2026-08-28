/* Progressive enhancement for canonical Markdown transcript fragments.
 *
 * newt's wire/storage contract stays Markdown. This adapter adds browser-only
 * affordances after the server has rendered and sanitized that Markdown.
 * HTMX swaps and transcript SSE replacements both call the same public hook.
 */
(function () {
  "use strict";

  var configured = false;
  var selector =
    '[data-markdown-extension="mermaid"]:not([data-processed]):not([data-markdown-processing])';

  function candidates(root) {
    var nodes = [];
    if (root && root.matches && root.matches(selector)) nodes.push(root);
    if (root && root.querySelectorAll) {
      nodes.push.apply(nodes, root.querySelectorAll(selector));
    }
    return nodes;
  }

  /* Can an injected <style> element actually apply on this page?
   *
   * Mermaid themes its diagrams through a <style> element it injects, scoped
   * to a per-render id (`#mermaid-1787956159624{...}`), and it has no CSP
   * nonce support. Under a strict `style-src-elem` that element is blocked —
   * and the result is not an unstyled diagram, it is an UNREADABLE one:
   * measured on the real page, both node fill and text fill fall back to
   * black, so every diagram renders black-on-black. That is worse than not
   * rendering, and it fails silently, because an acceptance test asserts a
   * diagram is present and not that it can be read.
   *
   * Because the stylesheet is per-render, there is no hash and no static file
   * that could admit it. So the page states what its own policy permits and
   * we fall back to the diagram SOURCE when it does not (ADR law 5): a
   * readable ```mermaid block beats a black square.
   *
   * Read from the server rather than probed, because a probe would have to
   * inject a <style> to find out — tripping the very violation it tests for.
   */
  function diagramsMayRender() {
    var body = document.body;
    return !body || body.getAttribute("data-newt-diagrams") !== "source-only";
  }

  /* Present a diagram as its own source, labelled. */
  function fallBackToSource(node, why) {
    node.classList.add("mermaid-error");
    node.setAttribute("aria-label", why);
    node.setAttribute("data-processed", "true");
  }

  async function enhanceMarkdown(root) {
    // Capability FIRST: when diagrams are source-only the runtime is not even
    // served, so a `!window.mermaid` early return would skip the fallback and
    // leave the source unlabelled.
    if (!diagramsMayRender()) {
      // Diagrams cannot be themed here, so they cannot be read. Leave every
      // one as the source the server already escaped.
      var blocked = candidates(root || document);
      for (var b = 0; b < blocked.length; b += 1) {
        fallBackToSource(
          blocked[b],
          "Diagram shown as source: the page's security policy blocks the " +
            "diagram renderer's styles"
        );
      }
      return;
    }
    if (!window.mermaid) return;
    if (!configured) {
      window.mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
      });
      configured = true;
    }

    var nodes = candidates(root || document);
    for (var i = 0; i < nodes.length; i += 1) {
      var node = nodes[i];
      var source = node.textContent;
      // Mermaid itself owns `data-processed`. Marking it before `run()` makes
      // Mermaid correctly treat the node as already rendered and skip it.
      node.setAttribute("data-markdown-processing", "true");
      try {
        await window.mermaid.parse(source);
        await window.mermaid.run({ nodes: [node] });
      } catch (_error) {
        // Mermaid may replace invalid input with an error SVG before rejecting.
        // Restore the canonical source so a bad diagram never hides the reply.
        node.textContent = source;
        fallBackToSource(node, "Mermaid diagram could not be rendered");
      } finally {
        node.removeAttribute("data-markdown-processing");
      }
    }
  }

  window.newtEnhanceMarkdown = enhanceMarkdown;
  document.addEventListener("DOMContentLoaded", function () {
    enhanceMarkdown(document);
  });
  document.addEventListener("htmx:afterSwap", function (event) {
    enhanceMarkdown(event.detail.target);
  });
})();
