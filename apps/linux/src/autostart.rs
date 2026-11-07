//! Starting at login, so contacts can reach this computer before Threnody
//! is opened (the Android app starts its background service after a
//! reboot for the same reason). An XDG autostart entry runs
//! `threnody-desktop --background`, which opens the identity and keeps the
//! window hidden, as when it was closed to the background. It is on
//! unless switched off ([`crate::settings::START_AT_LOGIN`]); each start
//! writes the entry again, so it follows the binary if it moved.

use std::path::PathBuf;

/// The command-line flag the entry passes.
pub const FLAG: &str = "--background";

const NAME: &str = "org.threnody.Threnody.desktop";

/// `$XDG_CONFIG_HOME/autostart/org.threnody.Threnody.desktop`.
fn entry() -> PathBuf {
    gtk::glib::user_config_dir().join("autostart").join(NAME)
}

/// Starts Threnody at login from now on, or stops doing so. A build run
/// from a source tree (`target/debug`, `target/release`) never adds the
/// entry: it would outlive the build.
pub fn set(on: bool) -> std::io::Result<()> {
    let path = entry();
    let exe = std::env::current_exe()?;
    if !on || from_source_tree(&exe) {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        };
    }
    let text = contents(&exe.display().to_string());
    if std::fs::read_to_string(&path).is_ok_and(|t| t == text) {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
    std::fs::write(&path, text)
}

/// Takes the entry away (erasing everything).
pub fn remove() {
    let _ = std::fs::remove_file(entry());
}

fn from_source_tree(exe: &std::path::Path) -> bool {
    let mut up = exe.ancestors().skip(1);
    matches!(
        (up.next().and_then(|p| p.file_name()), up.next().and_then(|p| p.file_name())),
        (Some(profile), Some(target)) if target == "target" && (profile == "debug" || profile == "release")
    )
}

/// The entry for `exe`, quoted as the Desktop Entry spec asks.
fn contents(exe: &str) -> String {
    let quoted: String = exe
        .chars()
        .flat_map(|c| match c {
            '"' | '`' | '$' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Threnody\n\
         Comment=Keeps you reachable after you log in\n\
         Exec=\"{quoted}\" {FLAG}\n\
         Icon=org.threnody.Threnody\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_quotes_the_path_and_starts_hidden() {
        let c = contents("/home/a b/.local/bin/threnody-desktop");
        assert!(c.contains("Exec=\"/home/a b/.local/bin/threnody-desktop\" --background\n"));
        assert!(contents("/x/$y\"z").contains("Exec=\"/x/\\$y\\\"z\" --background"));
        assert!(c.starts_with("[Desktop Entry]\nType=Application\n"));
    }

    #[test]
    fn builds_in_a_source_tree_are_told_apart() {
        use std::path::Path;
        assert!(from_source_tree(Path::new(
            "/src/threnody/target/debug/threnody-desktop"
        )));
        assert!(from_source_tree(Path::new(
            "/src/threnody/target/release/threnody-desktop"
        )));
        assert!(!from_source_tree(Path::new(
            "/home/a/.local/bin/threnody-desktop"
        )));
        assert!(!from_source_tree(Path::new("/usr/bin/threnody-desktop")));
    }
}
