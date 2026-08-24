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
    pub warning: LockWarning,
    pub transition: LockTransition,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct LockWarning {
    pub duration_ms: u64,
    pub frost_in_ms: u64,
    pub frost_alpha: f32,
    pub wallpaper_in_ms: u64,
    pub easing: WarningEasing,
    pub cancel_on_motion_px: f64,
    /// How long past its scheduled commit a cancelable warning may be held
    /// waiting for the wallpaper before it locks anyway (0 = wait forever,
    /// the pre-0.4 behaviour). A wedged asset pipeline must not leave the
    /// machine unlocked (issue #56); a lock with a plain background beats
    /// an unlocked screen.
    pub wallpaper_hold_max_ms: u64,
    pub gui: WarningGui,
}

impl LockWarning {
    /// Upper bound on [`Self::wallpaper_hold_max_ms`]. The knob exists to
    /// bound how long the machine may sit unlocked waiting for an asset, so
    /// leaving it unbounded reintroduces the very problem it fixes — a
    /// fat-fingered value is indistinguishable in effect from "wait
    /// forever" but gives no signal. `0` stays an explicit, documented
    /// opt-out; anything else is clamped and logged, mirroring
    /// [`LockTransition::clamp`].
    pub const MAX_WALLPAPER_HOLD_MS: u64 = 30_000;

    /// Clamp the wallpaper hold. Returns whether anything changed. Never an
    /// error: a bad config must still lock.
    pub fn clamp(&mut self) -> bool {
        if self.wallpaper_hold_max_ms == 0
            || self.wallpaper_hold_max_ms <= Self::MAX_WALLPAPER_HOLD_MS
        {
            return false;
        }
        self.wallpaper_hold_max_ms = Self::MAX_WALLPAPER_HOLD_MS;
        true
    }
}

impl Default for LockWarning {
    fn default() -> Self {
        Self {
            duration_ms: 0,
            frost_in_ms: 1_500,
            frost_alpha: 0.35,
            wallpaper_in_ms: 1_500,
            easing: WarningEasing::EaseOut,
            cancel_on_motion_px: 8.0,
            wallpaper_hold_max_ms: 5_000,
            gui: WarningGui::default(),
        }
    }
}

/// The short, non-cancelable frost ramp around a manual or before-sleep
/// lock (issue #52). The idle warning is the cancelable, long form of the
/// same overlay; this one ignores input, commits on hotplug, and never waits
/// on the wallpaper. Zero durations restore the instant commit.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct LockTransition {
    /// Lock: tint ramps in over this, then the wallpaper.
    pub frost_in_ms: u64,
    pub wallpaper_in_ms: u64,
    /// Unlock: the wallpaper fades out over this, then the tint.
    pub wallpaper_out_ms: u64,
    pub frost_out_ms: u64,
    pub easing: WarningEasing,
}

impl Default for LockTransition {
    fn default() -> Self {
        Self {
            frost_in_ms: 150,
            wallpaper_in_ms: 250,
            wallpaper_out_ms: 250,
            frost_out_ms: 150,
            easing: WarningEasing::EaseOut,
        }
    }
}

impl LockTransition {
    /// Each ramp is bounded so a misconfiguration can delay, never prevent,
    /// the secure commit or the unlock.
    pub const MAX_RAMP_MS: u64 = 2_000;

    pub fn immediate() -> Self {
        Self {
            frost_in_ms: 0,
            wallpaper_in_ms: 0,
            wallpaper_out_ms: 0,
            frost_out_ms: 0,
            ..Self::default()
        }
    }

    pub fn in_ms(&self) -> u64 {
        self.frost_in_ms + self.wallpaper_in_ms
    }

    pub fn out_ms(&self) -> u64 {
        self.wallpaper_out_ms + self.frost_out_ms
    }

    pub fn ramps_in(&self) -> bool {
        self.in_ms() > 0
    }

    pub fn reveals(&self) -> bool {
        self.out_ms() > 0
    }

    /// The non-cancelable timeline for the frost-in ramp, sharing the tint
    /// alpha and the post-lock GUI animation with the idle warning so both
    /// lock paths look identical once locked.
    pub fn as_warning(&self, frost_alpha: f32, gui: WarningGui) -> LockWarning {
        LockWarning {
            duration_ms: self.in_ms(),
            frost_in_ms: self.frost_in_ms,
            frost_alpha,
            wallpaper_in_ms: self.wallpaper_in_ms,
            easing: self.easing,
            cancel_on_motion_px: f64::INFINITY,
            // The transition never waits on the wallpaper, so a hold cap is
            // meaningless for it.
            wallpaper_hold_max_ms: 0,
            gui,
        }
    }

    /// Clamp each ramp to [`Self::MAX_RAMP_MS`], scaling its halves
    /// proportionally. Returns whether anything changed. Never an error: a
    /// bad config must still lock.
    pub fn clamp(&mut self) -> bool {
        fn clamp_pair(first: &mut u64, second: &mut u64) -> bool {
            let total = *first + *second;
            if total <= LockTransition::MAX_RAMP_MS {
                return false;
            }
            *first = *first * LockTransition::MAX_RAMP_MS / total;
            *second = LockTransition::MAX_RAMP_MS - *first;
            true
        }
        let changed_in = clamp_pair(&mut self.frost_in_ms, &mut self.wallpaper_in_ms);
        let changed_out = clamp_pair(&mut self.wallpaper_out_ms, &mut self.frost_out_ms);
        changed_in || changed_out
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarningEasing {
    Linear,
    EaseOut,
    EaseInOut,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct WarningGui {
    pub start: WarningKeyframe,
    pub offset_ms: u64,
    pub duration_ms: u64,
    pub kind: WarningAnimation,
    pub element: Vec<WarningElement>,
}

impl Default for WarningGui {
    fn default() -> Self {
        Self {
            start: WarningKeyframe::Locked,
            offset_ms: 0,
            duration_ms: 400,
            kind: WarningAnimation::Fade,
            element: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct WarningElement {
    pub selector: String,
    pub start: WarningKeyframe,
    pub offset_ms: u64,
    pub duration_ms: u64,
    pub kind: WarningAnimation,
}

impl Default for WarningElement {
    fn default() -> Self {
        Self {
            selector: String::new(),
            start: WarningKeyframe::None,
            offset_ms: 0,
            duration_ms: 0,
            kind: WarningAnimation::None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarningKeyframe {
    Painted,
    FrostStart,
    FrostEnd,
    WallpaperStart,
    WallpaperSolid,
    Locked,
    None,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarningAnimation {
    Fade,
    SlideUp,
    Scale,
    None,
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
    pub fn validate_warning(&self) -> Result<(), String> {
        const SELECTORS: [&str; 5] = ["clock", "user_selector", "password", "status", "power"];
        let unknown: Vec<_> = self
            .lock
            .warning
            .gui
            .element
            .iter()
            .map(|element| element.selector.as_str())
            .filter(|selector| !SELECTORS.contains(selector))
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "unknown lock.warning.gui selectors: {}; supported: {}",
                unknown.join(", "),
                SELECTORS.join(", ")
            ))
        }
    }

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
        let user_path = xdg_paths::ConfigDirs::from_env()
            .ok()
            .map(|dirs| dirs.config_dir("vigil").join("config.toml"));
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
    fn parses_warning_and_element_timeline() {
        let config = parse(
            r#"
[lock.warning]
duration_ms = 10000
frost_in_ms = 1500
frost_alpha = 0.4
wallpaper_in_ms = 1200
easing = "ease_in_out"
cancel_on_motion_px = 9.5
wallpaper_hold_max_ms = 2000

[lock.warning.gui]
start = "locked"
duration_ms = 400
kind = "fade"

[[lock.warning.gui.element]]
selector = "clock"
start = "painted"
duration_ms = 0
kind = "none"
"#,
        )
        .unwrap();
        let warning = config.lock.warning;
        assert_eq!(warning.duration_ms, 10_000);
        // Parsed from the fixture above, not the Default impl: a key
        // whose TOML spelling drifts from the field must fail here.
        assert_eq!(warning.wallpaper_hold_max_ms, 2_000);
        assert_eq!(
            parse("").unwrap().lock.warning.wallpaper_hold_max_ms,
            5_000,
            "the default still applies when the key is absent"
        );
        assert_eq!(warning.easing, WarningEasing::EaseInOut);
        assert_eq!(warning.gui.element.len(), 1);
        assert_eq!(warning.gui.element[0].selector, "clock");
        assert_eq!(warning.gui.element[0].start, WarningKeyframe::Painted);
        assert_eq!(warning.gui.element[0].kind, WarningAnimation::None);
    }

    #[test]
    fn parses_lock_transition() {
        let config = parse(
            r#"
[lock.transition]
frost_in_ms = 100
wallpaper_in_ms = 200
wallpaper_out_ms = 300
frost_out_ms = 50
easing = "linear"
"#,
        )
        .unwrap();
        let transition = config.lock.transition;
        assert_eq!(transition.frost_in_ms, 100);
        assert_eq!(transition.wallpaper_in_ms, 200);
        assert_eq!(transition.wallpaper_out_ms, 300);
        assert_eq!(transition.frost_out_ms, 50);
        assert_eq!(transition.easing, WarningEasing::Linear);
        let warning = transition.as_warning(0.5, WarningGui::default());
        assert_eq!(warning.duration_ms, 300);
        assert_eq!(warning.frost_alpha, 0.5);
        assert_eq!(warning.easing, WarningEasing::Linear);
    }

    #[test]
    fn transition_defaults_are_short() {
        let transition = LockTransition::default();
        assert_eq!(transition.in_ms(), 400);
        assert_eq!(transition.out_ms(), 400);
        assert!(transition.ramps_in() && transition.reveals());
        assert_eq!(parse("").unwrap().lock.transition, transition);
    }

    #[test]
    fn the_wallpaper_hold_is_clamped() {
        // The knob exists to bound how long the machine sits unlocked, so
        // leaving it unbounded reintroduces the problem it fixes.
        let mut warning = LockWarning {
            wallpaper_hold_max_ms: u64::MAX,
            ..LockWarning::default()
        };
        assert!(warning.clamp());
        assert_eq!(
            warning.wallpaper_hold_max_ms,
            LockWarning::MAX_WALLPAPER_HOLD_MS
        );
        // The documented opt-out survives, and a sane value is untouched.
        let mut forever = LockWarning {
            wallpaper_hold_max_ms: 0,
            ..LockWarning::default()
        };
        assert!(!forever.clamp());
        assert_eq!(forever.wallpaper_hold_max_ms, 0);
        assert!(!LockWarning::default().clamp());
    }

    #[test]
    fn transition_clamps_each_ramp_to_max() {
        let mut transition = LockTransition {
            frost_in_ms: 3_000,
            wallpaper_in_ms: 3_000,
            ..LockTransition::default()
        };
        assert!(transition.clamp());
        assert_eq!(transition.in_ms(), LockTransition::MAX_RAMP_MS);
        assert_eq!(transition.frost_in_ms, 1_000);
        assert_eq!(transition.out_ms(), 400);
        assert!(!LockTransition::default().clone().clamp());
    }

    #[test]
    fn immediate_has_no_ramps() {
        let transition = LockTransition::immediate();
        assert!(!transition.ramps_in() && !transition.reveals());
        assert_eq!(transition.easing, LockTransition::default().easing);
    }

    #[test]
    fn warning_rejects_unknown_element_selectors() {
        let config = parse(
            r#"
[[lock.warning.gui.element]]
selector = "not-a-real-component"
"#,
        )
        .unwrap();
        assert!(
            config
                .validate_warning()
                .unwrap_err()
                .contains("not-a-real-component")
        );
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
