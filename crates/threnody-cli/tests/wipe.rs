//! `threnody wipe`: erasing this device's data directory. (What an erase
//! removes is tested in `threnody-core`'s `wipe` module.)

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_threnody");

/// Runs `threnody --home <home> args` with `stdin`, with the user's own
/// config and data directories out of reach.
fn threnody(home: &Path, args: &[&str], stdin: &str) -> Output {
    let sandbox = home.parent().unwrap().join("sandbox");
    let mut child = Command::new(BIN)
        .arg("--home")
        .arg(home)
        .args(args)
        .env("HOME", &sandbox)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env_remove("THRENODY_HOME")
        .env_remove("THRENODY_PASSPHRASE")
        .env_remove("THRENODY_CONTROL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run threnody");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn ok(o: &Output) -> bool {
    o.status.success()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// A home with an identity and an anonymous identity.
fn used(dir: &Path) -> std::path::PathBuf {
    let home = dir.join("home");
    let o = threnody(&home, &["init", "--no-keyring"], "");
    assert!(ok(&o), "{}", text(&o));
    let o = threnody(&home, &["persona", "new", "p", "--no-keyring"], "");
    assert!(ok(&o), "{}", text(&o));
    home
}

#[test]
fn wipe_asks_and_aborts_without_erase() {
    let t = tempfile::tempdir().unwrap();
    let home = used(t.path());
    let o = threnody(&home, &["wipe"], "no\n");
    assert!(!ok(&o));
    assert!(text(&o).contains("Type 'erase'"), "{}", text(&o));
    assert!(home.join("identity.cbor").exists());
    assert!(home.join("personas").exists());
}

#[test]
fn wipe_yes_leaves_nothing() {
    let t = tempfile::tempdir().unwrap();
    let home = used(t.path());
    std::fs::create_dir_all(t.path().join("home.restore")).unwrap();
    let o = threnody(&home, &["wipe", "--yes"], "");
    assert!(ok(&o), "{}", text(&o));
    assert!(!home.exists());
    assert!(!t.path().join("home.restore").exists());
    let id = threnody(&home, &["id"], "");
    assert!(!ok(&id));
    assert!(text(&id).contains("threnody init` first"), "{}", text(&id));
}

#[test]
fn wipe_with_typed_confirmation() {
    let t = tempfile::tempdir().unwrap();
    let home = used(t.path());
    let o = threnody(&home, &["wipe"], "erase\n");
    assert!(ok(&o), "{}", text(&o));
    assert!(text(&o).contains("Erased."));
    assert!(!home.exists());
}

#[test]
fn wipe_keeps_unrelated_files() {
    let t = tempfile::tempdir().unwrap();
    let home = used(t.path());
    std::fs::write(home.join("notes.txt"), b"mine").unwrap();
    let o = threnody(&home, &["wipe", "--yes"], "");
    assert!(ok(&o), "{}", text(&o));
    assert_eq!(std::fs::read(home.join("notes.txt")).unwrap(), b"mine");
    assert!(!home.join("identity.cbor").exists());
    assert!(!home.join("personas").exists());
    assert!(text(&o).contains("notes.txt"), "{}", text(&o));
}

#[test]
fn init_after_an_interrupted_wipe_works() {
    let t = tempfile::tempdir().unwrap();
    let home = used(t.path());
    std::fs::remove_file(home.join("identity.cbor")).unwrap();
    let o = threnody(&home, &["init", "--no-keyring"], "");
    assert!(ok(&o), "{}", text(&o));
    let o = threnody(&home, &["id"], "");
    assert!(ok(&o), "{}", text(&o));
    assert!(!home.join("personas").exists());
}

#[test]
fn wipe_refuses_while_the_desktop_app_holds_the_home() {
    let t = tempfile::tempdir().unwrap();
    let home = used(t.path());
    let f = std::fs::File::create(home.join("desktop.lock")).unwrap();
    f.try_lock().unwrap();
    let o = threnody(&home, &["wipe", "--yes"], "");
    assert!(!ok(&o));
    assert!(text(&o).contains("stop it first"), "{}", text(&o));
    assert!(home.join("identity.cbor").exists());
    drop(f);
}

#[test]
fn wipe_refuses_with_persona_flag() {
    let t = tempfile::tempdir().unwrap();
    let home = used(t.path());
    let o = threnody(&home, &["--persona", "p", "wipe", "--yes"], "");
    assert!(!ok(&o));
    assert!(text(&o).contains("persona burn"), "{}", text(&o));
    assert!(home.join("identity.cbor").exists());
}
