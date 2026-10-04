#!/usr/bin/env bash
# Builds the Threnody Android app (arm64) and optionally installs it.
#   apps/android/build.sh [--install]
# Needs: Android SDK + NDK (ANDROID_HOME, default ~/Android/Sdk), JDK 17/21
# (JAVA_HOME) and the aarch64-linux-android Rust target. Gradle comes from the wrapper.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
sdk="${ANDROID_HOME:-$HOME/Android/Sdk}"
ndk="$(ls -d "$sdk"/ndk/* | sort -V | tail -1)/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ndk/aarch64-linux-android35-clang"
export CC_aarch64_linux_android="$ndk/aarch64-linux-android35-clang"
export AR_aarch64_linux_android="$ndk/llvm-ar"

cd "$root"
cargo build -q --release -p threnody-ffi --target aarch64-linux-android
cargo build -q -p threnody-ffi
cargo run -q -p threnody-ffi --features bindgen --bin uniffi-bindgen -- generate \
    --library target/debug/libthrenody_ffi.so --language kotlin --no-format \
    --out-dir "$here/app/src/main/java"
mkdir -p "$here/app/src/main/jniLibs/arm64-v8a"
cp target/aarch64-linux-android/release/libthrenody_ffi.so "$here/app/src/main/jniLibs/arm64-v8a/"

cd "$here"
echo "sdk.dir=$sdk" > local.properties
# The wrapper pins the Gradle version the Android plugin needs.
./gradlew -q assembleDebug
apk="$here/app/build/outputs/apk/debug/app-debug.apk"
echo "built $apk"
if [[ "${1:-}" == "--install" ]]; then
    "$sdk/platform-tools/adb" install -r "$apk"
fi
