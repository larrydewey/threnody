#!/usr/bin/env bash
# Builds the library, generates bindings and runs the Python binding test.
#   crates/threnody-ffi/bindings.sh [out-dir]
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:-$root/target/bindings}"
cd "$root"
cargo build -q -p threnody-ffi --features boringtun
lib="$root/target/debug/libthrenody_ffi.so"
[[ -f "$lib" ]] || lib="$root/target/debug/libthrenody_ffi.dylib"
for lang in python kotlin swift; do
    cargo run -q -p threnody-ffi --features "bindgen,boringtun" --bin uniffi-bindgen -- \
        generate --library "$lib" --language "$lang" --out-dir "$out/$lang" --no-format
done
cp "$lib" "$out/python/"
PYTHONPATH="$out/python" python3 "$root/crates/threnody-ffi/tests/python/test_chat.py"
echo "bindings in $out/{python,kotlin,swift}"
