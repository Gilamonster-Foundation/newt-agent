# Rows under the transcript: one owner for where the next line lands

> **Status:** Implemented (first step) · **Owner:** hartsock · **Builds on:**
> the tty arbiter (`newt-core/src/tty/arbiter.rs`: `Terminal`, `RegionLease`,
> `Region::Rows`, `OnCollision`), `newt-tui/src/inline_viewport.rs`, and the
> real-PTY tier (`tests/pty`, `newt-tui/src/interaction_view_pty_test.rs`).

## The failure

Live runs on 2026-10-06 and 2026-10-07 left a tall blank band in the
scroller three times: twice after a permission frame, once after a tool's
live-output viewport. All three have one cause. Four surfaces each decided
"where does the transcript end" on their own, and each was right until the
first surprise:

- A frame leased the bottom rows (`lease_bottom_rows`). When something held
  them, the lease was *shifted* above the holder and the frame was parked
  rows below the transcript. On close the frame cleared from the cursor down
  and left it there, so the next committed line landed near the bottom of
  the screen under a band of blank rows. Three frames shared the shape: the
  permission modal, the clarification modal, and every operator panel.
- The live viewport paints from the cursor down and rewinds with
  `MoveUp(Σ painted rows)`; a stale width or a stray line under-counts the
  move, lands inside the frame, and strands the rows above it. (Its own doc
  names the hazard. That surface is the second step; see below.)

The arbiter already decided which rows each surface *held*. It owned no row
of the transcript, so nobody did.

## What the arbiter owns now

**One mint for rows under the transcript.** `Terminal::lease_below_cursor`
queries the cursor once, with stdin's other reader quiesced so the `ESC[6n`
reply cannot be stolen (#1950: this thread already owns stdin through a
prompt window, or the turn watcher's read token is held for the query), then
applies `place_below_cursor`, a pure rule:

- with the cursor at column zero, the frame opens on that row; otherwise
  it opens on the next row, leaving the partial transcript line intact;
- an occupied cursor row reserves one screen row outside the frame, so an
  oversized request is clamped to the remaining rows (a one-row screen
  cannot grant such a placed lease);
- if it would run off the bottom, the transcript scrolls up by the deficit
  (newlines from the bottom row, the cockpit presenter's byte plan), unless
  somebody holds rows, since the scroll would move theirs too;
- a request that would land on a holder, or scroll one, is contested and
  follows the caller's `OnCollision`: refused, taken as declared, or shifted
  to the nearest free rows above the holder, with the cursor still returned
  to where it was.

The rows are handed out blank (per-row `Clear(CurrentLine)`, never `ESC[J`,
which runs to the end of the screen through the holder below) with the
cursor parked on the first of them, and the lease records where the cursor
goes back to, with its column preserved and its row adjusted for scrolling.
For an uncontested partial line, output after closing appends to that line.
A contested `Shift` still may paint over transcript rows above a bottom holder;
returning the cursor does not restore that overwritten text. This change does
not claim transcript preservation for that inherited placement path.

**One release.** `RegionLease::drop` for a lease minted this way erases
exactly its rows and ends with `MoveTo(return_to)`. A lease from
`lease_region` keeps the documented "Drop does not erase": those rows are
the holder's to clean up. The type doc says which is which and why.

**Frames open where the lease says.** `inline_terminal` opens a placed lease
as `Viewport::Fixed`, so ratatui neither queries the cursor nor emits opening
newlines; a bottom-anchored lease (the prompt editor's) keeps `Inline`.
`terminal.clear()` and `terminal.resize()` are never called on a placed
frame: for a `Fixed` viewport ratatui implements both with `ESC[J` per row.
A resize closes and re-opens the frame instead, so the lease returns the
cursor and the next mint measures the resized screen from there.

**No guess.** When the terminal does not answer, when stdin's other reader
could not be quiesced, or when the rows are refused, the mint returns `None`
and the caller takes `lease_bottom_rows` and the behaviour it had before.

## What went

`InlineGuard`'s cursor-relative `Clear(FromCursorDown)`, and the
clarification modal's open-time `clear()` and close-time erase. Each was a
per-surface answer to the question the arbiter now answers once.

`inline_terminal`'s park-the-cursor rule for a shifted `Inline` lease stays,
because the fallback still opens `Inline` viewports: measured on the panel
tier, a panel re-opened after a grow beside a holder on a silent terminal
ran ratatui's opening newlines from the old panel's last row and scrolled
the holder's rows away (`a_panel_beside_a_row_holder_stays_open_at_its_
granted_height`, red with the rule deleted, green with it kept).

## Evidence

- `arbiter_tests.rs`: the placement rule, table-tested without a terminal.
- `interaction_view_pty_test.rs`, `after_a_frame_…` and siblings: a real
  pty whose cursor reports are answered from a replay of what the child
  painted (`Pump`), a frame opened under a short transcript, beside a holder
  of the bottom rows, near the bottom (scrolling), and shifted above a
  holder; each asserts the committed line's row and the cursor's. A fifth
  keeps the terminal silent and asserts the frame still opens, answers, and
  hands the terminal back. The partial-line regression writes a sentinel with
  no trailing newline and checks the replayed screen while the frame is open
  and after it closes, both with and without scrolling.

## Second step: the live viewport

`live_spill.rs` takes its rows from `Terminal::lease_below_cursor_quiet`,
the same query and placement rule with none of the arbiter's bytes: the
renderer paints and erases through its own writer (its unit tier replays
those bytes on a screen model, which the arbiter's stream would bypass) and
holds the `RegionLease` for the rows. A frame is placed once per generation
and repainted in place on every chunk; a resize, an expand, or a frame left
by another generation erases what is up and places afresh. At unchanged
screen dimensions the erase is `blank_rows` plus a move to the placement's
return point, the arbiter's own release on this writer. A height-only resize
invalidates the absolute rows
even when the collapsed frame size is unchanged. Cleanup follows the cursor
the terminal retains under the frame, uses bounded `Clear(CurrentLine)`
steps, restores the relative transcript return point, and re-places before
repainting. Finish rechecks both dimensions even without another chunk.

Width-reflow cleanup still uses `MoveUp` plus `Clear(FromCursorDown)`:
a reflowing terminal has re-wrapped its rows, the placement no longer names
them, and the cursor the terminal kept parked under the frame is the better
guide. `abandon` still touches nothing: the rows go back to the arbiter
silently and the residue stays.

The query is bounded: it waits up to 300 ms for the turn watcher's stdin
token, and a terminal that lets the query time out is not asked again for a
minute (a frame on every tool chunk cannot pay two seconds each time); in
both cases the frame is painted nowhere rather than somewhere guessed.
A queued quiet query reserves the next stdin turn: the polling watcher cannot
reacquire ahead of it and starve the bounded wait. This also applies to first
placement, before any existing frame or screen-height comparison is involved.

If placement refuses after erasing a visible frame, that generation retains a
replacement request. Later watcher ticks retry even though geometry is now
unchanged; the request survives repeated refusals and clears on success or
when the viewport is finished or discarded. The watcher checks this without
blocking on the output lock, which the waiting painter may hold. A first
placement refusal still belongs to the caller's plain-output fallback and
does not schedule a late frame over that output.

`cursor_relative_region_painters_are_the_declared_holdouts` retains its
`live_spill.rs` entry for that width-reflow limitation: holding a lease does
not make a cursor-relative clear-to-screen-end bounded by the leased rows.
The cockpit presenter is unchanged: it owns its pty and its block's top row
already is the transcript's end.

A grant must fit the complete live frame plus its blank parking row. The
partial-transcript rule can clamp a full-screen expanded request by one row;
the live painter declines that grant before emitting any scroll or paint bytes.
It leaves no frame to erase on finish or abandon and does not schedule a
placement retry for that undersized grant. Collapsing the view can fit again.
This reuses the no-frame fallback instead of introducing a second layout pass
that would have to keep plain and styled file-change rows in agreement.
