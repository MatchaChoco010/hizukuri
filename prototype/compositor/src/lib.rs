//! The verification compositor of hizukuri, a foundation for building
//! Wayland compositors (prototype).
//!
//! This crate connects the pieces:
//!
//! - [`compositor`]: the core. Reacts to protocol events
//!   (`proto-wl`), asks the window manager ([`wm`]) where windows go, has
//!   the scene ([`scene`]) draw, and talks to the backend
//!   (`proto-backend`).
//! - [`scene`]: the visual state of windows (where they are drawn, how
//!   opaque they are) and the textures of their surfaces. Kept apart from
//!   the logical state that protocols and the window manager deal with.
//! - [`script`]: drawing and window management written in a script.
//! - [`control`]: the control interface used for verification: capture the
//!   screen, inject input, dump state, quit.
//! - [`fallback`]: the least drawing, placement and hit testing the core
//!   does to keep a session usable while the script cannot.
//! - [`targets`]: the output images on the GPU API of `proto-gfx-core`.
//! - [`image`]: images read back from what draws, as PNG and OpenEXR.
//! - [`geometry`]: integer logical geometry for window management.
//! - [`shaders`] and [`project_shaders`]: a project's Slang shaders, the
//!   SPIR-V the compositor reads for them, and `build-shaders`.
//!
//! - [`safety`]: telling a virtual machine from real hardware, and a
//!   watchdog for a hung main loop.
//! - [`selftest`]: the KMS self test, which drives one output with a test
//!   pattern and writes down what happened.
//!
//! The `proto-compositor` binary wraps the core in an event loop; `proto-ctl`
//! talks to the control socket.

pub mod compositor;
pub mod control;
pub mod fallback;
pub mod geometry;
pub mod image;
pub mod project_shaders;
pub mod safety;
pub mod scene;
pub mod script;
pub mod selftest;
pub mod shaders;
pub mod targets;
pub mod timing;
pub mod wm;
pub mod x11;
