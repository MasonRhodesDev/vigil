#![forbid(unsafe_code)]

mod cost;
mod glow;
mod helper;
mod layer;
mod logo;
mod plate;
mod sandbox;
mod spec;
mod timeouts;

pub use helper::{
    DEFAULT_DEADLINE, HELPER_ABI, Helper, INSTALLED_HELPER, MAX_DEADLINE, MAX_MESSAGE,
};
pub use layer::{Layer, Request};
pub use logo::{
    Format, Logo, MAX_EDGE, MAX_LOGO_BYTES, MAX_RASTER_BYTES, Mask, read_bounded, read_logo,
};
pub use plate::{
    Layout, MAX_GLOW, MAX_PIXELS, MAX_SIDE, Placement, Plate, Rect, Sprite, mix, to_panel,
};
pub use sandbox::{NOTICE, confine, lock_down};
pub use spec::{
    DEFAULT_SPEC, Geometry, MAX_SPEC_BYTES, Scene, TagGeometry, ease, emit_spec, parse_color,
};
pub use timeouts::{STRIKES_APART, STRIKING_DEADLINE, TIMEOUTS_BEFORE_TOO_SLOW, TOO_SLOW_LASTS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Io(String),
    Invalid(String),
    Refused(String),
    TooLarge(String),
    Helper(String),
    /// The helper was stopped at its deadline. A slow host can cause it, so
    /// unlike a refusal it says nothing about the logo.
    Timeout(String),
    /// The logo timed out at a striking deadline `TIMEOUTS_BEFORE_TOO_SLOW`
    /// times at this size class, each `STRIKES_APART` after the last, so the library skips it without running the
    /// helper until `TOO_SLOW_LASTS` passes. A loaded host can cause it, so
    /// unlike a refusal it expires.
    TooSlow(String),
}

// The helper reports which error it hit by its exit status.
impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Io(_) => 10,
            Error::Invalid(_) => 11,
            Error::Refused(_) => 12,
            Error::TooLarge(_) => 13,
            Error::Helper(_) => 14,
            Error::Timeout(_) => 15,
            Error::TooSlow(_) => 16,
        }
    }

    fn from_exit(code: i32, message: String) -> Error {
        match code {
            10 => Error::Io(message),
            11 => Error::Invalid(message),
            12 => Error::Refused(message),
            13 => Error::TooLarge(message),
            _ => Error::Helper(format!("the helper exited with {code}: {message}")),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(m)
            | Error::Invalid(m)
            | Error::Refused(m)
            | Error::TooLarge(m)
            | Error::Helper(m)
            | Error::Timeout(m)
            | Error::TooSlow(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}
