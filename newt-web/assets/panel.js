/* Live transcript attachment — the fix that makes a CSP possible (#1854).
 *
 * This script exists because of a CSP constraint, not a JS one. `agent_panel`
 * used to embed its own `<script>` with the agent id baked in, and that panel
 * is served as an HTMX *fragment* and swapped into a page whose CSP header
 * came from an earlier response. A fragment's inline script would therefore
 * have to carry the nonce of a page it has never seen — a nonce outliving its
 * response and shared across many, which `csp.rs` rightly refuses to allow.
 *
 * Moving the behaviour here removes the question entirely: the panel carries
 * DATA (`data-agent-stream`), this file carries the behaviour, and no fragment
 * needs a nonce. `no_fragment_carries_inline_script` holds that line.
 *
 * Progressive enhancement, not a requirement: with scripting off the panel
 * still renders its server-side transcript, and every control is a real form.
 * What is lost is only the live update.
 */
(function () {
  "use strict";

  var ATTR = "data-agent-stream";
  var DONE = "data-stream-attached";
  var selector = "[" + ATTR + "]:not([" + DONE + "])";

  function attach(node) {
    var url = node.getAttribute(ATTR);
    if (!url) return;
    // Mark before opening: a second scan (DOMContentLoaded racing an HTMX
    // swap) must not open a second EventSource onto the same node.
    node.setAttribute(DONE, "true");

    var source = new EventSource(url);
    source.onmessage = function (event) {
      // The panel is replaced wholesale on a tab switch. When this node
      // leaves the document its stream is finished — closing here is what
      // the old inline hook did by looking the element up and finding it
      // gone, and it is why switching tabs does not leak connections.
      if (!node.isConnected) {
        source.close();
        return;
      }
      node.innerHTML = event.data;
      if (window.newtEnhanceMarkdown) {
        window.newtEnhanceMarkdown(node);
      }
      node.scrollTop = node.scrollHeight;
    };
    source.onerror = function () {
      if (!node.isConnected) source.close();
    };
  }

  function scan(root) {
    if (!root) return;
    if (root.matches && root.matches(selector)) attach(root);
    if (root.querySelectorAll) {
      var found = root.querySelectorAll(selector);
      for (var i = 0; i < found.length; i += 1) attach(found[i]);
    }
  }

  // Clear the prompt box after a successful send.
  //
  // This was `hx-on::after-request="this.reset()"` on the form. htmx EVALUATES
  // that attribute's value as JavaScript, which needs `script-src
  // 'unsafe-eval'` — inline script wearing an attribute, and exactly what the
  // policy exists to stop. Measured: it was the single `script-src :: eval`
  // violation on the page. A listener here needs no eval and no nonce.
  document.addEventListener("htmx:afterRequest", function (event) {
    var form = event.target;
    if (form && form.hasAttribute && form.hasAttribute("data-reset-on-send")) {
      var ok = event.detail && event.detail.successful;
      if (ok) form.reset();
    }
  });

  window.newtAttachStreams = scan;
  document.addEventListener("DOMContentLoaded", function () {
    scan(document);
  });
  document.addEventListener("htmx:afterSwap", function (event) {
    scan(event.detail.target);
  });
})();
