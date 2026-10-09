//! The app's own preferences, as `key=value` lines in
//! `$XDG_CONFIG_HOME/threnody/desktop.conf`. Every privacy setting reads
//! as on unless the user turned it off.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub const COVER: &str = "cover_traffic";
pub const ONION: &str = "onion_first";
pub const STRIP: &str = "strip_metadata";
pub const REACH: &str = "reach_internet";
pub const PRIVATE_NOTIFICATIONS: &str = "private_notifications";
pub const BACKGROUND: &str = "run_in_background";
pub const TIMER: &str = "default_timer";
pub const VOLUNTEERS: &str = "use_volunteers";
pub const SEND_READ: &str = "send_read_receipts";
pub const SEND_TYPING: &str = "send_typing";
/// GIF search through GIPHY: off until the user agrees to GIPHY seeing
/// their searches and IP address.
pub const GIPHY: &str = "giphy_search";
/// A GIPHY API key the user entered (it wins over the build's).
pub const GIPHY_KEY: &str = "giphy_api_key";

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
}

impl Settings {
    pub fn load() -> Self {
        let path = config_dir().join("desktop.conf");
        let values = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        Self { path, values }
    }

    fn save(&self) {
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
