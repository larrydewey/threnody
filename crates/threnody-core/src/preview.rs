//! Links in message text, and link previews: the card a sender attaches
//! to a message with a link (fetched by the sender, from the Open Graph
//! tags of the page, so receivers never contact the site).

use std::io::Read;
use std::ops::Range;
use std::time::Duration;

use reqwest::blocking::Client;
use scraper::{Html, Selector};

/// Maximum time to wait for a preview response.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum response size to process.
const MAX_RESPONSE_SIZE: usize = 1_000_000; // 1 MB

/// Preview information extracted from a URL's Open Graph meta tags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlPreview {
    /// The original URL.
    pub url: String,
    /// The page title (from og:title or <title>).
    pub title: Option<String>,
    /// The page description (from og:description or meta description).
    pub description: Option<String>,
    /// The preview image URL (from og:image).
    pub image_url: Option<String>,
    /// The site name (from og:site_name).
    pub site_name: Option<String>,
    /// The content type (from og:type).
    pub content_type: Option<String>,
}

/// Longest preview title, description and site name, in characters.
pub const MAX_PREVIEW_TITLE: usize = 200;
pub const MAX_PREVIEW_DESCRIPTION: usize = 300;
pub const MAX_PREVIEW_SITE: usize = 100;
/// Largest preview image (a JPEG thumbnail the sender's app makes).
pub const MAX_PREVIEW_IMAGE: usize = 24 * 1024;
/// Largest image [`fetch_link_preview`] downloads for the app to shrink.
const MAX_FETCHED_IMAGE: usize = 2 * 1024 * 1024;

/// The card a message shows for a link in its text: what the sender's
/// device found at `url`, with an optional JPEG thumbnail.
///
/// ```text
/// LinkPreview = { 0: url tstr, ? 1: title tstr, ? 2: description tstr,
///                 ? 3: site tstr, ? 4: image bstr (JPEG) }
/// ```
///
/// Messages carry it encoded (key 13 of a `Text`, key 14 of a group
/// text); receivers keep it only if [`LinkPreview::for_text`] accepts it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkPreview {
    pub url: String,
    pub title: String,
    pub description: String,
    pub site: String,
    /// A JPEG of at most [`MAX_PREVIEW_IMAGE`] bytes, or empty.
    pub image: Vec<u8>,
}

impl LinkPreview {
    /// This preview, cleaned up for `text`: kept only if its URL is one of
    /// the links in `text` (as [`find_links`] normalizes them), so the card
    /// can't send its reader somewhere the message doesn't. Texts are cut
    /// to their limits, on one line, without control characters or
    /// bidirectional overrides, and an image that
    /// isn't a JPEG within [`MAX_PREVIEW_IMAGE`] is dropped. None if
    /// nothing is left to show.
    pub fn for_text(self, text: &str) -> Option<Self> {
        let url = find_links(&self.url).into_iter().next()?.url;
        if !find_links(text).iter().any(|l| l.url == url) {
            return None;
        }
        let clean = |s: &str, max: usize| -> String {
            let words: Vec<&str> = s.split_whitespace().collect();
            words
                .join(" ")
                .chars()
                .filter(|c| !c.is_control() && !is_bidi_control(*c))
                .take(max)
                .collect()
        };
        let image = if self.image.len() <= MAX_PREVIEW_IMAGE
            && crate::media::kind(&self.image) == Some(crate::media::Kind::Jpeg)
        {
            self.image
        } else {
            Vec::new()
        };
        let p = Self {
            url,
            title: clean(&self.title, MAX_PREVIEW_TITLE),
            description: clean(&self.description, MAX_PREVIEW_DESCRIPTION),
            site: clean(&self.site, MAX_PREVIEW_SITE),
            image,
        };
        (!p.title.is_empty() || !p.description.is_empty() || !p.image.is_empty()).then_some(p)
    }

    pub fn encode(&self) -> crate::Result<Vec<u8>> {
        let size = self.url.len()
            + self.title.len()
            + self.description.len()
            + self.site.len()
            + self.image.len()
            + 32;
        crate::cbor::to_vec(size, |e| {
            e.map_len(
                1 + usize::from(!self.title.is_empty())
                    + usize::from(!self.description.is_empty())
                    + usize::from(!self.site.is_empty())
                    + usize::from(!self.image.is_empty()),
            )?;
            e.u8(0)?.str(&self.url)?;
            if !self.title.is_empty() {
                e.u8(1)?.str(&self.title)?;
            }
            if !self.description.is_empty() {
                e.u8(2)?.str(&self.description)?;
            }
            if !self.site.is_empty() {
                e.u8(3)?.str(&self.site)?;
            }
            if !self.image.is_empty() {
                e.u8(4)?.bytes(&self.image)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> crate::Result<Self> {
        let mut dec = const_cbor::Decoder::new(b);
        let mut p = Self::default();
        let mut url = None;
        crate::cbor::read_map(&mut dec, |k, d| {
            match k {
                0 => url = Some(d.str()?.to_owned()),
                1 => p.title = d.str()?.to_owned(),
                2 => p.description = d.str()?.to_owned(),
                3 => p.site = d.str()?.to_owned(),
                4 => p.image = d.bytes()?.to_vec(),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        crate::cbor::finish(&dec)?;
        p.url = crate::cbor::required(url, "preview url")?;
        Ok(p)
    }
}

/// Bidirectional overrides and isolates, which could make a card's text
/// read differently from what it says.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Fetches a preview of `url` for the sender to attach: the page's title,
/// description and site name, and its image as downloaded (any format, up
/// to 2 MB) for the app to shrink to a JPEG thumbnail of at most
/// [`MAX_PREVIEW_IMAGE`] bytes before sending ([`LinkPreview::for_text`]
/// drops anything else). Blocks; None if the page has nothing to show.
///
/// Like [`fetch_preview`], it goes straight from this device: only fetch
/// a link the user chose to preview.
pub fn fetch_link_preview(url: &str) -> Option<LinkPreview> {
    let page = fetch_preview(url)?;
    let image = page
        .image_url
        .as_deref()
        .and_then(|i| reqwest::Url::parse(url).ok()?.join(i).ok())
        .filter(|i| matches!(i.scheme(), "http" | "https"))
        .and_then(|i| fetch_image(i.as_str()))
        .unwrap_or_default();
    Some(LinkPreview {
        url: page.url,
        title: page.title.unwrap_or_default(),
        description: page.description.unwrap_or_default(),
        site: page.site_name.unwrap_or_default(),
        image,
    })
}

/// An image's bytes, if `url` answers with an image of at most
/// [`MAX_FETCHED_IMAGE`] bytes.
fn fetch_image(url: &str) -> Option<Vec<u8>> {
    let client = Client::builder()
        .timeout(PREVIEW_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .ok()?;
    let response = client.get(url).send().ok()?;
    let is_image = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.to_ascii_lowercase().starts_with("image/"));
    if !response.status().is_success() || !is_image {
        return None;
    }
    let mut body = Vec::new();
    response
        .take(MAX_FETCHED_IMAGE as u64 + 1)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() <= MAX_FETCHED_IMAGE).then_some(body)
}

/// A web link found in message text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextLink {
    /// Where it is in the text, in bytes.
    pub range: Range<usize>,
    /// The link as a browser would open it (normalized: lowercased scheme
    /// and host, international hosts in punycode).
    pub url: String,
    /// The host it goes to, in ASCII (punycode for international names),
    /// so a look-alike can't pass for the real one.
    pub host: String,
}

/// The http(s) links in `text`, in order. A link starts with `http://` or
/// `https://` (any case) not glued to a letter or digit before it, and
/// runs to whitespace or one of `<>"` and backtick; trailing punctuation
/// (`.,!?;:'*~`) and closing brackets without an opening one inside the
/// link are left out, so "(see https://a.example/x)." links only the
/// address. Anything that isn't a valid URL with a host written out
/// after the `//` is skipped.
pub fn find_links(text: &str) -> Vec<TextLink> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = next_scheme(&lower, from) {
        let start = from + at;
        let glued = text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        let len = text[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
            .unwrap_or(text.len() - start);
        let end = start + trimmed_len(&text[start..start + len]);
        from = start + len.max(1);
        // "https:///x" would parse with "x" as its host.
        let after = text[start..end].split_once("//").map_or("", |(_, a)| a);
        if glued || after.starts_with(['/', '\\']) {
            continue;
        }
        let Ok(url) = reqwest::Url::parse(&text[start..end]) else {
            continue;
        };
        let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
            continue;
        };
        out.push(TextLink {
            range: start..end,
            host: host.to_owned(),
            url: url.into(),
        });
    }
    out
}

/// Where the next "http://" or "https://" starts in `lower` at or after
/// `from`, relative to `from`.
fn next_scheme(lower: &str, from: usize) -> Option<usize> {
    let rest = &lower[from..];
    [rest.find("http://"), rest.find("https://")]
        .into_iter()
        .flatten()
        .min()
}

/// How much of `link` to keep: without trailing punctuation, nor closing
/// brackets that close nothing in the link.
fn trimmed_len(link: &str) -> usize {
    let mut s = link;
    loop {
        let Some(last) = s.chars().next_back() else {
            return 0;
        };
        let unopened = |open: char| s.matches(open).count() < s.matches(last).count();
        let drop = match last {
            '.' | ',' | '!' | '?' | ';' | ':' | '\'' | '*' | '~' => true,
            ')' => unopened('('),
            ']' => unopened('['),
            '}' => unopened('{'),
            _ => false,
        };
        if !drop {
            return s.len();
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

/// Fetches and parses a URL preview from the given URL.
/// Returns None if the URL cannot be fetched or has no preview data.
///
/// The request goes straight from this device, not through onion routes, so
/// the site sees its address. An app should only fetch a link the user chose
/// to preview, never one a contact sent on its own.
pub fn fetch_preview(url: &str) -> Option<UrlPreview> {
    // Validate URL
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }

    // Create HTTP client with timeout
    let client = Client::builder()
        .timeout(PREVIEW_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .ok()?;

    // Fetch the page
    let response = client.get(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }

    // Check content type is HTML
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.to_lowercase().contains("text/html") {
        return None;
    }

    // Read the body only up to the limit: a page that never ends is cut
    // there rather than held whole in memory. The cut can split a character,
    // so the text is read leniently.
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_SIZE as u64)
        .read_to_end(&mut body)
        .ok()?;
    let body = String::from_utf8_lossy(&body);

    // Parse HTML
    let document = Html::parse_document(&body);

    // Extract Open Graph meta tags
    let meta_selector = Selector::parse(
        "meta[property^='og:'], meta[name='description'], meta[name='twitter:card'], title",
    )
    .ok()?;

    let mut title = None;
    let mut description = None;
    let mut image_url = None;
    let mut site_name = None;
    let mut content_type = None;

    for element in document.select(&meta_selector) {
        if let Some(property) = element.value().attr("property") {
            let content = element.value().attr("content").unwrap_or("");
            match property {
                "og:title" => title = Some(content.to_string()),
                "og:description" => description = Some(content.to_string()),
                "og:image" => image_url = Some(content.to_string()),
                "og:site_name" => site_name = Some(content.to_string()),
                "og:type" => content_type = Some(content.to_string()),
                _ => {}
            }
        } else if let Some(name) = element.value().attr("name") {
            let content = element.value().attr("content").unwrap_or("");
            match name {
                "description" if description.is_none() => description = Some(content.to_string()),
                "twitter:card" if content_type.is_none() => {
                    content_type = Some(content.to_string())
                }
                _ => {}
            }
        } else if element.value().name() == "title" && title.is_none() {
            title = Some(element.text().collect::<String>());
        }
    }

    // Only return if we found at least some data
    if title.is_none()
        && description.is_none()
        && image_url.is_none()
        && site_name.is_none()
        && content_type.is_none()
    {
        return None;
    }

    Some(UrlPreview {
        url: url.to_string(),
        title,
        description,
        image_url,
        site_name,
        content_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(text: &str) -> Vec<(&str, String, String)> {
        find_links(text)
            .into_iter()
            .map(|l| (&text[l.range], l.url, l.host))
            .collect()
    }

    fn shown(text: &str) -> Vec<&str> {
        find_links(text)
            .into_iter()
            .map(|l| &text[l.range])
            .collect()
    }

    #[test]
    fn finds_links_in_text() {
        assert_eq!(
            links("see https://example.com/a?b=1#c ok"),
            vec![(
                "https://example.com/a?b=1#c",
                "https://example.com/a?b=1#c".into(),
                "example.com".into()
            )]
        );
        assert_eq!(
            shown("http://a.example and HTTPS://B.example/x"),
            vec!["http://a.example", "HTTPS://B.example/x"]
        );
        assert_eq!(links("HTTPS://B.example/x")[0].1, "https://b.example/x");
        assert!(find_links("no links here, nor ftp://a.example").is_empty());
    }

    #[test]
    fn leaves_out_punctuation_around_links() {
        assert_eq!(shown("Look: https://a.example."), vec!["https://a.example"]);
        assert_eq!(
            shown("(see https://a.example/x)."),
            vec!["https://a.example/x"]
        );
        assert_eq!(
            shown("https://en.wikipedia.org/wiki/Rust_(language)"),
            vec!["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(shown("<https://a.example/x>"), vec!["https://a.example/x"]);
        assert_eq!(shown("\"https://a.example\""), vec!["https://a.example"]);
        assert_eq!(shown("**https://a.example**"), vec!["https://a.example"]);
        assert_eq!(shown("https://a.example/?!?"), vec!["https://a.example/"]);
    }

    #[test]
    fn skips_what_is_not_a_link() {
        assert!(find_links("https://").is_empty());
        assert!(find_links("https:///path").is_empty());
        assert!(find_links("xhttps://a.example").is_empty());
        assert_eq!(shown("https://exa mple.com"), vec!["https://exa"]);
        assert_eq!(
            shown("1https://a.example 2https://b.example"),
            Vec::<&str>::new()
        );
    }

    /// The host shown is the one the browser goes to, in ASCII.
    #[test]
    fn hosts_are_the_real_destination() {
        assert_eq!(
            links("https://bank.example@evil.example/")[0].2,
            "evil.example"
        );
        assert_eq!(links("https://аpple.com/")[0].2, "xn--pple-43d.com");
    }

    #[test]
    fn ranges_are_bytes_of_the_text() {
        let t = "é → https://a.example/ü ok";
        let l = &find_links(t)[0];
        assert_eq!(&t[l.range.clone()], "https://a.example/ü");
        assert_eq!(l.url, "https://a.example/%C3%BC");
    }

    fn card(url: &str) -> LinkPreview {
        LinkPreview {
            url: url.into(),
            title: "Title".into(),
            ..LinkPreview::default()
        }
    }

    #[test]
    fn previews_round_trip() {
        let p = LinkPreview {
            url: "https://a.example/".into(),
            title: "T".into(),
            description: "D".into(),
            site: "S".into(),
            image: vec![0xFF, 0xD8, 0xFF, 0xE0],
        };
        assert_eq!(LinkPreview::decode(&p.encode().unwrap()).unwrap(), p);
        let bare = card("https://a.example/");
        assert_eq!(LinkPreview::decode(&bare.encode().unwrap()).unwrap(), bare);
    }

    /// A card only stands for a link the message has.
    #[test]
    fn previews_must_match_a_link_in_the_text() {
        let text = "look at https://a.example/x.";
        let p = card("https://A.example/x").for_text(text).unwrap();
        assert_eq!(p.url, "https://a.example/x");
        assert!(card("https://evil.example/").for_text(text).is_none());
        assert!(card("javascript:alert(1)").for_text(text).is_none());
        assert!(card("https://a.example/x").for_text("no link").is_none());
    }

    #[test]
    fn previews_are_cleaned_and_bounded() {
        let p = LinkPreview {
            url: "https://a.example/".into(),
            title: format!("  a\n\tb\u{202e}{}", "x".repeat(500)),
            description: String::new(),
            site: String::new(),
            image: b"\x89PNG\r\n\x1a\n".to_vec(),
        }
        .for_text("https://a.example/")
        .unwrap();
        assert!(p.title.starts_with("a b"));
        assert_eq!(p.title.chars().count(), MAX_PREVIEW_TITLE);
        assert!(!p.title.contains('\u{202e}'));
        // Not a JPEG: dropped.
        assert!(p.image.is_empty());
        let big = LinkPreview {
            image: [&[0xFF, 0xD8, 0xFF][..], &vec![0; MAX_PREVIEW_IMAGE]].concat(),
            ..card("https://a.example/")
        };
        assert!(big.for_text("https://a.example/").unwrap().image.is_empty());
        // Nothing to show: no card.
        let empty = LinkPreview {
            url: "https://a.example/".into(),
            ..LinkPreview::default()
        };
        assert!(empty.for_text("https://a.example/").is_none());
    }

    #[test]
    fn test_fetch_preview_invalid_url() {
        assert!(fetch_preview("not-a-url").is_none());
        assert!(fetch_preview("ftp://example.com").is_none());
    }

    /// A page that never stops sending is read only up to the limit, and a
    /// character cut in two at the limit doesn't lose the preview.
    #[test]
    fn endless_page_is_cut_at_the_limit() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<title>Endless</title>";
            let _ = s.write_all(head.as_bytes());
            // An odd offset makes the cut fall inside a two-byte character.
            let _ = s.write_all(b"x");
            let chunk = "\u{e9}".repeat(32 * 1024);
            while s.write_all(chunk.as_bytes()).is_ok() {}
        });
        let started = std::time::Instant::now();
        let p = fetch_preview(&format!("http://{addr}/")).expect("a preview");
        assert_eq!(p.title.as_deref(), Some("Endless"));
        assert!(started.elapsed() < PREVIEW_TIMEOUT);
    }
}
