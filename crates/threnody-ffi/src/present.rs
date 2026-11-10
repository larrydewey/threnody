//! How the apps present messages: labels for files, list previews,
//! search highlights, mentions, and the small checks they each made on
//! their own (is it an image, must it be converted, is it a handle).
//! Pure functions, so Android, iOS and Linux show the same thing and the
//! logic is tested once, here.

use threnody_core::history::Query;

use crate::{Clip, DEFAULT_COVER_MS, HistoryEntry, MentionInfo};

/// The cover-traffic interval on metered networks (cellular, or iOS's
/// Low Data Mode): about 46 MB a day per connected contact.
pub const METERED_COVER_MS: u32 = 10_000;

/// A range of a string in UTF-16 code units (what Swift's NSRange and
/// Kotlin's String indices count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct TextRange {
    pub start: u32,
    pub len: u32,
}

/// One mention as shown: a UTF-16 range of [`ShownText::text`]; `mine`:
/// it's us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct MentionSpan {
    pub start: u32,
    pub len: u32,
    pub mine: bool,
}

/// A message's text as shown, with where its mentions are.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ShownText {
    pub text: String,
    pub mentions: Vec<MentionSpan>,
}

/// A web link in message text: a UTF-16 range of it, the URL to open, and
/// the host it goes to in ASCII (punycode for international names), to
/// show before leaving the app.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TextLink {
    pub start: u32,
    pub len: u32,
    pub url: String,
    pub host: String,
}

/// The http(s) links in `text` (as `threnody_core::find_links` finds
/// them), with UTF-16 ranges.
#[uniffi::export]
pub fn text_links(text: String) -> Vec<TextLink> {
    threnody_core::find_links(&text)
        .into_iter()
        .map(|l| {
            let units = |s: &str| s.encode_utf16().count() as u32;
            let start = units(&text[..l.range.start]);
            TextLink {
                start,
                len: units(&text[l.range]),
                url: l.url,
                host: l.host,
            }
        })
        .collect()
}

/// Whether `name` looks like an image by its extension (jpg, jpeg, png,
/// gif, webp, bmp, avif, heic, heif, tif, tiff; any case).
#[uniffi::export]
pub fn is_image_name(name: String) -> bool {
    threnody_core::media::looks_like_image(&name)
}

/// Whether a picked image must be re-encoded (as JPEG) before sending so
/// that it carries no metadata. The bytes decide first: JPEG, PNG and
/// WebP (which the node strips) and GIF don't; HEIF/AVIF, TIFF (camera
/// raw files included) and BMP do, whatever the file is called; any
/// other ISO-BMFF file (a video or sound) is no image and doesn't. Bytes
/// that say nothing leave it to the name. `head`: at least the first 32
/// bytes.
#[uniffi::export]
pub fn image_needs_conversion(name: String, head: Vec<u8>) -> bool {
    use threnody_core::media::{kind, needs_converting};
    if kind(&head).is_some() || head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return false;
    }
    // Every photo the node would refuse to send unconverted, at least.
    if needs_converting(&head) || is_bmp(&head) {
        return true;
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        /// Image brands of ISO-BMFF; anything else in it is video or sound.
        const IMAGE: [&[u8; 4]; 12] = [
            b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1",
            b"msf1", b"avif", b"avis",
        ];
        return IMAGE.iter().any(|b| &head[8..12] == *b);
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    is_image_name(name) && !["jpg", "jpeg", "png", "webp", "gif"].contains(&ext.as_str())
}

/// A Windows bitmap: "BM", then reserved zeros, then a known header size
/// (two letters alone would take any text starting "BM" for one).
fn is_bmp(h: &[u8]) -> bool {
    h.len() >= 18
        && h.starts_with(b"BM")
        && h[6..10] == [0, 0, 0, 0]
        && [12, 40, 52, 56, 64, 108, 124]
            .contains(&u32::from_le_bytes([h[14], h[15], h[16], h[17]]))
}

/// Minutes and seconds: "0:12".
fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// What a list row or notification says for a file: "📷 Photo", "📎
/// report.pdf", "🎤 Voice message (0:12)", or the caption.
#[uniffi::export]
pub fn file_label(name: String, sensitive: bool, caption: String, clip: Option<Clip>) -> String {
    if let Some(c) = clip {
        let (icon, what) = if c.video {
            ("🎥", "Video message")
        } else {
            ("🎤", "Voice message")
        };
        return match (sensitive, c.duration_ms) {
            (true, _) => format!("{icon} Sensitive {}", what.to_lowercase()),
            (false, 0) => format!("{icon} {what}"),
            (false, ms) => format!("{icon} {what} ({})", clock(u64::from(ms))),
        };
    }
    let caption = caption.as_str();
    match (sensitive, is_image_name(name.clone())) {
        (true, true) => "📷 Sensitive photo".into(),
        (true, false) => "📎 Sensitive file".into(),
        (false, true) => format!("📷 {}", if caption.is_empty() { "Photo" } else { caption }),
        (false, false) => format!("📎 {}", if caption.is_empty() { &name } else { caption }),
    }
}

/// A one-line preview of a stored message: [`file_label`] for files, "📊
/// question" for polls, "📋 title" for shared lists, else the first line
/// of the text. Apps put "You: " in front of outgoing ones themselves.
#[uniffi::export]
pub fn entry_preview(entry: HistoryEntry) -> String {
    let first = entry.text.lines().next().unwrap_or("").to_owned();
    if let Some(f) = entry.file {
        file_label(f.name, f.sensitive, entry.text, f.clip)
    } else if entry.poll.is_some() {
        format!("📊 {first}")
    } else if entry.list.is_some() {
        format!("📋 {}", if first.is_empty() { "List" } else { &first })
    } else {
        first
    }
}

/// Lowercases one character at a time, so positions in the result are
/// positions in the original.
fn fold(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// The terms of `query` as the node parses it, folded as [`fold`] does.
fn terms(query: &str) -> Vec<Vec<char>> {
    Query::new(query)
        .terms()
        .iter()
        .map(|t| fold(t))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Where `term` occurs in `text`, as char ranges, not overlapping.
fn occurrences(text: &[char], term: &[char]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + term.len() <= text.len() {
        if text[i..i + term.len()] == *term {
            found.push((i, i + term.len()));
            i += term.len();
        } else {
            i += 1;
        }
    }
    found
}

/// Where `query` (parsed as the node parses it) matches `text`, ignoring
/// case: sorted, overlapping and adjacent matches merged, in UTF-16
/// units. Empty when the query has no terms.
#[uniffi::export]
pub fn search_ranges(text: String, query: String) -> Vec<TextRange> {
    let folded = fold(&text);
    let mut found: Vec<(usize, usize)> = terms(&query)
        .iter()
        .flat_map(|t| occurrences(&folded, t))
        .collect();
    found.sort_unstable();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (s, e) in found {
        match spans.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => spans.push((s, e)),
        }
    }
    // UTF-16 offset of each char position.
    let mut at = Vec::with_capacity(folded.len() + 1);
    let mut n = 0u32;
    at.push(0);
    for c in text.chars() {
        n += c.len_utf16() as u32;
        at.push(n);
    }
    spans
        .into_iter()
        .map(|(s, e)| TextRange {
            start: at[s],
            len: at[e] - at[s],
        })
        .collect()
}

/// Whether every term of `query` occurs in `text`, ignoring case, as the
/// node's own search matches messages. False when the query has no
/// terms. For finding conversations by name.
#[uniffi::export]
pub fn search_matches(text: String, query: String) -> bool {
    let folded = fold(&text);
    let terms = terms(&query);
    !terms.is_empty()
        && terms
            .iter()
            .all(|t| folded.windows(t.len()).any(|w| w == &t[..]))
}

/// A message's text with each mention replaced by "@" and `names[i]`
/// (the app's name for `mentions[i].device`). Mentions count chars
/// (Unicode scalars) of `text`, as senders write them; ones out of range
/// or overlapping an earlier one are skipped. The spans returned are in
/// UTF-16 units of the shown text.
#[uniffi::export]
pub fn show_mentions(text: String, mentions: Vec<MentionInfo>, names: Vec<String>) -> ShownText {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut units = 0u32;
    let mut spans = Vec::new();
    let mut from = 0usize;
    let mut push = |out: &mut String, s: &str| {
        out.push_str(s);
        units += s.encode_utf16().count() as u32;
        units
    };
    for (i, m) in mentions.iter().enumerate() {
        let (a, b) = (m.start as usize, m.start as usize + m.len as usize);
        if a < from || b > chars.len() {
            continue;
        }
        let before: String = chars[from..a].iter().collect();
        let start = push(&mut out, &before);
        let name = names
            .get(i)
            .cloned()
            .unwrap_or_else(|| m.device.chars().take(9).collect());
        let end = push(&mut out, &format!("@{name}"));
        spans.push(MentionSpan {
            start,
            len: end - start,
            mine: m.mine,
        });
        from = b;
    }
    let rest: String = chars[from.min(chars.len())..].iter().collect();
    push(&mut out, &rest);
    ShownText {
        text: out,
        mentions: spans,
    }
}

/// Whether `text` is someone's handle (`name-XXXX-XXXX`, Appendix S), as
/// `connect` would take it: never an invite or link, nor anything that
/// is a fingerprint. Cheap: it doesn't check the code (no Argon2id).
#[uniffi::export]
pub fn looks_like_handle(text: String) -> bool {
    let t = text.trim();
    !t.contains("://")
        && t.parse::<threnody_core::Fingerprint>().is_err()
        && t.parse::<threnody_core::handle::Handle>().is_ok()
}

/// The cover-traffic interval to set: none when the user turned cover
/// traffic off, [`METERED_COVER_MS`] on metered networks, else
/// [`DEFAULT_COVER_MS`].
#[uniffi::export]
pub fn cover_interval_ms(on: bool, metered: bool) -> Option<u32> {
    match (on, metered) {
        (false, _) => None,
        (true, true) => Some(METERED_COVER_MS),
        (true, false) => Some(DEFAULT_COVER_MS),
    }
}

/// A file name safe to put in a path: the part after the last '/' or
/// '\\', characters other than A-Z, a-z, 0-9, '.', '_' and '-' replaced
/// by '_', at most the last 80 characters; "file" if that leaves nothing,
/// "." or "..".
#[uniffi::export]
pub fn safe_file_name(name: String) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let safe: Vec<char> = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe: String = safe[safe.len().saturating_sub(80)..].iter().collect();
    if safe.is_empty() || safe == "." || safe == ".." {
        "file".into()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileFetch, FileInfo, PollInfo, TickMark};

    fn entry(text: &str) -> HistoryEntry {
        HistoryEntry {
            at_ms: 1,
            outgoing: false,
            device: "d".into(),
            text: text.into(),
            disappearing: false,
            file: None,
            delivered: false,
            read: false,
            id: 0,
            edited: false,
            recipients: 0,
            delivered_to: 0,
            reactions: vec![],
            quote: None,
            mentions: vec![],
            poll: None,
            forwarded: false,
            list: None,
            tick: TickMark::None,
        }
    }

    fn r(start: u32, len: u32) -> TextRange {
        TextRange { start, len }
    }

    #[test]
    fn links_count_utf16_units() {
        let links = text_links("😀 see https://a.example/x.".into());
        assert_eq!(links.len(), 1);
        let l = &links[0];
        assert_eq!((l.start, l.len), (7, 19));
        assert_eq!(l.url, "https://a.example/x");
        assert_eq!(l.host, "a.example");
    }

    #[test]
    fn file_labels_say_what_the_file_is() {
        let label = |n: &str, s: bool, c: &str, clip| file_label(n.into(), s, c.into(), clip);
        assert_eq!(label("a.jpg", false, "", None), "📷 Photo");
        assert_eq!(label("a.jpg", false, "holiday", None), "📷 holiday");
        assert_eq!(label("a.jpg", true, "holiday", None), "📷 Sensitive photo");
        assert_eq!(label("report.pdf", false, "", None), "📎 report.pdf");
        assert_eq!(label("report.pdf", true, "", None), "📎 Sensitive file");
        let voice = |ms| {
            Some(Clip {
                video: false,
                duration_ms: ms,
            })
        };
        assert_eq!(
            label("v.ogg", false, "", voice(12_400)),
            "🎤 Voice message (0:12)"
        );
        assert_eq!(label("v.ogg", false, "", voice(0)), "🎤 Voice message");
        let video = Some(Clip {
            video: true,
            duration_ms: 5000,
        });
        assert_eq!(
            label("v.webm", true, "", video),
            "🎥 Sensitive video message"
        );
    }

    #[test]
    fn previews_use_the_first_line_or_the_file_label() {
        assert_eq!(entry_preview(entry("a\nb")), "a");
        let mut f = entry("beach");
        f.file = Some(FileInfo {
            name: "p.jpg".into(),
            size: 1,
            location: None,
            sensitive: false,
            album: 0,
            clip: None,
            fetch: FileFetch::NotHere,
        });
        assert_eq!(entry_preview(f), "📷 beach");
        let mut poll = entry("Lunch?\nreally");
        poll.poll = Some(PollInfo {
            options: vec![],
            multi: false,
            closed: false,
            voters: 0,
        });
        assert_eq!(entry_preview(poll), "📊 Lunch?");
        let mut list = entry("");
        list.list = Some("00".into());
        assert_eq!(entry_preview(list.clone()), "📋 List");
        list.text = "Groceries".into();
        assert_eq!(entry_preview(list), "📋 Groceries");
    }

    #[test]
    fn images_are_recognised_by_name() {
        for n in ["a.JPG", "b.heic", "c.tiff"] {
            assert!(is_image_name(n.into()), "{n}");
        }
        for n in ["a.pdf", "noext"] {
            assert!(!is_image_name(n.into()), "{n}");
        }
    }

    #[test]
    fn only_unstrippable_images_need_conversion() {
        let needs = |n: &str, b: &[u8]| image_needs_conversion(n.into(), b.to_vec());
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
        let mp4 = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2";
        let mut bmp = b"BM\x46\0\0\0\0\0\0\0\x36\0\0\0\x28\0\0\0".to_vec();
        bmp.resize(32, 0);
        // The node strips these, or there's nothing to strip.
        assert!(!needs("x.jpg", b"\xFF\xD8\xFF\xE0\0\x10JFIF"));
        assert!(!needs("x.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(!needs("x.webp", b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!needs("x.gif", b"GIF89a\x01\0\x01\0"));
        assert!(!needs("notes.txt", b"BMW is a car, and so on and on."));
        // Photos with metadata only conversion removes, whatever the name.
        assert!(needs("x.heic", heic));
        assert!(needs("x.jpg", heic));
        assert!(needs("x.bin", b"\0\0\0\x1cftypavif\0\0\0\0avifmif1"));
        assert!(needs("x.tif", b"II*\0\x08\0\0\0"));
        assert!(needs("x.bin", &bmp));
        assert!(needs("x.bmp", b"BM"));
        assert!(needs("x.heif", b"unknown bytes here"));
        // A video is never converted; a JPEG called .heic is stripped.
        assert!(!needs("x.mp4", mp4));
        assert!(!needs("x.heic", mp4));
        assert!(!needs("x.heic", b"\xFF\xD8\xFF\xE1\0\x10Exif"));
    }

    #[test]
    fn search_ranges_count_utf16_and_merge() {
        let ranges = |t: &str, q: &str| search_ranges(t.into(), q.into());
        assert_eq!(
            ranges("Shall we have LUNCH at noon? lunch", "lunch \"at noon\""),
            vec![r(14, 5), r(20, 7), r(29, 5)]
        );
        assert_eq!(ranges("👍 lunch", "lunch"), vec![r(3, 5)]);
        assert_eq!(ranges("abcdef", "abc cde"), vec![r(0, 5)]);
        let i = ranges("İstanbul", "istanbul");
        assert_eq!(i, vec![r(0, 8)]);
        assert!(ranges("anything", "").is_empty());
        assert!(ranges("anything", "   ").is_empty());
    }

    #[test]
    fn search_matches_needs_every_term() {
        let m = |t: &str, q: &str| search_matches(t.into(), q.into());
        assert!(m("Alice Smith", "ali smi"));
        assert!(!m("Alice Smith", "ali bob"));
        assert!(!m("Alice Smith", ""));
        assert!(m("Alice Smith", "\"alice smith\""));
    }

    #[test]
    fn handles_are_recognised_without_argon2() {
        let h = |t: &str| looks_like_handle(t.into());
        assert!(h("larry-0123-4567"));
        assert!(h(" @larry-0123-4567 "));
        assert!(h("larry 0123 4567"));
        assert!(!h("larry"));
        assert!(!h("192.168.1.2:7450"));
        let fp = threnody_core::Identity::generate()
            .public()
            .fingerprint()
            .to_string();
        assert!(!h(&fp));
        assert!(!h("threnody://larry-0123-4567"));
        assert!(!h("threnody-link://larry-0123-4567"));
    }

    #[test]
    fn mentions_are_shown_by_name_in_utf16() {
        let m = |start, len, mine| MentionInfo {
            start,
            len,
            device: "dev".into(),
            mine,
        };
        let names = vec!["Ann".to_string(), "Bob".to_string()];
        let s = show_mentions(
            "hi @x and @y".into(),
            vec![m(3, 2, false), m(10, 2, true)],
            names.clone(),
        );
        assert_eq!(s.text, "hi @Ann and @Bob");
        let span = |start, len, mine| MentionSpan { start, len, mine };
        assert_eq!(s.mentions, vec![span(3, 4, false), span(12, 4, true)]);
        // A flag is two scalars and four UTF-16 units.
        let s = show_mentions("🇫🇷 @x".into(), vec![m(3, 2, false)], names.clone());
        assert_eq!(s.text, "🇫🇷 @Ann");
        assert_eq!(s.mentions, vec![span(5, 4, false)]);
        // Past the end, or overlapping the one before: skipped, text kept.
        let s = show_mentions(
            "hi @x".into(),
            vec![m(3, 2, false), m(4, 1, false), m(9, 2, false)],
            names,
        );
        assert_eq!(s.text, "hi @Ann");
        assert_eq!(s.mentions, vec![span(3, 4, false)]);
    }

    #[test]
    fn cover_interval_follows_the_setting_and_metering() {
        assert_eq!(cover_interval_ms(false, false), None);
        assert_eq!(cover_interval_ms(false, true), None);
        assert_eq!(cover_interval_ms(true, false), Some(DEFAULT_COVER_MS));
        assert_eq!(cover_interval_ms(true, true), Some(METERED_COVER_MS));
    }

    #[test]
    fn file_names_are_made_safe() {
        let s = |n: &str| safe_file_name(n.into());
        assert_eq!(s("../../etc/passwd"), "passwd");
        assert_eq!(s("a b/ç.jpg"), "_.jpg");
        assert_eq!(s("..\\..\\x.png"), "x.png");
        assert_eq!(s(".."), "file");
        assert_eq!(s(""), "file");
        assert_eq!(s("dir/"), "file");
        let long = format!("{}.jpg", "a".repeat(196));
        let out = s(&long);
        assert_eq!(out.len(), 80);
        assert!(out.ends_with(".jpg"));
    }
}
