//! Output into a pipe whose reader has gone (`threnody id | head -0`)
//! ends quietly, as with other command-line tools.

use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_threnody");

#[test]
fn a_closed_pipe_ends_output_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("a");
    let init = Command::new(BIN)
        .arg("--home")
        .arg(&home)
        .args(["init", "--no-keyring"])
        .env_remove("THRENODY_PASSPHRASE")
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    for _ in 0..5 {
        let mut child = Command::new(BIN)
            .arg("--home")
            .arg(&home)
            .args(["persona", "list"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Closed before it writes.
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{err}");
        assert!(out.status.success(), "{err}");
    }
}
