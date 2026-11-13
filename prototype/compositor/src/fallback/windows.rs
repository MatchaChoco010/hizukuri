//! Window management in the fallback: the table of windows, and the rules
//! the core applies when nobody else decides.
//!
//! The table holds, for every window, its logical position, the size it is
//! configured with, its place in the stacking order, and which window has
//! the focus. [`Windows`] is a [`WindowManager`] by itself: a new window is
//! put in the [cascade](super::placement), on top, with the focus; the
//! focused window is the topmost; a window on an output that goes away moves
//! to the first remaining output, keeping its position relative to the
//! output's origin; when the focused window goes away, the topmost one
//! gets the focus.
//!
//! The script's window management ([`ScriptWm`](crate::script::ScriptWm))
//! edits the same table. While the script is in charge, it decides
//! placement, stacking and focus, and the rules above are not enforced;
//! where the script says nothing (it defines no window management, a
//! callback fails, or the script is out), they are.

use crate::wm::{ConfigureHint, OutputId, Placement, Point, Rect, Size, WindowId};

use super::placement::Cascade;
use crate::wm::{WindowManager, WmRequest};

/// One window of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    placement: Placement,
    /// The size the window is configured with. `None`: the client chooses.
    configure: Option<Size>,
    /// Whether whoever manages the windows said the window is not seen
    /// (see [`Windows::set_visible`]).
    hidden: bool,
    /// Where its popups are kept, relative to the window geometry, when
    /// whoever edits the table said so. `None`: the output it is on.
    popup_bounds: Option<Rect>,
}

/// See the module documentation.
#[derive(Debug, Default)]
pub struct Windows {
    /// Stacking order, bottom to top.
    windows: Vec<Window>,
    focus: Option<WindowId>,
    outputs: Vec<(OutputId, Rect)>,
    cascade: Cascade,
    requests: Vec<WmRequest>,
    /// Bumped by every change of placement, stacking, focus or configured
    /// size.
    revision: u64,
    /// Windows that were removed and that whoever edits the table may not
    /// have heard about yet (see [`Self::departed`]).
    departed: Vec<WindowId>,
}

impl Windows {
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts the changes of the table, so a caller can tell whether
    /// anything changed since it last looked.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn window(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.placement.window == id)
    }

    pub fn contains(&self, id: WindowId) -> bool {
        self.windows.iter().any(|w| w.placement.window == id)
    }

    /// Whether a window was removed and not yet forgotten
    /// ([`Self::forget_departed`]). A script that manages the windows keeps
    /// its own lists and learns of a removal later than the table does:
    /// until then its requests about the window are not mistakes.
    pub fn departed(&self, id: WindowId) -> bool {
        self.departed.contains(&id)
    }

    /// The removal of a window was passed on.
    pub fn forget_departed(&mut self, id: WindowId) {
        self.departed.retain(|departed| *departed != id);
    }

    /// The first output, which new windows are put on.
    fn first_output(&self) -> Option<(OutputId, Rect)> {
        self.outputs.first().copied()
    }

    // -- outputs ----------------------------------------------------------------

    /// Adds an output, or updates its rectangle when it already exists.
    pub fn set_output(&mut self, id: OutputId, rect: Rect) {
        match self.outputs.iter_mut().find(|(o, _)| *o == id) {
            Some(output) => output.1 = rect,
            None => self.outputs.push((id, rect)),
        }
    }

    /// Removes an output. Its windows move to the first remaining output,
    /// keeping their position relative to the output's origin, so that they
    /// stay reachable. Without another output they belong to none.
    pub fn remove_output(&mut self, id: OutputId) {
        let Some(index) = self.outputs.iter().position(|(o, _)| *o == id) else {
            return;
        };
        let (_, removed) = self.outputs.remove(index);
        let remaining = self.first_output();
        for window in self
            .windows
            .iter_mut()
            .filter(|w| w.placement.output == Some(id))
        {
            let placement = &mut window.placement;
            match remaining {
                Some((output, rect)) => {
                    placement.output = Some(output);
                    placement.position = Point::new(
                        placement.position.x - removed.x + rect.x,
                        placement.position.y - removed.y + rect.y,
                    );
                }
                None => placement.output = None,
            }
            self.revision += 1;
        }
    }

    // -- windows ----------------------------------------------------------------

    /// Adds a window on top of the stack, at the next place of the cascade,
    /// without giving it the focus. Adding a window that exists returns its
    /// placement unchanged.
    pub fn insert(&mut self, id: WindowId) -> Placement {
        if let Some(existing) = self.window(id) {
            return existing.placement;
        }
        let placement = self.cascade.place(id, self.first_output());
        self.windows.push(Window {
            placement,
            configure: None,
            hidden: false,
            popup_bounds: None,
        });
        self.revision += 1;
        placement
    }

    /// Removes a window. When it had the focus, the topmost remaining
    /// window gets it. Returns `true` when the focus moved.
    pub fn remove(&mut self, id: WindowId) -> bool {
        let before = self.windows.len();
        self.windows.retain(|w| w.placement.window != id);
        if self.windows.len() != before {
            self.revision += 1;
            // Bounded, for a table nobody reports removals from.
            if self.departed.len() >= 256 {
                self.departed.remove(0);
            }
            self.departed.push(id);
        }
        if self.focus == Some(id) {
            self.focus = self.windows.last().map(|w| w.placement.window);
            self.revision += 1;
            true
        } else {
            false
        }
    }

    /// Records the size of the window geometry the client committed.
    /// Returns `true` when the placement changed.
    pub fn set_size(&mut self, id: WindowId, size: Size) -> bool {
        match self.window(id) {
            Some(window) if window.placement.size != Some(size) => {
                window.placement.size = Some(size);
                self.revision += 1;
                true
            }
            _ => false,
        }
    }

    /// Moves a window (global logical coordinates). It then belongs to the
    /// output its top left corner is on; a window moved to where no output
    /// is keeps the output it had. Returns `true` when it moved.
    pub fn place(&mut self, id: WindowId, position: Point) -> bool {
        let output = self
            .outputs
            .iter()
            .find(|(_, rect)| rect.contains(position))
            .map(|(id, _)| *id);
        match self.window(id) {
            Some(window) if window.placement.position != position => {
                window.placement.position = position;
                if output.is_some() {
                    window.placement.output = output;
                }
                self.revision += 1;
                true
            }
            _ => false,
        }
    }

    /// Sets the size a window is asked to have; `None` lets the client
    /// choose. Returns `false` for an unknown window.
    pub fn configure(&mut self, id: WindowId, size: Option<Size>) -> bool {
        let Some(window) = self.window(id) else {
            return false;
        };
        if window.configure != size {
            window.configure = size;
            self.revision += 1;
        }
        true
    }

    /// Sets where the popups of a window are kept (relative to its window
    /// geometry); `None` puts it back to the output the window is on.
    /// Returns `false` for an unknown window.
    pub fn set_popup_bounds(&mut self, id: WindowId, bounds: Option<Rect>) -> bool {
        let Some(window) = self.window(id) else {
            return false;
        };
        if window.popup_bounds != bounds {
            window.popup_bounds = bounds;
            self.revision += 1;
        }
        true
    }

    /// Puts every window's popups back to the output it is on.
    pub fn clear_popup_bounds(&mut self) {
        for window in &mut self.windows {
            if window.popup_bounds.take().is_some() {
                self.revision += 1;
            }
        }
    }

    /// Gives the focus to a window, or to nothing, without changing the
    /// stacking order. Returns `true` when the focus changed; unknown
    /// windows are ignored.
    pub fn set_focus(&mut self, id: Option<WindowId>) -> bool {
        if id.is_some_and(|id| !self.contains(id)) || self.focus == id {
            return false;
        }
        self.focus = id;
        self.revision += 1;
        true
    }

    /// Moves a window to the top of the stacking order. Returns `true` when
    /// the order changed.
    pub fn raise(&mut self, id: WindowId) -> bool {
        match self.windows.iter().position(|w| w.placement.window == id) {
            Some(index) if index + 1 != self.windows.len() => {
                let window = self.windows.remove(index);
                self.windows.push(window);
                self.revision += 1;
                true
            }
            _ => false,
        }
    }

    /// Raises the focused window, so that the window that gets the keys is
    /// the one in front. Returns `true` when the order changed.
    pub fn raise_focused(&mut self) -> bool {
        match self.focus {
            Some(id) => self.raise(id),
            None => false,
        }
    }

    /// Sets the stacking order, bottom to top. Windows that are not listed
    /// stay below the listed ones, in their current order. Unknown ids are
    /// ignored; listing a window twice counts its first place.
    pub fn restack(&mut self, order: &[WindowId]) {
        let before: Vec<WindowId> = self.windows.iter().map(|w| w.placement.window).collect();
        let listed = |w: &Window| order.contains(&w.placement.window);
        let mut stack: Vec<Window> = self
            .windows
            .iter()
            .copied()
            .filter(|w| !listed(w))
            .collect();
        for id in order {
            if let Some(window) = self.windows.iter().find(|w| w.placement.window == *id)
                && !stack.iter().any(|w| w.placement.window == *id)
            {
                stack.push(*window);
            }
        }
        self.windows = stack;
        if self.windows.iter().map(|w| w.placement.window).ne(before) {
            self.revision += 1;
        }
    }

    /// Says whether a window is seen. Every window is, until it is said
    /// otherwise; the core then holds its frame callbacks and tells its
    /// client it is suspended. Not a change of the layout: the revision
    /// stays. Returns `false` when the window is unknown.
    pub fn set_visible(&mut self, id: WindowId, visible: bool) -> bool {
        match self.window(id) {
            Some(window) => {
                window.hidden = !visible;
                true
            }
            None => false,
        }
    }

    /// Whether a window is seen (see [`Self::set_visible`]). An unknown
    /// window is.
    pub fn is_visible(&self, id: WindowId) -> bool {
        !self
            .windows
            .iter()
            .any(|w| w.placement.window == id && w.hidden)
    }

    /// Every window is seen again: for a new script, which says again what
    /// it hides.
    pub fn show_all(&mut self) {
        for window in &mut self.windows {
            window.hidden = false;
        }
    }

    /// Asks the client of a window to close it.
    pub fn request_close(&mut self, id: WindowId) {
        self.requests.push(WmRequest::Close(id));
    }

    /// Queues something else for the core to do (see [`WmRequest`]).
    pub fn request(&mut self, request: WmRequest) {
        self.requests.push(request);
    }
}

impl WindowManager for Windows {
    fn name(&self) -> &'static str {
        "fallback"
    }

    fn set_output(&mut self, id: OutputId, rect: Rect) {
        Windows::set_output(self, id, rect);
    }

    fn remove_output(&mut self, id: OutputId) {
        Windows::remove_output(self, id);
    }

    /// The new window goes on top and gets the focus.
    fn add_window(&mut self, id: WindowId) -> Placement {
        if let Some(existing) = self.window(id) {
            return existing.placement;
        }
        let placement = self.insert(id);
        self.set_focus(Some(id));
        placement
    }

    fn remove_window(&mut self, id: WindowId) {
        self.remove(id);
    }

    fn set_window_size(&mut self, id: WindowId, size: Size) -> bool {
        self.set_size(id, size)
    }

    fn move_window(&mut self, id: WindowId, position: Point) -> bool {
        self.place(id, position)
    }

    /// Focuses and raises a window, or clears the focus.
    fn focus_window(&mut self, id: Option<WindowId>) -> bool {
        match id {
            Some(id) if !self.contains(id) => false,
            Some(id) => {
                let focused = self.set_focus(Some(id));
                let raised = self.raise(id);
                focused || raised
            }
            None => self.set_focus(None),
        }
    }

    fn focus(&self) -> Option<WindowId> {
        self.focus
    }

    fn placement(&self, id: WindowId) -> Option<Placement> {
        self.windows
            .iter()
            .find(|w| w.placement.window == id)
            .map(|w| w.placement)
    }

    fn placements(&self) -> Vec<Placement> {
        self.windows.iter().map(|w| w.placement).collect()
    }

    fn configure_hint(&self, id: WindowId) -> ConfigureHint {
        ConfigureHint {
            size: self
                .windows
                .iter()
                .find(|w| w.placement.window == id)
                .and_then(|w| w.configure),
            activated: self.focus == Some(id),
        }
    }

    fn popup_bounds(&self, id: WindowId) -> Option<Rect> {
        self.windows
            .iter()
            .find(|w| w.placement.window == id)
            .and_then(|w| w.popup_bounds)
    }

    fn take_requests(&mut self) -> Vec<WmRequest> {
        std::mem::take(&mut self.requests)
    }

    fn is_visible(&self, id: WindowId) -> bool {
        Windows::is_visible(self, id)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    const OUT: OutputId = OutputId(1);

    fn windows() -> Windows {
        let mut windows = Windows::new();
        windows.set_output(OUT, Rect::new(0, 0, 1280, 720));
        windows
    }

    fn order(windows: &Windows) -> Vec<WindowId> {
        windows.placements().iter().map(|p| p.window).collect()
    }

    #[test]
    fn a_new_window_is_on_top_with_the_focus() {
        let mut windows = windows();
        windows.add_window(WindowId(1));
        windows.add_window(WindowId(2));
        assert_eq!(windows.focus(), Some(WindowId(2)));
        assert_eq!(order(&windows), [WindowId(1), WindowId(2)]);
        assert!(windows.configure_hint(WindowId(2)).activated);
        assert!(!windows.configure_hint(WindowId(1)).activated);
        assert_eq!(windows.configure_hint(WindowId(2)).size, None);
    }

    #[test]
    fn adding_twice_keeps_the_placement() {
        let mut windows = windows();
        windows.add_window(WindowId(1));
        windows.move_window(WindowId(1), Point::new(5, 6));
        let again = windows.add_window(WindowId(1));
        assert_eq!(again.position, Point::new(5, 6));
        assert_eq!(windows.placements().len(), 1);
    }

    #[test]
    fn focusing_raises_and_removal_hands_the_focus_on() {
        let mut windows = windows();
        windows.add_window(WindowId(1));
        windows.add_window(WindowId(2));
        assert!(windows.focus_window(Some(WindowId(1))));
        assert_eq!(order(&windows), [WindowId(2), WindowId(1)]);
        assert!(!windows.focus_window(Some(WindowId(1))), "no change");
        assert!(!windows.focus_window(Some(WindowId(9))), "unknown window");
        windows.remove_window(WindowId(1));
        assert_eq!(windows.focus(), Some(WindowId(2)));
        assert!(windows.focus_window(None));
        assert_eq!(windows.focus(), None);
    }

    #[test]
    fn the_focused_window_can_be_raised_after_the_fact() {
        let mut windows = windows();
        windows.add_window(WindowId(1));
        windows.add_window(WindowId(2));
        // What a script may do: focus without raising.
        windows.set_focus(Some(WindowId(1)));
        assert_eq!(order(&windows), [WindowId(1), WindowId(2)]);
        assert!(windows.raise_focused());
        assert_eq!(order(&windows), [WindowId(2), WindowId(1)]);
        assert!(!windows.raise_focused());
    }

    #[test]
    fn windows_of_a_removed_output_move_to_the_remaining_one() {
        let mut windows = Windows::new();
        windows.set_output(OutputId(1), Rect::new(0, 0, 800, 600));
        windows.set_output(OutputId(2), Rect::new(800, 0, 800, 600));
        windows.add_window(WindowId(1));
        windows.move_window(WindowId(1), Point::new(10, 20));
        windows.remove_output(OutputId(1));
        let p = windows.placement(WindowId(1)).unwrap();
        assert_eq!(p.output, Some(OutputId(2)));
        assert_eq!(p.position, Point::new(810, 20));
        windows.remove_output(OutputId(2));
        assert_eq!(windows.placement(WindowId(1)).unwrap().output, None);
    }

    #[test]
    fn restacking_keeps_unlisted_windows_below() {
        let mut windows = windows();
        for id in 1..=4 {
            windows.add_window(WindowId(id));
        }
        windows.restack(&[WindowId(2), WindowId(1), WindowId(9), WindowId(2)]);
        assert_eq!(
            order(&windows),
            [WindowId(3), WindowId(4), WindowId(2), WindowId(1)]
        );
        let revision = windows.revision();
        windows.restack(&[WindowId(2), WindowId(1)]);
        assert_eq!(windows.revision(), revision, "no change, no revision");
    }

    /// One sequence of events with two outputs, checked step by step.
    #[test]
    fn a_sequence_of_events_on_two_outputs() {
        let mut windows = Windows::new();
        windows.set_output(OutputId(1), Rect::new(-1920, 0, 1920, 1080));
        windows.set_output(OutputId(2), Rect::new(0, 0, 1280, 720));
        let place = |windows: &Windows, id: u64| windows.placement(WindowId(id)).unwrap();
        let ids = |list: &[u64]| list.iter().map(|id| WindowId(*id)).collect::<Vec<_>>();

        // Ten windows cascade on the first output; the last one is focused.
        for id in 1..=10 {
            windows.add_window(WindowId(id));
            windows.set_window_size(WindowId(id), Size::new(100 + id as i32, 80));
        }
        for id in 1..=10u64 {
            let slot = (id as i32 - 1) % 8 + 1;
            let p = place(&windows, id);
            assert_eq!(p.output, Some(OutputId(1)), "window {id}");
            assert_eq!(p.position, Point::new(-1920 + 32 * slot, 32 * slot));
            assert_eq!(p.size, Some(Size::new(100 + id as i32, 80)));
            assert_eq!(windows.configure_hint(WindowId(id)).size, None);
        }
        assert_eq!(windows.focus(), Some(WindowId(10)));
        assert_eq!(order(&windows), ids(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));

        // Focusing raises.
        windows.focus_window(Some(WindowId(3)));
        assert_eq!(windows.focus(), Some(WindowId(3)));
        assert_eq!(order(&windows), ids(&[1, 2, 4, 5, 6, 7, 8, 9, 10, 3]));
        assert!(windows.configure_hint(WindowId(3)).activated);
        assert!(!windows.configure_hint(WindowId(10)).activated);

        // A moved window belongs to the output under its corner.
        windows.move_window(WindowId(4), Point::new(100, 100));
        assert_eq!(place(&windows, 4).output, Some(OutputId(2)));
        assert_eq!(place(&windows, 4).position, Point::new(100, 100));

        // The focused window closes: the topmost one gets the focus.
        windows.remove_window(WindowId(3));
        assert_eq!(windows.focus(), Some(WindowId(10)));

        // The first output goes away: its windows move to the other one,
        // at the same place relative to the origin.
        windows.remove_output(OutputId(1));
        let p = place(&windows, 1);
        assert_eq!(p.output, Some(OutputId(2)));
        assert_eq!(p.position, Point::new(32, 32));
        assert_eq!(place(&windows, 10).position, Point::new(64, 64));
        assert_eq!(place(&windows, 4).position, Point::new(100, 100));

        // The cascade goes on from where it was, on the remaining output.
        let p = windows.add_window(WindowId(11));
        assert_eq!(p.output, Some(OutputId(2)));
        assert_eq!(p.position, Point::new(96, 96));
        assert_eq!(windows.focus(), Some(WindowId(11)));

        // No focus; closing an unfocused window gives none.
        windows.focus_window(None);
        windows.remove_window(WindowId(11));
        assert_eq!(windows.focus(), None);
        assert_eq!(windows.placements().len(), 9);
    }
}
