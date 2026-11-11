//! Link previews: the card a message shows for a link in its text, and
//! the bar above the composer that offers one for a link being written.
//! Nothing is fetched until the user asks: the fetch goes from this
//! computer, so the site sees its address. Receivers never fetch; the
//! card comes with the message.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk::gdk_pixbuf::PixbufLoader;
use gtk::{gdk, glib};
use threnody_ffi::LinkPreviewInfo;

use crate::core::Node;
use crate::ui;

/// Longest side of a card's picture.
const THUMB_SIDE: i32 = 400;

/// Where `url` really goes, in ASCII (as the confirm dialog says).
fn host(url: &str) -> String {
    threnody_core::find_links(url)
        .into_iter()
        .next()
        .map(|l| l.host)
        .unwrap_or_default()
}

/// A message's card: its picture, title, description and site. Clicking
/// it asks before opening the link, as the link itself does.
pub fn card(window: &adw::ApplicationWindow, p: &LinkPreviewInfo) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    b.add_css_class("link-card");
    b.set_overflow(gtk::Overflow::Hidden);
    if !p.image.is_empty()
        && let Ok(t) = gdk::Texture::from_bytes(&glib::Bytes::from(&p.image[..]))
    {
        let pic = gtk::Picture::for_paintable(&t);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.set_can_shrink(true);
        pic.set_size_request(-1, 160);
        b.append(&pic);
    }
    let line = |t: &str, class: &str, lines: i32| {
        if t.is_empty() {
            return;
        }
        let l = ui::label(t, &[class]);
        l.set_wrap(true);
        l.set_lines(lines);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_max_width_chars(50);
        l.set_xalign(0.0);
        b.append(&l);
    };
    line(&p.title, "link-card-title", 2);
    line(&p.description, "link-card-description", 3);
    let site = if p.site.is_empty() {
        host(&p.url)
    } else {
        p.site.clone()
    };
    line(&site, "link-card-site", 1);
    b.set_tooltip_text(Some(&p.url));
    b.set_cursor_from_name(Some("pointer"));
    let click = gtk::GestureClick::new();
    let (w, url) = (window.clone(), p.url.clone());
    click.connect_released(move |g, _, _, _| {
        g.set_state(gtk::EventSequenceState::Claimed);
        ui::confirm_link(&w, &url);
    });
    b.add_controller(click);
    b.upcast()
}

/// A fetched picture as a card's thumbnail: at most [`THUMB_SIDE`] on its
/// longer side (decoded at that size, so a huge one never decodes whole),
/// a JPEG within what a message carries. Empty if it can't be.
fn shrink(raw: &[u8]) -> Vec<u8> {
    if raw.is_empty() {
        return Vec::new();
    }
    let loader = PixbufLoader::new();
    loader.connect_size_prepared(|l, w, h| {
        let side = w.max(h);
        if side > THUMB_SIDE {
            let scale = f64::from(THUMB_SIDE) / f64::from(side);
            l.set_size(
                ((f64::from(w) * scale) as i32).max(1),
                ((f64::from(h) * scale) as i32).max(1),
            );
        }
    });
    if loader.write(raw).is_err() || loader.close().is_err() {
        return Vec::new();
    }
    let Some(pixbuf) = loader.pixbuf() else {
        return Vec::new();
    };
    let limit = threnody_ffi::max_preview_image() as usize;
    for quality in ["80", "65", "50", "35"] {
        if let Ok(jpeg) = pixbuf.save_to_bufferv("jpeg", &[("quality", quality)])
            && jpeg.len() <= limit
        {
            return jpeg;
        }
    }
    Vec::new()
}

/// Gives the node to fetch with, when there is one.
type NodeSource = Box<dyn Fn() -> Option<Node>>;

#[derive(Default)]
struct State {
    /// The link on offer or shown.
    url: Option<String>,
    card: Option<LinkPreviewInfo>,
    fetching: bool,
    /// Links dropped, or with nothing to show: not offered again.
    dropped: HashSet<String>,
}

/// Above the composer: offers a card for the first link being written,
/// fetches it when asked, and shows it with a button to drop it.
pub struct PreviewBar {
    pub widget: gtk::Box,
    state: RefCell<State>,
    /// The node to fetch with (see [`PreviewBar::set_node`]).
    node: RefCell<Option<NodeSource>>,
}

impl PreviewBar {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        widget.add_css_class("preview-bar");
        widget.set_visible(false);
        Rc::new(Self {
            widget,
            state: RefCell::new(State::default()),
            node: RefCell::new(None),
        })
    }

    /// Where fetches get the node from (the chat's, when asked).
    pub fn set_node(&self, node: impl Fn() -> Option<Node> + 'static) {
        *self.node.borrow_mut() = Some(Box::new(node));
    }

    /// Follows the composer.
    pub fn on_text(self: &Rc<Self>, text: &str) {
        let first = {
            let s = self.state.borrow();
            threnody_core::find_links(text)
                .into_iter()
                .map(|l| l.url)
                .find(|u| !s.dropped.contains(u))
        };
        {
            let mut s = self.state.borrow_mut();
            if first == s.url {
                return;
            }
            s.url = first;
            s.card = None;
            s.fetching = false;
        }
        self.show();
    }

    /// The card to send with `text`, if its link is still there; resets.
    pub fn take(self: &Rc<Self>, text: &str) -> Option<LinkPreviewInfo> {
        let card = {
            let mut s = std::mem::take(&mut *self.state.borrow_mut());
            s.card.take().filter(|c| {
                threnody_core::find_links(text)
                    .iter()
                    .any(|l| l.url == c.url)
            })
        };
        self.show();
        card
    }

    fn show(self: &Rc<Self>) {
        while let Some(c) = self.widget.first_child() {
            self.widget.remove(&c);
        }
        let s = self.state.borrow();
        let Some(url) = s.url.clone() else {
            self.widget.set_visible(false);
            return;
        };
        let h = host(&url);
        if let Some(c) = &s.card {
            let what = [&c.title, &c.description]
                .into_iter()
                .find(|t| !t.is_empty())
                .cloned()
                .unwrap_or_else(|| h.clone());
            let l = ui::label(&format!("🔗 {what}"), &[]);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            l.set_hexpand(true);
            l.set_xalign(0.0);
            self.widget.append(&l);
        } else if s.fetching {
            let l = ui::label(&format!("Getting a preview of {h}…"), &["dim-label"]);
            l.set_hexpand(true);
            l.set_xalign(0.0);
            self.widget.append(&l);
        } else {
            let b = gtk::Button::with_label(&format!("Add a preview of {h}"));
            b.add_css_class("flat");
            b.set_hexpand(true);
            b.set_halign(gtk::Align::Start);
            b.set_tooltip_text(Some(
                "Fetches the page from this computer: the site sees your network address",
            ));
            let weak = Rc::downgrade(self);
            let u = url.clone();
            b.connect_clicked(move |_| {
                if let Some(t) = weak.upgrade() {
                    t.fetch(u.clone());
                }
            });
            self.widget.append(&b);
        }
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_tooltip_text(Some(if s.card.is_some() {
            "Remove the preview"
        } else {
            "Don't add a preview"
        }));
        let weak = Rc::downgrade(self);
        close.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.drop_link(&url);
            }
        });
        self.widget.append(&close);
        self.widget.set_visible(true);
    }

    fn drop_link(self: &Rc<Self>, url: &str) {
        {
            let mut s = self.state.borrow_mut();
            s.dropped.insert(url.to_owned());
            s.url = None;
            s.card = None;
            s.fetching = false;
        }
        self.show();
    }

    fn fetch(self: &Rc<Self>, url: String) {
        let Some(node) = self.node.borrow().as_ref().and_then(|n| n()) else {
            return;
        };
        self.state.borrow_mut().fetching = true;
        self.show();
        let weak = Rc::downgrade(self);
        let u = url.clone();
        ui::bg(
            move || {
                node.fetch_link_preview(u).map(|mut p| {
                    p.image = shrink(&p.image);
                    p
                })
            },
            move |got| {
                let Some(t) = weak.upgrade() else { return };
                if t.state.borrow().url.as_deref() != Some(url.as_str()) {
                    return;
                }
                match got {
                    Some(p) => {
                        {
                            let mut s = t.state.borrow_mut();
                            s.fetching = false;
                            s.card = Some(p);
                        }
                        t.show();
                    }
                    None => t.drop_link(&url),
                }
            },
        );
    }
}
