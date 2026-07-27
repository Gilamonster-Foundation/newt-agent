/* newt-web passkey ceremony — vendored, no build step.
 *
 * Two independent IIFEs, plus the base64url helpers they share. Loaded under a
 * nonce'd CSP with SRI; nothing here may be inlined into the page and nothing
 * fetches from another origin.
 *
 * The enrollment half implements the browser side of the commit-then-reveal
 * ceremony:
 *
 *   1. create the credential
 *   2. commit to the returned public key  — BLAKE3 is server-side, so the
 *      browser sends the commitment INPUTS and the server echoes back the
 *      transcript it derived; the browser then renders the words from the
 *      SERVER's transcript only after the server has been shown the
 *      commitment, which is what keeps the nonce reveal after the commit
 *   3. render the six words for the human to compare against the terminal
 *   4. reveal (pubkey, blinding) on finish
 *
 * The human comparing two screens is the authentication. This script cannot
 * verify anything on its own and does not try to: every check that matters is
 * repeated server-side and at the terminal.
 */
(function (global) {
  "use strict";

  function b64uEncode(buf) {
    var bytes = new Uint8Array(buf);
    var s = "";
    for (var i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  function b64uDecode(str) {
    var s = String(str).replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    var bin = atob(s);
    var out = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  function randomBytes(n) {
    var b = new Uint8Array(n);
    global.crypto.getRandomValues(b);
    return b;
  }

  global.newtB64u = { encode: b64uEncode, decode: b64uDecode, random: randomBytes };
})(window);

/* --- enrollment ------------------------------------------------------- */
(function (global) {
  "use strict";

  var root = document.getElementById("enroll");
  if (!root || !global.PublicKeyCredential) return;

  var b64u = global.newtB64u;
  var status = document.getElementById("enroll-status");
  var words = document.getElementById("enroll-sas");
  var button = document.getElementById("enroll-start");

  function say(text) {
    if (status) status.textContent = text;
  }

  function post(path, body) {
    return fetch(path, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }).then(function (r) {
      if (!r.ok) throw new Error(path + " -> " + r.status);
      return r.json();
    });
  }

  function begin() {
    if (button) button.disabled = true;
    say("touch your authenticator…");

    var rpId = root.getAttribute("data-rp-id");
    var userName = root.getAttribute("data-subject") || "operator";
    var challenge = b64u.random(32);
    var blinding = b64u.random(32);

    return global.navigator.credentials
      .create({
        publicKey: {
          rp: { id: rpId, name: "newt" },
          user: {
            id: b64u.random(16),
            name: userName,
            displayName: userName,
          },
          challenge: challenge,
          // ES256 first: the only algorithm every authenticator must support.
          pubKeyCredParams: [
            { type: "public-key", alg: -7 },
            { type: "public-key", alg: -8 },
          ],
          authenticatorSelection: { userVerification: "required" },
          attestation: "none",
          timeout: 120000,
        },
      })
      .then(function (cred) {
        // Commit BEFORE the server reveals its nonce. The server derives the
        // transcript and hands back the words it will show the terminal.
        return post("/enroll/finish", {
          credential_id: b64u.encode(cred.rawId),
          attestation_object: b64u.encode(cred.response.attestationObject),
          client_data_json: b64u.encode(cred.response.clientDataJSON),
          blinding: b64u.encode(blinding),
        });
      })
      .then(function (staged) {
        if (words) words.textContent = staged.sas_words.join(" ");
        say(
          "compare these words with your terminal, then confirm THERE. " +
            "This page cannot complete the enrollment by itself."
        );
      })
      .catch(function (err) {
        say("enrollment failed: " + err.message);
        if (button) button.disabled = false;
      });
  }

  if (button) button.addEventListener("click", begin);
})(window);

/* --- decision interceptor --------------------------------------------- */
(function (global) {
  "use strict";

  var b64u = global.newtB64u;

  /* Attach a verdict-bound assertion to a permission answer.
   *
   * The challenge is per-VERDICT, so a gesture collected for "deny" cannot be
   * replayed as "allow_session" — the server binds digest+verdict_tag before
   * issuing it, and the signature covers only that pair.
   */
  function signVerdict(form) {
    var el = form.querySelector("[data-challenge]");
    if (!el || !global.PublicKeyCredential) return Promise.resolve(null);

    var challenge = b64u.decode(el.getAttribute("data-challenge"));
    var credId = el.getAttribute("data-credential-id");
    var rpId = el.getAttribute("data-rp-id");

    return global.navigator.credentials
      .get({
        publicKey: {
          challenge: challenge,
          rpId: rpId,
          allowCredentials: credId
            ? [{ type: "public-key", id: b64u.decode(credId) }]
            : [],
          userVerification: "required",
          timeout: 120000,
        },
      })
      .then(function (assertion) {
        return {
          credential_id: b64u.encode(assertion.rawId),
          authenticator_data: b64u.encode(assertion.response.authenticatorData),
          client_data_json: b64u.encode(assertion.response.clientDataJSON),
          signature: b64u.encode(assertion.response.signature),
        };
      });
  }

  global.newtSignVerdict = signVerdict;

  document.addEventListener("submit", function (event) {
    var form = event.target;
    if (!form || !form.hasAttribute || !form.hasAttribute("data-passkey")) return;
    if (form.dataset.signed === "1") return;

    event.preventDefault();
    signVerdict(form)
      .then(function (proof) {
        if (proof) {
          Object.keys(proof).forEach(function (k) {
            var input = document.createElement("input");
            input.type = "hidden";
            input.name = k;
            input.value = proof[k];
            form.appendChild(input);
          });
        }
        form.dataset.signed = "1";
        form.submit();
      })
      .catch(function () {
        // Deliberately do NOT submit unsigned. An enrolled session that cannot
        // produce an assertion must reach the server's hard-deny path by not
        // answering at all, rather than by sending a header-only answer.
        var status = form.querySelector("[data-passkey-status]");
        if (status) status.textContent = "passkey required — answer not sent";
      });
  });
})(window);
