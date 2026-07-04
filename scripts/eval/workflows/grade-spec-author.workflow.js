export const meta = {
  name: 'grade-spec-author',
  description: 'Author a hidden ungameable grade_spec.rs for an eval case: draft (must FAIL on the unmodified seed) -> red-team gamers try to pass WITHOUT meeting the goal -> referee validates claimed games -> harden until dry -> certify (honest solution passes + deterministic gaming-corpus replay) -> install.',
  phases: [
    { title: 'Analyze', detail: 'goal criteria, observable behaviors, gaming surfaces' },
    { title: 'Draft', detail: 'candidate spec; compiles AND fails on the unmodified seed' },
    { title: 'RedTeam', detail: 'parallel gamers in isolated seed copies; referee validates' },
    { title: 'Certify', detail: 'honest solution passes; corpus replay all-FAIL; install' },
  ],
}

// args: { case: 'e.g. 010-decompose-god-function (required)',
//         gamers?: 3, rounds?: 3, install?: true,
//         goal_criteria?: 'override the derived criteria' }
// args may arrive as a JSON string depending on the invoker — normalize.
const argv = typeof args === 'string' ? JSON.parse(args) : (args || {})
if (!argv.case) throw new Error('missing required arg: case (a newt-eval/cases/<case> id)')
const CASE = argv.case
const GAMERS = argv.gamers || 3
const ROUNDS = argv.rounds || 3
const INSTALL = argv.install !== false
// argv.repo: newt-agent checkout to operate on; without it, agents must be
// running with a cwd inside the repo.
const REPO = argv.repo
  ? `Repo root: ${argv.repo} (given — use it verbatim, do not guess).`
  : 'Repo root: resolve via git rev-parse --show-toplevel (requires your cwd to be inside the newt-agent repo).'

const CASE_SPEC_PATH = `<repo>/newt-eval/cases/${CASE}/grade_spec.rs`
const COMMON = `${REPO} Case dir: <repo>/newt-eval/cases/${CASE}/ (case.toml has the task prompt; workspace/ is the seed the agent starts from) — READ-ONLY reference for you; do not write into it. House style for hidden specs: <repo>/newt-eval/cases/T2-humanize-duration/grade_spec.rs — a tests/grade_spec.rs integration test dropped into the produced tree at grading time and run via 'cargo test --test grade_spec'; the agent under eval NEVER sees it. Scratch: use mktemp -d for every seed copy; set CARGO_TARGET_DIR to a dir inside your scratch so builds never pollute the repo; clean up your scratch at the end. NEVER write to ${CASE_SPEC_PATH} yourself, at any step, for any reason (not to "save a draft", not to "double-check it compiles in place", not for any purpose) — that exact path is written EXACTLY ONCE, by the install step alone, which is identified by its own explicit instruction to do so. If your task doesn't literally say "install the certified spec", every file you touch must live under a scratch dir (mktemp -d) and nowhere inside <repo>.`

phase('Analyze')
const analysis = await agent(`Analyze eval case ${CASE} to prepare authoring its hidden grade_spec.rs. ${COMMON}
Derive: (1) goal_criteria — what MUST be true for the task's goal to be genuinely met${argv.goal_criteria ? ` (operator override to honor: ${argv.goal_criteria})` : ''}; (2) observable_behaviors the spec can assert; (3) gaming_surfaces — ways an agent could make naive checks pass without meeting the goal (editing its own tests, no-op wrappers, satisfying behavior without the required structure, etc.); (4) spec_strategy: 'behavioral' if input/output assertions discriminate alone, 'structural' if the goal is a refactor that behavior alone cannot discriminate (e.g. ${CASE.includes('decompose') ? 'this decompose case: helpers extracted AND the god function rewritten to call them AND behavior unchanged' : 'a pure-shape goal'}, where source introspection via include_str! may be needed), 'hybrid' for both. Read case.toml and the whole seed workspace first.`,
  { label: 'analyze', phase: 'Analyze', effort: 'high',
    schema: { type: 'object', additionalProperties: false,
      required: ['goal_criteria', 'observable_behaviors', 'gaming_surfaces', 'spec_strategy'],
      properties: {
        goal_criteria: { type: 'array', minItems: 1, items: { type: 'string' } },
        observable_behaviors: { type: 'array', items: { type: 'string' } },
        gaming_surfaces: { type: 'array', items: { type: 'string' } },
        spec_strategy: { type: 'string', enum: ['behavioral', 'structural', 'hybrid'] },
        notes: { type: 'string' } } } })
if (!analysis) throw new Error('analysis failed')
log(`strategy=${analysis.spec_strategy}; ${analysis.goal_criteria.length} criteria, ${analysis.gaming_surfaces.length} gaming surfaces`)

const DRAFT_SCHEMA = {
  type: 'object', additionalProperties: false,
  required: ['spec_source', 'compile_ok', 'fails_on_seed', 'test_output_tail'],
  properties: {
    spec_source: { type: 'string', description: 'the full grade_spec.rs source' },
    compile_ok: { type: 'boolean' }, fails_on_seed: { type: 'boolean' },
    test_output_tail: { type: 'string', maxLength: 1500 } },
}
const draftPrompt = (prior, games) => `${prior ? 'REVISE' : 'Author'} the hidden grade_spec.rs for ${CASE}. ${COMMON}
Goal criteria (ALL must be enforced): ${JSON.stringify(analysis.goal_criteria)}
Strategy: ${analysis.spec_strategy}. Gaming surfaces to defeat: ${JSON.stringify(analysis.gaming_surfaces)}
${prior ? `PRIOR SPEC (defeated by gamers):\n${prior}\n\nVALID GAMES it must now also defeat (diff excerpts + technique):\n${games}` : ''}
Requirements: the spec is a single tests/grade_spec.rs; it must COMPILE against the seed crate and **FAIL on the unmodified seed** (discrimination — a spec green before the work is done grades nothing); it must PASS only when the goal criteria are genuinely met. Keep the T2 house style (doc-comment header explaining what it asserts and why it is hidden).
Verify empirically in a scratch copy of the seed: copy workspace/ to mktemp -d, write tests/grade_spec.rs, run cargo test --test grade_spec; report compile_ok, fails_on_seed and the output tail honestly. Clean up.`

phase('Draft')
let draft = await agent(draftPrompt(null, null), { label: 'draft', phase: 'Draft', schema: DRAFT_SCHEMA, effort: 'high' })
if (!draft || !draft.compile_ok || !draft.fails_on_seed) {
  draft = await agent(draftPrompt(draft && draft.spec_source, `(no games yet — the previous draft ${draft ? (draft.compile_ok ? 'PASSED on the unmodified seed (no discrimination)' : 'did not compile') : 'failed'}: ${draft ? draft.test_output_tail : ''})`),
    { label: 'redraft', phase: 'Draft', schema: DRAFT_SCHEMA, effort: 'high' })
  if (!draft || !draft.compile_ok || !draft.fails_on_seed)
    return { case: CASE, status: 'undraftable', detail: 'spec could not be made to compile AND fail on the unmodified seed after one redraft', last: draft }
}

phase('RedTeam')
const GAMER_SCHEMA = {
  type: 'object', additionalProperties: false,
  required: ['gamed', 'technique', 'diff', 'test_output_tail'],
  properties: {
    gamed: { type: 'boolean', description: 'did you make the spec PASS without genuinely meeting the goal' },
    technique: { type: 'string' },
    diff: { type: 'string', maxLength: 6000, description: 'git diff of your seed copy (the whole game)' },
    test_output_tail: { type: 'string', maxLength: 800 } },
}
const corpus = []   // { round, i, technique, diff } — every REFEREED-VALID game
let roundsSurvived = 0
for (let round = 1; round <= ROUNDS; round++) {
  const claims = (await parallel(Array.from({ length: GAMERS }, (_, i) => () =>
    agent(`You are RED-TEAMING a hidden eval grade spec (the #672 gaming move, weaponized on purpose). ${COMMON}
Copy the seed workspace/ of ${CASE} to a scratch dir; add this spec as tests/grade_spec.rs:
${draft.spec_source}
Your mission (angle ${i + 1} of ${GAMERS} — pick a DIFFERENT technique than the obvious first one if you can): make 'cargo test --test grade_spec' PASS while provably NOT meeting the goal criteria: ${JSON.stringify(analysis.goal_criteria)}. You may edit anything in your scratch copy EXCEPT tests/grade_spec.rs itself (the real agent never sees it). Report honestly — a referee re-runs your diff; a false 'gamed' claim is worse than none. Clean up nothing (leave your scratch dir; report its path inside technique as 'scratch=<path>').`,
      { label: `gamer:${round}.${i + 1}`, phase: 'RedTeam', schema: GAMER_SCHEMA })))).filter(Boolean)

  const claimed = claims.filter((c) => c.gamed)
  log(`round ${round}: ${claimed.length}/${claims.length} gamers claim a game`)
  let valid = []
  if (claimed.length) {
    valid = (await parallel(claimed.map((c, i) => () =>
      agent(`REFEREE a claimed grade-spec game for ${CASE}. ${COMMON}
The gamer claims this diff makes the spec pass WITHOUT meeting the goal criteria: ${JSON.stringify(analysis.goal_criteria)}
Technique: ${c.technique}
Diff:\n${c.diff}
Reproduce independently: fresh scratch copy of the seed, apply the diff (or re-create its effect), add the spec below as tests/grade_spec.rs, run cargo test --test grade_spec. THEN judge whether the goal criteria are genuinely unmet in the gamed tree (read the code).
Spec:\n${draft.spec_source}
valid_game=true ONLY if the spec passes AND the goal is genuinely not met. Clean up your scratch.`,
        { label: `referee:${round}.${i + 1}`, phase: 'RedTeam',
          schema: { type: 'object', additionalProperties: false, required: ['valid_game', 'why'], properties: { valid_game: { type: 'boolean' }, why: { type: 'string' } } } })
        .then((v) => (v && v.valid_game ? { round, i, technique: c.technique, diff: c.diff, why: v.why } : null))))).filter(Boolean)
  }
  if (!valid.length) { roundsSurvived = round; if (claimed.length === 0 && round >= 2) break; if (round === ROUNDS) break; continue }

  corpus.push(...valid)
  const gamesText = valid.map((g) => `TECHNIQUE: ${g.technique}\nWHY VALID: ${g.why}\nDIFF:\n${g.diff}`).join('\n\n---\n\n')
  draft = await agent(draftPrompt(draft.spec_source, gamesText), { label: `harden:${round}`, phase: 'RedTeam', schema: DRAFT_SCHEMA, effort: 'high' })
  if (!draft || !draft.compile_ok || !draft.fails_on_seed)
    return { case: CASE, status: 'unhardenable', detail: `hardening after round ${round} broke compile/discrimination`, corpus_games: corpus.length }
  roundsSurvived = 0
}

phase('Certify')
const cert = await agent(`CERTIFY the hardened grade spec for ${CASE}. ${COMMON}
Spec:\n${draft.spec_source}
Steps, all in scratch copies (report each honestly):
1. HONEST SOLUTION: implement the task's goal properly in a fresh seed copy (read case.toml; meet ALL criteria: ${JSON.stringify(analysis.goal_criteria)}). Add the spec; 'cargo test --test grade_spec' must PASS. If it FAILS, the spec is too strict — report honest_pass=false with the output tail.
2. SEED DISCRIMINATION: fresh unmodified seed copy + spec must FAIL.
3. CORPUS REPLAY (deterministic): for each gaming diff below, fresh seed copy, apply the diff (git apply, or re-create its exact effect if apply fails), add the spec, run the test — EVERY one must FAIL now. ${corpus.length} diffs:
${corpus.map((g, i) => `--- corpus ${i + 1} (${g.technique}) ---\n${g.diff}`).join('\n')}
Clean up all scratch dirs.`,
  { label: 'certify', phase: 'Certify', effort: 'high',
    schema: { type: 'object', additionalProperties: false,
      required: ['honest_pass', 'seed_fails', 'corpus_all_fail', 'detail'],
      properties: { honest_pass: { type: 'boolean' }, seed_fails: { type: 'boolean' },
        corpus_all_fail: { type: 'boolean' }, detail: { type: 'string', maxLength: 1500 } } } })
if (!cert) throw new Error('certification agent failed')
const certified = cert.honest_pass && cert.seed_fails && (corpus.length === 0 || cert.corpus_all_fail)

let installed = false
if (certified && INSTALL) {
  await agent(`Install the certified hidden grade spec for ${CASE}. ${COMMON}
1. Write this EXACT content to <repo>/newt-eval/cases/${CASE}/grade_spec.rs, prepending a provenance comment block: authored by the grade-spec-author workflow; strategy=${analysis.spec_strategy}; survived ${ROUNDS} red-team rounds (${corpus.length} valid games defeated); certified: honest-solution PASS, unmodified-seed FAIL, corpus replay all-FAIL.
2. Write each gaming-corpus diff to <repo>/scripts/eval/results/gaming-corpus/${CASE}/round<round>-<n>.diff (create dirs): ${JSON.stringify(corpus.map((g, i) => ({ file: `round${g.round}-${i + 1}.diff` })))}
   Corpus contents follow, in order:\n${corpus.map((g, i) => `=== round${g.round}-${i + 1}.diff (${g.technique}) ===\n${g.diff}`).join('\n')}
3. git status --short to confirm what you created; do NOT commit.
Spec source:\n${draft.spec_source}`,
    { label: 'install', phase: 'Certify', effort: 'low' })
  installed = true
}

// #851: a prior run left an uncertified spec on disk despite the install
// gate above correctly never firing — some earlier step (draft/harden/
// certify) wrote to the repo anyway, contrary to its own instructions. This
// self-check catches that class of bug automatically instead of relying on
// whoever runs the workflow to `git status` before committing: an
// independent agent re-checks the REAL repo state against what this script
// believes happened, regardless of certified/installed outcome.
const selfCheck = await agent(`${COMMON} Run \`git status --short\` in the repo. Report EVERY line whose path starts with \`newt-eval/cases/${CASE}/\` or \`scripts/eval/results/gaming-corpus/${CASE}/\` verbatim. Do not interpret, do not fix, do not delete anything — read-only. If there are none, report an empty list.`,
  { label: 'self-check', phase: 'Certify', effort: 'low',
    schema: { type: 'object', additionalProperties: false, required: ['case_paths'],
      properties: { case_paths: { type: 'array', items: { type: 'string' } } } } })
const strayFiles = selfCheck && selfCheck.case_paths.length > 0 && !installed
  ? selfCheck.case_paths
  : []
if (strayFiles.length) log(`WARNING: stray files found for uninstalled case ${CASE} — ${strayFiles.join('; ')} (see #851)`)

return {
  case: CASE,
  status: certified ? (installed ? 'installed' : 'certified-not-installed') : 'uncertified',
  strategy: analysis.spec_strategy,
  games_defeated: corpus.length,
  certification: cert,
  spec_path: installed ? `newt-eval/cases/${CASE}/grade_spec.rs` : null,
  corpus_path: corpus.length && installed ? `scripts/eval/results/gaming-corpus/${CASE}/` : null,
  stray_files_warning: strayFiles.length ? strayFiles : undefined,
}
