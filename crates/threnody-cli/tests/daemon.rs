//! `threnody run --daemon` and `threnody ctl`: a node run as a service
//! takes commands over its control socket, and its log keeps message
//! text out.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_threnody");

fn threnody(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--home")
        .arg(home)
        .args(args)
        .env_remove("THRENODY_CONTROL")
        .output()
        .expect("run threnody")
}

/// A node run as a service, killed when dropped.
struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn start(home: &Path, extra: &[&str]) -> Self {
        Self::start_at(home, &[], extra, None)
    }

    /// With `pre` before `run` (such as `--persona <id>`), and its control
    /// socket at `control` (`$THRENODY_CONTROL`).
    fn start_at(home: &Path, pre: &[&str], extra: &[&str], control: Option<&Path>) -> Self {
        let log = home.with_extension("log");
        let mut cmd = Command::new(BIN);
        cmd.env_remove("THRENODY_CONTROL");
        if let Some(c) = control {
            cmd.env("THRENODY_CONTROL", c);
        }
        let child = cmd
            .arg("--home")
            .arg(home)
            .args(pre)
            .args([
                "run",
                "--daemon",
                "--listen",
                "127.0.0.1:0",
                "--no-discover",
                "--no-rendezvous",
                "--no-cover",
            ])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start daemon");
        Self { child, log }
    }

    /// Waits for it to stop on its own; false if it doesn't in `secs`.
    fn exits_within(&mut self, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Waits for `needle` in the log.
    fn wait_for(&self, needle: &str) -> String {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            let log = self.log();
            if log.contains(needle) {
                return log;
            }
            assert!(Instant::now() < end, "no {needle:?} in the log:\n{log}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ctl(home: &Path, command: &[&str]) -> (bool, String) {
    let mut args = vec!["ctl"];
    args.extend(command);
    let out = threnody(home, &args);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn init(home: &Path) -> String {
    let out = threnody(home, &["init", "--no-keyring"]);
    assert!(out.status.success(), "{out:?}");
    let who = threnody(home, &["id"]);
    let text = String::from_utf8_lossy(&who.stdout).into_owned();
    text.split_whitespace()
        .find(|w| w.len() > 30 && w.contains('-'))
        .unwrap_or_else(|| panic!("no fingerprint in {text:?}"))
        .to_owned()
}

#[test]
fn a_service_takes_commands_and_keeps_messages_out_of_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    init(&a);
    let b_fp = init(&b);

    let relay = Daemon::start(&a, &[]);
    let log = relay.wait_for("Running as a service");
    let port = log
        .split("Listening on 127.0.0.1:")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .expect("listening port");
    let addr = format!("127.0.0.1:{port}");
    let invite = String::from_utf8_lossy(&threnody(&a, &["invite", &addr, "--no-qr"]).stdout)
        .split_whitespace()
        .find(|w| w.starts_with("threnody://"))
        .expect("invite link")
        .to_owned();

    // Only one node per data directory.
    let again = threnody(&a, &["run", "--daemon", "--listen", "127.0.0.1:0"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already running"));

    let phone = Daemon::start(&b, &["-c", &invite]);
    relay.wait_for(&format!("connected to {b_fp}"));

    let (ok, peers) = ctl(&a, &["/peers"]);
    assert!(ok && peers.contains(&b_fp), "{peers}");
    let (ok, said) = ctl(&a, &["/approve", &b_fp]);
    assert!(ok && said.contains("approved"), "{said}");
    let (ok, said) = ctl(&a, &["/nonsense"]);
    assert!(!ok && said.starts_with("! "), "{said}");

    // A message: in the history `ctl` shows, never in either log.
    let (ok, _) = ctl(&b, &["meet", "at", "the", "harbour"]);
    assert!(ok);
    relay.wait_for("sent a message");
    let (_, history) = ctl(&a, &["/history"]);
    assert!(history.contains("meet at the harbour"), "{history}");
    for log in [relay.log(), phone.log()] {
        assert!(!log.contains("harbour"), "{log}");
        assert!(!log.contains("meet"), "{log}");
    }
}

#[test]
fn ctl_without_a_service_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let out = threnody(dir.path(), &["ctl", "/peers"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no node running as a service"));
}

#[test]
fn nothing_else_writes_to_a_data_directory_a_node_has_open() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    init(&a);
    // Its socket elsewhere, so only the lock tells others it runs.
    let node = Daemon::start_at(&a, &[], &[], Some(&dir.path().join("a.sock")));
    node.wait_for("Running as a service");

    let refused = |args: &[&str]| {
        let out = threnody(&a, args);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(!out.status.success(), "{args:?} ran: {err}");
        assert!(err.contains("already running"), "{args:?}: {err}");
    };
    // It would write its own contacts over these.
    refused(&["revoke", "bob"]);
    refused(&["approve", "bob"]);
    refused(&["name", "bob", "robert"]);
    refused(&["forget", "bob"]);
    refused(&["restore", "--force", "--no-keyring", "some.backup"]);
    refused(&["init", "--force", "--no-keyring"]);
    // A second node, interactive or not.
    refused(&[
        "run",
        "--listen",
        "127.0.0.1:0",
        "--no-discover",
        "--no-rendezvous",
    ]);

    drop(node);
    let out = threnody(&a, &["revoke", "bob"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no contact matches"), "{err}");
}

#[test]
fn a_replaced_identity_starts_with_nothing_of_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let old = init(&a);
    // State sealed under the old seed.
    assert!(threnody(&a, &["invite", "127.0.0.1:1", "--no-qr"]).status.success());
    std::fs::create_dir_all(a.join("downloads")).unwrap();
    std::fs::write(a.join("downloads/photo.jpg"), b"jpeg").unwrap();

    let out = threnody(&a, &["init", "--force", "--no-keyring"]);
    assert!(out.status.success(), "{out:?}");
    let new = threnody(&a, &["id"]);
    assert!(!String::from_utf8_lossy(&new.stdout).contains(&old));
    let invite = threnody(&a, &["invite", "127.0.0.1:1", "--no-qr"]);
    assert!(invite.status.success(), "{invite:?}");
    let run = threnody(
        &a,
        &[
            "run",
            "--listen",
            "127.0.0.1:0",
            "--no-discover",
            "--no-rendezvous",
        ],
    );
    assert!(run.status.success(), "{run:?}");
    assert!(a.join("downloads/photo.jpg").exists());
}

#[test]
fn an_anonymous_identity_burns_itself_when_its_time_is_up() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    init(&a);
    let made = threnody(
        &a,
        &["persona", "new", "brief", "--burn-after", "4s", "--no-keyring"],
    );
    assert!(made.status.success(), "{made:?}");
    let made = String::from_utf8_lossy(&made.stdout).into_owned();
    let id = made
        .split_whitespace()
        .nth(2)
        .unwrap_or_else(|| panic!("no persona id in {made:?}"))
        .to_owned();

    let mut node = Daemon::start_at(
        &a,
        &["--persona", &id],
        &[],
        Some(&dir.path().join("p.sock")),
    );
    node.wait_for("Running as a service");
    // Not under its running node.
    let burn = threnody(&a, &["persona", "burn", &id]);
    assert!(!burn.status.success());
    assert!(
        String::from_utf8_lossy(&burn.stderr).contains("already running"),
        "{burn:?}"
    );
    assert!(a.join("personas").join(&id).exists());

    assert!(node.exits_within(20), "still running past its time");
    assert!(node.log().contains("time is up"), "{}", node.log());
    assert!(!a.join("personas").join(&id).exists());
    let list = threnody(&a, &["persona", "list"]);
    assert!(
        String::from_utf8_lossy(&list.stdout).contains("No anonymous identities"),
        "{list:?}"
    );
}
