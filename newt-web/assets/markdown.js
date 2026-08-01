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

  async function enhanceMarkdown(root) {
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
        node.classList.add("mermaid-error");
        node.setAttribute("aria-label", "Mermaid diagram could not be rendered");
        node.setAttribute("data-processed", "true");
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
