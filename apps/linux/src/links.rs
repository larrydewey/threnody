//! Which Threnody links to act on: clicked, scanned and copied invites,
//! device link codes and directory links, as `threnody_ffi::classify_link`
//! recognises them. Pure, so the rules are tested without a display.

use threnody_ffi::{LinkInfo, LinkKind};

/// Where a link came from: it decides how a bad one is reported.
/// (Copied links are offered on their own terms: see `App::check_clipboard`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkFrom {
    /// Clicked in another app, or given on the command line.
    Opened,
    /// Read from a QR code.
    Scanned,
}

pub const NOT_A_CODE: &str = "That QR code isn't a Threnody invite, link code or directory link.";
pub const NOT_A_LINK: &str = "That isn't a Threnody link";

/// Said with every link code offered or filled in: joining another
/// account is the most damaging thing a link can lead to.
pub const OWN_DEVICES_ONLY: &str = "Only continue if you made this code on one of your own \
     devices: joining shares your contacts and history with that account.";

/// Most bytes of clipboard text read: one more than `classify_link` takes,
/// to tell "too long" from "exactly at the limit".
pub const CLIP_READ_MAX: usize = 4097;

/// One of our own invites or link codes: `own` holds this device's, each
/// anonymous identity's and the account's other devices' fingerprints.
pub fn is_ours(info: &LinkInfo, own: &[String]) -> bool {
    info.kind != LinkKind::Directory && info.fingerprint.as_ref().is_some_and(|fp| own.contains(fp))
}

/// Whether a copied link is worth offering: not ours, not a directory we
/// already subscribe to (`subscribed`: their `id_hex`), and not the one
/// offered last.
pub fn should_offer(
    info: &LinkInfo,
    own: &[String],
    subscribed: &[String],
    last: Option<&str>,
) -> bool {
    if is_ours(info, own) || last == Some(info.link.as_str()) {
        return false;
    }
    !(info.kind == LinkKind::Directory
        && info
            .directory_id_hex
            .as_ref()
            .is_some_and(|id| subscribed.contains(id)))
}

/// Clipboard bytes as text, or None when it isn't UTF-8, is empty, or is
/// longer than `classify_link` accepts (`n`: the bytes actually read).
pub fn clip_text(buf: &[u8], n: usize) -> Option<String> {
    if n == 0 || n >= CLIP_READ_MAX || n > buf.len() {
        return None;
    }
    std::str::from_utf8(&buf[..n]).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: LinkKind, link: &str, fp: Option<&str>, id_hex: Option<&str>) -> LinkInfo {
        LinkInfo {
            kind,
            link: link.into(),
            fingerprint: fp.map(Into::into),
            directory_id: id_hex.map(|h| h[..8].to_owned()),
            directory_id_hex: id_hex.map(Into::into),
        }
    }

    #[test]
    fn own_links_are_not_offered() {
        let own = vec!["AAAA-BBBB".to_owned()];
        for kind in [LinkKind::Invite, LinkKind::DeviceLink] {
            let mine = info(kind, "threnody://AAAABBBB@x:1", Some("AAAA-BBBB"), None);
            assert!(is_ours(&mine, &own));
            assert!(!should_offer(&mine, &own, &[], None));
            let theirs = info(kind, "threnody://CCCCDDDD@x:1", Some("CCCC-DDDD"), None);
            assert!(!is_ours(&theirs, &own));
            assert!(should_offer(&theirs, &own, &[], None));
        }
    }

    #[test]
    fn each_link_is_offered_once() {
        let l = info(LinkKind::Invite, "threnody://C@x:1", Some("C"), None);
        assert!(should_offer(&l, &[], &[], Some("threnody://other")));
        assert!(!should_offer(&l, &[], &[], Some("threnody://C@x:1")));
    }

    #[test]
    fn subscribed_directories_are_not_offered() {
        let hex = "07".repeat(32);
        let upper = info(
            LinkKind::Directory,
            "threnody-dir://ABC@x:1",
            None,
            Some(&hex),
        );
        // The same issuer, spelled in lower case: the same hex id.
        let lower = info(
            LinkKind::Directory,
            "threnody-dir://abc@x:1",
            None,
            Some(&hex),
        );
        let subscribed = vec![hex.clone()];
        assert!(!should_offer(&upper, &[], &subscribed, None));
        assert!(!should_offer(&lower, &[], &subscribed, None));
        assert!(should_offer(&lower, &[], &["08".repeat(32)], None));
    }

    #[test]
    fn directory_links_have_no_owner() {
        let d = info(
            LinkKind::Directory,
            "threnody-dir://A@x:1",
            None,
            Some(&"07".repeat(32)),
        );
        assert!(!is_ours(&d, &["AAAA".to_owned(), String::new()]));
    }

    #[test]
    fn another_device_of_the_account_is_ours() {
        // This laptop and the phone linked into its account.
        let own = vec!["LAPT-OP00".to_owned(), "PHON-E000".to_owned()];
        let phone = info(
            LinkKind::Invite,
            "threnody://PHONE000@x:1",
            Some("PHON-E000"),
            None,
        );
        assert!(!should_offer(&phone, &own, &[], None));
    }

    #[test]
    fn clipboard_text_is_bounded() {
        let mut buf = vec![b'a'; CLIP_READ_MAX];
        assert_eq!(clip_text(&buf, 4096).map(|s| s.len()), Some(4096));
        assert_eq!(clip_text(&buf, 3).as_deref(), Some("aaa"));
        assert_eq!(clip_text(&buf, CLIP_READ_MAX), None);
        assert_eq!(clip_text(&buf, 0), None);
        buf[1] = 0xff;
        assert_eq!(clip_text(&buf, 3), None);
    }
}
