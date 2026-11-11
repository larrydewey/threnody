//! The app's own preferences, as `key=value` lines in
//! `$XDG_CONFIG_HOME/threnody/desktop.conf`. Every privacy setting reads
//! as on unless the user turned it off.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub const COVER: &str = "cover_traffic";
/// Under cover traffic, messages wait for the next beat (on unless off).
pub const PACE: &str = "pace_messages";
pub const ONION: &str = "onion_first";
pub const STRIP: &str = "strip_metadata";
/// Photos and voice and video messages from our other devices download by
/// themselves (on unmetered networks).
pub const AUTO_FETCH: &str = "fetch_from_own_devices";
pub const REACH: &str = "reach_internet";
pub const PRIVATE_NOTIFICATIONS: &str = "private_notifications";
pub const BACKGROUND: &str = "run_in_background";
/// Starting hidden at login (`crate::autostart`): on unless switched off.
pub const START_AT_LOGIN: &str = "start_at_login";
pub const TIMER: &str = "default_timer";
pub const VOLUNTEERS: &str = "use_volunteers";
/// The always-on relay: the fingerprint of a contact (a relay node run on
/// a server) the main identity keeps a session with. None unless chosen.
pub const ANCHOR: &str = "always_on_relay";
/// WireGuard tunnels to mutually approved contacts: off unless chosen,
/// since bringing the interface up needs root. Peers are kept, encrypted,
/// across restarts while on.
pub const TUNNELS: &str = "wireguard_tunnels";
/// A handle others can type to find us (Appendix S): its name and
/// version. None unless chosen.
pub const HANDLE_NAME: &str = "handle_name";
pub const HANDLE_VERSION: &str = "handle_version";
pub const SEND_READ: &str = "send_read_receipts";
pub const SEND_TYPING: &str = "send_typing";
/// GIF search through GIPHY: off until the user agrees to GIPHY seeing
/// their searches and IP address.
pub const GIPHY: &str = "giphy_search";
/// A GIPHY API key the user entered (it wins over the build's).
pub const GIPHY_KEY: &str = "giphy_api_key";
/// The offer to set up app lock was shown (once per identity).
pub const LOCK_OFFERED: &str = "lock_offered";
/// Which conversations the list shows: "all", "chats" or "groups".
pub const LIST_TAB: &str = "list_tab";
/// Bluetooth LE (advertising a private beacon, dialing contacts nearby)
/// and joining contacts' Wi-Fi Direct groups: off unless chosen. They
/// show this computer's presence on radios; they aren't protections.
pub const BLUETOOTH: &str = "bluetooth";
pub const WIFI_DIRECT: &str = "wifi_direct";
/// Volunteering as a relay at these public addresses, and running a
/// relay directory at this one: none unless chosen.
pub const VOLUNTEER_ADDRS: &str = "volunteer_addrs";
pub const DIRECTORY_ADDR: &str = "directory_addr";
/// The directory reviews relays before listing them (a protection
/// against Sybil relays, so on unless turned off).
pub const DIRECTORY_REVIEW: &str = "directory_review";

/// Cover interval on unmetered networks, and on metered ones.
pub const COVER_UNMETERED_MS: u32 = 2_000;
pub const COVER_METERED_MS: u32 = 10_000;

/// Disappearing-message timers offered, in seconds (`None`: off).
pub const TIMERS: &[(&str, Option<u32>)] = &[
    ("Off", None),
    ("30 seconds", Some(30)),
    ("5 minutes", Some(300)),
    ("1 hour", Some(3_600)),
    ("1 day", Some(86_400)),
    ("1 week", Some(604_800)),
    ("4 weeks", Some(2_419_200)),
];

pub fn timer_label(secs: Option<u32>) -> String {
    TIMERS.iter().find(|(_, s)| *s == secs).map_or_else(
        || format!("{} s", secs.unwrap_or(0)),
        |(l, _)| (*l).to_owned(),
    )
}

pub struct Settings {
    path: PathBuf,
    values: BTreeMap<String, String>,
    /// Erased: nothing is written any more.
    gone: bool,
}

impl Settings {
    pub fn load() -> Self {
        let path = config_dir().join("desktop.conf");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        Self::parse(path, &text)
    }

    fn parse(path: PathBuf, text: &str) -> Self {
        let values = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        Self {
            path,
            values,
            gone: false,
        }
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self {
            path,
            values: BTreeMap::new(),
            gone: false,
        }
    }

    /// Forgets every setting and deletes the file ("Erase all Threnody
    /// data"); later changes aren't saved, so they can't bring it back.
    pub fn erase(&mut self) {
        self.values.clear();
        self.gone = true;
        let _ = std::fs::remove_file(&self.path);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }

    fn save(&self) {
        if self.gone {
            return;
        }
        let body: String = self
            .values
            .iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.path, body);
    }

    /// A switch; protective ones default to on.
    pub fn flag(&self, key: &str) -> bool {
        self.values.get(key).is_none_or(|v| v != "false")
    }

    pub fn set_flag(&mut self, key: &str, on: bool) {
        self.values.insert(key.to_owned(), on.to_string());
        self.save();
    }

    /// A switch that stays off until the user turns it on.
    pub fn opted_in(&self, key: &str) -> bool {
        self.values.get(key).is_some_and(|v| v == "true")
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    pub fn set_text(&mut self, key: &str, value: &str) {
        // One line per key: a newline would split it.
        let value: String = value.chars().filter(|c| !c.is_control()).collect();
        self.values.insert(key.to_owned(), value.trim().to_owned());
        self.save();
    }

    pub fn number(&self, key: &str) -> Option<u32> {
        self.values.get(key)?.parse().ok()
    }

    pub fn set_number(&mut self, key: &str, n: Option<u32>) {
        match n {
            Some(n) => self.values.insert(key.to_owned(), n.to_string()),
            None => self.values.remove(key),
        };
        self.save();
    }
}

fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("threnody");
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".config/threnody")
}

/// The CLI's data directory: `$THRENODY_HOME`, else `$XDG_DATA_HOME/threnody`.
pub fn default_home() -> PathBuf {
    if let Some(h) = std::env::var_os("THRENODY_HOME") {
        return h.into();
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(x).join("threnody");
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".local/share/threnody")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_erase_removes_the_file_and_stops_saving() {
        let dir = std::env::temp_dir().join(format!("threnody-settings-{}", std::process::id()));
        let path = dir.join("threnody").join("desktop.conf");
        let mut s = Settings::at(path.clone());
        s.set_flag(COVER, false);
        assert!(path.exists());
        s.erase();
        assert!(!path.exists());
        s.set_flag(COVER, false);
        s.set_text(HANDLE_NAME, "me");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nearby_and_roles_default_off_review_on() {
        let s = Settings::parse(PathBuf::from("/nonexistent/desktop.conf"), "");
        assert!(!s.opted_in(BLUETOOTH));
        assert!(!s.opted_in(WIFI_DIRECT));
        assert_eq!(s.text(VOLUNTEER_ADDRS), None);
        assert_eq!(s.text(DIRECTORY_ADDR), None);
        assert!(s.flag(DIRECTORY_REVIEW));
        let s = Settings::parse(
            PathBuf::from("/nonexistent/desktop.conf"),
            "bluetooth=true\n",
        );
        assert!(s.opted_in(BLUETOOTH));
        assert!(!s.opted_in(WIFI_DIRECT));
    }
}
