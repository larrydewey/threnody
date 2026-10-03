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
    out=$(tamarin-prover --prove "$model" 2>&1)
    results=$(grep -E '^\s+\S+ \((all-traces|exists-trace)\): ' <<<"$out" || true)
    if [[ -z "$results" ]]; then
        echo "$model: tamarin produced no results" >&2
        tail -20 <<<"$out" >&2
        status=1
        continue
    fi
    echo "== $model"
    echo "$results"
    if grep -qvE ': verified \(' <<<"$results" || grep -q 'WARNING' <<<"$out"; then
        status=1
    fi
done
exit $status
