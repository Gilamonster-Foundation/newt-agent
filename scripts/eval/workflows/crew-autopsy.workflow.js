export const meta = {
  name: 'crew-autopsy',
  description: 'Classify failed and PASS?gameable crew runs against the 7-mechanism taxonomy of docs/design/improving-crew-results.md §3, with evidence quotes required and skeptical re-checks under a confidence floor. Deterministic aggregation names the biggest mover.',
  phases: [
    { title: 'Inventory', detail: 'resolve TSV rows / run dirs into autopsy items' },
    { title: 'Classify', detail: 'one evidence-bound classifier per run' },
    { title: 'Recheck', detail: 'skeptical re-read of low-confidence classifications' },
    { title: 'Synthesize', detail: 'frequency tables (computed in code) + narrative' },
  ],
}

// args: { source: 'sweep dir | path to RATCHET tsv | comma-separated run dirs (required)',
//         id: 'output name (required, e.g. 2026-07-02-baseline)',
//         include?: 'failures'|'failures+gameable'|'all' (default failures+gameable),
//         limit?: 24, floor?: 0.7, out_dir?: '<repo>/scripts/eval/results/autopsy' }
if (!args || !args.source || !args.id) throw new Error('args.source and args.id are required')
const INCLUDE = args.include || 'failures+gameable'
const LIMIT = args.limit || 24
const FLOOR = args.floor ?? 0.7

// The taxonomy is DATA (three-Cs): keys, definitions, and the evidence
// signature a classification must quote. Mirrors #802 §3.
const TAXONOMY = [
  { key: 'fail-stop', what: 'run_plan_with_reground hard-breaks on the first leaf Err; pending leaves never dispatch', signature: 'plan log shows leaves marked pending/never dispatched after the first failure' },
  { key: 'nothing-to-land', what: 'a leaf verified but produced no diff -> "no changes to land" Err', signature: '"no changes to land" / "nothing to land" in the plan log' },
  { key: 'endstate-verify-on-intermediate', what: 'per-leaf gate ran the task\'s full behavioral test on a non-final leaf, which cannot pass', signature: 'an intermediate leaf failing the full task test while its own deliverable looks done' },
  { key: 'worker-ignores-scope', what: 'worker implemented the whole goal (or invented orphan/vacuum files) instead of its one leaf', signature: 'diff shows work far beyond the leaf instruction, or new files never wired in (orphans)' },
  { key: 'worker-spurious-edits', what: 'off-target edits (e.g. Cargo.toml on a pure-logic fix), retries exhausted, NEEDS-HUMAN-REVIEW', signature: 'diff touches unrelated files; NEEDS-HUMAN-REVIEW after retry attempts' },
  { key: 'planner-over-decomposition', what: 'non-actionable inspect/examine leaves or trailing validate/run-tests leaves padding the plan', signature: 'plan leaves named inspect/examine/validate/run-tests that produce no diff' },
  { key: 'grading-integrity', what: 'seed test green without the goal genuinely met (gameable rung), or the crew edited its own test', signature: 'PASS?gameable grade, or edited_own_test=yes, or diff weakens/edits assertions' },
  { key: 'ops-noise', what: 'infrastructure, not the crew: endpoint down, model not pulled, timeout before inference', signature: 'connection errors / model-not-found / timeout with no inference in the log' },
  { key: 'other', what: 'a real mechanism not in the taxonomy', signature: 'quote the evidence; a verifier will re-read' },
]
const KEYS = TAXONOMY.map((t) => t.key)

const CLASSIFICATION_SCHEMA = {
  type: 'object', additionalProperties: false,
  required: ['mechanism', 'evidence', 'confidence'],
  properties: {
    mechanism: { type: 'string', enum: KEYS },
    secondary: { type: 'array', items: { type: 'string', enum: KEYS } },
    evidence: { type: 'array', minItems: 1, items: { type: 'object', additionalProperties: false,
      required: ['source', 'quote'],
      properties: { source: { type: 'string', description: 'file+line-range or git ref inside the run dir' }, quote: { type: 'string', maxLength: 600 } } } },
    leaf_shape: { type: 'object', additionalProperties: false, properties: {
      leaves: { type: 'integer' }, diff_leaves: { type: 'integer' }, noop_leaves: { type: 'integer' } } },
    confidence: { type: 'number', minimum: 0, maximum: 1 },
    notes: { type: 'string', maxLength: 800 },
  },
}

phase('Inventory')
const INVENTORY_SCHEMA = {
  type: 'object', additionalProperties: false, required: ['items'],
  properties: { items: { type: 'array', items: { type: 'object', additionalProperties: false,
    required: ['task', 'mode', 'model', 'behavioral', 'run_dir'],
    properties: { task: { type: 'string' }, mode: { type: 'string' }, model: { type: 'string' },
      behavioral: { type: 'string' }, run_dir: { type: 'string' }, details: { type: 'string' } } } } },
}
const inv = await agent(`Build the autopsy inventory from this source: ${args.source}
- If it is a directory containing sweep.tsv: parse the RATCHET rows (tab-separated; cols 2-6 = task,mode,model,behavioral,details; dir=<path> inside details is the run dir).
- If it is a .tsv file: same parsing.
- If it is a comma-separated list of directories: each is a run dir; infer task/mode/model from any .plan.log or leave "unknown".
Include ONLY rows/dirs matching include='${INCLUDE}' (failures = behavioral FAIL; +gameable adds PASS?gameable; all = everything). Drop items whose run dir no longer exists on disk (they were reaped) — note how many you dropped in a final check by listing them, but only return existing ones.`,
  { label: 'inventory', phase: 'Inventory', schema: INVENTORY_SCHEMA, effort: 'low' })
if (!inv || !inv.items.length) return { id: args.id, top_mechanism: null, note: 'no autopsy items (nothing failed, or run dirs were reaped)' }
const items = inv.items.slice(0, LIMIT)
if (inv.items.length > LIMIT) log(`capped at ${LIMIT} of ${inv.items.length} items — oldest dropped`)

phase('Classify')
const taxText = TAXONOMY.map((t) => `- ${t.key}: ${t.what}\n  evidence signature: ${t.signature}`).join('\n')
const classifyPrompt = (it) => `Autopsy ONE crew-eval run. Item: ${JSON.stringify(it)}
Investigate the run dir: read ${it.run_dir}/.plan.log; run (read-only) git -C ${it.run_dir} branch --list 'crew/*', git -C ${it.run_dir} log --oneline --all, and git diffs between the baseline commit and the crew/* tips. Do NOT modify anything in the dir.
Classify the PRIMARY mechanism from this taxonomy (secondary mechanisms allowed):
${taxText}
RULES: every classification needs at least one VERBATIM quote from the run artifacts (schema requires it). If the evidence genuinely fits none, use 'other'. If the log shows the model was never exercised (connection/pull errors), use 'ops-noise'. Report confidence honestly — a verifier re-reads anything below ${FLOOR}.`
const chunk = (arr, n) => arr.reduce((a, x, i) => ((i % n ? a[a.length - 1].push(x) : a.push([x])), a), [])
let classified = []
for (const batch of chunk(items, 8)) {
  const res = await parallel(batch.map((it) => () =>
    agent(classifyPrompt(it), { label: `autopsy:${it.model}/${it.task}`, phase: 'Classify', schema: CLASSIFICATION_SCHEMA })
      .then((c) => (c ? { item: it, ...c } : null))))
  classified.push(...res.filter(Boolean))
}

phase('Recheck')
const doubtful = classified.filter((c) => c.confidence < FLOOR || c.mechanism === 'other')
if (doubtful.length) {
  const rechecks = await parallel(doubtful.map((c) => () =>
    agent(`Skeptically RE-CHECK this autopsy classification by re-reading the run dir yourself (read-only). Try to REFUTE it.
Classification: ${JSON.stringify({ item: c.item, mechanism: c.mechanism, evidence: c.evidence, notes: c.notes })}
Taxonomy:\n${taxText}
Return confirmed=true only if the primary mechanism is right; otherwise give the corrected mechanism with your own quote.`,
      { label: `recheck:${c.item.model}/${c.item.task}`, phase: 'Recheck',
        schema: { type: 'object', additionalProperties: false, required: ['confirmed'], properties: {
          confirmed: { type: 'boolean' }, corrected_mechanism: { type: 'string', enum: KEYS },
          corrected_quote: { type: 'string' }, why: { type: 'string' } } } })
      .then((v) => ({ c, v }))))
  for (const { c, v } of rechecks.filter(Boolean)) {
    if (v && !v.confirmed && v.corrected_mechanism) {
      c.mechanism = v.corrected_mechanism
      c.notes = `${c.notes || ''} [recheck override: ${v.why || ''}]`.trim()
    }
  }
}

phase('Synthesize')
// deterministic aggregation — the numbers the narrative must embed verbatim
const graded = classified.filter((c) => c.mechanism !== 'ops-noise')
const freq = {}
for (const c of graded) freq[c.mechanism] = (freq[c.mechanism] || 0) + 1
const xModel = {}, xTask = {}
for (const c of graded) {
  ;(xModel[c.mechanism] = xModel[c.mechanism] || new Set()).add(c.item.model)
  ;(xTask[c.mechanism] = xTask[c.mechanism] || new Set()).add(c.item.task)
}
const ranked = Object.entries(freq).sort((a, b) => b[1] - a[1] || xModel[b[0]].size - xModel[a[0]].size)
const top = ranked[0] ? ranked[0][0] : null
const freqTable = ['| mechanism | count | models affected | tasks affected |', '|---|---|---|---|',
  ...ranked.map(([m, n]) => `| ${m} | ${n} | ${[...xModel[m]].join(', ')} | ${[...xTask[m]].join(', ')} |`),
  `| (ops-noise, excluded) | ${classified.length - graded.length} | | |`]

const OUTD = args.out_dir || null
const synth = await agent(`Write the autopsy for '${args.id}'. Resolve the repo root (git rev-parse --show-toplevel; the source path ${args.source} is inside or near it) and write TWO files under ${OUTD || '<repo>/scripts/eval/results/autopsy'}/:
1. ${args.id}.json — EXACTLY this JSON, verbatim: ${JSON.stringify({ id: args.id, source: args.source, top_mechanism: top, frequency: freq, items: graded.map((c) => ({ ...c.item, mechanism: c.mechanism, secondary: c.secondary || [], confidence: c.confidence, evidence: c.evidence })) })}
2. ${args.id}.md — the narrative. RULES: embed this table VERBATIM (never recount):
${freqTable.join('\n')}
Lead with the biggest mover (${top || 'none'}) and why it wins (count x cross-model spread). Quote ONE exemplar evidence snippet per mechanism from the JSON above. End with a '## Next lever' section arguing what to fix first, and a pointer to run /propose-verify with the JSON as findings.
Then return a 4-sentence summary.`,
  { label: 'synthesize', phase: 'Synthesize', effort: 'high' })

return {
  id: args.id, top_mechanism: top, frequency: freq,
  classified: graded.length, ops_noise: classified.length - graded.length,
  json_path: `${OUTD || '<repo>/scripts/eval/results/autopsy'}/${args.id}.json`,
  md_path: `${OUTD || '<repo>/scripts/eval/results/autopsy'}/${args.id}.md`,
  summary: synth,
}
