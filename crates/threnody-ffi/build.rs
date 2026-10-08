//! On Android with the `calls` feature, keeps WebRTC's `JNI_OnLoad` and the
//! native methods its Java classes call (`android-jni.ld`: the linker would
//! otherwise drop those nothing in Rust references) in the library, and
//! exported (`android-jni.map`).
//! The app loads the library with `System.loadLibrary`, so Android runs
//! WebRTC's `JNI_OnLoad`, which hands WebRTC the `JavaVM`. That way no Rust
//! code of ours needs `unsafe` to receive it.

fn main() {
    println!("cargo:rerun-if-changed=android-jni.map");
    println!("cargo:rerun-if-changed=android-jni.ld");
    let android = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "android");
    let calls = std::env::var_os("CARGO_FEATURE_CALLS").is_some();
    if android && calls {
        let dir =
            std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
        // A linker script of EXTERN(...) only: like -u for each symbol.
        println!(
            "cargo:rustc-cdylib-link-arg={}",
            dir.join("android-jni.ld").display()
        );
        println!(
            "cargo:rustc-cdylib-link-arg=-Wl,--version-script={}",
            dir.join("android-jni.map").display()
        );
    }
}
