//! URL preview functionality: fetches Open Graph meta tags from URLs.

use std::time::Duration;

use reqwest::blocking::Client;
use scraper::{Html, Selector};

use crate::error::{Error, Result};

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

    // Read body with size limit
    let body = response
        .bytes()
        .ok()?
        .iter()
        .take(MAX_RESPONSE_SIZE)
        .cloned()
        .collect::<Vec<_>>();
    let body = String::from_utf8(body).ok()?;

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
}
