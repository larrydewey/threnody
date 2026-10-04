//! Generates Kotlin / Swift / Python bindings from the compiled library:
//! `cargo run -p threnody-ffi --features bindgen --bin uniffi-bindgen -- generate --library <lib> --language <lang> --out-dir <dir>`
fn main() {
    uniffi::uniffi_bindgen_main();
}
