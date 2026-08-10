//! Shared config for the vigil pair (/etc/greetd/vigil.toml; DESIGN.md §9 G1). Parse-only, snake_case keys, every key optional; a broken config must never block login — load() always returns a usable Config.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const SYSTEM_CONFIG: &str = "/etc/greetd/vigil.toml";

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    pub look: Look,
    pub keyboard: Keyboard,
    pub sessions: Sessions,
    pub profiles: Profiles,
    pub render: Render,
    pub users: Users,
    pub power: Power,
    pub greeter: Greeter,
    pub lock: Lock,
    pub output: HashMap<String, OutputOverride>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Look {
    pub theme: Option<PathBuf>,
    pub background: Option<PathBuf>,
    pub fit: Option<String>,
    pub clock_format: String,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            theme: None,
            background: None,
            fit: None,
            clock_format: "%H:%M".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Keyboard {
    pub layout: String,
    pub variant: String,
    pub options: String,
    pub model: String,
    pub rules: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Sessions {
    pub dirs: Vec<String>,
    pub remember: bool,
    pub state_file: PathBuf,
    pub default: String,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            remember: true,
            state_file: "/var/lib/vigil/state.toml".into(),
            default: String::new(),
        }
    }
}

/// Which renderer draws the greeter.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Render {
    /// "software" (default) or "gl".
    ///
    /// Software is the zero-GPU baseline and what every machine can run;
    /// "gl" is fidelity headroom and requires a build with the `gl` feature.
    /// Anything unrecognised, unavailable, or that fails to initialise falls
    /// back to software -- a rendering preference must never be why someone
    /// cannot log in.
    pub backend: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Profiles {
    /// Monitor-layout profile directory, shared with the session manager so
    /// the login screen and the desktop agree on scale, position and mode.
    /// Defaults to the packaged directory; an absent directory is not an
    /// error and simply means outputs are laid out in DRM scan order, as
    /// they were before profiles existed. Set to "" to force that off.
    pub dir: Option<PathBuf>,
}

impl Default for Profiles {
    fn default() -> Self {
        Self {
            dir: Some(PathBuf::from("/etc/monitor-profiles")),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Users {
    /// Show the machine's users as a list. Set false on multi-tenant or
    /// privacy-sensitive machines: the greeter then asks for a typed name.
    pub show_list: bool,
}

impl Default for Users {
    fn default() -> Self {
        Self { show_list: true }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Power {
    pub enabled: bool,
}

impl Default for Power {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Greeter {
    pub user: String,
    pub cmd: Vec<String>,
    pub banner_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Lock {
    /// Seconds after locking during which a key/click unlocks without
    /// auth (0 = disabled). Never survives suspend (dual-clock deadline).
    pub grace_secs: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct OutputOverride {
    pub background: Option<PathBuf>,
    pub fit: Option<String>,
    pub scale: Option<f32>,
}

/// Parse TOML source (exposed for tests).
pub fn parse(source: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(source)
}

impl Config {
    /// Greeter load: explicit path, else SYSTEM_CONFIG. Missing file =
    /// silent defaults; unreadable/invalid = eprintln!("vigil-config: {path}: {err}; using defaults") + defaults.
    pub fn load(path: Option<&Path>) -> Config {
        load_file(path.unwrap_or_else(|| Path::new(SYSTEM_CONFIG))).unwrap_or_default()
    }

    /// Lock load: explicit path, else $XDG_CONFIG_HOME/vigil/config.toml
    /// (fallback $HOME/.config/vigil/config.toml) if that file exists,
    /// else SYSTEM_CONFIG. Same error philosophy.
    pub fn load_layered(path: Option<&Path>) -> Config {
        if let Some(path) = path {
            return load_file(path).unwrap_or_default();
        }
        let user_path = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|base| base.join("vigil/config.toml"));
        if let Some(path) = user_path
            && path.exists()
        {
            return load_file(&path).unwrap_or_default();
        }
        load_file(Path::new(SYSTEM_CONFIG)).unwrap_or_default()
    }
}

/// Persisted greeter state (`[sessions] state_file`): who logged in last
/// and which session they picked. Stored by NAME, not index — list order
/// changes as sessions are installed/removed.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct State {
    pub user: String,
    pub session: String,
}

impl State {
    /// None on any failure: missing file is silent, anything else logs.
    pub fn load(path: &Path) -> Option<State> {
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                eprintln!("vigil-config: {}: {e}", path.display());
                return None;
            }
        };
        match toml::from_str(&source) {
            Ok(state) => Some(state),
            Err(e) => {
                eprintln!("vigil-config: {}: {e}", path.display());
                None
            }
        }
    }

    /// Best-effort write (tmp + rename): a read-only or absent state dir
    /// must never break login, so failures log and return.
    pub fn store(&self, path: &Path) {
        let escape = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let body = format!(
            "user = \"{}\"\nsession = \"{}\"\n",
            escape(&self.user),
            escape(&self.session)
        );
        let tmp = path.with_extension("tmp");
        let result = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = result {
            eprintln!(
                "vigil-config: cannot store state at {}: {e}",
                path.display()
            );
        }
    }
}

fn load_file(path: &Path) -> Option<Config> {
    match std::fs::read_to_string(path) {
        Ok(source) => match parse(&source) {
            Ok(config) => Some(config),
            Err(err) => {
                eprintln!("vigil-config: {}: {err}; using defaults", path.display());
                Some(Config::default())
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            eprintln!("vigil-config: {}: {err}; using defaults", path.display());
            Some(Config::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_file_missing() {
        let config = Config::load(Some(Path::new("/nonexistent/vigil-test.toml")));
        assert_eq!(config, Config::default());
        assert_eq!(config.look.clock_format, "%H:%M");
        assert!(config.power.enabled);
        assert!(config.sessions.remember);
        assert!(config.sessions.default.is_empty());
        // Defaults to the packaged shared directory; absent = feature off.
        assert_eq!(
            config.profiles.dir,
            Some(PathBuf::from("/etc/monitor-profiles"))
        );
        assert!(config.users.show_list);
        assert_eq!(config.lock.grace_secs, 0);
        assert!(config.greeter.banner_file.is_none());
        assert_eq!(
            config.sessions.state_file,
            PathBuf::from("/var/lib/vigil/state.toml")
        );
        assert!(config.output.is_empty());
    }

    #[test]
    fn parses_full_schema() {
        let config = parse(
            r#"
[look]
theme = "/theme.slint"
background = "/background.png"
fit = "contain"
clock_format = "%I:%M %p"
[keyboard]
layout = "us"
variant = "intl"
options = "caps:escape"
model = "pc105"
rules = "evdev"
[sessions]
dirs = ["/tmp/s"]
remember = false
state_file = "/tmp/st.toml"
default = "Hyprland"
[profiles]
dir = "/etc/monitor-profiles"
[users]
show_list = false
[power]
enabled = false
[greeter]
user = "kiosk"
cmd = ["sway"]
banner_file = "/run/vigil/banner"
[lock]
grace_secs = 5
[output."DP-1"]
background = "/side.png"
fit = "center"
scale = 1.25
"#,
        )
        .unwrap();
        assert_eq!(config.look.theme, Some(PathBuf::from("/theme.slint")));
        assert_eq!(
            config.look.background,
            Some(PathBuf::from("/background.png"))
        );
        assert_eq!(config.look.fit.as_deref(), Some("contain"));
        assert_eq!(config.look.clock_format, "%I:%M %p");
        assert_eq!(config.keyboard.layout, "us");
        assert_eq!(config.keyboard.variant, "intl");
        assert_eq!(config.keyboard.options, "caps:escape");
        assert_eq!(config.keyboard.model, "pc105");
        assert_eq!(config.keyboard.rules, "evdev");
        assert_eq!(config.sessions.dirs, ["/tmp/s"]);
        assert!(!config.sessions.remember);
        assert_eq!(config.sessions.state_file, PathBuf::from("/tmp/st.toml"));
        assert_eq!(config.sessions.default, "Hyprland");
        assert_eq!(
            config.profiles.dir,
            Some(PathBuf::from("/etc/monitor-profiles"))
        );
        assert!(!config.users.show_list);
        assert!(!config.power.enabled);
        assert_eq!(config.greeter.user, "kiosk");
        assert_eq!(config.greeter.cmd, ["sway"]);
        assert_eq!(
            config.greeter.banner_file,
            Some(PathBuf::from("/run/vigil/banner"))
        );
        assert_eq!(config.lock.grace_secs, 5);
        let output = &config.output["DP-1"];
        assert_eq!(output.background, Some(PathBuf::from("/side.png")));
        assert_eq!(output.fit.as_deref(), Some("center"));
        assert_eq!(output.scale, Some(1.25));
    }

    #[test]
    fn invalid_toml_falls_back() {
        let path = std::env::temp_dir().join(format!("vigil-test-{}.toml", std::process::id()));
        std::fs::write(&path, "not = [toml").unwrap();
        assert_eq!(Config::load(Some(&path)), Config::default());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unknown_keys_ignored() {
        let config = parse("[look]\nbogus = 1\ntheme = \"/x\"\n[nonsense]\na = 2").unwrap();
        assert_eq!(config.look.theme, Some(PathBuf::from("/x")));
    }

    #[test]
    fn layered_prefers_user_file() {
        let path =
            std::env::temp_dir().join(format!("vigil-layered-test-{}.toml", std::process::id()));
        std::fs::write(&path, "[look]\nclock_format = \"%S\"").unwrap();
        assert_eq!(Config::load_layered(Some(&path)).look.clock_format, "%S");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn state_round_trip_with_escapes() {
        let tmp =
            std::env::temp_dir().join(format!("vigil-state-test-{}.toml", std::process::id()));
        let state = State {
            user: "al\"ice".into(),
            session: "Test DE".into(),
        };
        state.store(&tmp);
        assert_eq!(State::load(&tmp), Some(state));
        std::fs::remove_file(tmp).unwrap();
    }

    #[test]
    fn state_load_missing_is_none() {
        assert_eq!(
            State::load(Path::new("/nonexistent/vigil-state.toml")),
            None
        );
    }
}
