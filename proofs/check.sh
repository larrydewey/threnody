#!/usr/bin/env bash
# Proves every Tamarin model in this directory; fails unless all lemmas verify.
set -euo pipefail
cd "$(dirname "$0")"

# Some distributions (e.g. Arch) install Maude's prelude without exporting MAUDE_LIB.
if [[ -z "${MAUDE_LIB:-}" && -f /usr/share/maude/prelude.maude ]]; then
    export MAUDE_LIB=/usr/share/maude
fi

status=0
for model in *.spthy; do
    # The derivation (well-formedness) check times out after 5 s by default,
    # which slower machines (CI runners) can hit on sealed.spthy.
    out=$(tamarin-prover --derivcheck-timeout="${DERIVCHECK_TIMEOUT:-120}" --prove "$model" 2>&1)
    results=$(grep -E '^\s+\S+ \((all-traces|exists-trace)\): ' <<<"$out" || true)
    if [[ -z "$results" ]]; then
        echo "$model: tamarin produced no results" >&2
        tail -20 <<<"$out" >&2
        status=1
        continue
    fi
    echo "== $model"
    echo "$results"
    if grep -qvE ': verified \(' <<<"$results"; then
        status=1
    fi
    if grep -q 'WARNING' <<<"$out"; then
        echo "$model: tamarin warned:" >&2
        grep -A12 'WARNING' <<<"$out" >&2
        status=1
    fi
done
exit $status
