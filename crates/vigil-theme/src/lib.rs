//! Theme subsystem (DESIGN.md §6): load `/etc/greetd/theme.slint` via
//! slint-interpreter, validate it against the versioned contract (every
//! required property/callback present and correctly typed), and fall back to
//! the compiled-in default theme on any diagnostic. Themes are declarative —
//! no code execution; worst case is a bad image path.

/// Contract version this build of vigil implements (DESIGN.md §6).
pub const CONTRACT_VERSION: u32 = 1;

/// Source of the compiled-in default theme (themes/default/theme.slint),
/// embedded so the fallback needs no filesystem.
pub const DEFAULT_THEME_SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../themes/default/theme.slint"
));

#[derive(Debug)]
pub enum ThemeError {
    /// Compile diagnostics, already formatted for the log.
    Compile(String),
    /// The theme compiled but is missing/mistyping contract surface.
    Contract(String),
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Compile(d) => write!(f, "theme compile: {d}"),
            ThemeError::Contract(d) => write!(f, "theme contract: {d}"),
        }
    }
}
impl std::error::Error for ThemeError {}

/// A validated, instantiable theme.
pub struct Theme {
    _private: (),
}

impl Theme {
    /// Load and validate the theme at `_path`; on any error, log why and
    /// return the compiled-in default (which must always validate).
    pub fn load_or_default(_path: Option<&std::path::Path>) -> Theme {
        todo!("M1: interpreter compile + contract validation + fallback")
    }

    /// Instantiate the theme for one output (one component per output).
    pub fn instantiate(&self) -> Result<slint_interpreter::ComponentInstance, ThemeError> {
        todo!("M1")
    }
}
