//! The verification compositor of hizukuri, a foundation for building
//! Wayland compositors (prototype).
//!
//! This crate connects the pieces:
//!
//! - [`compositor`]: the core. Reacts to protocol events
//!   (`proto-wl`), asks the layout (`proto-layout`) where windows go,
//!   keeps the scene, draws with the renderer (`proto-render`) and talks
//!   to the backend (`proto-backend`).
//! - [`scene`]: the visual state of windows (where they are drawn, how
//!   opaque they are) and the textures of their surfaces. Kept apart from
//!   the logical state that protocols and the layout deal with.
//! - [`control`]: the control interface used for verification: capture the
//!   screen, inject input, dump state, quit.
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
pub mod safety;
pub mod scene;
pub mod script;
pub mod selftest;
pub mod timing;
pub mod wm;
