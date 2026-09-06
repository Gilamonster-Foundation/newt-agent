# Decision: parse untrusted input with a real parser (AST), not a regex

**Status:** Accepted (decided by Shawn Hartsock, 2026-06-21)
<!-- INERT-CODE-RATCHET: S04 DOCUMENT: three documents describe nonexistent forge resolver code as shipped or current. -->
**Date:** 2026-06-21
**Related:** `docs/decisions/agentic_object_capability_security.md` (the OCAP
leash this protects), `docs/decisions/ocap_confinement_model.md` (the honest
confinement model this parsing serves), `docs/decisions/mcp_transport_security.md`,
`docs/security/ocap-deviations.md`, and the first application —
`newt-tui/src/forge_context.rs` + `newt-core/src/forge_resolvers.rs` (the
harness forge-URL resolver, which parses links with the `url` crate and matches
hosts/paths structurally).

---

## TL;DR

When newt parses **untrusted or security-relevant input** — URLs, filesystem
paths, command lines, capability/caveat tokens, config that crosses an authority
boundary — it uses a **structural parser** (a real grammar that yields typed,
normalized components you can inspect), **not** a regular expression. Regex is
reserved for trusted, cosmetic, display/UX matching where the worst failure is a
wrong-looking line of text.

## Context — why this matters more for newt than for most tools

newt is an **object-capability (OCAP)** system. Authority is granted by holding
unforgeable, attenuated capabilities; the leash (agent-bridle), the workspace
fence, the confined shell, and the dispatch caveats all decide *what the agent
may do* by **parsing and validating input at a boundary**. In that design **every
parse is an authority boundary** — and a parser that is wrong, lax, or
exploitable is an authority bypass, i.e. a confused deputy.

Regular expressions are the wrong tool at those boundaries:

- **No grammar, no structure.** A regex that "matches a URL" does not *parse* a
  URL. It can't reliably tell you the host, because the URL grammar (userinfo
  `@`, ports, IPv6 literals, backslashes, percent-encoding, dot-segments) is not
  regular. `https://github.com@evil.tld/...`, `https://github.com\.evil.tld`,
  `https://github.com%2F..%2F...` all defeat naive host regexes. A structural
  parser (`url::Url`) extracts a *normalized* host you can compare exactly.
- **Parser differentials.** Security breaks when two components disagree about
  what a string means: the regex that authorizes vs. the library that acts. The
  classic SSRF / open-redirect / path-traversal bugs are differentials — the
  validator's regex and the HTTP client (or the filesystem) parse the same bytes
  differently. One parser, used for both the check and the action, removes the gap.
- **ReDoS.** A user- or agent-supplied pattern (or a careless built-in) can
  backtrack catastrophically and hang the process — a denial-of-service handed
  to exactly the party we are trying to confine.
- **Validate-by-rejection is fragile; parse-by-construction is robust.** "Reject
  anything that looks bad" (deny-list regex) loses to the next encoding trick.
  "Parse into typed parts, then permit only well-formed values" (allow-list on a
  real AST) fails closed.

## Decision

1. **Untrusted / authority-bearing input is parsed structurally.** Reach for a
   real parser and validate the *typed result*:
   - **URLs** → the `url` crate; treat `host` as an exact-match allow-list;
     restrict path captures to a known-safe charset; HTTPS-only; never follow
     redirects across hosts.
   - **Filesystem paths** → canonicalize + component checks against the fence,
     never a regex over the string.
   - **Command lines / argv** → structured construction (argv vectors, not shell
     string interpolation); the confined shell parses, we don't regex.
   - **Capability / caveat tokens, signed envelopes, config that grants
     authority** → typed deserialization + validation, not pattern scraping.

2. **Regex stays in its lane.** It is fine for **trusted, cosmetic** matching
   where a miss is harmless: help-text rollups, log/line scraping, display
   formatting, internal tokenization of data we produced. The test is: *if this
   match is wrong, can it widen authority or crash the process?* If yes, it is
   not a regex job.

3. **Reviewers enforce it.** A regex applied to untrusted input that feeds a
   security decision is a review blocker, the same way an unwitnessed capability
   amplification is. Prefer the parser; if a regex is genuinely unavoidable at a
   boundary, it must be justified in the PR and bounded against ReDoS.

## First application

The harness forge-URL resolver (issue/PR/MR auto-fetch) was built this way from
the start, and is the reference example:

- `url::Url` parses each candidate link; a resolver fires **only** when the
  parsed host exactly matches its declared `host` (`github.com`, `gitlab.com`, …).
- The path is matched by a **structural segment template**
  (`/{project}/issues/{number}`), not a regex; `{number}` admits only digits and
  `{project}` segments are restricted to `[A-Za-z0-9._-]`, so no host- or
  traversal-shaped value can reach the command/HTTP templates.
- The out-of-band fetch is HTTPS-only with redirects disabled, and tokens
  (`token_env`/`token_file`) are read by the harness and **never** injected into
  the model's context.

This replaced an earlier regex-per-resolver draft specifically because the config
is user-editable and the fetch crosses a network boundary — precisely where a
regex differential would have been an SSRF foothold.

## Consequences

- New code that parses untrusted input should start from a parser crate, not a
  pattern. Expect a few more lines than a one-liner regex; that is the cost of a
  real boundary.
- Existing regex on security-relevant paths is tech debt to migrate as it is
  touched (tracked case-by-case, not a big-bang sweep).
- This doc is the standing answer to "can I just regex this?" for anything that
  touches authority. If the input is untrusted and the failure mode is an
  authority bypass, the answer is no — parse it.
