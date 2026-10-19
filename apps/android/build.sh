#!/usr/bin/env bash
# Builds the Threnody Android app and optionally installs it.
#   apps/android/build.sh [--emulator] [--release] [--install] [--native-only]
# Phones need arm64-v8a; --emulator also builds x86_64 for the emulator.
# --release builds the release APK, signed if THRENODY_KEYSTORE (and its
# passwords) are set, as in CI; otherwise the debug APK is built.
# --native-only stops before Gradle (the F-Droid recipe runs its own).
# Needs: Android SDK + NDK (ANDROID_HOME, default ~/Android/Sdk), JDK 17/21
# (JAVA_HOME) and the aarch64-linux-android Rust target. Gradle comes from the wrapper.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
sdk="${ANDROID_HOME:-$HOME/Android/Sdk}"
# WebRTC's build (calls) finds the NDK through ANDROID_NDK_HOME.
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$(ls -d "$sdk"/ndk/* | sort -V | tail -1)}"
ndk="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
install=0
release=0
native_only=0
abis=(arm64-v8a)
for arg in "$@"; do
    case "$arg" in
        --install) install=1 ;;
        --emulator) abis+=(x86_64) ;;
        --release) release=1 ;;
        --native-only) native_only=1 ;;
        *) echo "unknown option $arg" >&2; exit 2 ;;
    esac
done

cd "$root"
# Keep build-machine paths out of the library (panic locations, C asserts),
# so the same source builds the same bytes anywhere.
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
remap="--remap-path-prefix=$root=. --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup"
cmap="-ffile-prefix-map=$root=. -ffile-prefix-map=$cargo_home=/cargo"
rm -rf "$here/app/src/main/jniLibs"
for abi in "${abis[@]}"; do
    case "$abi" in
        arm64-v8a) triple=aarch64-linux-android ;;
        x86_64) triple=x86_64-linux-android ;;
    esac
    var="${triple//-/_}"
    export "CARGO_TARGET_${var^^}_LINKER=$ndk/${triple}35-clang"
    export "CC_${var}=$ndk/${triple}35-clang"
    export "CXX_${var}=$ndk/${triple}35-clang++"
    export "AR_${var}=$ndk/llvm-ar"
    export "CARGO_TARGET_${var^^}_RUSTFLAGS=$remap"
    export "CFLAGS_${var}=$cmap" "CXXFLAGS_${var}=$cmap"
    cargo build -q --locked --release -p threnody-ffi --target "$triple" --features boringtun,calls
    mkdir -p "$here/app/src/main/jniLibs/$abi"
    cp "target/$triple/release/libthrenody_ffi.so" "$here/app/src/main/jniLibs/$abi/"
done
# WebRTC's Java half (audio devices), from the prebuilt the build fetched.
jar="$(find target -path '*-release/libwebrtc.jar' -print -quit)"
[[ -n "$jar" ]] || { echo "libwebrtc.jar not found under target/" >&2; exit 1; }
mkdir -p "$here/app/libs"
cp "$jar" "$here/app/libs/libwebrtc.jar"
cargo build -q --locked -p threnody-ffi --features boringtun
cargo run -q --locked -p threnody-ffi --features bindgen --bin uniffi-bindgen -- generate \
    --library target/debug/libthrenody_ffi.so --language kotlin --no-format \
    --out-dir "$here/app/src/main/java"
(( native_only )) && exit 0

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
