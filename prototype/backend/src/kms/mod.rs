//! The DRM/KMS backend.
//!
//! The parts, from pure to hardware:
//!
//! - [`model`]: a plain-data snapshot of a DRM device (connectors, CRTCs,
//!   planes, properties), which can be saved to and loaded from JSON.
//! - [`plan`]: decisions made from a snapshot, as pure functions: which
//!   connector, CRTC, plane and mode; how to assemble atomic commits; which
//!   scanout path.
//! - [`device`]: [`device::KmsDevice`], the thin interface to a device.
//! - [`mock`]: a device in memory that follows the kernel's rules, for
//!   tests without hardware.
//! - [`clock`]: a clock tests can replace.
//! - [`frame`]: when an output may draw, as a state machine.
//! - [`input`]: device input to backend input events, and the emergency
//!   key combinations.
//! - `drm_device` (feature `kms`): [`device::KmsDevice`] over a real DRM
//!   device. Translates, decides nothing.
//! - `session` (feature `kms`): libseat, udev and libinput on a calloop
//!   loop, feeding the backend. Needs a seat; moves events, decides nothing.
//! - [`backend`]: [`KmsBackend`], which puts these together and implements
//!   [`crate::Backend`]. Generic over the device and the clock.

pub mod backend;
pub mod blob;
pub mod clock;
pub mod device;
#[cfg(feature = "kms")]
pub mod drm_device;
pub mod drm_info;
pub mod frame;
pub mod input;
pub mod mock;
pub mod model;
pub mod plan;
#[cfg(feature = "kms")]
pub mod session;

pub use backend::{ActiveScanout, KmsBackend, KmsConfig, KmsOutputStatus, KmsStatus};
