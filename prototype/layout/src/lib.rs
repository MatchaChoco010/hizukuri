//! Logical window placement.
//!
//! This crate decides *where windows are* in the logical sense: which
//! output they belong to, their position and size in the global logical
//! coordinate space, their stacking order and which one has the focus. It
//! knows nothing about Wayland or rendering, so it can be unit tested and,
//! later, driven from Lua.
//!
//! It does not decide what a window *looks like* on screen. The scene (in
//! the `proto-compositor` crate) copies the logical placement into a visual state
//! that animations and 3D placement are free to change.
//!
//! The only policy is a plain floating layout: windows keep the size their
//! client chooses and are placed in a cascade. A different policy replaces
//! the placement; the surface of this crate (ids, rectangles, events in,
//! placements out) is independent of it.

mod floating;
mod geometry;

pub use floating::{ConfigureHint, Layout, Placement};
pub use geometry::{Point, Rect, Size};

/// Identifies a window. Allocated by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// Identifies an output. Allocated by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u64);
