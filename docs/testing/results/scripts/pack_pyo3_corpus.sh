#!/usr/bin/env bash
# Corpus packer (#75) — assemble newt-agent's own PyO3 crates into a working-set
# corpus for the ground-truth rig.
#
# The combined size of the packed crates' binding files is the INDEPENDENT
# VARIABLE the rig sweeps: a ~14k-token effective window (what nemotron3 had in
# the incident) overflows around 5 crates, so
#   --crates 4  ≈ 12k tokens  -> fits-window  (the control)
#   --crates 8  ≈ 20k tokens  -> overflow     (the incident-faithful stress)
#
# Usage:
#   pack_pyo3_corpus.sh --repo DIR --out DIR [--crates N]   # default N = all 8
#
# Output:
#   <out>/corpus/<crate>/src/pyo3_module.rs  per-crate PyO3 binding surface
#   <out>/corpus/README.md                   what the corpus is + the prompt
#   <out>/python_surface.json                the REAL importable surface
#                                            (#339/#340: the full newt_agent.*
#                                            module set — every submodule exists
#                                            regardless of how many we pack)
#   ... and a size report (per-crate + cumulative tokens, fits/overflow verdict)
set -euo pipefail

# Pack order = fits-window crates first. crate -> newt_agent submodule, from
# newt-agent-py/src/lib.rs `#[pymodule]` (each `<crate>::pyo3_module::register`).
CRATES=(newt-core newt-data newt-tools newt-inference newt-coder newt-eval newt-acp-worker newt-mcp-server)
declare -A SUBMOD=(
  [newt-core]=core [newt-data]=data [newt-tools]=tools [newt-inference]=inference
  [newt-coder]=coder [newt-eval]=eval [newt-acp-worker]=acp_worker [newt-mcp-server]=mcp
)
WINDOW=14000  # the ~14k effective window the incident overflowed

die() { echo "pack: $*" >&2; exit 1; }

REPO="" OUT="" N=${#CRATES[@]}
while [ $# -gt 0 ]; do
  case "$1" in
    --repo)   REPO=$2; shift 2 ;;
    --out)    OUT=$2; shift 2 ;;
    --crates) N=$2; shift 2 ;;
    *) die "unknown arg: $1" ;;
  esac
done
[ -n "$REPO" ] || die "--repo DIR required"
[ -n "$OUT" ]  || die "--out DIR required"
{ [ "$N" -ge 1 ] && [ "$N" -le "${#CRATES[@]}" ]; } || die "--crates must be 1..${#CRATES[@]}"

CORPUS="$OUT/corpus"
mkdir -p "$CORPUS"

# The REAL importable surface: every submodule exists in the real newt_agent,
# independent of how many crates we pack into the working set. Module-level;
# symbol-level resolution needs the FFI manifest (#74).
modules='["newt_agent"'
for c in "${CRATES[@]}"; do modules="$modules, \"newt_agent.${SUBMOD[$c]}\""; done
modules="$modules]"
printf '{"modules": %s}\n' "$modules" > "$OUT/python_surface.json"

# Corpus = the first N crates (the working-set knob).
total=0
echo "pack: assembling $N-crate corpus from $REPO" >&2
for ((i=0; i<N; i++)); do
  c=${CRATES[$i]}
  f="$REPO/$c/src/pyo3_module.rs"
  [ -f "$f" ] || die "missing $f"
  mkdir -p "$CORPUS/$c/src"
  cp "$f" "$CORPUS/$c/src/pyo3_module.rs"
  b=$(wc -c <"$f"); total=$((total + b))
  printf "  %-18s ~%s tok\n" "$c" "$((b / 4))" >&2
done

cat > "$CORPUS/README.md" <<EOF
# PyO3 crate corpus ($N crates)

Assembled by pack_pyo3_corpus.sh for the ground-truth rig (#75). Each
\`<crate>/src/pyo3_module.rs\` is a PyO3 binding surface the model must read to
write a usable example. The rig prompt:

> create an examples folder and write one python script as an example for each
> and every PyO3 crate in this repository.
EOF

tok=$((total / 4))
if [ "$tok" -gt "$WINDOW" ]; then verdict="OVERFLOW (stress)"; else verdict="fits-window (control)"; fi
echo "pack: $N crates, ~$tok tokens vs ~$WINDOW window -> $verdict" >&2
echo "pack: corpus=$CORPUS surface=$OUT/python_surface.json" >&2
