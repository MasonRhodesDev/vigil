#![forbid(unsafe_code)]

mod cost;
mod glow;
mod logo;
mod plate;
mod spec;

pub use logo::{Logo, MAX_EDGE, MAX_LOGO_BYTES, MAX_RASTER_BYTES, Mask, read_bounded};
pub use plate::{MAX_GLOW, MAX_PIXELS, MAX_SIDE, Plate, Rect, Sprite, check_output, mix, to_panel};
pub use spec::{
    DEFAULT_SPEC, Geometry, MAX_SPEC_BYTES, Scene, TagGeometry, ease, emit_spec, parse_color,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Io(String),
    Invalid(String),
    Refused(String),
    TooLarge(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(m) | Error::Invalid(m) | Error::Refused(m) | Error::TooLarge(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for Error {}
