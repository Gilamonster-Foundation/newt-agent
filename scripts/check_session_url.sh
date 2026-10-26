#!/usr/bin/env bash
# check_session_url.sh — reject a Claude session URL / trailer in a commit
# message. Usage: check_session_url.sh MSG_FILE   (`-` reads stdin)
# Shared by .githooks/commit-msg and the `commit-messages` job in
# .github/workflows/ci.yml. See the hook's header for why.
set -u

# Match the session URL in any form, and the trailer key, case-insensitively.
SESSION_PATTERN='claude\.ai/code/session|^[[:space:]]*Claude-Session[[:space:]]*:'

if grep -qiE "$SESSION_PATTERN" "$1"; then
    cat >&2 <<'MSG'
✗ Claude session URL / trailer found in the commit message.

  A `Claude-Session:` trailer or a claude.ai/code/session link is agent-session
  plumbing — it does not belong in repository history (it leaks into public
  clones/forks forever). Remove it and commit again.

  Keep `Co-Authored-By:` attribution if you like — only the session link is
  banned. Policy: CLAUDE.md → "expunge Claude sessions".
MSG
    exit 1
fi
exit 0
