#!/usr/bin/env bash
# Builds the Threnody Android app and optionally installs it.
#   apps/android/build.sh [--emulator] [--release] [--install]
# Phones need arm64-v8a; --emulator also builds x86_64 for the emulator.
# --release builds the release APK, signed if THRENODY_KEYSTORE (and its
# passwords) are set, as in CI; otherwise the debug APK is built.
# Needs: Android SDK + NDK (ANDROID_HOME, default ~/Android/Sdk), JDK 17/21
# (JAVA_HOME) and the aarch64-linux-android Rust target. Gradle comes from the wrapper.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
sdk="${ANDROID_HOME:-$HOME/Android/Sdk}"
ndk="${ANDROID_NDK_HOME:-$(ls -d "$sdk"/ndk/* | sort -V | tail -1)}/toolchains/llvm/prebuilt/linux-x86_64/bin"
install=0
release=0
abis=(arm64-v8a)
for arg in "$@"; do
    case "$arg" in
        --install) install=1 ;;
        --emulator) abis+=(x86_64) ;;
        --release) release=1 ;;
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
    cargo build -q --release -p threnody-ffi --target "$triple" --features boringtun
    mkdir -p "$here/app/src/main/jniLibs/$abi"
    cp "target/$triple/release/libthrenody_ffi.so" "$here/app/src/main/jniLibs/$abi/"
done
cargo build -q -p threnody-ffi --features boringtun
cargo run -q -p threnody-ffi --features bindgen --bin uniffi-bindgen -- generate \
    --library target/debug/libthrenody_ffi.so --language kotlin --no-format \
    --out-dir "$here/app/src/main/java"

cd "$here"
echo "sdk.dir=$sdk" > local.properties
# The wrapper pins the Gradle version the Android plugin needs.
if (( release )); then
    ./gradlew -q assembleRelease
    apk="$(ls "$here"/app/build/outputs/apk/release/app-release*.apk | head -1)"
else
    ./gradlew -q assembleDebug
    apk="$here/app/build/outputs/apk/debug/app-debug.apk"
fi
echo "built $apk"
if (( install )); then
    "$sdk/platform-tools/adb" install -r "$apk"
fi
