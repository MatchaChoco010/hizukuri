//! The scene: what windows look like on screen, and where input lands.
//!
//! The layout and the protocols deal with the *logical* state of a window:
//! where it is in the logical space, how big it is, whether it has the
//! focus. The scene holds the *visual* state: where it is actually drawn and
//! how. Keeping the two apart is what lets animations, and placing windows
//! in 3D, change what is drawn without touching what clients are told.
//!
//! # The seam
//!
//! [`Scene`] is the boundary between the compositor core and whatever
//! draws. The core never draws itself; it goes through three
//! ports:
//!
//! 1. **Contents in**: [`Scene::surface_contents`] hands over the pixels a
//!    surface committed, together with what they mean (image description,
//!    alpha mode).
//! 2. **Frames out**: [`Scene::draw`] is asked to draw the windows into the
//!    render target of an output.
//! 3. **Input**: [`Scene::hit_test`] decides which surface is at a point of
//!    the screen and where inside it. The scene decides, because only the
//!    scene knows where things were drawn.
//!
//! `script::ScriptScene` is the implementation that draws: drawing
//! assembled by a script, behind these three ports, with the core's
//! fallback taking over while the script cannot. [`NullScene`] draws
//! nothing and needs no GPU (protocol tests, `--no-drawing`).

use std::collections::HashMap;

use proto_backend::{OutputInfo, TargetError, TargetId, Targets};
use proto_color::{AlphaMode, ImageDescription};
use proto_wl::{BufferContents, BufferTransform, BufferUpdate, DamageRect, SurfaceId};
use serde_json::{Value, json};

use crate::fallback;
use crate::image::{CompositeImage, EncodedImage};
use crate::wm::WindowId;

/// How a window is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisualWindow {
    /// Position of the window geometry in the global logical space.
    pub position: (f64, f64),
    /// Multiplies the window's color and alpha.
    pub opacity: f32,
}

/// One surface of a window, as the core describes it to the scene.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceView {
    pub surface: SurfaceId,
    /// Position relative to the window geometry, in logical coordinates.
    pub offset: (i32, i32),
    /// Size in logical coordinates.
    pub size: (i32, i32),
    /// The part of the buffer that is shown, in buffer pixels
    /// (x, y, width, height). `None` when the surface has no contents.
    pub source: Option<(f64, f64, f64, f64)>,
    pub image_description: ImageDescription,
    pub alpha_mode: AlphaMode,
}

/// A window with its surfaces, back to front, as the core describes it to
/// the scene. Windows are passed in stacking order, bottom to top.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowView {
    pub window: WindowId,
    pub surfaces: Vec<SurfaceView>,
}

/// What the cursor looks like.
#[derive(Debug, Clone, PartialEq)]
pub enum CursorImage {
    /// The scene's own arrow.
    Default,
    /// Surfaces a client provided, back to front. Their offsets are
    /// relative to the pointer position (the hotspot is applied).
    Surfaces(Vec<SurfaceView>),
}

/// The pointer cursor to draw on top of everything.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorView {
    /// The pointer position in the global logical space.
    pub position: (f64, f64),
    pub image: CursorImage,
}

/// The answer to a hit test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub window: WindowId,
    pub surface: SurfaceId,
    /// The point in surface-local logical coordinates.
    pub local: (f64, f64),
}

/// A frame read back for verification.
pub struct Capture {
    /// The output after encoding.
    pub encoded: EncodedImage,
    /// The compositing buffer (linear light). `None` when whatever draws
    /// does not have one, or does not show it.
    pub composite: Option<CompositeImage>,
}

/// What [`Scene::draw`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawStatus {
    /// A new frame is in the render target and can be presented.
    Drawn,
    /// No usable frame was produced (the drawing code failed). The output
    /// keeps showing what it showed before; the target must not be
    /// presented.
    Kept,
}

/// A window as the core knows it, for scenes that decide more than how to
/// draw rectangles (see [`Scene::update_world`]).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSnapshot {
    pub window: WindowId,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub mapped: bool,
    /// Size of the window geometry in logical coordinates, once mapped.
    pub size: Option<(i32, i32)>,
    pub activated: bool,
    /// Logical position of the window geometry in the global space.
    pub position: (i32, i32),
    /// The surfaces of the window, back to front.
    pub surfaces: Vec<proto_wl::SurfaceNode>,
    /// The window this one belongs to (a dialog's main window).
    pub parent: Option<WindowId>,
    /// The size limits the client gave; zero on an axis is no limit.
    pub min_size: (i32, i32),
    pub max_size: (i32, i32),
    /// The window is an X11 window, shown through xwayland-satellite.
    pub x11: bool,
}

/// Everything the core knows that drawing or placing windows may depend on.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorldSnapshot {
    /// Stacking order, bottom to top.
    pub windows: Vec<WindowSnapshot>,
    pub outputs: Vec<OutputInfo>,
    /// The window with the keyboard focus.
    pub focus: Option<WindowId>,
    /// The pointer position in the global logical space, once it moved.
    pub pointer: Option<(f64, f64)>,
    /// The cursor to draw, if the core wants one drawn.
    pub cursor: Option<CursorView>,
    /// The surfaces of the icon of a drag in progress, back to front, with
    /// offsets relative to the pointer.
    pub drag_icon: Vec<SurfaceView>,
    /// The color outputs are cleared to (straight RGB, like content without
    /// color information).
    pub background: [f32; 3],
}

/// Something a scene asks the core to do. See [`Scene::take_requests`].
#[derive(Debug, Clone, PartialEq)]
pub enum SceneRequest {
    /// Draw another frame even if nothing else changed (animations).
    Redraw,
}

/// Errors of a scene.
#[derive(Debug, thiserror::Error)]
pub enum SceneError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error("the scene does not draw")]
    NotDrawing,
    /// A client's buffer could not be read. The surface shows nothing; the
    /// session goes on.
    #[error("unreadable buffer: {0}")]
    Unreadable(String),
    #[error("{0}")]
    Other(String),
}

/// The boundary between the compositor core and whatever draws. See the
/// module documentation.
pub trait Scene {
    // -- visual state -------------------------------------------------------

    /// A window exists now, at this logical position.
    fn window_added(&mut self, window: WindowId, position: (i32, i32));

    /// The logical position of a window changed.
    fn window_moved(&mut self, window: WindowId, position: (i32, i32));

    fn window_removed(&mut self, window: WindowId);

    /// Sets the opacity a window is drawn with. Returns `false` when the
    /// window is unknown.
    fn set_window_opacity(&mut self, window: WindowId, opacity: f32) -> bool;

    /// How a window is currently drawn.
    fn visual(&self, window: WindowId) -> Option<VisualWindow>;

    /// The state of the core changed. Called before drawing and before hit
    /// tests whenever something may be different. Scenes that only draw what
    /// [`Scene::draw`] hands them ignore it.
    fn update_world(&mut self, _world: &WorldSnapshot) {}

    /// Something the scene needs done by the core since the last call.
    fn take_requests(&mut self) -> Vec<SceneRequest> {
        Vec::new()
    }

    // -- port 1: contents in ------------------------------------------------

    /// A surface committed new pixels. `contents` is only valid during the
    /// call; the scene takes what it needs (uploads it). `damage` is what
    /// the client marked as changed.
    fn surface_contents(
        &mut self,
        surface: SurfaceId,
        contents: &BufferContents,
        damage: &[DamageRect],
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> Result<(), SceneError>;

    /// A surface committed a new buffer. The scene takes what it needs from
    /// `update` and may keep it: the client's buffer stays held until the
    /// update is dropped, which is what a scene that draws from an imported
    /// dmabuf needs.
    ///
    /// By default the buffer is read as upright pixels in CPU memory (which
    /// undoes a buffer transform and maps a dmabuf) and handed to
    /// [`Scene::surface_contents`]. A buffer that cannot be read that way is
    /// [`SceneError::Unreadable`].
    fn surface_buffer(
        &mut self,
        surface: SurfaceId,
        update: BufferUpdate,
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> Result<(), SceneError> {
        let contents = update
            .upright_pixels()
            .map_err(|e| SceneError::Unreadable(e.to_string()))?;
        // Damage in buffer coordinates describes the buffer before the
        // transform was undone; rather than map it, report "everything".
        let damage: &[DamageRect] = match update.transform {
            BufferTransform::Normal => &update.damage,
            _ => &[],
        };
        self.surface_contents(surface, &contents, damage, image_description, alpha_mode)
    }

    /// A surface has no contents any more (unmapped or destroyed).
    fn surface_removed(&mut self, surface: SurfaceId);

    /// Whether the scene holds contents for a surface.
    fn has_contents(&self, surface: SurfaceId) -> bool;

    // -- port 2: frames out -------------------------------------------------

    /// Draws the windows into the render target of an output. `target` is
    /// `None` when the backend has nothing to render into; the scene then
    /// only keeps its state up to date. `cursor` is drawn above everything.
    fn draw(
        &mut self,
        output: &OutputInfo,
        target: Option<TargetId>,
        background: [f32; 3],
        windows: &[WindowView],
        cursor: Option<&CursorView>,
    ) -> Result<DrawStatus, SceneError>;

    /// What the backend needs to create render targets and to present what
    /// [`Scene::draw`] drew into them.
    fn targets(&mut self) -> Targets<'_>;

    // -- port 3: input ------------------------------------------------------

    /// Finds the topmost surface at a point of the global logical space.
    /// `accepts_input` tells whether a surface takes input at a
    /// surface-local point (inside the surface and its input region).
    fn hit_test(
        &self,
        point: (f64, f64),
        windows: &[WindowView],
        accepts_input: &dyn Fn(SurfaceId, f64, f64) -> bool,
    ) -> Option<Hit>;

    /// Where a point of the global logical space lies in the coordinates of
    /// one given surface, whether or not the point is inside it. Used while
    /// a pointer button is held: the events keep going to the surface the
    /// button was pressed on.
    fn locate(
        &self,
        point: (f64, f64),
        surface: SurfaceId,
        windows: &[WindowView],
    ) -> Option<(f64, f64)>;

    // -- verification -------------------------------------------------------

    /// Reads back the last frame drawn into a target.
    fn capture(&mut self, target: TargetId) -> Result<Capture, SceneError>;

    /// Describes the scene for state dumps (kind, GPU device, script).
    fn describe(&self) -> Value;

    /// Handles a control request that only this kind of scene understands.
    /// `None` when the command is not one of them.
    fn control(&mut self, _command: &str, _args: &Value) -> Option<Result<Value, String>> {
        None
    }
}

/// A scene that draws nothing and needs neither a GPU nor a script.
///
/// It keeps the visual state (windows at their logical position, their
/// opacity) and answers hit tests with the fallback's
/// ([`fallback::hit_test`]), so everything but drawing works: protocol
/// tests, and the compositor started with `--no-drawing`. Contents are
/// not kept.
#[derive(Debug, Default)]
pub struct NullScene {
    windows: HashMap<WindowId, VisualWindow>,
}

impl NullScene {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Scene for NullScene {
    fn window_added(&mut self, window: WindowId, position: (i32, i32)) {
        self.windows.insert(
            window,
            VisualWindow {
                position: (position.0 as f64, position.1 as f64),
                opacity: 1.0,
            },
        );
    }

    fn window_moved(&mut self, window: WindowId, position: (i32, i32)) {
        if let Some(visual) = self.windows.get_mut(&window) {
            visual.position = (position.0 as f64, position.1 as f64);
        }
    }

    fn window_removed(&mut self, window: WindowId) {
        self.windows.remove(&window);
    }

    fn set_window_opacity(&mut self, window: WindowId, opacity: f32) -> bool {
        match self.windows.get_mut(&window) {
            Some(visual) => {
                visual.opacity = opacity.clamp(0.0, 1.0);
                true
            }
            None => false,
        }
    }

    fn visual(&self, window: WindowId) -> Option<VisualWindow> {
        self.windows.get(&window).copied()
    }

    fn surface_contents(
        &mut self,
        _surface: SurfaceId,
        _contents: &BufferContents,
        _damage: &[DamageRect],
        _image_description: ImageDescription,
        _alpha_mode: AlphaMode,
    ) -> Result<(), SceneError> {
        Ok(())
    }

    fn surface_removed(&mut self, _surface: SurfaceId) {}

    fn has_contents(&self, _surface: SurfaceId) -> bool {
        false
    }

    fn draw(
        &mut self,
        _output: &OutputInfo,
        _target: Option<TargetId>,
        _background: [f32; 3],
        _windows: &[WindowView],
        _cursor: Option<&CursorView>,
    ) -> Result<DrawStatus, SceneError> {
        Ok(DrawStatus::Drawn)
    }

    fn targets(&mut self) -> Targets<'_> {
        None
    }

    fn hit_test(
        &self,
        point: (f64, f64),
        windows: &[WindowView],
        accepts_input: &dyn Fn(SurfaceId, f64, f64) -> bool,
    ) -> Option<Hit> {
        fallback::hit_test(
            |window| self.windows.get(&window).map(|v| v.position),
            point,
            windows,
            accepts_input,
        )
    }

    fn locate(
        &self,
        point: (f64, f64),
        surface: SurfaceId,
        windows: &[WindowView],
    ) -> Option<(f64, f64)> {
        fallback::locate(
            |window| self.windows.get(&window).map(|v| v.position),
            point,
            surface,
            windows,
        )
    }

    fn capture(&mut self, _target: TargetId) -> Result<Capture, SceneError> {
        Err(SceneError::NotDrawing)
    }

    fn describe(&self) -> Value {
        json!({
            "kind": "none",
            "gpu": Value::Null,
            "frames_rendered": 0,
        })
    }
}

/// The built-in arrow cursor: `X` is the black outline, `.` the white
/// inside. The hotspot is the top left pixel.
const ARROW: [&str; 19] = [
    "X           ",
    "XX          ",
    "X.X         ",
    "X..X        ",
    "X...X       ",
    "X....X      ",
    "X.....X     ",
    "X......X    ",
    "X.......X   ",
    "X........X  ",
    "X.........X ",
    "X......XXXXX",
    "X...X..X    ",
    "X..X X..X   ",
    "X.X  X..X   ",
    "XX    X..X  ",
    "X     X..X  ",
    "       X..X ",
    "        XX  ",
];
pub const ARROW_WIDTH: u32 = ARROW[0].len() as u32;
pub const ARROW_HEIGHT: u32 = ARROW.len() as u32;

/// The arrow as premultiplied B, G, R, A pixels.
pub fn arrow_image() -> (u32, u32, Vec<u8>) {
    let pixels = ARROW
        .iter()
        .flat_map(|row| row.bytes())
        .flat_map(|cell| match cell {
            b'X' => [0, 0, 0, 255],
            b'.' => [255, 255, 255, 255],
            _ => [0, 0, 0, 0],
        })
        .collect();
    (ARROW_WIDTH, ARROW_HEIGHT, pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arrow_image_is_well_formed() {
        assert!(ARROW.iter().all(|row| row.len() == ARROW_WIDTH as usize));
        let (width, height, pixels) = arrow_image();
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        // The tip is an outline pixel; the far corner is transparent.
        assert_eq!(pixels[..4], [0, 0, 0, 255]);
        assert_eq!(pixels[pixels.len() - 4..], [0, 0, 0, 0]);
        // There is a white inside.
        assert!(pixels.chunks_exact(4).any(|p| p == [255, 255, 255, 255]));
    }

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

    #[test]
    fn visual_position_follows_the_logical_position() {
        let mut scene = NullScene::new();
        scene.window_added(WindowId(1), (10, 20));
        assert_eq!(scene.visual(WindowId(1)).unwrap().position, (10.0, 20.0));
        scene.window_moved(WindowId(1), (30, 40));
        assert_eq!(scene.visual(WindowId(1)).unwrap().position, (30.0, 40.0));
        scene.window_moved(WindowId(2), (1, 1)); // unknown: ignored
        scene.window_removed(WindowId(1));
        assert!(scene.visual(WindowId(1)).is_none());
    }

    #[test]
    fn opacity_is_clamped() {
        let mut scene = NullScene::new();
        scene.window_added(WindowId(1), (0, 0));
        assert_eq!(scene.visual(WindowId(1)).unwrap().opacity, 1.0);
        assert!(scene.set_window_opacity(WindowId(1), 0.25));
        assert_eq!(scene.visual(WindowId(1)).unwrap().opacity, 0.25);
        assert!(scene.set_window_opacity(WindowId(1), 7.0));
        assert_eq!(scene.visual(WindowId(1)).unwrap().opacity, 1.0);
        assert!(!scene.set_window_opacity(WindowId(2), 0.5));
    }

    #[test]
    fn hit_test_prefers_the_topmost_window_and_surface() {
        let mut scene = NullScene::new();
        scene.window_added(WindowId(1), (0, 0));
        scene.window_added(WindowId(2), (50, 50));
        // Bottom to top. Window 1 has a second surface above its first.
        let windows = [
            view(1, &[(10, (0, 0)), (11, (80, 0))]),
            view(2, &[(20, (0, 0))]),
        ];

        let hit = scene.hit_test((60.0, 70.0), &windows, &inside).unwrap();
        assert_eq!((hit.window, hit.surface), (WindowId(2), SurfaceId(20)));
        assert_eq!(hit.local, (10.0, 20.0));

        let hit = scene.hit_test((10.0, 10.0), &windows, &inside).unwrap();
        assert_eq!((hit.window, hit.surface), (WindowId(1), SurfaceId(10)));

        // Where both surfaces of window 1 overlap, the upper one wins.
        let hit = scene.hit_test((90.0, 10.0), &windows, &inside).unwrap();
        assert_eq!(hit.surface, SurfaceId(11));
        assert_eq!(hit.local, (10.0, 10.0));

        assert!(scene.hit_test((500.0, 500.0), &windows, &inside).is_none());
    }

    #[test]
    fn hit_test_uses_the_visual_position() {
        let mut scene = NullScene::new();
        scene.window_added(WindowId(1), (0, 0));
        let windows = [view(1, &[(10, (0, 0))])];
        scene.window_moved(WindowId(1), (300, 0));
        assert!(scene.hit_test((10.0, 10.0), &windows, &inside).is_none());
        let hit = scene.hit_test((310.0, 10.0), &windows, &inside).unwrap();
        assert_eq!(hit.local, (10.0, 10.0));
    }

    #[test]
    fn the_null_scene_draws_nothing_and_cannot_capture() {
        let mut scene = NullScene::new();
        assert!(scene.targets().is_none());
        assert!(matches!(
            scene.capture(TargetId::from_raw(1)),
            Err(SceneError::NotDrawing)
        ));
        assert_eq!(scene.describe()["gpu"], Value::Null);
        assert_eq!(scene.describe()["frames_rendered"], 0);
        assert!(!scene.has_contents(SurfaceId(1)));
    }
}
