//! Hit testing in the fallback: windows drawn flat at a position, without
//! transforms. Going from the screen to a surface is a translation.

use crate::wm::WindowId;
use proto_wl::SurfaceId;

use crate::scene::{Hit, WindowView};

/// The topmost surface that takes input at `point` (global logical
/// coordinates). `position` gives where a window is drawn; windows it does
/// not know are skipped. `accepts_input` tells whether a surface takes input
/// at a surface-local point.
pub fn hit_test(
    position: impl Fn(WindowId) -> Option<(f64, f64)>,
    point: (f64, f64),
    windows: &[WindowView],
    accepts_input: &dyn Fn(SurfaceId, f64, f64) -> bool,
) -> Option<Hit> {
    // Topmost window first, and within a window the topmost surface.
    for window in windows.iter().rev() {
        let Some(origin) = position(window.window) else {
            continue;
        };
        for surface in window.surfaces.iter().rev() {
            let local = (
                point.0 - (origin.0 + surface.offset.0 as f64),
                point.1 - (origin.1 + surface.offset.1 as f64),
            );
            if accepts_input(surface.surface, local.0, local.1) {
                return Some(Hit {
                    window: window.window,
                    surface: surface.surface,
                    local,
                });
            }
        }
    }
    None
}

/// Where `point` lies in the coordinates of `surface`, whether or not the
/// point is inside it (see [`Scene::locate`](crate::scene::Scene::locate)).
pub fn locate(
    position: impl Fn(WindowId) -> Option<(f64, f64)>,
    point: (f64, f64),
    surface: SurfaceId,
    windows: &[WindowView],
) -> Option<(f64, f64)> {
    for window in windows {
        if let Some(view) = window.surfaces.iter().find(|s| s.surface == surface) {
            let origin = position(window.window)?;
            return Some((
                point.0 - (origin.0 + view.offset.0 as f64),
                point.1 - (origin.1 + view.offset.1 as f64),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use proto_color::{AlphaMode, ImageDescription};

    use super::*;
    use crate::scene::SurfaceView;

    fn view(window: u64, surfaces: &[(u64, (i32, i32))]) -> WindowView {
        WindowView {
            window: WindowId(window),
            surfaces: surfaces
                .iter()
                .map(|(id, offset)| SurfaceView {
                    surface: SurfaceId(*id),
                    offset: *offset,
                    size: (100, 100),
                    source: None,
                    image_description: ImageDescription::SRGB_GAMMA22,
                    alpha_mode: AlphaMode::PremultipliedElectrical,
                })
                .collect(),
        }
    }

    /// Every surface is 100x100 and takes input everywhere inside.
    fn inside(_: SurfaceId, x: f64, y: f64) -> bool {
        (0.0..100.0).contains(&x) && (0.0..100.0).contains(&y)
    }

    fn at(window: WindowId) -> Option<(f64, f64)> {
        match window.0 {
            1 => Some((0.0, 0.0)),
            2 => Some((50.0, 50.0)),
            _ => None,
        }
    }

    #[test]
    fn the_topmost_window_and_surface_win() {
        // Bottom to top. Window 1 has a second surface above its first.
        let windows = [
            view(1, &[(10, (0, 0)), (11, (80, 0))]),
            view(2, &[(20, (0, 0))]),
        ];
        let hit = hit_test(at, (60.0, 70.0), &windows, &inside).unwrap();
        assert_eq!((hit.window, hit.surface), (WindowId(2), SurfaceId(20)));
        assert_eq!(hit.local, (10.0, 20.0));
        let hit = hit_test(at, (90.0, 10.0), &windows, &inside).unwrap();
        assert_eq!(hit.surface, SurfaceId(11));
        assert_eq!(hit.local, (10.0, 10.0));
        assert!(hit_test(at, (500.0, 500.0), &windows, &inside).is_none());
    }

    #[test]
    fn windows_without_a_position_are_skipped() {
        // Window 3 is on top, but nothing says where it is drawn.
        let windows = [view(1, &[(10, (0, 0))]), view(3, &[(30, (0, 0))])];
        let hit = hit_test(at, (10.0, 10.0), &windows, &inside).unwrap();
        assert_eq!(hit.surface, SurfaceId(10));
        assert_eq!(locate(at, (10.0, 10.0), SurfaceId(30), &windows), None);
    }

    #[test]
    fn locate_answers_outside_the_surface_too() {
        let windows = [view(2, &[(20, (5, 7))])];
        assert_eq!(
            locate(at, (0.0, 0.0), SurfaceId(20), &windows),
            Some((-55.0, -57.0))
        );
        assert_eq!(
            locate(at, (300.0, 60.0), SurfaceId(20), &windows),
            Some((245.0, 3.0))
        );
        assert_eq!(locate(at, (0.0, 0.0), SurfaceId(99), &windows), None);
    }
}
