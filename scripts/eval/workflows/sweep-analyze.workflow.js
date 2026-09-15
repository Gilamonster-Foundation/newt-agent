export const meta = {
  name: 'sweep-analyze',
  description: 'Honest statistical readout of a sweep.sh dir: per-cell pass rates with Wilson 95% CIs, ungradable/underpowered flags, rung pooling, optional baseline diff. Numbers computed in script code, never by an agent.',
  phases: [
    { title: 'Ingest', detail: 'transcribe sweep.tsv/grid/meta into structured rows' },
    { title: 'Stats', detail: 'deterministic aggregation in script code' },
    { title: 'Report', detail: 'REPORT.md with the computed tables embedded verbatim' },
  ],
}

// args: { dir: 'path to a sweep dir (required)', baseline?: 'path to a prior
//         sweep dir', partial?: bool (analyze a still-growing sweep), }
// args may arrive as a JSON string depending on the invoker — normalize.
const argv = typeof args === 'string' ? JSON.parse(args) : (args || {})
if (!argv.dir) throw new Error('missing required arg: dir (sweep directory)')
const DIR = argv.dir

// ---------- deterministic stats (self-checked; tool-neutral method in README.md)
const wilson = (pass, n, z = 1.96) => {
  if (!n) return { lo: 0, hi: 1 }
  const p = pass / n, z2 = z * z, den = 1 + z2 / n
  const center = (p + z2 / (2 * n)) / den
  const half = (z * Math.sqrt((p * (1 - p)) / n + z2 / (4 * n * n))) / den
  return { lo: Math.max(0, center - half), hi: Math.min(1, center + half) }
}
{ // self-check: 4/5 -> Wilson 95% ~ [0.376, 0.964]
  const w = wilson(4, 5)
  if (Math.abs(w.lo - 0.376) > 0.01 || Math.abs(w.hi - 0.964) > 0.01)
    throw new Error(`wilson self-check failed: ${JSON.stringify(w)}`)
}
const pct = (x) => `${(100 * x).toFixed(0)}%`
const ci = (pass, n) => { const w = wilson(pass, n); return `[${pct(w.lo)}, ${pct(w.hi)}]` }

// ---------- Ingest
const ROWS_SCHEMA = {
  type: 'object', additionalProperties: false,
  required: ['done', 'rows', 'grid', 'meta_git_sha'],
  properties: {
    done: { type: 'boolean', description: 'does DONE exist in the dir' },
    meta_git_sha: { type: 'string' },
    rows: { type: 'array', items: { type: 'object', additionalProperties: false,
      required: ['task', 'mode', 'model', 'behavioral', 'details'],
      properties: {
        task: { type: 'string' }, mode: { type: 'string' }, model: { type: 'string' },
        behavioral: { type: 'string' }, details: { type: 'string' },
        dur_s: { type: 'integer' },
      } } },
    grid: { type: 'array', items: { type: 'object', additionalProperties: false,
      required: ['task', 'mode', 'model', 'trials'],
      properties: { task: { type: 'string' }, mode: { type: 'string' }, model: { type: 'string' }, trials: { type: 'integer' } } } },
  },
}
const ingestPrompt = (d) => `Transcribe the sweep at ${d} into structured data. Read ${d}/sweep.tsv (tab-separated; rows start with RATCHET; cols: RATCHET, task, mode, model, behavioral, details, timestamp, dur_s, params), ${d}/sweep.grid (task<TAB>mode<TAB>model<TAB>trials), ${d}/sweep.meta.json, and check whether ${d}/DONE exists. Transcribe EVERY well-formed RATCHET row exactly — do not summarize, dedup, or fix anything; skip only rows with fewer than 6 columns. behavioral is the exact col-5 string (PASS, FAIL, UNGRADABLE(reason), legacy PASS?gameable, ...).`

phase('Ingest')
const ingests = await parallel([
  () => agent(ingestPrompt(DIR), { label: 'ingest:current', phase: 'Ingest', schema: ROWS_SCHEMA, effort: 'low' }),
  ...(argv.baseline ? [() => agent(ingestPrompt(argv.baseline), { label: 'ingest:baseline', phase: 'Ingest', schema: ROWS_SCHEMA, effort: 'low' })] : []),
])
const cur = ingests[0]
const base = argv.baseline ? ingests[1] : null
if (!cur) throw new Error('ingest failed')
if (!cur.done && !argv.partial) throw new Error(`${DIR} has no DONE marker — the sweep is still growing. Pass partial:true to analyze anyway (results will be stamped PARTIAL).`)

phase('Stats')
// per-cell aggregation — all numbers computed here, in code
const key = (r) => `${r.task}|${r.mode}|${r.model}`
const cells = new Map()
for (const g of cur.grid) cells.set(key(g), { task: g.task, mode: g.mode, model: g.model, target: g.trials, pass: 0, ungradable: 0, fail: 0, other: 0 })
for (const r of cur.rows) {
  const c = cells.get(key(r)) || cells.set(key(r), { task: r.task, mode: r.mode, model: r.model, target: 0, pass: 0, ungradable: 0, fail: 0, other: 0 }).get(key(r))
  if (r.behavioral === 'PASS') c.pass++
  // #2317: UNGRADABLE(..) and legacy PASS?gameable graded nothing canonically
  else if (/^UNGRADABLE\(/.test(r.behavioral) || r.behavioral === 'PASS?gameable') c.ungradable++
  else if (r.behavioral === 'FAIL') c.fail++
  else c.other++
}
const cellList = [...cells.values()].map((c) => {
  const n = c.pass + c.fail // graded trials only
  return { ...c, n, underpowered: n < 5, ungradable_cell: c.ungradable > 0 }
})
// rung pooling (task x mode across models) — file-regressions.sh semantics
// (only exact PASS counts as pass there, and here)
const rungs = new Map()
for (const c of cellList) {
  const k = `${c.task}/${c.mode}`
  const r = rungs.get(k) || rungs.set(k, { rung: k, pass: 0, n: 0, ungradable: 0 }).get(k)
  r.pass += c.pass; r.ungradable += c.ungradable; r.n += c.n
}
// baseline delta per rung
let deltas = []
if (base) {
  const bRungs = new Map()
  for (const r of base.rows) {
    const k = `${r.task}/${r.mode}`
    const b = bRungs.get(k) || bRungs.set(k, { pass: 0, n: 0 }).get(k)
    if (r.behavioral === 'PASS') b.pass++
    if (r.behavioral === 'PASS' || r.behavioral === 'FAIL') b.n++
  }
  deltas = [...rungs.values()].map((r) => {
    const b = bRungs.get(r.rung)
    if (!b || !b.n) return { rung: r.rung, delta: null }
    return { rung: r.rung, cur: `${r.pass}/${r.n}`, base: `${b.pass}/${b.n}`, delta: r.n && b.n ? (r.pass / r.n - b.pass / b.n) : null }
  }).filter((d) => d.delta !== null)
}

// markdown tables — built here so the report embeds them VERBATIM
const cellTable = ['| task | mode | model | pass | ungradable | fail | n | pass-rate | Wilson 95% | flags |', '|---|---|---|---|---|---|---|---|---|---|']
for (const c of cellList.sort((a, b) => (a.task + a.mode + a.model).localeCompare(b.task + b.mode + b.model))) {
  const flags = [c.underpowered ? 'UNDERPOWERED(n<5)' : '', c.ungradable_cell ? 'UNGRADABLE-ROWS' : '', c.n < c.target ? `INCOMPLETE(${c.n}/${c.target})` : ''].filter(Boolean).join(' ')
  cellTable.push(`| ${c.task} | ${c.mode} | ${c.model} | ${c.pass} | ${c.ungradable} | ${c.fail} | ${c.n} | ${c.n ? pct(c.pass / c.n) : '-'} | ${c.n ? ci(c.pass, c.n) : '-'} | ${flags} |`)
}
const rungTable = ['| rung (task/mode) | pass/n (PASS only) | ungradable (out of n) | Wilson 95% |', '|---|---|---|---|']
for (const r of [...rungs.values()].sort((a, b) => a.rung.localeCompare(b.rung)))
  rungTable.push(`| ${r.rung} | ${r.pass}/${r.n} | ${r.ungradable} | ${r.n ? ci(r.pass, r.n) : '-'} |`)
const deltaTable = deltas.length
  ? ['| rung | baseline | current | delta |', '|---|---|---|---|', ...deltas.map((d) => `| ${d.rung} | ${d.base} | ${d.cur} | ${(100 * d.delta).toFixed(0)}pp |`)]
  : []

const failRows = cur.rows.filter((r) => r.behavioral === 'FAIL')
const failDirs = failRows.map((r) => (r.details.match(/dir=(\S+)/) || [])[1]).filter(Boolean)
const ungradableDirs = cur.rows.filter((r) => /^UNGRADABLE\(/.test(r.behavioral) || r.behavioral === 'PASS?gameable').map((r) => (r.details.match(/dir=(\S+)/) || [])[1]).filter(Boolean)

phase('Report')
const stamp = cur.done ? '' : ' **PARTIAL — sweep not DONE; do not cite these as results.**'
const report = await agent(`Write the sweep analysis report to ${DIR}/REPORT.md. RULES: every number in the report must come from the tables below, embedded VERBATIM — do not recount, recompute, or invent any statistic. Per-cell claims with the UNDERPOWERED flag must carry that stamp in prose. UNGRADABLE rows (and legacy PASS?gameable rows) must never be described as passes and are not in n; UNGRADABLE-ROWS cells are missing grading material or ran no checks, so say which and that the claim about them is incomplete.${stamp}

Sweep: ${DIR} (git sha ${cur.meta_git_sha}; graded rows ${cur.rows.length}; DONE=${cur.done})
${argv.baseline ? `Baseline: ${argv.baseline}` : ''}

## Per-cell table (embed verbatim)
${cellTable.join('\n')}

## Per-rung pooled table (embed verbatim)
${rungTable.join('\n')}
${deltaTable.length ? `\n## Baseline delta (embed verbatim)\n${deltaTable.join('\n')}` : ''}

You MAY additionally read up to 5 of these FAIL run dirs' .plan.log files for qualitative color (quote what you cite): ${failDirs.slice(0, 5).join(', ') || '(none)'}

Structure: ## Summary (3-5 sentences, honest, lead with what is and is not statistically supported), ## Per-cell results, ## Per-rung pooled, ${deltaTable.length ? '## Baseline comparison, ' : ''}## Failure pointers (list the FAIL and ungradable run dirs for /crew-autopsy), ## Caveats (n<5 cells, ungradable rows, partial status). Write the file, then return a 5-sentence summary of what the data shows.`,
  { label: 'report', phase: 'Report', effort: 'high' })

return {
  done: cur.done,
  cells: cellList,
  rungs: [...rungs.values()],
  deltas,
  underpowered_cells: cellList.filter((c) => c.underpowered).map((c) => `${c.task}/${c.mode}/${c.model} (n=${c.n})`),
  ungradable_cells: cellList.filter((c) => c.ungradable_cell).map((c) => `${c.task}/${c.mode}/${c.model}`),
  fail_run_dirs: failDirs,
  ungradable_run_dirs: ungradableDirs,
  report_path: `${DIR}/REPORT.md`,
  summary: report,
}
