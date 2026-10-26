#!/usr/bin/env bash
# check_commit_identity.sh — reject placeholder commit identities.
#
# Usage: check_commit_identity.sh EMAIL [EMAIL...]
# Exits 1 if any EMAIL is empty or its domain has no dot (`x@y`,
# `a@localhost`, `test@example`). Shared by .githooks/commit-msg and the
# `commit-messages` job in .github/workflows/ci.yml.
#
# WHY: GitHub attributes a commit to whichever account has claimed its email.
# A placeholder like `x@y` can belong to an unrelated real account, which then
# shows up as a contributor to this repository. Twenty-two commits on main
# carry one; this keeps any more from landing.
set -u

# The rule, as data: local part, then a domain of two or more dot-separated
# labels. GitHub noreply (…@users.noreply.github.com) and noreply@anthropic.com
# both satisfy it.
VALID_EMAIL='^[^@[:space:]]+@[^@.[:space:]]+(\.[^@.[:space:]]+)+$'

rc=0
for email in "$@"; do
    if ! printf '%s\n' "$email" | grep -qE "$VALID_EMAIL"; then
        printf '✗ placeholder commit identity: <%s>\n' "$email" >&2
        rc=1
    fi
done
if [ "$rc" -ne 0 ]; then
    cat >&2 <<'MSG'
  A commit email needs a real domain (one with a dot). GitHub links an email
  to whichever account claimed it, so a placeholder like `x@y` can credit an
  unrelated person as a contributor. Set a real address, e.g.:
    git config user.email <you>@users.noreply.github.com
MSG
fi
exit "$rc"
