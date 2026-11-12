//! A floating layout with cascade placement.

use crate::geometry::{Point, Rect, Size};
use crate::{OutputId, WindowId};

/// Distance between the origins of consecutively placed windows.
const CASCADE_STEP: i32 = 32;
/// Number of cascade positions before wrapping around.
const CASCADE_SLOTS: i32 = 8;

/// Where a window is, logically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub window: WindowId,
    /// The output the window was placed on, if any output exists.
    pub output: Option<OutputId>,
    /// Position of the window geometry in global logical coordinates.
    pub position: Point,
    /// Size of the window geometry. `None` until the window reports one.
    pub size: Option<Size>,
}

impl Placement {
    /// The rectangle the window occupies, once its size is known.
    pub fn rect(&self) -> Option<Rect> {
        self.size.map(|size| Rect::from_parts(self.position, size))
    }
}

/// What the layout wants a window to be told when it is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigureHint {
    /// The size the layout asks for. `None` lets the client choose.
    pub size: Option<Size>,
    /// Whether the window has the focus.
    pub activated: bool,
}

#[derive(Debug, Clone, Copy)]
struct Output {
    id: OutputId,
    rect: Rect,
}

/// The layout state.
///
/// Windows are kept in stacking order, bottom to top. The focused window,
/// when there is one, is always the topmost.
#[derive(Debug, Default)]
pub struct Layout {
    outputs: Vec<Output>,
    windows: Vec<Placement>,
    focus: Option<WindowId>,
    placed: i32,
}

impl Layout {
    pub fn new() -> Self {
        Self::default()
    }

    // -- outputs ------------------------------------------------------------

    /// Adds an output, or updates its rectangle when it already exists.
    pub fn set_output(&mut self, id: OutputId, rect: Rect) {
        match self.outputs.iter_mut().find(|o| o.id == id) {
            Some(output) => output.rect = rect,
            None => self.outputs.push(Output { id, rect }),
        }
    }

    /// Removes an output. Windows on it move to the first remaining output,
    /// keeping their position relative to the output origin.
    pub fn remove_output(&mut self, id: OutputId) {
        let Some(index) = self.outputs.iter().position(|o| o.id == id) else {
            return;
        };
        let removed = self.outputs.remove(index);
        let fallback = self.outputs.first().copied();
        for window in self.windows.iter_mut().filter(|w| w.output == Some(id)) {
            match fallback {
                Some(output) => {
                    window.output = Some(output.id);
                    window.position = Point::new(
                        window.position.x - removed.rect.x + output.rect.x,
                        window.position.y - removed.rect.y + output.rect.y,
                    );
                }
                None => window.output = None,
            }
        }
    }

    /// The rectangle of an output.
    pub fn output_rect(&self, id: OutputId) -> Option<Rect> {
        self.outputs.iter().find(|o| o.id == id).map(|o| o.rect)
    }

    // -- windows ------------------------------------------------------------

    /// Adds a window on top of the stack and gives it the focus. Returns its
    /// placement. Adding an id that already exists returns the existing
    /// placement unchanged.
    pub fn add_window(&mut self, id: WindowId) -> Placement {
        if let Some(existing) = self.placement(id) {
            return existing;
        }
        let output = self.outputs.first().copied();
        let origin = output.map(|o| o.rect.origin()).unwrap_or_default();
        let slot = self.placed % CASCADE_SLOTS;
        self.placed += 1;
        let placement = Placement {
            window: id,
            output: output.map(|o| o.id),
            position: Point::new(
                origin.x + CASCADE_STEP * (slot + 1),
                origin.y + CASCADE_STEP * (slot + 1),
            ),
            size: None,
        };
        self.windows.push(placement);
        self.focus = Some(id);
        placement
    }

    /// Removes a window. The focus moves to the topmost remaining window.
    pub fn remove_window(&mut self, id: WindowId) {
        self.windows.retain(|w| w.window != id);
        if self.focus == Some(id) {
            self.focus = self.windows.last().map(|w| w.window);
        }
    }

    /// Records the size of the window geometry the client committed.
    /// Returns `true` when the placement changed.
    pub fn set_window_size(&mut self, id: WindowId, size: Size) -> bool {
        match self.windows.iter_mut().find(|w| w.window == id) {
            Some(window) if window.size != Some(size) => {
                window.size = Some(size);
                true
            }
            _ => false,
        }
    }

    /// Moves a window to a position in global logical coordinates. Returns
    /// `true` when the placement changed.
    ///
    /// The window then belongs to the output its top left corner is on. A
    /// window moved to where no output is keeps the output it had.
    pub fn move_window(&mut self, id: WindowId, position: Point) -> bool {
        let output = self
            .outputs
            .iter()
            .find(|o| o.rect.contains(position))
            .map(|o| o.id);
        match self.windows.iter_mut().find(|w| w.window == id) {
            Some(window) if window.position != position => {
                window.position = position;
                if output.is_some() {
                    window.output = output;
                }
                true
            }
            _ => false,
        }
    }

    /// Gives the focus to a window and raises it to the top, or clears the
    /// focus with `None`. Returns `true` when focus or stacking changed.
    /// Unknown ids are ignored.
    pub fn focus_window(&mut self, id: Option<WindowId>) -> bool {
        match id {
            None => {
                let changed = self.focus.is_some();
                self.focus = None;
                changed
            }
            Some(id) => {
                let Some(index) = self.windows.iter().position(|w| w.window == id) else {
                    return false;
                };
                let top = self.windows.len() - 1;
                let changed = self.focus != Some(id) || index != top;
                let window = self.windows.remove(index);
                self.windows.push(window);
                self.focus = Some(id);
                changed
            }
        }
    }

    /// The window that has the focus.
    pub fn focus(&self) -> Option<WindowId> {
        self.focus
    }

    /// The placement of one window.
    pub fn placement(&self, id: WindowId) -> Option<Placement> {
        self.windows.iter().find(|w| w.window == id).copied()
    }

    /// All placements in stacking order, bottom to top.
    pub fn placements(&self) -> &[Placement] {
        &self.windows
    }

    /// What a window should be told when it is configured.
    pub fn configure_hint(&self, id: WindowId) -> ConfigureHint {
        ConfigureHint {
            // The floating layout never dictates a size.
            size: None,
            activated: self.focus == Some(id),
        }
    }

    /// The topmost window whose rectangle contains `point`, with the point
    /// relative to the window origin.
    ///
    /// This is the coarse, rectangle-level answer. The scene refines it with
    /// the real surfaces (input regions, popups, transforms).
    pub fn window_at(&self, point: Point) -> Option<(WindowId, Point)> {
        self.windows.iter().rev().find_map(|w| {
            let rect = w.rect()?;
            rect.contains(point)
                .then(|| (w.window, Point::new(point.x - rect.x, point.y - rect.y)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const OUT: OutputId = OutputId(1);

    fn layout() -> Layout {
        let mut layout = Layout::new();
        layout.set_output(OUT, Rect::new(0, 0, 1280, 720));
        layout
    }

    #[test]
    fn windows_cascade_from_the_output_origin() {
        let mut layout = Layout::new();
        layout.set_output(OUT, Rect::new(100, 50, 1280, 720));
        let a = layout.add_window(WindowId(1));
        let b = layout.add_window(WindowId(2));
        assert_eq!(a.position, Point::new(132, 82));
        assert_eq!(b.position, Point::new(164, 114));
        assert_eq!(a.output, Some(OUT));
        assert_eq!(a.size, None);
        assert_eq!(a.rect(), None);
    }

    #[test]
    fn cascade_wraps_around() {
        let mut layout = layout();
        let first = layout.add_window(WindowId(0)).position;
        for i in 1..CASCADE_SLOTS as u64 {
            layout.add_window(WindowId(i));
        }
        let wrapped = layout.add_window(WindowId(100)).position;
        assert_eq!(wrapped, first);
    }

    #[test]
    fn adding_twice_keeps_the_placement() {
        let mut layout = layout();
        let first = layout.add_window(WindowId(1));
        layout.move_window(WindowId(1), Point::new(5, 6));
        let again = layout.add_window(WindowId(1));
        assert_eq!(again.position, Point::new(5, 6));
        assert_ne!(again.position, first.position);
        assert_eq!(layout.placements().len(), 1);
    }

    #[test]
    fn new_window_takes_focus_and_removal_returns_it() {
        let mut layout = layout();
        layout.add_window(WindowId(1));
        layout.add_window(WindowId(2));
        assert_eq!(layout.focus(), Some(WindowId(2)));
        assert!(layout.configure_hint(WindowId(2)).activated);
        assert!(!layout.configure_hint(WindowId(1)).activated);
        layout.remove_window(WindowId(2));
        assert_eq!(layout.focus(), Some(WindowId(1)));
        layout.remove_window(WindowId(1));
        assert_eq!(layout.focus(), None);
    }

    #[test]
    fn focusing_raises_the_window() {
        let mut layout = layout();
        layout.add_window(WindowId(1));
        layout.add_window(WindowId(2));
        assert!(layout.focus_window(Some(WindowId(1))));
        let order: Vec<_> = layout.placements().iter().map(|p| p.window).collect();
        assert_eq!(order, [WindowId(2), WindowId(1)]);
        assert!(
            !layout.focus_window(Some(WindowId(1))),
            "no change the second time"
        );
        assert!(!layout.focus_window(Some(WindowId(99))), "unknown window");
        assert!(layout.focus_window(None));
        assert_eq!(layout.focus(), None);
    }

    #[test]
    fn window_at_prefers_the_topmost_window() {
        let mut layout = layout();
        layout.add_window(WindowId(1));
        layout.add_window(WindowId(2));
        layout.set_window_size(WindowId(1), Size::new(100, 100));
        layout.set_window_size(WindowId(2), Size::new(100, 100));
        layout.move_window(WindowId(1), Point::new(0, 0));
        layout.move_window(WindowId(2), Point::new(50, 50));

        assert_eq!(
            layout.window_at(Point::new(10, 10)),
            Some((WindowId(1), Point::new(10, 10)))
        );
        assert_eq!(
            layout.window_at(Point::new(60, 70)),
            Some((WindowId(2), Point::new(10, 20)))
        );
        assert_eq!(layout.window_at(Point::new(500, 500)), None);

        layout.focus_window(Some(WindowId(1)));
        assert_eq!(
            layout.window_at(Point::new(60, 70)),
            Some((WindowId(1), Point::new(60, 70)))
        );
    }

    #[test]
    fn windows_without_a_size_are_not_hit() {
        let mut layout = layout();
        let placement = layout.add_window(WindowId(1));
        assert_eq!(layout.window_at(placement.position), None);
    }

    #[test]
    fn a_moved_window_belongs_to_the_output_under_its_corner() {
        let mut layout = Layout::new();
        layout.set_output(OutputId(1), Rect::new(0, 0, 800, 600));
        layout.set_output(OutputId(2), Rect::new(800, 0, 640, 480));
        layout.add_window(WindowId(1));
        assert_eq!(
            layout.placement(WindowId(1)).unwrap().output,
            Some(OutputId(1))
        );
        // The last pixel column of the first output.
        layout.move_window(WindowId(1), Point::new(799, 10));
        assert_eq!(
            layout.placement(WindowId(1)).unwrap().output,
            Some(OutputId(1))
        );
        layout.move_window(WindowId(1), Point::new(800, 10));
        assert_eq!(
            layout.placement(WindowId(1)).unwrap().output,
            Some(OutputId(2))
        );
        // Off every output: it stays with the one it was on.
        layout.move_window(WindowId(1), Point::new(5000, 5000));
        assert_eq!(
            layout.placement(WindowId(1)).unwrap().output,
            Some(OutputId(2))
        );
        layout.move_window(WindowId(1), Point::new(-10, 0));
        assert_eq!(
            layout.placement(WindowId(1)).unwrap().output,
            Some(OutputId(2))
        );
        layout.move_window(WindowId(1), Point::new(0, 0));
        assert_eq!(
            layout.placement(WindowId(1)).unwrap().output,
            Some(OutputId(1))
        );
    }

    #[test]
    fn removing_an_output_moves_its_windows() {
        let mut layout = Layout::new();
        layout.set_output(OutputId(1), Rect::new(0, 0, 800, 600));
        layout.set_output(OutputId(2), Rect::new(800, 0, 800, 600));
        layout.add_window(WindowId(1));
        layout.move_window(WindowId(1), Point::new(10, 20));
        layout.remove_output(OutputId(1));
        let p = layout.placement(WindowId(1)).unwrap();
        assert_eq!(p.output, Some(OutputId(2)));
        assert_eq!(p.position, Point::new(810, 20));
        layout.remove_output(OutputId(2));
        assert_eq!(layout.placement(WindowId(1)).unwrap().output, None);
    }

    #[test]
    fn size_and_move_report_changes() {
        let mut layout = layout();
        layout.add_window(WindowId(1));
        assert!(layout.set_window_size(WindowId(1), Size::new(10, 10)));
        assert!(!layout.set_window_size(WindowId(1), Size::new(10, 10)));
        assert!(!layout.set_window_size(WindowId(2), Size::new(10, 10)));
        assert!(layout.move_window(WindowId(1), Point::new(1, 1)));
        assert!(!layout.move_window(WindowId(1), Point::new(1, 1)));
    }

    #[derive(Debug, Clone)]
    enum Op {
        Add(u64),
        Remove(u64),
        Focus(Option<u64>),
        Move(u64, i32, i32),
        Resize(u64, i32, i32),
        SetOutput(u64, i32, i32),
        RemoveOutput(u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        let id = 0..6u64;
        prop_oneof![
            id.clone().prop_map(Op::Add),
            id.clone().prop_map(Op::Remove),
            proptest::option::of(id.clone()).prop_map(Op::Focus),
            (id.clone(), -2000..2000i32, -2000..2000i32).prop_map(|(i, x, y)| Op::Move(i, x, y)),
            (id.clone(), 1..3000i32, 1..3000i32).prop_map(|(i, w, h)| Op::Resize(i, w, h)),
            (0..3u64, -2000..2000i32, -2000..2000i32).prop_map(|(i, x, y)| Op::SetOutput(i, x, y)),
            (0..3u64).prop_map(Op::RemoveOutput),
        ]
    }

    proptest! {
        // Invariants that must hold after any sequence of operations.
        #[test]
        fn invariants_hold(ops in proptest::collection::vec(op(), 0..60)) {
            let mut layout = Layout::new();
            for op in ops {
                match op {
                    Op::Add(i) => { layout.add_window(WindowId(i)); }
                    Op::Remove(i) => layout.remove_window(WindowId(i)),
                    Op::Focus(i) => { layout.focus_window(i.map(WindowId)); }
                    Op::Move(i, x, y) => { layout.move_window(WindowId(i), Point::new(x, y)); }
                    Op::Resize(i, w, h) => { layout.set_window_size(WindowId(i), Size::new(w, h)); }
                    Op::SetOutput(i, x, y) => layout.set_output(OutputId(i), Rect::new(x, y, 800, 600)),
                    Op::RemoveOutput(i) => layout.remove_output(OutputId(i)),
                }

                let ids: Vec<_> = layout.placements().iter().map(|p| p.window).collect();
                let mut unique = ids.clone();
                unique.sort();
                unique.dedup();
                prop_assert_eq!(unique.len(), ids.len(), "duplicate window in the stack");

                if let Some(focus) = layout.focus() {
                    prop_assert_eq!(ids.last().copied(), Some(focus), "focus is not topmost");
                }
                for p in layout.placements() {
                    if let Some(output) = p.output {
                        prop_assert!(layout.output_rect(output).is_some(), "window on a removed output");
                    }
                    if let Some(rect) = p.rect() {
                        // The topmost window under a point inside a window is
                        // some window that contains the point.
                        let (hit, local) = layout.window_at(rect.origin()).expect("point is inside a window");
                        let hit_rect = layout.placement(hit).unwrap().rect().unwrap();
                        prop_assert!(hit_rect.contains(rect.origin()));
                        prop_assert_eq!(local, Point::new(rect.x - hit_rect.x, rect.y - hit_rect.y));
                    }
                }
            }
        }
    }
}
