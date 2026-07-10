---
name: repository-roadmap
description: Create, read, and maintain a ROADMAP.md execution plan whose ground truth is GitHub issues — phased/waved delivery toward a version milestone, coordinable across multiple repos from a central starting repo.
when_to_use: When asked to plan a release or milestone, check how far along a roadmap actually is, update or re-scope a ROADMAP.md, or coordinate work spanning multiple repos from a central repo. Also when a repo's ROADMAP.md cites issue numbers and you need live progress rather than trusting the document.
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["gh", "git"] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Repository Roadmap

A `ROADMAP.md` at the repo root is an **execution plan toward a named
version** (e.g. v0.8.0). Its defining property:

> **GitHub issues are the state; the document is the map.**
> Every work item carries a tracking issue/PR number. When the document and
> GitHub disagree, GitHub wins. The document may be stale; the issues cannot.

This makes the roadmap safe to read months later: an agent reconciles the
map against live issue state instead of trusting prose.

## Reading a roadmap (checking progress)

1. Extract every issue/PR number from the document.
2. Query live state — never assume the doc is current:

```bash
gh issue view <N> --repo <owner>/<repo> --json number,title,state,closedAt
# bulk:
gh issue list --repo <owner>/<repo> --search "<N1> <N2> …" --state all \
  --json number,title,state
```

3. A **phase is done** when every issue in it is closed or carries a comment
   explicitly re-scoping it out of the milestone.
4. Report progress per phase: `closed / total`, plus any issue whose state
   contradicts the document (flag those for a roadmap-update PR).

## Writing a roadmap

Required structure:

1. **Header** — current version → target version, creation date, pointer to
   this skill.
2. **Ground truth protocol** — the reconciliation commands, verbatim, and
   the "GitHub wins" rule.
3. **Source plans** — links to the merged design docs each phase came from.
   A roadmap sequences existing plans; it does not replace them.
4. **Phases (or waves)** — ordered tables of `Item | Issue | Notes`, each
   with an **Exit:** line stating the observable completion condition.
   Dependencies are stated as `Blocked by #N` on the item.
5. **Release criteria** — what closes the milestone (tests, changelog,
   version-bump PR).
6. **Deliberately out** — what was considered and excluded, with the
   condition under which it re-enters. Prevents silent re-litigation.

Rules:

- **No item without a number.** If a work item has no issue, file the issue
  first, then add the row. Design-only PRs get a separate implementation
  tracking issue (the PR closes; the work remains visible).
- **Conditionals live inside issues.** "Do X only if metric Y shows a gap"
  goes in issue X's body, so closing the issue records either the adoption
  or the measured-inert verdict. The roadmap only sequences it.
- **Update in the same PR that changes reality.** Re-scoping an issue out of
  the milestone = one PR containing the issue comment link and the roadmap
  edit.
- Keep phases small enough that each item is one reviewable PR.

## Multi-repo coordination

A roadmap in a **central repo** (e.g. `newt-agent`) may sequence work in
satellite repos. Conventions:

- Cross-repo items use the full reference: `owner/repo#N` — `gh issue view
  N --repo owner/repo` resolves them the same way.
- The central ROADMAP.md is the **only** sequencing authority; satellite
  repos may keep their own ROADMAP.md for internal ordering, but its header
  must link back to the central roadmap and name which central phase it
  serves.
- Phase exit criteria in the central roadmap may span repos ("`owner/lib#12`
  released to crates.io **and** `owner/app#34` consumes it").
- When a satellite repo lacks an issue tracker or is external (upstream
  projects), track the work as an issue in the central repo whose body links
  the external PR — the number the roadmap cites must always be one `gh`
  call away from live state.

## Maintaining

- On every milestone-relevant merge, check whether a roadmap row's state
  line needs no edit (normally none — state lives in GitHub). Edit the
  document only for **structural** change: items added/removed/re-phased,
  exit criteria changed, milestone re-targeted.
- When the milestone ships: move the roadmap to
  `docs/roadmaps/ROADMAP-<version>.md` (history), and start the next
  version's ROADMAP.md at the root.
