export const meta = {
  name: 'propose-verify',
  description: 'Facet-parallel propose -> default-skeptical adversarial verify (inertness checked against how the ratchet ACTUALLY grades) -> synthesized design doc with the rejected section kept visible. The generalized form of the 15-agent workflow that killed five inert fixes in #802.',
  phases: [
    { title: 'Ground', detail: 'derive facets from the findings (skipped when args.facets given)' },
    { title: 'Propose+Verify', detail: 'pipeline per facet: grounded proposal -> skeptical verdict' },
    { title: 'Synthesize', detail: 'design doc; rejected proposals stay visible' },
  ],
}

// args: { findings: 'path to the evidence base — an autopsy .json/.md or a
//                    design doc (required)',
//         out: 'output doc path, repo-relative or absolute (required)',
//         facets?: [{key, focus, read:[paths]}], max_facets?: 7 }
// args may arrive as a JSON string depending on the invoker — normalize.
const argv = typeof args === 'string' ? JSON.parse(args) : (args || {})
if (!argv.findings || !argv.out) throw new Error('missing required args: findings, out')
const MAXF = argv.max_facets || 7
// argv.repo: newt-agent checkout; without it, agents resolve from their cwd.
const REPO = argv.repo
  ? `The repo root is ${argv.repo} (given — use it verbatim).`
  : 'Resolve the repo root via git rev-parse --show-toplevel (requires a cwd inside the newt-agent repo).'

// ---- institutional memory as DATA (three-Cs): what killed five plausible
// fixes in #802 §4a, embedded so every verifier starts calibrated.
const GRADING_TRUTH = `
HOW THE RATCHET ACTUALLY GRADES (load-bearing; misread this and your verdict is worthless):
- scripts/eval/ratchet.sh selects the produced tree via git branch --list 'crew/*' | tail -1,
  drops the case's HIDDEN grade_spec.rs into tests/, and runs ONLY that (cargo test --test grade_spec).
- plan_rc / run.complete / exit codes are emitted DIAGNOSTICS, not the verdict. Landed work
  persists to the crew/* branch BEFORE any fail-stop break (commit_to_branch runs first).
- Therefore: a proposal that only changes REPORTING (exit codes, plan_rc, completeness flags)
  moves ZERO cells. The sweep cells fail because the correct code is not on the tip branch.
CALIBRATION — five fixes REJECTED as inert in #802 §4a (do not let their cousins through):
1. fail-soft (exit 1 -> 3): grade ignores exit codes; landed work already persists. Inert.
2. nothing-to-land -> flip plan_rc: decoupled grade again; the tip branch already carries the fix
   when a trailing no-op fails. Inert.
3. per-leaf compile-gate instead of behavioral gate: the observed death was a NO-DIFF leaf
   ("no changes to land"), not a gate false-fail; a compile-gate cannot make a no-diff leaf land. Inert.
4. single-vs-crew router: the router's own spec classifies the failing tasks as Decompose
   (multi-def-file), so it diverts none of the failing cells. Plausible-but-inert.
5. run.complete flip for gameable rungs: behavioral is set by the ratchet independently;
   changing run.complete moves no PASS/FAIL cell. Inert.
ALSO REMEMBER THE n=1 LESSON (#803): expected_lift claims about specific cells must be stated
as hypotheses needing n>=5 A/B confirmation (/ab-gate), never as facts.`

const CONSTRAINTS = `
HARD CONSTRAINTS every proposal MUST respect (newt-agent is the drake-swarm training ground):
- OCAP: Caveats meet-only (attenuate, never widen); exec/net fail-closed; the per-leaf worktree
  is the real fs boundary. No fix may widen authority.
- HONEST GATES: a fix must NOT make the behavioral grade easier to fake (no leaf self-passing,
  no editing tests, no lowering verify).
- Plain-scroller TUI: no ratatui/alternate-screen in the chat path.
- Three-Cs: knowledge (lexicons, heuristics, thresholds) belongs in droppable DATA, not hardcoded logic.
- Fully-mocked unit tier: no real fs/net/subprocess/clock in unit tests.`

const PROPOSAL_SCHEMA = {
  type: 'object', additionalProperties: false,
  required: ['title', 'problem', 'fix', 'files', 'test_plan', 'expected_lift_cells', 'risk', 'constraint_alignment'],
  properties: {
    title: { type: 'string' },
    problem: { type: 'string', description: 'the specific observed failure this targets, citing the findings' },
    fix: { type: 'string', description: 'the concrete change, grounded in real file:line and function names you read' },
    files: { type: 'array', items: { type: 'string' } },
    test_plan: { type: 'string', description: 'fully-mocked unit tier' },
    expected_lift_cells: { type: 'array', items: { type: 'object', additionalProperties: false,
      required: ['cell', 'claim'],
      properties: { cell: { type: 'string', description: 'e.g. "qwen3-coder:30b x T2-humanize-duration crew"' },
        claim: { type: 'string', enum: ['flip', 'harden', 'integrity', 'no-regression'] } } },
      description: 'what /ab-gate should later score this proposal against' },
    risk: { type: 'string', enum: ['low', 'medium', 'high'] },
    constraint_alignment: { type: 'string' },
  },
}
const VERDICT_SCHEMA = {
  type: 'object', additionalProperties: false,
  required: ['sound', 'fixes_observed', 'inert_on_grade', 'violates', 'keep', 'revised_risk', 'concerns'],
  properties: {
    sound: { type: 'boolean', description: 'mechanism correct and grounded in the real code' },
    fixes_observed: { type: 'boolean', description: 'would it move the observed failures, not just sound plausible' },
    inert_on_grade: { type: 'boolean', description: 'true if it changes reporting/diagnostics but cannot change what lands on the crew/* tip branch' },
    inertness_reason: { type: 'string' },
    violates: { type: 'string', description: 'the hard constraint it violates, or "none"' },
    keep: { type: 'boolean' },
    revised_risk: { type: 'string', enum: ['low', 'medium', 'high'] },
    concerns: { type: 'array', items: { type: 'string' } },
    improvement: { type: 'string', description: 'the single change that most strengthens it' },
  },
}

phase('Ground')
let facets = argv.facets
if (!facets || !facets.length) {
  const g = await agent(`Derive improvement facets from this evidence base: read ${argv.findings} in full (if it is an autopsy .json, the 'items' carry mechanisms + evidence quotes; the frequency table ranks mechanisms). Resolve the repo root (git rev-parse --show-toplevel) and skim the code regions the evidence points at. Produce at most ${MAXF} facets, ranked by (frequency x cross-model spread) of the mechanism they target. Each facet: key (kebab slug), focus (2-5 sentences: the observed failure + the direction of a fix, naming real files/functions), read (2-4 repo-relative paths a proposer must read).`,
    { label: 'ground', phase: 'Ground', effort: 'high',
      schema: { type: 'object', additionalProperties: false, required: ['facets'], properties: {
        facets: { type: 'array', maxItems: MAXF, items: { type: 'object', additionalProperties: false,
          required: ['key', 'focus', 'read'],
          properties: { key: { type: 'string' }, focus: { type: 'string' }, read: { type: 'array', items: { type: 'string' } } } } } } } })
  if (!g || !g.facets.length) throw new Error('grounding produced no facets')
  facets = g.facets
}
log(`${facets.length} facets: ${facets.map((f) => f.key).join(', ')}`)

phase('Propose+Verify')
const proposePrompt = (f) => `You are improving newt-agent's autonomous crew/plan-execution. Evidence base: read ${argv.findings} first.
${GRADING_TRUTH}
${CONSTRAINTS}
YOUR FACET: ${f.key} — ${f.focus}
Read the real code (resolve the repo root via git rev-parse --show-toplevel): ${(f.read || []).join(', ')}. Ground every claim in actual functions/lines you saw. Produce ONE concrete, minimal, shippable proposal — the smallest change that moves the observed failures. State expected_lift_cells as testable /ab-gate hypotheses.`
const verifyPrompt = (prop, f) => `Adversarially verify this proposal for newt-agent's crew executor. Default to SKEPTICAL — your job is to refute it.
${GRADING_TRUTH}
${CONSTRAINTS}
PROPOSAL (facet ${f.key}): ${JSON.stringify(prop, null, 2)}
Read the cited code yourself (repo root via git rev-parse --show-toplevel). Check:
1. SOUND? right files/functions, mechanism as described?
2. FIXES OBSERVED? would the named cells actually move, or is it plausible-but-inert? Compare against the five calibration rejections above.
3. INERT ON GRADE? does it change only reporting/diagnostics rather than what lands on the crew/* tip?
4. VIOLATES a hard constraint? (gameable gates, widened authority, ratatui, hardcoded knowledge, real-fs unit tests)
keep=false if unsound, inert, or constraint-violating.`

const results = await pipeline(
  facets,
  (f) => agent(proposePrompt(f), { label: `propose:${f.key}`, phase: 'Propose+Verify', schema: PROPOSAL_SCHEMA }),
  (prop, f) => {
    if (!prop) return null
    return agent(verifyPrompt(prop, f), { label: `verify:${f.key}`, phase: 'Propose+Verify', schema: VERDICT_SCHEMA, effort: 'high' })
      .then((v) => ({ facet: f.key, proposal: prop, verdict: v }))
  },
)
const judged = results.filter(Boolean).filter((r) => r.verdict)
const kept = judged.filter((r) => r.verdict.keep)
const rejected = judged.filter((r) => !r.verdict.keep)
log(`${kept.length} kept, ${rejected.length} rejected of ${facets.length} facets`)

phase('Synthesize')
const synth = await agent(`Write the design doc to ${argv.out} (resolve relative to the repo root via git rev-parse --show-toplevel; create parent dirs). House style = docs/design/improving-crew-results.md (read it): separate what is SHOWN (the findings) from what is PROPOSED; keep the REJECTED analysis visible in its own section (never hide it — it is the doc's immune system); every per-cell expectation is a hypothesis requiring n>=5 /ab-gate confirmation, not a claim.

Evidence base: ${argv.findings}
KEPT proposals (sharpen each with its verdict.improvement; order the roadmap by grade-lift ÷ risk):
${JSON.stringify(kept, null, 2)}
REJECTED (state each verdict's reason, especially inert_on_grade):
${JSON.stringify(rejected.map((r) => ({ facet: r.facet, title: r.proposal.title, why: r.verdict.inert_on_grade ? `INERT: ${r.verdict.inertness_reason}` : r.verdict.violates !== 'none' ? `violates ${r.verdict.violates}` : r.verdict.concerns.join('; ') })), null, 2)}

End with '## What to run next': for each kept proposal, the literal /ab-gate invocation (lever slug, the two sweep arms to produce, expected_flip_cells) that would confirm or kill it. Write the file, then return a 4-sentence summary.`,
  { label: 'synthesize', phase: 'Synthesize', effort: 'high' })

return {
  kept: kept.map((r) => ({ facet: r.facet, title: r.proposal.title, risk: r.verdict.revised_risk, expected_lift_cells: r.proposal.expected_lift_cells })),
  rejected: rejected.map((r) => ({ facet: r.facet, title: r.proposal.title, inert: !!r.verdict.inert_on_grade })),
  doc_path: argv.out,
  summary: synth,
}
