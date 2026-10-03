# Fuzzing

Coverage-guided fuzz targets for every parser that sees untrusted bytes. They need nightly Rust and `cargo install cargo-fuzz`.

```sh
cargo +nightly fuzz run envelope
cargo +nightly fuzz run handshake_respond
cargo +nightly fuzz run handshake_finish
cargo +nightly fuzz run app_message
```

On stable Rust, `crates/threnody-core/src/robustness.rs` runs a seeded mutation test over the same parsers as part of `cargo test`.
