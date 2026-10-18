#!/usr/bin/env bash
# Builds threnody-ffi for iOS devices and simulators, generates the Swift
# bindings and packages ThrenodyFFI.xcframework.
#   apps/ios/build-xcframework.sh [--debug]
# Needs macOS with Xcode and the Rust targets aarch64-apple-ios,
# aarch64-apple-ios-sim and x86_64-apple-ios.
# Calls (WebRTC) and tunnels (boringtun) are left out: neither feature is
# built for iOS yet, so the bindings have neither.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
profile=release
for arg in "$@"; do
    case "$arg" in
        --debug) profile=debug ;;
        *) echo "unknown option $arg" >&2; exit 2 ;;
    esac
done
flag=()
[[ $profile == release ]] && flag=(--release)
export IPHONEOS_DEPLOYMENT_TARGET=17.0

out="$here/Generated"
rm -rf "$out"
mkdir -p "$out/headers" "$out/sim" "$here/Threnody/Generated"

cd "$root"
for t in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    # Only the static library: the app links it into its binary.
    cargo rustc -q -p threnody-ffi --lib --locked ${flag[@]+"${flag[@]}"} --target "$t" --crate-type staticlib
done
lib=libthrenody_ffi.a
lipo -create \
    "target/aarch64-apple-ios-sim/$profile/$lib" \
    "target/x86_64-apple-ios/$profile/$lib" \
    -output "$out/sim/$lib"

# Bindings from a host build with the same (default) features, so their
# checksums match the iOS libraries.
cargo build -q -p threnody-ffi --lib --locked
cargo run -q -p threnody-ffi --locked --features bindgen --bin uniffi-bindgen -- generate \
    --library target/debug/libthrenody_ffi.dylib --language swift --no-format --out-dir "$out/swift"
cp "$out/swift/threnody_ffiFFI.h" "$out/headers/"
cp "$out/swift/threnody_ffiFFI.modulemap" "$out/headers/module.modulemap"
cp "$out/swift/threnody_ffi.swift" "$here/Threnody/Generated/"

xcodebuild -create-xcframework \
    -library "target/aarch64-apple-ios/$profile/$lib" -headers "$out/headers" \
    -library "$out/sim/$lib" -headers "$out/headers" \
    -output "$out/ThrenodyFFI.xcframework" >/dev/null
echo "built $out/ThrenodyFFI.xcframework"
