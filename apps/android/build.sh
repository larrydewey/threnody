#!/usr/bin/env bash
# Builds the Threnody Android app and optionally installs it.
#   apps/android/build.sh [--emulator] [--install]
# Phones need arm64-v8a; --emulator also builds x86_64 for the emulator.
# Needs: Android SDK + NDK (ANDROID_HOME, default ~/Android/Sdk), JDK 17/21
# (JAVA_HOME) and the aarch64-linux-android Rust target. Gradle comes from the wrapper.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
sdk="${ANDROID_HOME:-$HOME/Android/Sdk}"
ndk="$(ls -d "$sdk"/ndk/* | sort -V | tail -1)/toolchains/llvm/prebuilt/linux-x86_64/bin"
install=0
abis=(arm64-v8a)
for arg in "$@"; do
    case "$arg" in
        --install) install=1 ;;
        --emulator) abis+=(x86_64) ;;
        *) echo "unknown option $arg" >&2; exit 2 ;;
    esac
done

cd "$root"
rm -rf "$here/app/src/main/jniLibs"
for abi in "${abis[@]}"; do
    case "$abi" in
        arm64-v8a) triple=aarch64-linux-android ;;
        x86_64) triple=x86_64-linux-android ;;
    esac
    var="${triple//-/_}"
    export "CARGO_TARGET_${var^^}_LINKER=$ndk/${triple}35-clang"
    export "CC_${var}=$ndk/${triple}35-clang"
    export "AR_${var}=$ndk/llvm-ar"
    cargo build -q --release -p threnody-ffi --target "$triple"
    mkdir -p "$here/app/src/main/jniLibs/$abi"
    cp "target/$triple/release/libthrenody_ffi.so" "$here/app/src/main/jniLibs/$abi/"
done
cargo build -q -p threnody-ffi
cargo run -q -p threnody-ffi --features bindgen --bin uniffi-bindgen -- generate \
    --library target/debug/libthrenody_ffi.so --language kotlin --no-format \
    --out-dir "$here/app/src/main/java"

cd "$here"
echo "sdk.dir=$sdk" > local.properties
# The wrapper pins the Gradle version the Android plugin needs.
./gradlew -q assembleDebug
apk="$here/app/build/outputs/apk/debug/app-debug.apk"
echo "built $apk"
if (( install )); then
    "$sdk/platform-tools/adb" install -r "$apk"
fi
