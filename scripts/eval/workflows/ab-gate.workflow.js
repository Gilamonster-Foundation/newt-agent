export const meta = {
  name: 'ab-gate',
  description: 'A/B lift verification for one lever over two completed sweep.sh arm dirs (baseline vs candidate build). Fisher exact + min-n power advice computed in script code. Verdict: LIFT | NO-LIFT | UNDERPOWERED | UNGRADEABLE | NO-OP-AB.',
  phases: [
    { title: 'Ingest', detail: 'transcribe both arm dirs + grade_spec presence' },
    { title: 'Verdict', detail: 'Fisher exact per cell, deterministic' },
    { title: 'Report', detail: 'verdict.md with the computed tables verbatim' },
  ],
}

// args: { lever: 'slug naming this A/B (required)',
//         baseline_dir, candidate_dir: 'completed sweep dirs (required)',
//         alpha?: 0.05, expected_flip_cells?: ['<model> x <task>', ...],
//         out?: 'verdict dir (default: <candidate_dir>/ab-<lever>)' }
if (!args || !args.lever || !args.baseline_dir || !args.candidate_dir)
  throw new Error('args.lever, args.baseline_dir, args.candidate_dir are required')
const ALPHA = args.alpha || 0.05
const OUT = args.out || `${args.candidate_dir}/ab-${args.lever}`

// ---------- deterministic stats (self-checked; method in README.md)
const logFact = (n) => { let s = 0; for (let i = 2; i <= n; i++) s += Math.log(i); return s }
const logChoose = (n, k) => (k < 0 || k > n ? -Infinity : logFact(n) - logFact(k) - logFact(n - k))
// one-sided (candidate-better) Fisher exact on [candPass a, candFail b; basePass c, baseFail d]
const fisherOneSided = (a, b, c, d) => {
  const r1 = a + b, r2 = c + d, c1 = a + c, N = r1 + r2
  let p = 0
  for (let x = a; x <= Math.min(r1, c1); x++) {
    const y = c1 - x
    if (y < 0 || y > r2) continue
    p += Math.exp(logChoose(r1, x) + logChoose(r2, y) - logChoose(N, c1))
  }
  return Math.min(1, p)
}
{ // self-check: cand 5/0 vs base 1/4 -> p = C(5,1)/C(10,6) = 5/210
  const p = fisherOneSided(5, 0, 1, 4)
  if (Math.abs(p - 5 / 210) > 1e-9) throw new Error(`fisher self-check failed: ${p}`)
}
// smallest per-arm n at which the OBSERVED rates could reach significance
const minNforPower = (pBase, pCand, alpha) => {
  if (pCand <= pBase) return null
  for (let n = 5; n <= 200; n++) {
    const a = Math.round(pCand * n), c = Math.round(pBase * n)
    if (fisherOneSided(a, n - a, c, n - c) < alpha) return n
  }
  return null
}

// ---------- Ingest (same transcription contract as /sweep-analyze)
const ROWS_SCHEMA = {
  type: 'object', additionalProperties: false,
  required: ['done', 'rows', 'meta_git_sha', 'grade_spec_tasks'],
  properties: {
    done: { type: 'boolean' },
    meta_git_sha: { type: 'string' },
    grade_spec_tasks: { type: 'array', items: { type: 'string' }, description: 'tasks appearing in the rows whose newt-eval/cases/<task>/grade_spec.rs exists in the repo' },
    rows: { type: 'array', items: { type: 'object', additionalProperties: false,
      required: ['task', 'mode', 'model', 'behavioral'],
      properties: { task: { type: 'string' }, mode: { type: 'string' }, model: { type: 'string' }, behavioral: { type: 'string' }, details: { type: 'string' } } } },
  },
}
const ingestPrompt = (d) => `Transcribe the sweep at ${d}: read ${d}/sweep.tsv (tab-separated RATCHET rows: cols task,mode,model,behavioral,details at positions 2-6), ${d}/sweep.meta.json (git_sha), check ${d}/DONE exists. Transcribe EVERY well-formed row exactly (skip rows with <6 cols). Then, for each distinct task in the rows, check whether <repo>/newt-eval/cases/<task>/grade_spec.rs exists (resolve the repo root with git rev-parse --show-toplevel from your cwd, or from the sweep dir's location) and list the tasks that HAVE one in grade_spec_tasks.`

phase('Ingest')
const [base, cand] = await parallel([
  () => agent(ingestPrompt(args.baseline_dir), { label: 'ingest:baseline', phase: 'Ingest', schema: ROWS_SCHEMA, effort: 'low' }),
  () => agent(ingestPrompt(args.candidate_dir), { label: 'ingest:candidate', phase: 'Ingest', schema: ROWS_SCHEMA, effort: 'low' }),
])
if (!base || !cand) throw new Error('ingest failed')
if (!base.done || !cand.done) throw new Error(`both arms must be DONE (baseline=${base.done}, candidate=${cand.done}) — a half-run arm biases the comparison`)
if (base.meta_git_sha === cand.meta_git_sha)
  log(`WARNING: both arms record the same git sha (${base.meta_git_sha}) — if the lever is a code change this is a NO-OP A/B`)

phase('Verdict')
const tally = (rows) => {
  const m = new Map()
  for (const r of rows) {
    const k = `${r.task} ${r.mode} ${r.model}`
    const t = m.get(k) || m.set(k, { task: r.task, mode: r.mode, model: r.model, pass: 0, gameable: 0, fail: 0 }).get(k)
    if (r.behavioral === 'PASS') t.pass++
    else if (r.behavioral === 'PASS?gameable') t.gameable++
    else if (r.behavioral === 'FAIL') t.fail++
  }
  return m
}
const bT = tally(base.rows), cT = tally(cand.rows)
const gradeSpecTasks = new Set([...(base.grade_spec_tasks || []), ...(cand.grade_spec_tasks || [])])
const sameSha = base.meta_git_sha === cand.meta_git_sha

const cellVerdicts = []
for (const [k, c] of cT) {
  const b = bT.get(k)
  if (!b) { cellVerdicts.push({ cell: k, verdict: 'NO-BASELINE' }); continue }
  const ungameable = gradeSpecTasks.has(c.task)
  // On an ungameable rung only PASS counts; on a gameable one nothing certifies.
  const cp = c.pass, cn = c.pass + c.gameable + c.fail
  const bp = b.pass, bn = b.pass + b.gameable + b.fail
  const p = fisherOneSided(cp, cn - cp, bp, bn - bp)
  const deltaPasses = cp / Math.max(1, cn) - bp / Math.max(1, bn)
  let verdict
  if (sameSha) verdict = 'NO-OP-AB'
  else if (!ungameable) verdict = 'UNGRADEABLE'
  else if (p < ALPHA && cp / cn > bp / bn) verdict = 'LIFT'
  else if (deltaPasses > 0) verdict = 'UNDERPOWERED'
  else verdict = 'NO-LIFT'
  cellVerdicts.push({
    cell: k, verdict, p_one_sided: +p.toFixed(4),
    candidate: `${cp}/${cn}${c.gameable ? ` (+${c.gameable} gameable)` : ''}`,
    baseline: `${bp}/${bn}${b.gameable ? ` (+${b.gameable} gameable)` : ''}`,
    min_n_for_power: verdict === 'UNDERPOWERED' ? minNforPower(bp / bn, cp / cn, ALPHA) : null,
  })
}
const rank = { LIFT: 0, UNDERPOWERED: 1, 'NO-LIFT': 2, UNGRADEABLE: 3, 'NO-OP-AB': 4, 'NO-BASELINE': 5 }
cellVerdicts.sort((a, b2) => rank[a.verdict] - rank[b2.verdict] || a.cell.localeCompare(b2.cell))
const overall = sameSha ? 'NO-OP-AB'
  : cellVerdicts.some((v) => v.verdict === 'LIFT') ? 'LIFT'
  : cellVerdicts.some((v) => v.verdict === 'UNDERPOWERED') ? 'UNDERPOWERED'
  : cellVerdicts.every((v) => v.verdict === 'UNGRADEABLE') && cellVerdicts.length ? 'UNGRADEABLE'
  : 'NO-LIFT'

const vTable = ['| cell | verdict | candidate | baseline | p (one-sided) | min n/arm for power |', '|---|---|---|---|---|---|',
  ...cellVerdicts.map((v) => `| ${v.cell} | **${v.verdict}** | ${v.candidate || '-'} | ${v.baseline || '-'} | ${v.p_one_sided ?? '-'} | ${v.min_n_for_power ?? '-'} |`)]

// expected-flip scorecard (accountability for the proposal that motivated the lever)
const scorecard = (args.expected_flip_cells || []).map((exp) => {
  // tokens split on whitespace; a bare 'x' separator token is dropped —
  // never split inside a token (models like 'mixtral' contain x)
  const toks = exp.split(/\s+/).filter((t) => t && t.toLowerCase() !== 'x')
  const hit = cellVerdicts.find((v) => toks.every((tok) => v.cell.includes(tok)))
  return `| ${exp} | ${hit ? hit.verdict : 'cell not found'} |`
})

phase('Report')
const summary = await agent(`Write the A/B verdict to ${OUT}/verdict.md (create the dir). RULES: every number comes from the tables below, embedded VERBATIM — never recompute. State the overall verdict in the first line. If UNGRADEABLE cells exist, say the rung needs a hidden grade_spec.rs (/grade-spec-author) before any lift claim. If UNDERPOWERED, state the min n/arm the observed rates would need. Note the caveat that arms were run sequentially, not interleaved (endpoint drift is uncontrolled). At n=5/arm, significance at alpha=${ALPHA} requires roughly 5/5 vs <=1/5 — say so if relevant.

Lever: ${args.lever}
Overall verdict: ${overall}
Baseline arm: ${args.baseline_dir} (sha ${base.meta_git_sha}) | Candidate arm: ${args.candidate_dir} (sha ${cand.meta_git_sha})

## Per-cell verdicts (embed verbatim)
${vTable.join('\n')}
${scorecard.length ? `\n## Expected-flip scorecard (embed verbatim)\n| expected cell | observed verdict |\n|---|---|\n${scorecard.join('\n')}` : ''}

Structure: ## Verdict, ## Per-cell table, ${scorecard.length ? '## Expected-flip scorecard, ' : ''}## Method (Fisher one-sided exact, alpha=${ALPHA}, PASS-only on ungameable rungs), ## Caveats. Write the file, then return a 3-sentence summary.`,
  { label: 'report', phase: 'Report', effort: 'high' })

return { lever: args.lever, overall, cells: cellVerdicts, verdict_path: `${OUT}/verdict.md`, summary }
