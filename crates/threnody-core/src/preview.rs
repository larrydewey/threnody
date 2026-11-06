//! URL preview functionality: fetches Open Graph meta tags from URLs.

use std::io::Read;
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
