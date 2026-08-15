//! Theme subsystem (DESIGN.md §6): load `/etc/greetd/theme.slint` via
//! slint-interpreter, validate it against the versioned contract (every
//! required property/callback present and correctly typed), and fall back to
//! the compiled-in default theme on any diagnostic. Themes are declarative —
//! no code execution; worst case is a bad image path.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use slint_interpreter::{ComponentDefinition, ComponentInstance, Value, ValueType};

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
    definition: ComponentDefinition,
}

impl Theme {
    /// Load and validate the theme at `path`; on any error, log why and
    /// return the compiled-in default (which must always validate).
    pub fn load_or_default(path: Option<&Path>) -> Theme {
        if let Some(path) = path {
            match compile_path(path) {
                Ok(theme) => return theme,
                Err(error) => eprintln!(
                    "failed to load theme {}: {error}; using default",
                    path.display()
                ),
            }
        }

        compile_source(
            DEFAULT_THEME_SOURCE,
            PathBuf::from("<embedded-default-theme>"),
        )
        .unwrap_or_else(|error| panic!("embedded default theme is invalid (build defect): {error}"))
    }

    /// Instantiate the theme for one output (one component per output).
    pub fn instantiate(&self) -> Result<ComponentInstance, ThemeError> {
        self.definition.create().map_err(|error| {
            ThemeError::Compile(format!("could not instantiate component: {error}"))
        })
    }
}

/// Validate a theme without instantiating a greeter. `Ok(())` means the
/// theme compiles and satisfies contract v1.
pub fn check(path: &Path) -> Result<(), ThemeError> {
    compile_path(path).map(|_| ())
}

/// A theme is either a `.slint` file or a directory containing
/// `theme.slint` (with its assets beside it — `@image-url` resolves
/// relative to the file, so a theme folder is self-contained).
fn compile_path(path: &Path) -> Result<Theme, ThemeError> {
    let file = if path.is_dir() {
        let candidate = path.join("theme.slint");
        if !candidate.is_file() {
            return Err(ThemeError::Compile(format!(
                "{} is a directory with no theme.slint",
                path.display()
            )));
        }
        candidate
    } else {
        path.to_path_buf()
    };
    let result = block_on(compiler().build_from_path(&file));
    finish_compilation(result)
}

fn compile_source(source: &str, path: PathBuf) -> Result<Theme, ThemeError> {
    let result = block_on(compiler().build_from_source(source.to_owned(), path));
    finish_compilation(result)
}

fn compiler() -> slint_interpreter::Compiler {
    let mut compiler = slint_interpreter::Compiler::default();
    let ui = slint_kit::ui_dir();
    let mut paths = vec![ui.clone()];
    if let Some(root) = ui.parent() {
        paths.push(root.to_path_buf());
    }
    compiler.set_include_paths(paths);
    compiler
}

fn finish_compilation(result: slint_interpreter::CompilationResult) -> Result<Theme, ThemeError> {
    if result.has_errors() {
        // Lead with the first error: a theme author needs the cause, not a
        // wall of cascading parse noise. Warnings are not failures.
        let errors: Vec<String> = result
            .diagnostics()
            .filter(|d| d.level() == slint_interpreter::DiagnosticLevel::Error)
            .map(|d| d.to_string())
            .collect();
        let first = errors
            .first()
            .cloned()
            .unwrap_or_else(|| "unknown compile error".to_owned());
        let rest = errors.len().saturating_sub(1);
        return Err(ThemeError::Compile(if rest > 0 {
            format!("{first} (+{rest} more)")
        } else {
            first
        }));
    }

    let names: Vec<String> = result.component_names().map(|n| n.to_string()).collect();
    let component_name = names
        .iter()
        .find(|n| *n == "DefaultTheme")
        .cloned()
        .or_else(|| names.into_iter().next())
        .ok_or_else(|| ThemeError::Compile("theme does not export a root component".to_owned()))?;
    let definition = result.component(&component_name).ok_or_else(|| {
        ThemeError::Compile(format!(
            "exported component `{component_name}` was not generated"
        ))
    })?;
    validate_contract(&definition)?;
    Ok(Theme { definition })
}

/// Validate the public contract from DESIGN.md §6.
///
/// Slint 1.17 exposes callback names but not their argument types through
/// reflection. A single probe instance is therefore used to invoke each
/// callback with its v1 arguments; invocation fails for a wrong arity or type.
fn validate_contract(definition: &ComponentDefinition) -> Result<(), ThemeError> {
    const PROPERTIES: &[(&str, ValueType)] = &[
        ("output-name", ValueType::String),
        ("panel-visible", ValueType::Bool),
        ("background-source", ValueType::Image),
        ("clock-text", ValueType::String),
        ("auth-state", ValueType::String),
        ("prompt-text", ValueType::String),
        ("prompt-is-secret", ValueType::Bool),
        ("info-message", ValueType::String),
        ("error-message", ValueType::String),
        ("sessions", ValueType::Model),
        ("selected-session", ValueType::Number),
        ("caps-lock", ValueType::Bool),
        ("status-banner", ValueType::String),
    ];
    let properties: HashMap<_, _> = definition.properties().collect();
    let probe = definition.create().map_err(|error| {
        ThemeError::Contract(format!(
            "could not instantiate theme for validation: {error}"
        ))
    })?;
    for &(name, expected) in PROPERTIES {
        match properties.get(name) {
            None => return Err(ThemeError::Contract(format!("missing property `{name}`"))),
            Some(actual) if *actual != expected => {
                return Err(ThemeError::Contract(format!(
                    "property `{name}` has type {actual:?}, expected {expected:?}"
                )));
            }
            Some(_) => {}
        }
        let current = probe.get_property(name).map_err(|error| {
            ThemeError::Contract(format!("property `{name}` cannot be read: {error}"))
        })?;
        probe.set_property(name, current).map_err(|error| {
            ThemeError::Contract(format!(
                "property `{name}` is not an input property: {error}"
            ))
        })?;
    }

    let callbacks: HashSet<_> = definition.callbacks().collect();
    let callback_arguments = [
        ("submit", vec![Value::from(slint::SharedString::default())]),
        ("cancel", vec![]),
        ("session-changed", vec![Value::from(0)]),
        (
            "power-action",
            vec![Value::from(slint::SharedString::default())],
        ),
    ];
    for (name, arguments) in callback_arguments {
        if !callbacks.contains(name) {
            return Err(ThemeError::Contract(format!("missing callback `{name}`")));
        }
        probe.invoke(name, &arguments).map_err(|_| {
            ThemeError::Contract(format!("callback `{name}` has the wrong signature"))
        })?;
    }
    Ok(())
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::rc::Rc;

    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, PlatformError, WindowAdapter};

    use super::*;

    const CONTRACT_SURFACE: &str = r#"
        in property <string> output-name;
        in property <bool> panel-visible;
        in property <image> background-source;
        in property <string> clock-text;
        in property <string> auth-state;
        in property <string> prompt-text;
        in property <bool> prompt-is-secret;
        in property <string> info-message;
        in property <string> error-message;
        in property <[string]> sessions;
        in property <int> selected-session;
        in property <bool> caps-lock;
        in property <string> status-banner;
        callback submit(string);
        callback cancel();
        callback session-changed(int);
        callback power-action(string);
    "#;

    struct HeadlessPlatform;

    impl Platform for HeadlessPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer))
        }
    }

    fn init_headless() {
        // set_platform is thread-local without an event-loop provider, so every
        // unit-test thread can install an independent software-only backend.
        let _ = slint::platform::set_platform(Box::new(HeadlessPlatform));
    }

    fn source(surface: &str) -> String {
        format!("export component TestTheme inherits Window {{ {surface} }}")
    }

    #[test]
    fn default_theme_validates_and_instantiates() {
        init_headless();
        let theme = compile_source(DEFAULT_THEME_SOURCE, PathBuf::from("default.slint")).unwrap();
        theme.instantiate().unwrap();
    }

    #[test]
    fn directory_with_theme_slint_loads() {
        init_headless();
        let dir = std::env::temp_dir().join(format!("vigil-theme-dir-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("theme.slint"), source(CONTRACT_SURFACE)).unwrap();
        let theme = Theme::load_or_default(Some(&dir));
        theme.instantiate().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_without_theme_slint_is_an_error() {
        let dir =
            std::env::temp_dir().join(format!("vigil-theme-empty-dir-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let error = check(&dir).unwrap_err();
        assert!(matches!(error, ThemeError::Compile(message) if message.contains("theme.slint")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn check_accepts_the_default_theme() {
        init_headless();
        assert!(
            check(Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../themes/default/theme.slint"
            )))
            .is_ok()
        );
    }

    #[test]
    fn missing_property_is_a_contract_error() {
        init_headless();
        let surface = CONTRACT_SURFACE.replace("in property <bool> caps-lock;", "");
        let error = compile_source(&source(&surface), PathBuf::from("missing.slint"))
            .err()
            .unwrap();
        assert!(matches!(error, ThemeError::Contract(message) if message.contains("caps-lock")));
    }

    #[test]
    fn wrong_property_type_is_a_contract_error() {
        init_headless();
        let surface = CONTRACT_SURFACE.replace(
            "in property <bool> caps-lock;",
            "in property <string> caps-lock;",
        );
        let error = compile_source(&source(&surface), PathBuf::from("wrong-type.slint"))
            .err()
            .unwrap();
        assert!(matches!(error, ThemeError::Contract(message) if message.contains("caps-lock")));
    }

    #[test]
    fn syntax_error_is_a_compile_error() {
        init_headless();
        let error = compile_source("export component Broken {", PathBuf::from("broken.slint"))
            .err()
            .unwrap();
        assert!(matches!(error, ThemeError::Compile(_)));
    }

    #[test]
    fn broken_file_falls_back_to_default() {
        init_headless();
        let path = std::env::temp_dir().join(format!(
            "vigil-theme-broken-{}-{}.slint",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, "export component Broken {").unwrap();
        let theme = Theme::load_or_default(Some(&path));
        std::fs::remove_file(path).unwrap();
        theme.instantiate().unwrap();
    }
}
