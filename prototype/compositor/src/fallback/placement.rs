//! Placement in the fallback: where a new window goes when nobody else
//! decides. A cascade on the first output: each new window lands
//! [`STEP`] logical pixels right of and below the previous one, and after
//! [`SLOTS`] windows the cascade starts over.

use proto_layout::{OutputId, Placement, Point, Rect, WindowId};

/// Distance between the origins of consecutively placed windows.
pub const STEP: i32 = 32;
/// Number of cascade positions before wrapping around.
pub const SLOTS: i32 = 8;

/// The cascade. It counts the windows placed so far; removing a window does
/// not move the cascade back.
#[derive(Debug, Clone, Default)]
pub struct Cascade {
    placed: i32,
}

impl Cascade {
    pub fn new() -> Self {
        Self::default()
    }

    /// The place of the next new window. `first_output` is the first output
    /// that exists, if any; without one the cascade starts at the origin of
    /// the global space and the window is on no output. The window has no
    /// size yet.
    pub fn place(&mut self, window: WindowId, first_output: Option<(OutputId, Rect)>) -> Placement {
        let origin = first_output
            .map(|(_, rect)| rect.origin())
            .unwrap_or_default();
        let slot = self.placed % SLOTS;
        self.placed += 1;
        Placement {
            window,
            output: first_output.map(|(id, _)| id),
            position: Point::new(origin.x + STEP * (slot + 1), origin.y + STEP * (slot + 1)),
            size: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use proto_layout::Layout;

    use super::*;

    /// Places the same windows with the cascade and with the floating layout
    /// of `proto-layout`, which uses the same rule. Outputs are set before
    /// the window with the same index is added; `None` adds no output.
    fn compare(outputs: &[Option<(OutputId, Rect)>], windows: u64) {
        let mut cascade = Cascade::new();
        let mut layout = Layout::new();
        let mut first: Option<(OutputId, Rect)> = None;
        for id in 1..=windows {
            if let Some(Some((output, rect))) = outputs.get(id as usize - 1) {
                layout.set_output(*output, *rect);
                first.get_or_insert((*output, *rect));
            }
            let expected = layout.add_window(WindowId(id));
            assert_eq!(cascade.place(WindowId(id), first), expected, "window {id}");
        }
    }

    #[test]
    fn the_cascade_matches_the_floating_layout() {
        let output = (OutputId(1), Rect::new(0, 0, 1280, 720));
        compare(&[Some(output)], 20);
    }

    #[test]
    fn the_cascade_starts_at_the_first_output() {
        let left = (OutputId(1), Rect::new(-1920, 100, 1920, 1080));
        let right = (OutputId(2), Rect::new(0, 0, 1280, 720));
        compare(&[Some(left), Some(right)], 12);
    }

    #[test]
    fn without_an_output_the_cascade_starts_at_the_origin() {
        let output = (OutputId(7), Rect::new(200, 300, 800, 600));
        // Three windows before any output exists, then one appears.
        compare(&[None, None, None, Some(output)], 10);
        let mut cascade = Cascade::new();
        let placement = cascade.place(WindowId(1), None);
        assert_eq!(placement.output, None);
        assert_eq!(placement.position, Point::new(STEP, STEP));
    }

    #[test]
    fn the_cascade_wraps_around() {
        let mut cascade = Cascade::new();
        let positions: Vec<Point> = (1..=SLOTS as u64 + 2)
            .map(|id| cascade.place(WindowId(id), None).position)
            .collect();
        assert_eq!(positions[0], Point::new(STEP, STEP));
        assert_eq!(
            positions[SLOTS as usize - 1],
            Point::new(STEP * SLOTS, STEP * SLOTS)
        );
        assert_eq!(positions[SLOTS as usize], positions[0]);
        assert_eq!(positions[SLOTS as usize + 1], positions[1]);
    }
}
