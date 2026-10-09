//! GIF search through GIPHY's API. GIPHY sees what is searched for and this
//! computer's IP address, so nothing is asked of it until the user agrees
//! (`settings::GIPHY`). The GIF picked is downloaded here and sent like a
//! photo: the contact's device never contacts GIPHY.
//!
//! The API key comes from the build (`GIPHY_API_KEY` in the environment
//! when it was compiled), or from one the user enters, which wins.

use std::cell::Cell;
use std::io::Read;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::ui::bg;

const API: &str = "https://api.giphy.com/v1/gifs";
const LIMIT: u32 = 30;
const TIMEOUT: Duration = Duration::from_secs(15);
/// Previews are small; anything bigger than this isn't one.
const MAX_PREVIEW: u64 = 2 * 1024 * 1024;
/// Search answers are JSON of a few hundred kilobytes.
const MAX_ANSWER: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Gif {
    pub id: String,
    pub title: String,
    pub preview: String,
    pub full: String,
}

/// The key the user entered, else the build's (empty when neither).
pub fn key(entered: Option<&str>) -> String {
    entered
        .map(str::to_owned)
        .unwrap_or_else(|| option_env!("GIPHY_API_KEY").unwrap_or("").to_owned())
}

/// Trending GIFs for an empty `query`, else matches. Blocks.
pub fn search(key: &str, query: &str) -> Result<Vec<Gif>, String> {
    let mut url = reqwest::Url::parse(&format!(
        "{API}/{}",
        if query.trim().is_empty() {
            "trending"
        } else {
            "search"
        }
    ))
    .map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("api_key", key)
        .append_pair("limit", &LIMIT.to_string())
        .append_pair("rating", "pg-13");
    if !query.trim().is_empty() {
        url.query_pairs_mut().append_pair("q", query.trim());
    }
    let body: serde_json::Value =
        serde_json::from_slice(&get(url.as_str(), MAX_ANSWER)?).map_err(|e| e.to_string())?;
    let data = body
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("GIPHY's answer has no GIFs")?;
    Ok(data
        .iter()
        .filter_map(|o| {
            let images = o.get("images")?;
            let url = |names: &[&str]| {
                names.iter().find_map(|n| {
                    images
                        .get(n)?
                        .get("url")?
                        .as_str()
                        .filter(|u| u.starts_with("https://"))
                        .map(str::to_owned)
                })
            };
            let title = o.get("title").and_then(|t| t.as_str()).unwrap_or("");
            Some(Gif {
                id: o
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_owned(),
                title: if title.trim().is_empty() {
                    "GIF".to_owned()
                } else {
                    title.to_owned()
                },
                preview: url(&[
                    "fixed_width_small",
                    "fixed_width_downsampled",
                    "fixed_width",
                ])?,
                full: url(&["downsized", "fixed_width", "original"])?,
            })
        })
        .collect())
}

/// At most `max` bytes from `url`, over HTTPS only. Blocks.
pub fn get(url: &str, max: u64) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err("not HTTPS".into());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .https_only(true)
        .build()
        .map_err(|e| e.to_string())?;
    let r = client.get(url).send().map_err(|e| e.to_string())?;
    match r.status().as_u16() {
        200 => {}
        401 | 403 => return Err("GIPHY refused the API key".into()),
        code => return Err(format!("GIPHY answered {code}")),
    }
    if r.content_length().is_some_and(|n| n > max) {
        return Err("too large".into());
    }
    let mut out = Vec::new();
    r.take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() as u64 > max {
        return Err("too large".into());
    }
    Ok(out)
}

/// GIFs from GIPHY in a dialog: trending ones, or a search. `pick` gets
/// the one clicked; the dialog then closes. `files` offers the user's own
/// GIFs instead.
pub fn picker(
    parent: &impl IsA<gtk::Widget>,
    key: String,
    files: impl Fn() + 'static,
    pick: impl Fn(Gif) + 'static,
) {
    let d = adw::Dialog::builder()
        .title("GIFs")
        .content_width(560)
        .content_height(600)
        .build();
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Search GIPHY")
        .hexpand(true)
        .build();
    let status = crate::ui::label("", &["dim-label"]);
    status.set_margin_top(24);
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(3)
        .max_children_per_line(4)
        .row_spacing(6)
        .column_spacing(6)
        .homogeneous(true)
        .valign(gtk::Align::Start)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&grid)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let own = gtk::Button::with_label("Choose a GIF file…");
    own.add_css_class("flat");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_bottom(12);
    body.append(&entry);
    body.append(&status);
    body.append(&scroll);
    // GIPHY asks apps to say where the GIFs come from.
    let foot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    foot.append(&crate::ui::label(
        "Powered by GIPHY",
        &["caption", "dim-label"],
    ));
    foot.append(&gtk::Box::builder().hexpand(true).build());
    foot.append(&own);
    body.append(&foot);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&body));
    d.set_child(Some(&view));

    let pick: Rc<dyn Fn(Gif)> = Rc::new(pick);
    let generation = Rc::new(Cell::new(0u32));
    let run = {
        let (grid, status, d) = (grid.clone(), status.clone(), d.clone());
        Rc::new(move |query: String| {
            let n = generation.get().wrapping_add(1);
            generation.set(n);
            status.set_label("Searching…");
            status.set_visible(true);
            while let Some(c) = grid.first_child() {
                grid.remove(&c);
            }
            let (key, generation, grid, status, d, pick) = (
                key.clone(),
                generation.clone(),
                grid.clone(),
                status.clone(),
                d.clone(),
                pick.clone(),
            );
            bg(
                move || search(&key, &query),
                move |found| {
                    // A newer search is under way: this answer is stale.
                    if generation.get() != n {
                        return;
                    }
                    match found {
                        Ok(gifs) if gifs.is_empty() => status.set_label("No GIFs found"),
                        Ok(gifs) => {
                            status.set_visible(false);
                            for g in gifs {
                                grid.append(&tile(&g, &d, &pick));
                            }
                        }
                        Err(e) => status.set_label(&format!("Couldn't reach GIPHY: {e}")),
                    }
                },
            );
        })
    };
    let r = run.clone();
    entry.connect_search_changed(move |e| r(e.text().to_string()));
    let dd = d.clone();
    own.connect_clicked(move |_| {
        dd.close();
        files();
    });
    run(String::new());
    d.present(Some(parent));
    entry.grab_focus();
}

/// One GIF's preview, loaded in the background; a click picks it.
fn tile(g: &Gif, d: &adw::Dialog, pick: &Rc<dyn Fn(Gif)>) -> gtk::Button {
    let pic = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .height_request(110)
        .build();
    let b = gtk::Button::builder()
        .child(&pic)
        .tooltip_text(&g.title)
        .build();
    b.add_css_class("flat");
    b.add_css_class("gif-tile");
    let url = g.preview.clone();
    let p = pic.clone();
    bg(
        move || get(&url, MAX_PREVIEW),
        move |bytes| {
            if let Ok(bytes) = bytes
                && let Ok(t) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
            {
                p.set_paintable(Some(&t));
            }
        },
    );
    let (g, d, pick) = (g.clone(), d.clone(), pick.clone());
    b.connect_clicked(move |_| {
        d.close();
        pick(g.clone());
    });
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https() {
        assert!(get("http://api.giphy.com/v1/gifs/trending", 10).is_err());
    }

    /// `cargo test -p threnody-desktop -- --ignored giphy`: reaches GIPHY.
    #[test]
    #[ignore = "reaches GIPHY over the internet"]
    fn giphy_refuses_a_bad_key() {
        let e = search("not-a-key", "cats").unwrap_err();
        assert!(e.contains("refused") || e.contains("answered 4"), "{e}");
    }
}
