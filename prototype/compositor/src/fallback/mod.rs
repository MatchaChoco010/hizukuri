//! The fallback: the least the core does to keep a session usable when the
//! script cannot.
//!
//! Drawing, hit testing and window management belong to the script layer.
//! The script can fail, though: it can throw, run out of its time or memory
//! budget, or not load at all. The core then still has to put the windows on
//! screen and deliver input to them, or a broken script would leave the
//! person in front of a frozen or empty screen with no way to fix it. That,
//! and nothing more, is why this module is in the core:
//!
//! - [`draw`]: windows flat at their logical position, in stacking order,
//!   with their opacity, the cursor on top, and a bar along the top edge
//!   that marks fallback frames. No color conversion, no transforms, no
//!   decoration.
//! - [`placement`]: where a new window goes when nobody else decides (a
//!   cascade on the first output).
//! - [`hit_test`]: which surface is at a point, for windows drawn flat.
//!
//! None of this is a feature of a compositor. A compositor built on
//! hizukuri does its drawing and window management in the script layer;
//! the fallback only bridges the time until the script works again.

pub mod draw;
pub mod hit_test;
pub mod placement;

pub use draw::{Drawer, MARKER_COLOR, MARKER_HEIGHT};
pub use hit_test::{hit_test, locate};
pub use placement::Cascade;
