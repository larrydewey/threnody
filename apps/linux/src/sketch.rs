//! What the photo editor draws, and its undo: strokes and text labels in
//! photo pixels, as the Android editor keeps them. No GTK here.

pub type Colour = u32;

/// The editor's colours (0xRRGGBB), the Android editor's.
pub const COLOURS: [Colour; 8] = [
    0xFF_FFFF, 0x00_0000, 0xE5_3935, 0xFB_8C00, 0xFD_D835, 0x43_A047, 0x1E_88E5, 0x8E_24AA,
];
/// Pen widths, in screen pixels: thin, medium, thick.
pub const PEN_PX: [f64; 3] = [3.0, 6.0, 12.0];

#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    pub points: Vec<(f64, f64)>,
    pub colour: Colour,
    pub width: f64,
}

/// Text, centred on (`x`, `y`), `size` pixels high.
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub text: String,
    pub colour: Colour,
    pub x: f64,
    pub y: f64,
    pub size: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    Stroke(Stroke),
    Label(Label),
}

/// How to take one change back. Indices stay valid because undoing goes
/// newest first.
#[derive(Debug)]
enum Undo {
    /// A mark was added at this index.
    Remove(usize),
    /// This mark was removed from this index.
    Restore(usize, Mark),
    Recolour(usize, Colour),
    Retext(usize, String),
    /// A label was moved or resized from (x, y, size).
    Place(usize, f64, f64, f64),
}

#[derive(Default, Debug)]
pub struct Sketch {
    marks: Vec<Mark>,
    undos: Vec<Undo>,
    /// The stroke being drawn.
    open: Option<usize>,
    /// The label being moved, changed or recoloured.
    pub selected: Option<usize>,
    /// Where the label being placed started: (x, y, size).
    before: Option<(f64, f64, f64)>,
}

impl Sketch {
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    pub fn can_undo(&self) -> bool {
        !self.undos.is_empty()
    }

    pub fn begin_stroke(&mut self, x: f64, y: f64, colour: Colour, width: f64) {
        self.cancel_stroke();
        self.marks.push(Mark::Stroke(Stroke {
            points: vec![(x, y)],
            colour,
            width,
        }));
        self.open = Some(self.marks.len() - 1);
    }

    fn open_stroke(&mut self) -> Option<&mut Stroke> {
        match self.marks.get_mut(self.open?) {
            Some(Mark::Stroke(s)) => Some(s),
            _ => None,
        }
    }

    pub fn extend_stroke(&mut self, x: f64, y: f64) {
        if let Some(s) = self.open_stroke() {
            s.points.push((x, y));
        }
    }

    /// Finishes the stroke; a tap leaves a dot.
    pub fn end_stroke(&mut self, x: f64, y: f64) {
        let Some(s) = self.open_stroke() else { return };
        if s.points.len() == 1 && s.points[0] == (x, y) {
            s.points.push((x + 0.1, y));
        } else {
            s.points.push((x, y));
        }
        if let Some(i) = self.open.take() {
            self.undos.push(Undo::Remove(i));
        }
    }

    pub fn cancel_stroke(&mut self) {
        if let Some(i) = self.open.take() {
            self.marks.remove(i);
        }
    }

    fn label_mut(&mut self, i: usize) -> Option<&mut Label> {
        match self.marks.get_mut(i) {
            Some(Mark::Label(l)) => Some(l),
            _ => None,
        }
    }

    pub fn label(&self, i: usize) -> Option<&Label> {
        match self.marks.get(i) {
            Some(Mark::Label(l)) => Some(l),
            _ => None,
        }
    }

    /// Adds text (and selects it); empty text adds nothing.
    pub fn add_label(
        &mut self,
        text: &str,
        colour: Colour,
        x: f64,
        y: f64,
        size: f64,
    ) -> Option<usize> {
        if text.is_empty() {
            return None;
        }
        self.marks.push(Mark::Label(Label {
            text: text.to_owned(),
            colour,
            x,
            y,
            size,
        }));
        let i = self.marks.len() - 1;
        self.undos.push(Undo::Remove(i));
        self.selected = Some(i);
        Some(i)
    }

    /// Changes a label's text; empty text removes the label.
    pub fn set_text(&mut self, i: usize, text: &str) {
        let Some(l) = self.label_mut(i) else { return };
        if text.is_empty() {
            let m = self.marks.remove(i);
            self.undos.push(Undo::Restore(i, m));
            self.selected = match self.selected {
                Some(s) if s == i => None,
                Some(s) if s > i => Some(s - 1),
                s => s,
            };
        } else if l.text != text {
            let old = std::mem::replace(&mut l.text, text.to_owned());
            self.undos.push(Undo::Retext(i, old));
        }
    }

    pub fn recolour_selected(&mut self, colour: Colour) {
        let Some(i) = self.selected else { return };
        let Some(l) = self.label_mut(i) else { return };
        if l.colour != colour {
            let old = std::mem::replace(&mut l.colour, colour);
            self.undos.push(Undo::Recolour(i, old));
        }
    }

    /// Starts moving or resizing label `i`.
    pub fn begin_place(&mut self, i: usize) {
        self.before = self.label(i).map(|l| (l.x, l.y, l.size));
    }

    /// Puts label `i` `dx`, `dy` from where it started, `scale` times its
    /// size then (kept between `min` and `max`).
    pub fn place(&mut self, i: usize, dx: f64, dy: f64, scale: f64, min: f64, max: f64) {
        let Some((x, y, size)) = self.before else {
            return;
        };
        if let Some(l) = self.label_mut(i) {
            l.x = x + dx;
            l.y = y + dy;
            l.size = (size * scale).clamp(min, max.max(min));
        }
    }

    /// Finishes a move; one that changed nothing isn't recorded.
    pub fn end_place(&mut self, i: usize) {
        let Some((x, y, size)) = self.before.take() else {
            return;
        };
        if self
            .label(i)
            .is_some_and(|l| (l.x, l.y, l.size) != (x, y, size))
        {
            self.undos.push(Undo::Place(i, x, y, size));
        }
    }

    /// The topmost label at (`x`, `y`), `pad` around each counting;
    /// `extent` gives a label's width and height.
    pub fn label_at(
        &self,
        x: f64,
        y: f64,
        pad: f64,
        extent: impl Fn(&Label) -> (f64, f64),
    ) -> Option<usize> {
        self.marks.iter().enumerate().rev().find_map(|(i, m)| {
            let Mark::Label(l) = m else { return None };
            let (w, h) = extent(l);
            ((x - l.x).abs() <= w / 2.0 + pad && (y - l.y).abs() <= h / 2.0 + pad).then_some(i)
        })
    }

    /// Takes back the newest change.
    pub fn undo(&mut self) -> bool {
        self.cancel_stroke();
        self.before = None;
        let Some(u) = self.undos.pop() else {
            return false;
        };
        match u {
            Undo::Remove(i) => {
                if i < self.marks.len() {
                    self.marks.remove(i);
                }
                if let Some(s) = self.selected
                    && s > i
                {
                    self.selected = Some(s - 1);
                }
            }
            Undo::Restore(i, m) => {
                self.marks.insert(i.min(self.marks.len()), m);
                if let Some(s) = self.selected
                    && s >= i
                {
                    self.selected = Some(s + 1);
                }
            }
            Undo::Recolour(i, c) => {
                if let Some(l) = self.label_mut(i) {
                    l.colour = c;
                }
            }
            Undo::Retext(i, t) => {
                if let Some(l) = self.label_mut(i) {
                    l.text = t;
                }
            }
            Undo::Place(i, x, y, size) => {
                if let Some(l) = self.label_mut(i) {
                    (l.x, l.y, l.size) = (x, y, size);
                }
            }
        }
        if self.selected.is_some_and(|s| self.label(s).is_none()) {
            self.selected = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(s: &mut Sketch, from: (f64, f64), to: (f64, f64)) {
        s.begin_stroke(from.0, from.1, 0xE5_3935, 4.0);
        s.extend_stroke((from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
        s.end_stroke(to.0, to.1);
    }

    fn label(s: &Sketch, i: usize) -> Label {
        s.label(i).cloned().unwrap()
    }

    #[test]
    fn strokes_undo_newest_first() {
        let mut s = Sketch::default();
        stroke(&mut s, (0.0, 0.0), (10.0, 10.0));
        stroke(&mut s, (5.0, 5.0), (20.0, 0.0));
        assert_eq!(s.marks().len(), 2);
        let Mark::Stroke(first) = s.marks()[0].clone() else {
            panic!()
        };
        assert!(s.undo());
        assert_eq!(s.marks(), &[Mark::Stroke(first)]);
        assert!(s.undo());
        assert!(s.marks().is_empty());
        assert!(!s.can_undo());
        assert!(!s.undo());
    }

    #[test]
    fn a_tap_leaves_a_dot() {
        let mut s = Sketch::default();
        s.begin_stroke(3.0, 4.0, 0, 2.0);
        s.end_stroke(3.0, 4.0);
        let Mark::Stroke(st) = &s.marks()[0] else {
            panic!()
        };
        assert_eq!(st.points.len(), 2);
        assert!(s.can_undo());
    }

    #[test]
    fn a_cancelled_stroke_leaves_nothing_to_undo() {
        let mut s = Sketch::default();
        s.begin_stroke(1.0, 1.0, 0, 2.0);
        s.extend_stroke(2.0, 2.0);
        s.cancel_stroke();
        assert!(s.marks().is_empty());
        assert!(!s.can_undo());
    }

    #[test]
    fn empty_text_adds_no_label() {
        let mut s = Sketch::default();
        assert_eq!(s.add_label("", 0, 1.0, 1.0, 20.0), None);
        assert!(s.marks().is_empty() && !s.can_undo());
    }

    #[test]
    fn removing_a_label_undoes_back_in_place() {
        let mut s = Sketch::default();
        stroke(&mut s, (0.0, 0.0), (1.0, 1.0));
        let i = s.add_label("hello", 0xFF_FFFF, 50.0, 60.0, 30.0).unwrap();
        stroke(&mut s, (2.0, 2.0), (3.0, 3.0));
        let before = s.marks().to_vec();
        s.set_text(i, "");
        assert_eq!(s.marks().len(), 2);
        assert_eq!(s.selected, None);
        assert!(s.undo());
        assert_eq!(s.marks(), &before[..]);
        assert_eq!(label(&s, 1).text, "hello");
    }

    #[test]
    fn recolour_and_retext_undo() {
        let mut s = Sketch::default();
        let i = s.add_label("one", 0xFF_FFFF, 0.0, 0.0, 20.0).unwrap();
        s.recolour_selected(0x1E_88E5);
        s.set_text(i, "two");
        s.recolour_selected(0x43_A047);
        // Recolouring to the same colour changes nothing.
        s.recolour_selected(0x43_A047);
        assert_eq!(
            (label(&s, i).text.as_str(), label(&s, i).colour),
            ("two", 0x43_A047)
        );
        s.undo();
        assert_eq!(label(&s, i).colour, 0x1E_88E5);
        s.undo();
        assert_eq!(label(&s, i).text, "one");
        s.undo();
        assert_eq!(label(&s, i).colour, 0xFF_FFFF);
    }

    #[test]
    fn moves_undo_to_where_they_started() {
        let mut s = Sketch::default();
        let i = s.add_label("x", 0, 10.0, 20.0, 30.0).unwrap();
        s.begin_place(i);
        s.place(i, 5.0, 5.0, 1.0, 1.0, 100.0);
        s.place(i, 15.0, -5.0, 2.0, 1.0, 100.0);
        s.end_place(i);
        assert_eq!(
            (label(&s, i).x, label(&s, i).y, label(&s, i).size),
            (25.0, 15.0, 60.0)
        );
        s.undo();
        assert_eq!(
            (label(&s, i).x, label(&s, i).y, label(&s, i).size),
            (10.0, 20.0, 30.0)
        );
        // A move that went nowhere isn't one.
        s.begin_place(i);
        s.place(i, 0.0, 0.0, 1.0, 1.0, 100.0);
        s.end_place(i);
        s.undo();
        assert!(
            s.marks().is_empty(),
            "only the label's adding was left to undo"
        );
    }

    #[test]
    fn label_size_is_clamped() {
        let mut s = Sketch::default();
        let i = s.add_label("x", 0, 0.0, 0.0, 30.0).unwrap();
        s.begin_place(i);
        s.place(i, 0.0, 0.0, 100.0, 12.0, 160.0);
        assert_eq!(label(&s, i).size, 160.0);
        s.place(i, 0.0, 0.0, 0.01, 12.0, 160.0);
        assert_eq!(label(&s, i).size, 12.0);
        s.end_place(i);
    }

    #[test]
    fn label_at_finds_the_topmost() {
        let mut s = Sketch::default();
        let extent = |l: &Label| (l.text.len() as f64 * 10.0, l.size);
        let a = s.add_label("aaaa", 0, 50.0, 50.0, 20.0).unwrap();
        let b = s.add_label("bb", 0, 60.0, 50.0, 20.0).unwrap();
        stroke(&mut s, (0.0, 0.0), (100.0, 100.0));
        assert_eq!(s.label_at(60.0, 50.0, 0.0, extent), Some(b));
        assert_eq!(s.label_at(32.0, 50.0, 0.0, extent), Some(a));
        assert_eq!(s.label_at(28.0, 50.0, 0.0, extent), None);
        assert_eq!(s.label_at(28.0, 50.0, 3.0, extent), Some(a));
        // Strokes aren't hit.
        assert_eq!(s.label_at(5.0, 5.0, 2.0, extent), None);
    }

    #[test]
    fn undo_clears_a_vanished_selection() {
        let mut s = Sketch::default();
        let i = s.add_label("x", 0, 0.0, 0.0, 20.0);
        assert_eq!(s.selected, i);
        s.undo();
        assert_eq!(s.selected, None);
    }
}
