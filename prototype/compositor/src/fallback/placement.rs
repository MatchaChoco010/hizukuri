//! Placement in the fallback: where a new window goes when nobody else
//! decides. A cascade on the first output: each new window lands
//! [`STEP`] logical pixels right of and below the previous one, and after
//! [`SLOTS`] windows the cascade starts over.

use crate::wm::{OutputId, Placement, Point, Rect, WindowId};

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
    use super::*;

    /// The place of the window in cascade slot `slot` (1 to [`SLOTS`]) on an
    /// output with this origin.
    fn at(origin: (i32, i32), slot: i32) -> Point {
        Point::new(origin.0 + STEP * slot, origin.1 + STEP * slot)
    }

    #[test]
    fn the_cascade_steps_down_and_right_from_the_output_origin() {
        let output = (OutputId(1), Rect::new(0, 0, 1280, 720));
        let mut cascade = Cascade::new();
        for id in 1..=20 {
            let placement = cascade.place(WindowId(id), Some(output));
            let slot = (id as i32 - 1) % SLOTS + 1;
            assert_eq!(
                placement,
                Placement {
                    window: WindowId(id),
                    output: Some(OutputId(1)),
                    position: at((0, 0), slot),
                    size: None,
                },
                "window {id}"
            );
        }
    }

    #[test]
    fn the_cascade_starts_at_the_first_output() {
        // The caller passes the first output; the cascade follows its
        // origin, also a negative one.
        let left = (OutputId(1), Rect::new(-1920, 100, 1920, 1080));
        let mut cascade = Cascade::new();
        for id in 1..=12 {
            let placement = cascade.place(WindowId(id), Some(left));
            let slot = (id as i32 - 1) % SLOTS + 1;
            assert_eq!(placement.output, Some(OutputId(1)));
            assert_eq!(placement.position, at((-1920, 100), slot), "window {id}");
        }
    }

    #[test]
    fn without_an_output_the_cascade_starts_at_the_origin() {
        let mut cascade = Cascade::new();
        // Three windows before any output exists, then one appears: the
        // count goes on, the origin is the output's.
        for id in 1..=3 {
            let placement = cascade.place(WindowId(id), None);
            assert_eq!(placement.output, None);
            assert_eq!(placement.position, at((0, 0), id as i32));
        }
        let output = (OutputId(7), Rect::new(200, 300, 800, 600));
        let placement = cascade.place(WindowId(4), Some(output));
        assert_eq!(placement.output, Some(OutputId(7)));
        assert_eq!(placement.position, at((200, 300), 4));
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
