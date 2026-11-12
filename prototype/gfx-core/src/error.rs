//! Errors returned by the checked GPU API.
//!
//! Every misuse a script can commit maps to one of these codes. The API never
//! panics and never forwards an invalid call to Vulkan.

use std::fmt;

/// Machine-readable category of a [`GfxError`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// The value is not a handle of this device (fabricated, or from a previous device).
    InvalidHandle,
    /// The handle is of another kind (for example an image where a buffer is expected).
    WrongKind,
    /// The handle refers to a resource that has been destroyed.
    Destroyed,
    /// An offset, size, index or count is outside the valid range.
    OutOfRange,
    /// An argument is malformed (unknown enum name, zero size, bad combination).
    InvalidArgument,
    /// The call is not allowed in the current state (no frame, pass open, ...).
    InvalidState,
    /// Formats, numeric classes or sizes of two things that must agree do not.
    Mismatch,
    /// Something the pipeline needs is not bound (vertex buffer, heap slot, ...).
    MissingBinding,
    /// Shader source failed to parse or validate.
    ShaderCompile,
    /// Shader stages do not fit each other or the pipeline description.
    ShaderInterface,
    /// A configured limit (object count, heap slots, memory budget) was reached.
    LimitExceeded,
    /// The driver reported that it is out of memory.
    OutOfMemory,
    /// The device is lost (reported by the driver, or assumed after a timeout).
    DeviceLost,
    /// The GPU did not finish in time.
    Timeout,
    /// The operation is valid Vulkan but outside what this spike implements,
    /// or the device does not support it.
    Unsupported,
    /// The object belongs to the host; scripts may use it but not change or
    /// destroy it.
    NotPermitted,
    /// Vulkan returned an error that is none of the above.
    Internal,
}

/// Error value of the checked GPU API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GfxError {
    pub code: ErrorCode,
    pub message: String,
}

impl GfxError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for GfxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for GfxError {}

pub type Result<T> = std::result::Result<T, GfxError>;

/// Builds an `Err(GfxError)` with a formatted message.
macro_rules! bail {
    ($code:ident, $($arg:tt)*) => {
        return Err($crate::error::GfxError::new(
            $crate::error::ErrorCode::$code,
            format!($($arg)*),
        ))
    };
}

pub(crate) use bail;
