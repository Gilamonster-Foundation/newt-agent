# Session network console

Status: proposed; the monitor and rule editor described here are not implemented.
The operator selected a native signed Network Extension for macOS, and requires
feature parity across native Linux, macOS, and Windows implementations.

Amended 2026-11-14 with the operator's [decisions](#operator-decisions-2026-11-14)
and a code [inventory](#inventory-2026-11-14).

## Operator contract

Newt needs a network console combining IPTraf-ng's live traffic views with a
browser-style site-permission editor. It covers the harness, inference clients,
MCP connections, shell/build descendants, and delegated workers. It is a general
session facility, not a special lab configuration.

**Access, observation, and filesystem confinement are independent.** Allowing
all networking changes the network decision; it does not turn off monitoring,
auditing, token accounting, or the filesystem fence. An observation is never a
grant. The model's familiar tools continue working through the harness's broker;
the model does not learn a permission-negotiation protocol.

The operator can:

- Review a destination and the requesting activity before DNS resolution or an
  outbound connection starts, then allow once, allow for the session, deny for
  the session, or save an explicit permanent allow/deny rule.
- Add, edit, disable, remove, and inspect saved domain/origin/URI rules. Edits
  show their exact scope and a match preview before applying.
- Monitor connections, attempts, transfer rates, totals, denials, and failures
  while work continues; filter by session, child, destination, or purpose.
- See where inference requests went and how many input/output tokens each
  endpoint and model reported, including retries and auxiliary inference.
- Revoke a grant and stop affected traffic. Previously transmitted bytes cannot
  be recalled; stopping an existing connection has a separate recorded outcome.

IPTraf-ng is the interaction reference for live connection and byte/packet
tables, sorting, filtering, and detail views. Its packet-display filters are not
authorization. Newt's display filter must likewise never change access policy.
Source: [IPTraf-ng README and manual](https://github.com/iptraf-ng/iptraf-ng).

## Operator decisions (2026-11-14)

1. **Selecting an inference backend is the grant for its endpoint.** There is
   no prompt and no new gate; the traffic is still recorded like any other
   flow. This extends to every inference path the position the crew loop
   already documents (`newt-scheduler/src/crew.rs`). The grant covers backends
   the operator configured or selected, auxiliary ones included; a model
   instruction cannot add one. An explicit deny and an inherited ceiling still
   win. A redirect to another host is not covered. The sovereign-only pin
   ([#2927](https://github.com/Gilamonster-Foundation/newt-agent/issues/2927))
   stays a separate policy.
2. **Observation ships first.** Step 2 below lands before any new grant or
   boundary, and adds no prompt and no allow or deny rule. Work starts after
   the v0.8.6 release.
3. **The Linux child boundary is undecided.** A spike compares two
   network-namespace shapes: the routed fence of
   [agent-bridle#276](https://github.com/Gilamonster-Foundation/agent-bridle/issues/276),
   and an empty namespace whose only way out is a Unix socket to the session
   proxy (see the inventory).

## One console, four views

Proposed entry: `/net`, extending the read-only traffic panel of
[#2657](https://github.com/Gilamonster-Foundation/newt-agent/issues/2657),
which takes its records from
[agent-bridle#413](https://github.com/Gilamonster-Foundation/agent-bridle/issues/413)
and renders with newtui#41. No model tool is added. Activity stays usable while
a turn is running or inference is disconnected. The existing panel host does not
provide that: it parks the cockpit presenter and runs on the session thread
(`newt-tui/src/panel.rs`, `session_worker.rs`). The nearest mechanism is the F4
inspector, where the presenter thread reads a bounded shared archive
(`newt-tui/src/cockpit/presenter.rs`, `newt-tui/src/completed_spill.rs`). F4
shows one snapshot; a live table is new work.

```text
Network    [Activity] [Requests] [Rules] [Inference]
Access: Ask for new destinations    Audit: recording
Coverage: brokered HTTP + child connections    Filesystem: confined

Owner       Purpose     Destination           State       Sent   Received
main        inference   api.example.test:443   streaming   84 KiB 12 KiB
build       dependency  crates.example:443    awaiting    0 B    0 B
tool:fetch  web         docs.example:443      denied      0 B    0 B

Enter details   / filter   s sort   Tab view   Esc back
```

This is illustrative data, not a capture. Activity displays both requested DNS
name and actual peer address when known, transport, start/last activity, rates,
byte totals, owner, decision source, and matched rule. Packet counters appear
only for a packet-capable source; HTTP byte counts are not labelled packet
counts. Closed flows remain in session history. Escape closes the view without
changing policy or stopping observation.

Requests shows destination, scheme/port, path where inspectable, requesting
tool/process, purpose, and the scope of each decision. Repeated requests for the
same destination/purpose share one pending decision. Denial and timeout return a
clear tool result; neither silently permits nor leaves an unbounded queue.
An approval for an inference endpoint the operator did not select, such as a
redirect target, uses local controls, never an inference request to the provider
whose connection is awaiting approval.

Rules uses the screenshot's editable site list: pattern, allow/deny, session or
permanent, purpose, provenance, last match, and enable/remove controls. A global
"Allow all networking" selection is a visible session policy with audit still
recording. Specific explicit denies continue to apply; switching modes does not
erase them. Global allow is not equivalent to Newt's `FullAccess` preset.

Inference groups by the destination captured at send time, backend, model,
session/worker, and purpose (primary, summarizer, embedding, classifier). It
shows attempts, successes/failures/cancellations, reported input/output/cache/
reasoning tokens where available, and optional price-derived cost. Missing
usage is **unknown**, not zero. Estimates stay separate from provider-reported
usage. Retries are distinct attempts; streamed chunks do not create extra
requests. A backend rename or endpoint change cannot rewrite earlier history.
The destination is the endpoint Newt actually contacted, including approved
redirects. Routing hidden behind a provider gateway stays unknown unless the
provider supplies attributable metadata; a configured model name is not proof
of which server or model ultimately handled the request.

## Rule meaning

Use explicit structured patterns, not arbitrary regexes or shell globs:

| Operator input | Meaning |
| --- | --- |
| `example.com` | Exact DNS name, any port/protocol permitted by the remaining policy |
| `[*.]example.com` | Apex plus descendants at DNS label boundaries |
| `https://example.com:443` | Exact scheme, DNS name, and port |
| `https://[*.]example.com:443` | Same origin constraints, apex plus subdomains |
| `https://example.com/api/*` | HTTP(S) requests under `/api/`, including descendants |
| `https://example.com/api` | Exact URL path; does not include `/apiary` or `/api/child` |

The editor explains omitted scheme/port and whether an entry is host-scoped or
URL-scoped. Domain normalization uses the existing URL/IDNA implementation,
case folding, and a consistent terminal-dot rule. Label matching must reject
`badexample.com` and `example.com.attacker.test` for `[*.]example.com`. Reject
ambiguous/unsupported patterns instead of falling back to host-wide permission.
Credentials, query strings, and fragments are not grant syntax. Restrictive
path rules must reject ambiguous encoded separators/traversal unless the broker
can guarantee identical interpretation at dispatch. Every redirect is a new
decision at its actual destination; an approved origin is not an open redirect
capability. TLS certificate verification and existing SSRF restrictions remain.
Do not turn reverse DNS, a cached DNS answer, or an IP shared by several domains
into domain authority. An unattributed IP-only flow stays IP-only and needs its
own explicit decision. Domain fronting, mismatched HTTP Host/TLS SNI, DNS
rebinding, and hostname metadata supplied by untrusted code need adversarial
tests at the broker. Host-only opaque TLS access must be labelled as such; it
does not prove which encrypted HTTP virtual host or URL was used.

Decision order: inherited authority ceiling and explicit deny, then applicable
session/permanent allow, then the operator's default (ask, deny, or allow all).
Deny wins overlapping rules. An operator removes/disables an overlapping deny
to permit its target; implicit specificity must not make policy surprising.
An URL-only grant cannot authorize an opaque tunnel to its whole host.

Session decisions expire with that session; a new process or restored
conversation must not silently resurrect transient approval. Saving permanently
is a separate deliberate operation through the existing signed grant store.
Repository config, fetched text, and child processes cannot write operator
policy. A delegated worker cannot obtain more than its inherited ceiling.

## Enforcement and observation

Put decisions before resolution/dial/HTTP dispatch, not in a post-hoc packet
viewer. Reuse one matcher and one decision vocabulary across in-process clients
and the child egress broker. Correlate policy decisions, connection lifecycle,
and inference attempts; do not infer owners from DNS/IP coincidence.

HTTP-aware trusted clients can enforce path rules before TLS encryption.
An ordinary HTTPS CONNECT proxy sees a destination, not the encrypted URI.
Arbitrary subprocess HTTPS therefore needs a separately supported HTTP-aware
channel to honor URI rules; until then a path-only grant must refuse that
channel rather than widen to a host grant. Do not install a TLS interception CA
as an incidental side effect of enabling this feature.

Native child traffic requires an OS boundary that prevents bypassing the broker,
including raw sockets, alternate resolvers, ambient proxies, and OS services
that send on the child's behalf. Environment variables alone are not that
boundary. A process/socket observer can supplement monitoring in allow-all
mode, but must report unattributed traffic and coverage gaps rather than claim
complete monitoring. No traffic is not proof that a collector is working.

On macOS, Bridle ADRs 0015/0016 currently hold restricted Seatbelt network
support: ambient service mediation is not proven complete, and L3 reports
restricted net authority as Unknown. Keep that refusal. Explicitly granted
unrestricted networking can coexist with a Seatbelt filesystem fence, but it
does not provide destination filtering. A macOS Network Extension or an
isolated execution backend is a separate implementation with native evidence,
deployment requirements, and operator setup. Apple documents the content-filter
and signing/deployment interfaces here:
[content filters](https://developer.apple.com/documentation/networkextension/content-filter-providers),
[deployment](https://developer.apple.com/documentation/technotes/tn3134-network-extension-provider-deployment).

The console must show actual enforcement and collection coverage separately:
broker decisions, native child boundary, direct-flow observation, and inference
accounting. Unsupported filtering cannot be displayed as active. Kernel-denied
flows and OS-deputy traffic are not automatically visible in proxy statistics.
Grant revocation cancels pending requests, blocks new requests, closes brokered
streams, and terminates/restricts native flows where the backend supports it.
Otherwise the operator sees which child must stop before revocation is complete.

## Cross-platform parity

Parity means the same operator-visible policy and outcomes, with platform-specific
mechanisms. Native Windows is a first-class target; WSL-only evidence does not
qualify it. No platform silently substitutes unrestricted access or unobserved
traffic when a requested feature is unavailable.

| Layer | Shared contract | Linux implementation direction | macOS implementation direction | Windows implementation direction |
| --- | --- | --- | --- | --- |
| Permissions | Same matcher, actions, inherited ceilings, expiry, revocation | Shared Rust policy and broker | Shared Rust policy and broker | Shared Rust policy and broker |
| Child boundary | Direct egress cannot bypass the decision point | Network namespace, shape undecided (decision 3); existing Landlock/seccomp filesystem/process boundary | Signed Network Extension content filter plus Seatbelt filesystem boundary | WFP application/flow authorization plus existing AppContainer filesystem/process boundary |
| Observation | Same flow lifecycle, units, ownership and loss indicators | Namespace/broker accounting; kernel events where required | Filter flow/report callbacks; application and process audit-token attribution | WFP flow/accounting facilities and service; validated process/application attribution |
| URL rules | Same HTTP semantics, redirects and TLS validation | Shared HTTP-aware broker/client | Shared HTTP-aware broker/client | Shared HTTP-aware broker/client |
| Inference | Same attempt identities, destinations, roles and reported usage | Existing inference send/settle hooks | Existing inference send/settle hooks | Existing inference send/settle hooks |
| Controls | Same four TUI views and signed rule history | Existing Newt panel host; a live table for Activity | Existing Newt panel host; a live table for Activity | Existing Newt panel host; a live table for Activity |

These are implementation directions, not claims of current support. In
particular, a kernel flow filter does not make encrypted URL paths visible on
any platform. The HTTP-aware mediation requirement is identical everywhere.

macOS packaging includes a signed containing app/system extension, Network
Extension entitlement, an authenticated control channel, and system activation
and filter consent. Xcode compilation or ad-hoc signing is not installation
evidence. Match both originating application and creating process audit tokens
when OS services act for a process; a bare PID or executable name is not session
identity. Apple exposes this distinction, but completeness for each supported
deputy remains a native test gate. A missing hostname must remain unknown;
Apple's `remoteHostname` is populated only for certain create-by-name APIs.

Linux packaging must probe namespace/firewall availability and privilege policy,
isolate the child's network stack, and supply a bounded setup helper when the
host disables unprivileged setup. Every descendant inherits the boundary;
joining another namespace or using inherited sockets must be covered by tests.
Network namespaces isolate networking resources; Landlock's port restrictions
alone do not implement a DNS-name or URL policy. Coordinate with
[agent-bridle#276](https://github.com/Gilamonster-Foundation/agent-bridle/issues/276).

Windows packaging includes a signed installer/service and authenticated local
control IPC. Prefer WFP's built-in filtering through a management service;
add a signed callout driver only for behavior the built-in filters cannot
provide (such as a required pending decision/accounting path). Driver signing,
installation, repair, uninstall, and compatibility with Windows Firewall are
delivery work, not assumptions. Prove both IPv4 and IPv6, native descendants,
UDP/raw traffic, and policy reauthorization on real Windows.

The cross-platform adapter reports what it can enforce/observe, owns start and
stop lifecycle, binds a session/process tree before launch, applies a policy
generation atomically, and acknowledges revocation. Those are responsibilities
to fit to existing Bridle interfaces, not a second authority issuer. Privileged
components accept only authenticated operator/harness control; untrusted
children cannot register themselves as unrestricted or change policy. Helper
restart, harness crash, missing collectors, version mismatch, and uninstall
must have explicit behavior. An active confined session stops when its required
boundary disappears; unrelated applications keep their normal networking.

One shared fixture suite specifies decisions and audit outcomes, run unchanged
against all adapters. Native suites then prove the actual kernel and IPC
boundary. Each release carries a Linux/macOS/Windows evidence matrix for:
prompt-before-egress; once/session/permanent allow and deny; wildcard/origin/URI
matching; DNS and redirect behavior; active-flow revocation; audit under
allow-all; inference retries/auxiliary calls; descendant attribution; collector
loss; helper crashes; filesystem isolation; startup/shutdown and cleanup. A
missing native run is pending evidence, never a pass. Do not call the feature
parity-complete while any required platform cell remains unsupported.

Platform references:
[Apple flow attribution](https://developer.apple.com/documentation/networkextension/nefilterflow/sourceprocessaudittoken),
[Apple hostname availability](https://developer.apple.com/documentation/networkextension/nefiltersocketflow/remotehostname),
[Linux namespaces](https://www.kernel.org/pub/linux/docs/man-pages/book/man-pages-6.11.pdf),
[WFP ALE](https://learn.microsoft.com/en-us/windows/win32/fwp/application-layer-enforcement--ale-),
[WFP implementation guidance](https://learn.microsoft.com/en-us/windows-hardware/drivers/network/callout-driver-programming-considerations).

## Inventory (2026-11-14)

Read from newt-agent source at `397df65e` and from agent-bridle's ADRs. Nothing
was measured by running the harness. The Linux items marked measured were run
with OS tools on one Ubuntu 24.04 host (kernel 6.8). macOS and Windows facts are
from documentation.

| Traffic | Grant today | Record today |
| --- | --- | --- |
| `web_fetch`; MCP over HTTP and its OAuth hops; git push and git fetch brokers | Exact host, checked before the dial. An unrestricted scope admits every host; MCP admits loopback without a grant | Prompted allow and deny decisions (permission log); denials when the denial journal is armed. No flow record |
| `gh pr create` broker | `github.com` is checked before `gh` starts; the API host and redirect hops are not ([#3104](https://github.com/Gilamonster-Foundation/newt-agent/issues/3104)) | As above |
| cargo dependency fetch | The two crates.io hosts are checked before cargo is spawned (on macOS, a one-time approval for all destinations); cargo's own connections are not host-checked | As above |
| `git pull`, `clone`, `ls-remote` and fetch forms the broker does not handle | A one-command approval for all destinations | The decision only |
| Primary inference | Backend selection | Per attempt: model, configured backend URL, request digest and reported tokens, in memory and written only with `--events`. Per turn: endpoint, elapsed time and tokens in the usage log. No contacted address, bytes or per-attempt timing |
| Auxiliary inference | Configuration | Summarizer and embeddings: none. Smart-harness judge: its requests, replies and endpoint. Crew: backend name and usage per role |
| Backend discovery, probes and warm-up; GPU telemetry polling; model downloads and provider model-list fetches; newt-web dock calls; Jupyter kernel; DNS | None | No flow record |
| Mesh peers | Admitted by mesh keys; no net-scope grant | No flow record |
| Confined children (shell, builds, stdio MCP) | Deny-all or allow-all; a host list is narrowed to deny-all at spawn | No flow record |
| Host-side children (provider plugins, ssh, browser, secret commands, full-access shell) | None | No flow record |
| Listeners: Prometheus endpoint (all interfaces when enabled), OAuth callback, newt-web, mesh bus, Bridle's loopback proxy | None | None |

- Production code has about 45 `reqwest::Client` construction sites in eight
  crates (`git grep`, test modules excluded) and no shared constructor. At that
  snapshot the two gated ones were in `newt-mcp-client`. newt-web also dials
  with `ureq`.
- Only primary inference passes `attempt_capture::send`. `AttemptRecord` holds
  the configured backend URL, not the destination actually contacted. The
  inference clients use `reqwest` defaults, so they follow redirects and honour
  proxy environment variables.
- newt starts Bridle's proxy through `start_egress_proxy`, which uses the null
  audit sink, so no session writes proxy audit events. Bridle's public
  `net_proxy::start` takes a sink, so switching audit on is a newt change.
- Grants live in five places: the config net list, `approve.toml`, the signed
  and encrypted permanent store, in-memory session grants, and launch flags.
  The permission log records each prompted decision with its host.
- [#3093](https://github.com/Gilamonster-Foundation/newt-agent/pull/3093),
  merged after this inventory, adds launch grants (`--net <host>`), a
  per-request host check for brokered `gh` reads, and with it a third gated
  client.

Child boundary, per platform:

- **Linux.** Landlock network rules match ports, not addresses, and TCP only on
  kernel 6.8. Ubuntu 24.04 restricts unprivileged user namespaces through
  AppArmor by default (measured: `unshare -U true` succeeds, `unshare -Urn true`
  fails writing `uid_map`), so the routed fence of agent-bridle#276 needs an
  installed AppArmor profile or a privileged helper there. An empty namespace
  needs neither (measured: under `unshare -Un` a child has no TCP or UDP route,
  loopback included, and still connects to a pathname Unix socket held by its
  parent). Only clients that can reach a proxy over a Unix socket can use it.
- **macOS.** Seatbelt can restrict a child's own sockets to loopback, and by
  port, without privilege (Bridle ADR 0015 reports native `sandbox-exec`
  measurements; not rerun here). It does not cover OS services acting for the
  child, so Bridle does not admit a proxy-fenced shape (ADRs 0015/0016).
  `sandbox-exec` is deprecated.
- **Windows.** An AppContainer reaches a loopback proxy only after a loopback
  exemption. That needs elevation, opens every loopback port, and is documented
  as a debugging facility.

Socket observer, per platform:

- **Linux.** `sock_diag` lists TCP sockets with peer address and byte counters
  to an unprivileged process (measured with `ss -tinp`). It lists every user's
  sockets in the network namespace, so the console filters to its own process
  tree. It does not see inside a child's own network namespace, and it is
  polled, so a short connection can be missed. UDP sockets show no byte
  counters (measured with `ss -uainp`).
- **Windows.** `GetExtendedTcpTable` lists TCP peers with the owning process.
  Per-connection byte counts need administrator rights.
- **macOS.** Not established. `nettop` reports per-process bytes through a
  private framework.

So byte counts come from the points that own the sockets: the harness's clients
and the proxy. The OS socket list is a cross-check, not an accounting source.

## Existing machinery to extend

Inventory before introducing records or another permission system:

- `newt-core/src/permissions.rs`: existing actions, session gate, and
  permission request/decision types; `newt-tui/src/permissions.rs` owns prompting
  and signed durable promotion. `permissions_panel.rs` already returns operator
  intents to the chat owner rather than persisting from the view.
- `newt-core/src/ocap_store.rs`: signed `approve.toml` and evaluated policy.
  Richer patterns need one matcher shared with Bridle, extending
  [agent-bridle#152](https://github.com/Gilamonster-Foundation/agent-bridle/issues/152)
  and [#153](https://github.com/Gilamonster-Foundation/agent-bridle/issues/153).
- Bridle `agent-bridle-core/src/net_proxy.rs`: `NetAuditEvent`, `NetEventSink`,
  connection outcomes and byte totals; `agent-bridle-tool-shell/src/bin/bridle-netmon.rs`
  already folds them for a live host table. Current auditing covers proxy
  traffic and is opt-in; it is not the complete session monitor requested here.
- `newt-core/src/agentic/attempt_capture.rs` and `attempts.rs`: send-time,
  content-addressed attempt identity, retry ordinals, model/backend attribution,
  outcomes, and reported usage. Add actual sanitized destination capture here,
  not by looking up today's backend config while rendering historical rows.
  Use `AttemptLedger` totals for cumulative usage: `ConversationTurn.tokens_in`
  and the turn-level usage merge retain the largest single prompt for context
  sizing, not the sum of input tokens sent across attempts. Keep context
  occupancy and cumulative inference use visibly distinct.
- `newt-core/src/flight_recorder.rs`: shadow capability observations, including
  allow-all use. These are authority observations, not complete socket/byte
  telemetry; retain that distinction.
- `event_journal.rs` and `permission_journal.rs`: existing Merkle-linked records,
  head anchors, and verification. `ContentId` identifies canonical records;
  `RawContentId` addresses opaque request bytes; `MerkleNode` links causal
  history. Use the published content-addressable implementation, not new hashes,
  UUID identity, or another JSONL chain implementation.
- `newt-core/src/grant_tree.rs`: a typed `Grant` with a net target and
  session/durable provenance, not yet wired. Base for a granted-destination row.
- `newt-mcp-client` (`net_policy.rs`, `build_pinned_client`): pre-dial host
  check, DNS pinning and a typed grant-required error. The pattern for a shared
  client constructor.
- `scripts/spawn_inventory.py` with `docs/security/spawn-inventory.toml`: a
  per-file call-site ratchet. Copy it for network call sites.
- `newt-core/tests/content_addressable_ratchet.rs` pins the number of
  unaddressed journals, so a flow record is a payload on `event_journal`.

Provenance-audit findings: attempt keys already derive content identity
(`attempts.rs`, `AttemptKey::canonical_form`), and observations are linked by
the existing journal (`AttemptLedger::observe`). There is no network-console
read/verifier or reversible rule editor yet. Existing signed policy is read by
`ocap_store::load_store`; a signature alone is not an edit history. Before the
console ships, production reads must verify the journal against its head and
refuse to present damaged history as verified. Rule changes append an event
naming the previous rule; undo appends the inverse, with an interleaved-edit
round-trip test. These are implementation gates, not satisfied by this design.

Audit metadata excludes authorization headers, URL credentials, query/fragment
secrets, prompts, and response bodies. An exact request body is represented by
its existing raw content address, not copied into a network log. Record audit
write failures and dropped observations visibly. In ask/deny mode, a required
audit write that fails blocks a new grant/dial; allow-all mode must pause on loss
of required audit rather than silently become blind. Storage bounds/retention
are explicit; pruning leaves a verified retention event and head anchor.

The chained journals (event, denial, permission) are best-effort today, sync on
every line and have no writer lock across threads or processes. Blocking a dial
on audit loss is therefore new behaviour. Step 2 requires no audit write, so it
records without blocking. Step 3 decides which writes are required; the rule
above then applies to them.

## Delivery and acceptance

Current cancellation coverage also needs a platform gate: Linux build execution
creates its cgroup only for `NetGrant::DenyAll`; an explicitly networked build
uses the existing process-group fallback. Decouple descendant cleanup from the
network mode and prove termination of detached descendants before claiming
equivalent cancellation coverage. Filesystem inheritance alone does not prove
that every descendant has stopped.

1. Correct explicit operator network-authority propagation through the build
   lane. Test that allow-all retains the filesystem fence, and that a build
   approval alone cannot grant networking. This resolves the narrow refusal
   mismatch; it does **not** deliver the console or complete network auditing.
2. Wire continuous session observation and actual inference destinations through
   existing journals and a read-only Activity/Inference view. Require native
   positive controls and display every collection gap. Audit stays on in all
   access modes, panel closed, and after model/backend changes.
   - One flow record shape, shared with agent-bridle#413.
   - Inference: the requested URL at the send; the final URL, bytes and timing
     at settle.
   - The other in-process clients move onto a shared client constructor, with a
     network call-site ratchet whose count only falls.
   - Bridle's proxy audit switched on wherever newt starts the proxy.
   - `web_fetch` and the brokers emit the same record.
   - On Linux, the socket-list cross-check for the harness and the children in
     its network namespace.
   - Shown as gaps, not covered: confined children under allow-all, host-side
     children, listeners.
3. Add shared structured matching, pre-dial pending decisions, session denials,
   and signed rule management. Ship URL rules only on channels whose HTTP
   requests can actually be mediated. Test redirects, DNS label boundaries,
   ports, encoded paths, denied-over-allowed overlaps, expiry and revocation.
   Decide grants for the harness traffic that has none today, whether an
   inference redirect to another host is refused, and which audit writes are
   required. The grant record gains port, scheme and purpose, matched
   through the decision point of
   [agent-bridle#152](https://github.com/Gilamonster-Foundation/agent-bridle/issues/152).
4. Enable destination-constrained native subprocesses per platform only after
   adversarial bypass tests prove the selected boundary. Cover DNS, TCP, UDP,
   raw/direct sockets, OS deputies, subprocess trees, collector failure, and
   abrupt session exit. Keep unsupported platforms explicit and fail-closed.
   The Linux spike (decision 3) comes first.

End-to-end acceptance: an unknown destination is visible pending with zero
origin bytes; deny never resolves/dials it; session allow repeats without a
prompt; saved rules survive restart while session rules do not; revoke ends an
active flow; allow-all still produces complete supported-channel history and
token attribution; workspace-external reads/writes remain denied; no model
instruction or repository file can authorize or disable this machinery.
