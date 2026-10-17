//! `threnody backup` and `threnody restore`: a new device takes over a
//! lost one's identity from a passphrase-sealed file. (What a backup
//! carries is tested in `threnody-core`'s `backup` module.)

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_threnody");
const PW: &str = "a long backup passphrase";

fn threnody(home: &Path, args: &[&str], pw: &str) -> Output {
    Command::new(BIN)
        .arg("--home")
        .arg(home)
        .args(args)
        .env("THRENODY_BACKUP_PASSPHRASE", pw)
        .env_remove("THRENODY_PASSPHRASE")
        .env_remove("THRENODY_CONTROL")
        .output()
        .expect("run threnody")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn a_backup_restores_the_identity_on_another_device() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (dir.path().join("old"), dir.path().join("new"));
    let file = dir.path().join("me.backup");
    let file = file.to_str().unwrap();

    assert!(
        threnody(&old, &["init", "--no-keyring"], PW)
            .status
            .success()
    );
    let id = stdout(&threnody(&old, &["id"], PW));

    let short = threnody(&old, &["backup", file], "too short");
    assert!(!short.status.success());
    assert!(stderr(&short).contains("at least"), "{}", stderr(&short));

    let made = threnody(&old, &["backup", file], PW);
    assert!(made.status.success(), "{}", stderr(&made));
    // No overwriting by accident.
    assert!(!threnody(&old, &["backup", file], PW).status.success());

    assert!(
        threnody(&new, &["init", "--no-keyring"], PW)
            .status
            .success()
    );
    let refused = threnody(&new, &["restore", file, "--no-keyring"], PW);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("--force"), "{}", stderr(&refused));

    let wrong = threnody(
        &new,
        &["restore", file, "--force", "--no-keyring"],
        "not the passphrase",
    );
    assert!(
        stderr(&wrong).contains("wrong passphrase"),
        "{}",
        stderr(&wrong)
    );
    assert_ne!(
        stdout(&threnody(&new, &["id"], PW)),
        id,
        "a failed restore changes nothing"
    );

    let done = threnody(&new, &["restore", file, "--force", "--no-keyring"], PW);
    assert!(done.status.success(), "{}", stderr(&done));
    assert_eq!(stdout(&threnody(&new, &["id"], PW)), id);
}
