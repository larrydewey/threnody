//! Links in message text, and URL previews: fetches Open Graph meta tags
//! from URLs.

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
