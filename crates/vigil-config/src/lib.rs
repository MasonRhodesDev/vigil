use std::collections::HashMap;
use std::io::Read;
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
    pub splash: Splash,
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Splash {
    pub enabled: bool,
    pub scene: PathBuf,
    pub logo: Option<PathBuf>,
    pub ground: Option<String>,
    pub mark: Option<String>,
    pub accent: Option<String>,
    pub debug_tags: bool,
    pub on_first_frame: String,
    pub max_wait_ms: u64,
}

pub const DEFAULT_SCENE: &str = "/usr/share/desktop-boot/scene.json";

impl Default for Splash {
    fn default() -> Self {
        Self {
            enabled: true,
            scene: PathBuf::from(DEFAULT_SCENE),
            logo: None,
            ground: None,
            mark: None,
            accent: None,
            debug_tags: false,
            on_first_frame: String::new(),
            max_wait_ms: 4000,
        }
    }
}

impl Splash {
    pub fn load_file(path: &Path) -> Splash {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(splash) => splash,
                Err(e) => {
                    vigil_log::say!("vigil: splash config {}: {e}", path.display());
                    Splash::default()
                }
            },
            Err(e) => {
                vigil_log::say!("vigil: splash config {}: {e}", path.display());
                Splash::default()
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Render {
    pub backend: String,
    pub keep_display_state: bool,
    pub hold_last_frame_on_exit: bool,
    pub hold_timeout_ms: Option<u32>,
}

impl Render {
    pub fn hold_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            u64::from(self.hold_timeout_ms.unwrap_or(10_000)).clamp(1_000, 60_000),
        )
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Profiles {
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
    pub wallpaper_hold_max_ms: u64,
    pub gui: WarningGui,
}

impl LockWarning {
    pub const MAX_WALLPAPER_HOLD_MS: u64 = 30_000;

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

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct LockTransition {
    pub frost_in_ms: u64,
    pub wallpaper_in_ms: u64,
    pub wallpaper_out_ms: u64,
    pub frost_out_ms: u64,
    pub easing: WarningEasing,
}

impl Default for LockTransition {
    fn default() -> Self {
        Self {
            frost_in_ms: 150,
            wallpaper_in_ms: 250,
            wallpaper_out_ms: 0,
            frost_out_ms: 0,
            easing: WarningEasing::EaseOut,
        }
    }
}

impl LockTransition {
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

    pub fn reveal_ms(&self) -> u64 {
        self.wallpaper_out_ms
    }

    pub fn reveal_frost_ms(&self) -> u64 {
        self.frost_out_ms
    }

    pub fn out_ms(&self) -> u64 {
        self.wallpaper_out_ms.max(self.frost_out_ms)
    }

    pub fn ramps_in(&self) -> bool {
        self.in_ms() > 0
    }

    pub fn reveals(&self) -> bool {
        self.out_ms() > 0
    }

    pub fn reveal_blurs(&self) -> bool {
        self.frost_out_ms > 0
    }

    pub fn as_warning(&self, frost_alpha: f32, gui: WarningGui) -> LockWarning {
        LockWarning {
            duration_ms: self.in_ms(),
            frost_in_ms: self.frost_in_ms,
            frost_alpha,
            wallpaper_in_ms: self.wallpaper_in_ms,
            easing: self.easing,
            cancel_on_motion_px: f64::INFINITY,
            wallpaper_hold_max_ms: 0,
            gui,
        }
    }

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
        fn clamp_one(value: &mut u64) -> bool {
            if *value > LockTransition::MAX_RAMP_MS {
                *value = LockTransition::MAX_RAMP_MS;
                true
            } else {
                false
            }
        }
        let changed_in = clamp_pair(&mut self.frost_in_ms, &mut self.wallpaper_in_ms);
        let changed_wallpaper_out = clamp_one(&mut self.wallpaper_out_ms);
        let changed_frost_out = clamp_one(&mut self.frost_out_ms);
        changed_in || changed_wallpaper_out || changed_frost_out
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

    pub fn load(path: Option<&Path>) -> Config {
        load_file(path.unwrap_or_else(|| Path::new(SYSTEM_CONFIG))).unwrap_or_default()
    }

    pub fn overlay(&mut self, user: &toml::Table) -> Vec<IgnoredOverlayKey> {
        let mut allowed = Vec::new();
        let mut ignored = Vec::new();
        walk_overlay("", user, &mut allowed, &mut ignored);
        let enforce_valid = self.validate_warning().is_ok();
        for (key, value) in allowed {
            let apply = OVERLAY
                .iter()
                .find(|(path, _)| *path == key)
                .map(|(_, apply)| apply)
                .expect("walk_overlay only yields OVERLAY keys");
            let restore = enforce_valid.then(|| self.clone());
            let refusal = match apply(self, value) {
                Err(err) => Some(OverlayRefusal::Invalid(
                    err.message().trim().replace('\n', "; "),
                )),
                Ok(()) => match self.validate_warning() {
                    Ok(()) => None,
                    Err(reason) if enforce_valid => Some(OverlayRefusal::Invalid(reason)),
                    Err(_) => None,
                },
            };
            if let Some(refusal) = refusal {
                if let Some(restore) = restore {
                    *self = restore;
                }
                ignored.push(IgnoredOverlayKey { key, refusal });
            }
        }
        ignored.sort_by(|a, b| a.key.cmp(&b.key));
        ignored
    }

    pub fn load_layered_reporting(path: Option<&Path>) -> (Config, Vec<IgnoredOverlayKey>) {
        if let Some(path) = path {
            return (load_file(path).unwrap_or_default(), Vec::new());
        }
        let user = xdg_paths::ConfigDirs::from_env()
            .ok()
            .map(|dirs| dirs.config_dir("vigil").join("config.toml"));
        load_layered_from(Path::new(SYSTEM_CONFIG), user.as_deref())
    }

    pub fn load_layered(path: Option<&Path>) -> Config {
        let (config, ignored) = Config::load_layered_reporting(path);
        for notice in ignored.iter().take(MAX_REPORTED_REFUSALS) {
            vigil_log::say!("vigil-config: {notice}");
        }
        if let Some(rest) = ignored.len().checked_sub(MAX_REPORTED_REFUSALS)
            && rest > 0
        {
            vigil_log::say!("vigil-config: user config: and {rest} more ignored keys");
        }
        config
    }
}

const MAX_REPORTED_REFUSALS: usize = 8;

type OverlaySetter = fn(&mut Config, toml::Value) -> Result<(), toml::de::Error>;

const OVERLAY: &[(&str, OverlaySetter)] = &[
    ("look.theme", |config, value| {
        config.look.theme = value.try_into()?;
        Ok(())
    }),
    ("look.background", |config, value| {
        config.look.background = value.try_into()?;
        Ok(())
    }),
    ("look.fit", |config, value| {
        config.look.fit = value.try_into()?;
        Ok(())
    }),
    ("look.clock_format", |config, value| {
        config.look.clock_format = value.try_into()?;
        Ok(())
    }),
    ("lock.transition.frost_in_ms", |config, value| {
        config.lock.transition.frost_in_ms = value.try_into()?;
        Ok(())
    }),
    ("lock.transition.wallpaper_in_ms", |config, value| {
        config.lock.transition.wallpaper_in_ms = value.try_into()?;
        Ok(())
    }),
    ("lock.transition.wallpaper_out_ms", |config, value| {
        config.lock.transition.wallpaper_out_ms = value.try_into()?;
        Ok(())
    }),
    ("lock.transition.frost_out_ms", |config, value| {
        config.lock.transition.frost_out_ms = value.try_into()?;
        Ok(())
    }),
    ("lock.transition.easing", |config, value| {
        config.lock.transition.easing = value.try_into()?;
        Ok(())
    }),
    ("lock.warning.frost_in_ms", |config, value| {
        config.lock.warning.frost_in_ms = value.try_into()?;
        Ok(())
    }),
    ("lock.warning.frost_alpha", |config, value| {
        config.lock.warning.frost_alpha = value.try_into()?;
        Ok(())
    }),
    ("lock.warning.easing", |config, value| {
        config.lock.warning.easing = value.try_into()?;
        Ok(())
    }),
    ("lock.warning.gui", |config, value| {
        config.lock.warning.gui = value.try_into::<OverlayWarningGui>()?.into();
        Ok(())
    }),
    ("output", |config, value| {
        let overrides: HashMap<String, OverlayOutputOverride> = value.try_into()?;
        for (name, over) in overrides {
            let entry = config.output.entry(name).or_default();
            let OverlayOutputOverride {
                background,
                fit,
                scale,
            } = over;
            if background.is_some() {
                entry.background = background;
            }
            if fit.is_some() {
                entry.fit = fit;
            }
            if scale.is_some() {
                entry.scale = scale;
            }
        }
        Ok(())
    }),
    ("profiles.dir", |config, value| {
        config.profiles.dir = value.try_into()?;
        Ok(())
    }),
];

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OverlayWarningGui {
    start: WarningKeyframe,
    offset_ms: u64,
    duration_ms: u64,
    kind: WarningAnimation,
    element: Vec<OverlayWarningElement>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OverlayWarningElement {
    selector: String,
    start: WarningKeyframe,
    offset_ms: u64,
    duration_ms: u64,
    kind: WarningAnimation,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OverlayOutputOverride {
    background: Option<PathBuf>,
    fit: Option<String>,
    scale: Option<f32>,
}

impl From<WarningGui> for OverlayWarningGui {
    fn from(gui: WarningGui) -> Self {
        let WarningGui {
            start,
            offset_ms,
            duration_ms,
            kind,
            element,
        } = gui;
        Self {
            start,
            offset_ms,
            duration_ms,
            kind,
            element: element.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<OverlayWarningGui> for WarningGui {
    fn from(gui: OverlayWarningGui) -> Self {
        let OverlayWarningGui {
            start,
            offset_ms,
            duration_ms,
            kind,
            element,
        } = gui;
        Self {
            start,
            offset_ms,
            duration_ms,
            kind,
            element: element.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<WarningElement> for OverlayWarningElement {
    fn from(element: WarningElement) -> Self {
        let WarningElement {
            selector,
            start,
            offset_ms,
            duration_ms,
            kind,
        } = element;
        Self {
            selector,
            start,
            offset_ms,
            duration_ms,
            kind,
        }
    }
}

impl From<OverlayWarningElement> for WarningElement {
    fn from(element: OverlayWarningElement) -> Self {
        let OverlayWarningElement {
            selector,
            start,
            offset_ms,
            duration_ms,
            kind,
        } = element;
        Self {
            selector,
            start,
            offset_ms,
            duration_ms,
            kind,
        }
    }
}

impl Default for OverlayWarningGui {
    fn default() -> Self {
        WarningGui::default().into()
    }
}

impl Default for OverlayWarningElement {
    fn default() -> Self {
        WarningElement::default().into()
    }
}

const SECURITY_FLOOR: &[&str] = &[
    "lock.grace_secs",
    "lock.warning.cancel_on_motion_px",
    "lock.warning.duration_ms",
    "lock.warning.wallpaper_hold_max_ms",
    "lock.warning.wallpaper_in_ms",
];

const GREETER_SCOPE: &[&str] = &[
    "greeter", "keyboard", "power", "render", "sessions", "users",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayRefusal {
    SecurityFloor,
    GreeterScope,
    NotOverlayable,
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredOverlayKey {
    pub key: String,
    pub refusal: OverlayRefusal,
}

impl std::fmt::Display for IgnoredOverlayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match &self.refusal {
            OverlayRefusal::SecurityFloor => "security policy — system config only".to_string(),
            OverlayRefusal::GreeterScope => {
                "greeter-scope — the greeter never reads the user file".to_string()
            }
            OverlayRefusal::NotOverlayable => {
                "not overlay-allowed — system config only".to_string()
            }
            OverlayRefusal::Invalid(err) => format!("invalid value — {err}"),
        };
        write!(f, "user config: ignoring {}: {reason}", self.key)
    }
}

fn walk_overlay(
    prefix: &str,
    table: &toml::Table,
    allowed: &mut Vec<(String, toml::Value)>,
    ignored: &mut Vec<IgnoredOverlayKey>,
) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if OVERLAY.iter().any(|(whitelisted, _)| *whitelisted == path) {
            allowed.push((path, value.clone()));
            continue;
        }
        let descend = format!("{path}.");
        match value.as_table() {
            Some(sub)
                if OVERLAY
                    .iter()
                    .any(|(deeper, _)| deeper.starts_with(&descend)) =>
            {
                walk_overlay(&path, sub, allowed, ignored);
            }
            _ => ignored.push(IgnoredOverlayKey {
                refusal: refuse(&path),
                key: path,
            }),
        }
    }
}

fn refuse(path: &str) -> OverlayRefusal {
    if SECURITY_FLOOR.contains(&path) {
        OverlayRefusal::SecurityFloor
    } else if GREETER_SCOPE.contains(&path.split('.').next().unwrap_or(path)) {
        OverlayRefusal::GreeterScope
    } else {
        OverlayRefusal::NotOverlayable
    }
}

const MAX_OVERLAY_BYTES: u64 = 256 * 1024;

fn read_overlay(path: &Path) -> Result<Option<String>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_OVERLAY_BYTES {
        return Err(format!(
            "{} bytes exceeds the {MAX_OVERLAY_BYTES} byte overlay limit",
            meta.len()
        ));
    }
    let file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    match file.metadata() {
        Ok(meta) if !meta.is_file() => return Err("not a regular file".into()),
        Ok(_) => {}
        Err(err) => return Err(err.to_string()),
    }
    let mut source = String::new();
    std::io::Read::take(file, MAX_OVERLAY_BYTES + 1)
        .read_to_string(&mut source)
        .map_err(|err| err.to_string())?;
    if source.len() as u64 > MAX_OVERLAY_BYTES {
        return Err(format!(
            "larger than the {MAX_OVERLAY_BYTES} byte overlay limit"
        ));
    }
    Ok(Some(source))
}

fn load_layered_from(system: &Path, user: Option<&Path>) -> (Config, Vec<IgnoredOverlayKey>) {
    let mut config = load_file(system).unwrap_or_default();
    let Some(user) = user else {
        return (config, Vec::new());
    };
    let source = match read_overlay(user) {
        Ok(Some(source)) => source,
        Ok(None) => return (config, Vec::new()),
        Err(reason) => {
            vigil_log::say!(
                "vigil-config: {}: overlay refused: {reason}",
                user.display()
            );
            return (config, Vec::new());
        }
    };
    match toml::from_str::<toml::Table>(&source) {
        Ok(table) => {
            let ignored = config.overlay(&table);
            (config, ignored)
        }
        Err(err) => {
            vigil_log::say!(
                "vigil-config: {}: {err}; ignoring the user overlay",
                user.display()
            );
            (config, Vec::new())
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct State {
    pub user: String,
    pub session: String,
    pub auto_started_at: Option<u64>,
    pub auto_streak: u32,
    pub unprompted_user: String,
    #[serde(deserialize_with = "complete_entries")]
    pub first_question: Vec<FirstQuestion>,
}

fn complete_entries<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<FirstQuestion>, D::Error> {
    let entries = Vec::<toml::Value>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .filter_map(|entry| entry.try_into().ok())
        .collect())
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FirstQuestion {
    pub user: String,
    pub text: String,
    pub secret: bool,
}

fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl State {
    pub fn load(path: &Path) -> Option<State> {
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                vigil_log::say!("vigil-config: {}: {e}", path.display());
                return None;
            }
        };
        match toml::from_str(&source) {
            Ok(state) => Some(state),
            Err(e) => {
                vigil_log::say!("vigil-config: {}: {e}", path.display());
                None
            }
        }
    }

    pub fn store(&self, path: &Path) -> bool {
        let mut body = format!(
            "user = {}\nsession = {}\nauto_streak = {}\n",
            quoted(&self.user),
            quoted(&self.session),
            self.auto_streak
        );
        if let Some(at) = self.auto_started_at {
            body.push_str(&format!("auto_started_at = {at}\n"));
        }
        if !self.unprompted_user.is_empty() {
            body.push_str(&format!(
                "unprompted_user = {}\n",
                quoted(&self.unprompted_user)
            ));
        }
        for question in &self.first_question {
            body.push_str(&format!(
                "\n[[first_question]]\nuser = {}\ntext = {}\nsecret = {}\n",
                quoted(&question.user),
                quoted(&question.text),
                question.secret
            ));
        }
        let tmp = path.with_extension("tmp");
        let result = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = &result {
            vigil_log::say!(
                "vigil-config: cannot store state at {}: {e}",
                path.display()
            );
        }
        result.is_ok()
    }
}

fn load_file(path: &Path) -> Option<Config> {
    match std::fs::read_to_string(path) {
        Ok(source) => match parse(&source) {
            Ok(config) => Some(config),
            Err(err) => {
                vigil_log::say!("vigil-config: {}: {err}; using defaults", path.display());
                Some(Config::default())
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            vigil_log::say!("vigil-config: {}: {err}; using defaults", path.display());
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
        assert_eq!(transition.out_ms(), 0);
        assert!(transition.ramps_in());
        assert!(!transition.reveals(), "unlock is instant by default");
        assert!(!transition.reveal_blurs(), "unlock is blur-free by default");
        assert_eq!(parse("").unwrap().lock.transition, transition);
    }

    #[test]
    fn the_wallpaper_hold_is_clamped() {
        let mut warning = LockWarning {
            wallpaper_hold_max_ms: u64::MAX,
            ..LockWarning::default()
        };
        assert!(warning.clamp());
        assert_eq!(
            warning.wallpaper_hold_max_ms,
            LockWarning::MAX_WALLPAPER_HOLD_MS
        );
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
            wallpaper_out_ms: 3_000,
            ..LockTransition::default()
        };
        assert!(transition.clamp());
        assert_eq!(transition.in_ms(), LockTransition::MAX_RAMP_MS);
        assert_eq!(transition.frost_in_ms, 1_000);
        assert_eq!(transition.out_ms(), LockTransition::MAX_RAMP_MS);
        assert!(!LockTransition::default().clone().clamp());
    }

    #[test]
    fn reveal_blur_is_an_independent_opt_in() {
        let blurring = LockTransition {
            wallpaper_out_ms: 300,
            frost_out_ms: 200,
            ..LockTransition::default()
        };
        assert!(blurring.reveals());
        assert!(blurring.reveal_blurs());
        assert_eq!(blurring.reveal_ms(), 300);
        assert_eq!(blurring.reveal_frost_ms(), 200);
        let blur_only = LockTransition {
            wallpaper_out_ms: 0,
            frost_out_ms: 200,
            ..LockTransition::default()
        };
        assert!(blur_only.reveals());
        assert!(blur_only.reveal_blurs());
    }

    #[test]
    fn clamp_bounds_the_reveal_fades_independently() {
        let mut ok = LockTransition {
            wallpaper_out_ms: LockTransition::MAX_RAMP_MS,
            frost_out_ms: LockTransition::MAX_RAMP_MS,
            ..LockTransition::default()
        };
        assert!(!ok.clamp(), "each fade alone is within the ceiling");
        assert_eq!(ok.wallpaper_out_ms, LockTransition::MAX_RAMP_MS);
        assert_eq!(ok.frost_out_ms, LockTransition::MAX_RAMP_MS);

        let mut over = LockTransition {
            wallpaper_out_ms: 300,
            frost_out_ms: 5_000,
            ..LockTransition::default()
        };
        assert!(over.clamp());
        assert_eq!(over.wallpaper_out_ms, 300, "the in-limit fade is untouched");
        assert_eq!(over.frost_out_ms, LockTransition::MAX_RAMP_MS);
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

    const SYSTEM: &str = "
[look]
clock_format = \"%H:%M\"
background = \"/etc/greetd/system.png\"

[lock]
grace_secs = 0

[lock.warning]
duration_ms = 20000
frost_in_ms = 1500
cancel_on_motion_px = 8.0
wallpaper_hold_max_ms = 5000

[lock.transition]
frost_in_ms = 150

[output.\"eDP-1\"]
background = \"/etc/greetd/internal.png\"
";

    fn overlay(user: &str) -> (Config, Vec<IgnoredOverlayKey>) {
        let mut config = parse(SYSTEM).unwrap();
        let table: toml::Table = toml::from_str(user).unwrap();
        let ignored = config.overlay(&table);
        (config, ignored)
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, body: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("vigil-{tag}-{}-{serial}.toml", std::process::id()));
            std::fs::write(&path, body).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[allow(clippy::type_complexity)]
    fn floor_probes() -> Vec<(&'static str, fn(&Config) -> String)> {
        vec![
            ("lock.grace_secs", |c| c.lock.grace_secs.to_string()),
            ("lock.warning.cancel_on_motion_px", |c| {
                c.lock.warning.cancel_on_motion_px.to_string()
            }),
            ("lock.warning.duration_ms", |c| {
                c.lock.warning.duration_ms.to_string()
            }),
            ("lock.warning.wallpaper_hold_max_ms", |c| {
                c.lock.warning.wallpaper_hold_max_ms.to_string()
            }),
            ("lock.warning.wallpaper_in_ms", |c| {
                c.lock.warning.wallpaper_in_ms.to_string()
            }),
        ]
    }

    struct SetterProbe {
        key: &'static str,
        source: &'static str,
        expect: fn(&mut Config),
        adversarial: &'static str,
    }

    fn setter_probes() -> Vec<SetterProbe> {
        macro_rules! probe {
            ($key:literal, $source:literal, $adversarial:literal, $expect:expr) => {
                SetterProbe {
                    key: $key,
                    source: $source,
                    adversarial: $adversarial,
                    expect: $expect,
                }
            };
        }
        vec![
            probe!(
                "look.theme",
                "[look]\ntheme = \"/tmp/t.slint\"\n",
                "[look]\ntheme = \"/proc/self/mem\"\n",
                |c| c.look.theme = Some("/tmp/t.slint".into())
            ),
            probe!(
                "look.background",
                "[look]\nbackground = \"/tmp/b.png\"\n",
                "[look]\nbackground = \"/dev/zero\"\n",
                |c| c.look.background = Some("/tmp/b.png".into())
            ),
            probe!(
                "look.fit",
                "[look]\nfit = \"tile\"\n",
                "[look]\nfit = \"\"\n",
                |c| c.look.fit = Some("tile".into())
            ),
            probe!(
                "look.clock_format",
                "[look]\nclock_format = \"%S\"\n",
                "[look]\nclock_format = \"%\"\n",
                |c| c.look.clock_format = "%S".into()
            ),
            probe!(
                "lock.transition.frost_in_ms",
                "[lock.transition]\nfrost_in_ms = 321\n",
                "[lock.transition]\nfrost_in_ms = 9223372036854775807\n",
                |c| c.lock.transition.frost_in_ms = 321
            ),
            probe!(
                "lock.transition.wallpaper_in_ms",
                "[lock.transition]\nwallpaper_in_ms = 322\n",
                "[lock.transition]\nwallpaper_in_ms = 9223372036854775807\n",
                |c| c.lock.transition.wallpaper_in_ms = 322
            ),
            probe!(
                "lock.transition.wallpaper_out_ms",
                "[lock.transition]\nwallpaper_out_ms = 323\n",
                "[lock.transition]\nwallpaper_out_ms = 9223372036854775807\n",
                |c| c.lock.transition.wallpaper_out_ms = 323
            ),
            probe!(
                "lock.transition.frost_out_ms",
                "[lock.transition]\nfrost_out_ms = 324\n",
                "[lock.transition]\nfrost_out_ms = 9223372036854775807\n",
                |c| c.lock.transition.frost_out_ms = 324
            ),
            probe!(
                "lock.transition.easing",
                "[lock.transition]\neasing = \"linear\"\n",
                "[lock.transition]\neasing = \"ease_in_out\"\n",
                |c| c.lock.transition.easing = WarningEasing::Linear
            ),
            probe!(
                "lock.warning.frost_in_ms",
                "[lock.warning]\nfrost_in_ms = 325\n",
                "[lock.warning]\nfrost_in_ms = 9223372036854775807\n",
                |c| c.lock.warning.frost_in_ms = 325
            ),
            probe!(
                "lock.warning.frost_alpha",
                "[lock.warning]\nfrost_alpha = 0.25\n",
                "[lock.warning]\nfrost_alpha = -1000.0\n",
                |c| c.lock.warning.frost_alpha = 0.25
            ),
            probe!(
                "lock.warning.easing",
                "[lock.warning]\neasing = \"linear\"\n",
                "[lock.warning]\neasing = \"ease_in_out\"\n",
                |c| c.lock.warning.easing = WarningEasing::Linear
            ),
            probe!(
                "lock.warning.gui",
                "[lock.warning.gui]\nduration_ms = 777\n",
                "[lock.warning.gui]\nduration_ms = 9223372036854775807\noffset_ms = 9223372036854775807\n",
                |c| c.lock.warning.gui.duration_ms = 777
            ),
            probe!(
                "output",
                "[output.\"DP-1\"]\nscale = 2.0\n",
                "[output.\"DP-1\"]\nscale = 1e30\n",
                |c| {
                    c.output.insert(
                        "DP-1".to_string(),
                        OutputOverride {
                            scale: Some(2.0),
                            ..OutputOverride::default()
                        },
                    );
                }
            ),
            probe!(
                "profiles.dir",
                "[profiles]\ndir = \"/tmp/profiles\"\n",
                "[profiles]\ndir = \"/\"\n",
                |c| c.profiles.dir = Some("/tmp/profiles".into())
            ),
        ]
    }

    #[test]
    fn floor_probes_cover_the_floor() {
        let probed: Vec<_> = floor_probes().iter().map(|(key, _)| *key).collect();
        assert_eq!(
            probed,
            SECURITY_FLOOR.to_vec(),
            "a floor key with no effect probe is a floor nobody checks"
        );
    }

    #[test]
    fn setter_probes_cover_the_whitelist() {
        let probed: Vec<_> = setter_probes().iter().map(|probe| probe.key).collect();
        let whitelisted: Vec<_> = OVERLAY.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            probed, whitelisted,
            "every overlay key needs a probe: an unprobed setter is an unchecked write"
        );
    }

    #[test]
    fn each_overlay_key_writes_only_its_own_field() {
        for probe in setter_probes() {
            let (got, ignored) = overlay(probe.source);
            assert_eq!(ignored, Vec::new(), "{} was refused", probe.key);
            let mut want = parse(SYSTEM).unwrap();
            (probe.expect)(&mut want);
            assert_eq!(
                got, want,
                "{} did not write exactly its own field",
                probe.key
            );
        }
    }

    #[test]
    fn no_overlay_key_can_move_a_floored_field() {
        let system = parse(SYSTEM).unwrap();
        for probe in setter_probes() {
            for source in [probe.source, probe.adversarial] {
                let (got, _) = overlay(source);
                for (key, read) in floor_probes() {
                    assert_eq!(
                        read(&got),
                        read(&system),
                        "overlay key {} moved floored field {key}",
                        probe.key
                    );
                }
            }
        }
    }

    #[test]
    fn overlay_merges_cosmetic_keys_over_system() {
        let (config, ignored) = overlay(
            "
[look]
clock_format = \"%S\"

[lock.transition]
frost_in_ms = 400
frost_out_ms = 300

[lock.warning]
frost_alpha = 0.8
easing = \"linear\"

[output.\"DP-1\"]
scale = 2.0

[profiles]
dir = \"/etc/vigil/monitor-profiles\"
",
        );
        assert_eq!(ignored, Vec::new());
        assert_eq!(config.look.clock_format, "%S");
        assert_eq!(config.lock.transition.frost_in_ms, 400);
        assert_eq!(config.lock.transition.frost_out_ms, 300);
        assert_eq!(config.lock.warning.frost_alpha, 0.8);
        assert_eq!(config.lock.warning.easing, WarningEasing::Linear);
        assert_eq!(config.output["DP-1"].scale, Some(2.0));
        assert_eq!(
            config.output["eDP-1"].background,
            Some(PathBuf::from("/etc/greetd/internal.png"))
        );
        assert_eq!(
            config.profiles.dir,
            Some(PathBuf::from("/etc/vigil/monitor-profiles"))
        );
        assert_eq!(
            config.look.background,
            Some(PathBuf::from("/etc/greetd/system.png"))
        );
        assert_eq!(config.lock.warning.frost_in_ms, 1_500);
    }

    #[test]
    fn overlay_names_unknown_keys_inside_a_whole_table_take() {
        let (config, ignored) = overlay("[lock.warning.gui]\nwallpaper_hold_max_ms = 0\n");
        assert_eq!(config.lock.warning.wallpaper_hold_max_ms, 5_000);
        assert_eq!(
            config.lock.warning.gui,
            parse(SYSTEM).unwrap().lock.warning.gui
        );
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].key, "lock.warning.gui");
        assert!(
            matches!(&ignored[0].refusal, OverlayRefusal::Invalid(reason)
                if reason.contains("wallpaper_hold_max_ms")),
            "{:?}",
            ignored[0].refusal
        );

        let (_, ignored) =
            overlay("[[lock.warning.gui.element]]\nselector = \"clock\"\ngrace_secs = 99\n");
        assert_eq!(ignored.len(), 1);
        assert!(
            matches!(&ignored[0].refusal, OverlayRefusal::Invalid(reason)
                if reason.contains("grace_secs")),
            "{:?}",
            ignored[0].refusal
        );

        let (_, ignored) = overlay("[output.\"DP-1\"]\nscael = 2.0\n");
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].key, "output");
        assert!(
            matches!(&ignored[0].refusal, OverlayRefusal::Invalid(reason)
                if reason.contains("scael")),
            "{:?}",
            ignored[0].refusal
        );
    }

    #[test]
    fn overlay_output_merges_per_connector_and_per_field() {
        let (config, ignored) = overlay("[output.\"DP-1\"]\nscale = 2.0\n");
        assert_eq!(ignored, Vec::new());
        assert_eq!(
            config.output["eDP-1"].background,
            Some(PathBuf::from("/etc/greetd/internal.png")),
            "the operator's other connector was erased"
        );
        assert_eq!(config.output["DP-1"].scale, Some(2.0));

        let (config, ignored) = overlay("[output.\"eDP-1\"]\nscale = 1.5\n");
        assert_eq!(ignored, Vec::new());
        assert_eq!(config.output["eDP-1"].scale, Some(1.5));
        assert_eq!(
            config.output["eDP-1"].background,
            Some(PathBuf::from("/etc/greetd/internal.png"))
        );
    }

    #[test]
    fn refusal_output_is_capped_but_the_list_is_not() {
        let junk: String = (0..40).map(|n| format!("junk_key_{n} = {n}\n")).collect();
        let (_, ignored) = overlay(&junk);
        assert_eq!(ignored.len(), 40);
        assert!(MAX_REPORTED_REFUSALS < ignored.len());
    }

    #[test]
    fn overlay_cannot_change_grace_secs() {
        let (config, ignored) = overlay("[lock]\ngrace_secs = 86400\n");
        assert_eq!(config.lock.grace_secs, 0);
        assert_eq!(
            ignored,
            vec![IgnoredOverlayKey {
                key: "lock.grace_secs".into(),
                refusal: OverlayRefusal::SecurityFloor,
            }]
        );
    }

    #[test]
    fn overlay_cannot_disable_the_wallpaper_hold_cap() {
        let (config, ignored) = overlay("[lock.warning]\nwallpaper_hold_max_ms = 0\n");
        assert_eq!(config.lock.warning.wallpaper_hold_max_ms, 5_000);
        assert_eq!(
            ignored,
            vec![IgnoredOverlayKey {
                key: "lock.warning.wallpaper_hold_max_ms".into(),
                refusal: OverlayRefusal::SecurityFloor,
            }]
        );
    }

    #[test]
    fn overlay_cannot_postpone_the_commit_with_a_long_wallpaper_fade() {
        let (config, ignored) = overlay("[lock.warning]\nwallpaper_in_ms = 4294967295\n");
        assert_eq!(config.lock.warning.wallpaper_in_ms, 1_500);
        assert_eq!(
            ignored,
            vec![IgnoredOverlayKey {
                key: "lock.warning.wallpaper_in_ms".into(),
                refusal: OverlayRefusal::SecurityFloor,
            }]
        );
        let (config, ignored) = overlay("[lock.transition]\nwallpaper_in_ms = 400\n");
        assert_eq!(ignored, Vec::new());
        assert_eq!(config.lock.transition.wallpaper_in_ms, 400);
    }

    #[test]
    fn overlay_cannot_change_warning_duration_or_cancel_threshold() {
        let (config, ignored) = overlay(
            "
[lock.warning]
duration_ms = 0
cancel_on_motion_px = 100000.0
",
        );
        assert_eq!(config.lock.warning.duration_ms, 20_000);
        assert_eq!(config.lock.warning.cancel_on_motion_px, 8.0);
        assert_eq!(
            ignored,
            vec![
                IgnoredOverlayKey {
                    key: "lock.warning.cancel_on_motion_px".into(),
                    refusal: OverlayRefusal::SecurityFloor,
                },
                IgnoredOverlayKey {
                    key: "lock.warning.duration_ms".into(),
                    refusal: OverlayRefusal::SecurityFloor,
                },
            ]
        );
    }

    #[test]
    fn overlay_with_invalid_gui_selector_is_refused_not_fatal() {
        let mut config = parse(SYSTEM).unwrap();
        config.lock.warning.gui.element = vec![WarningElement {
            selector: "clock".into(),
            ..WarningElement::default()
        }];
        let system_gui = config.lock.warning.gui.clone();
        let table: toml::Table =
            toml::from_str("[[lock.warning.gui.element]]\nselector = \"pwn\"\n").unwrap();
        let ignored = config.overlay(&table);
        assert_eq!(
            config.lock.warning.gui, system_gui,
            "adopted the kill switch"
        );
        assert!(config.validate_warning().is_ok());
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].key, "lock.warning.gui");
        let OverlayRefusal::Invalid(reason) = &ignored[0].refusal else {
            panic!("expected Invalid, got {:?}", ignored[0].refusal);
        };
        assert!(reason.contains("pwn"), "{reason}");
    }

    #[test]
    fn overlay_greeter_keys_are_ignored_and_named() {
        let (config, ignored) = overlay(
            "
[keyboard]
layout = \"de\"

[sessions]
default = \"Hyprland\"

[users]
show_list = false

[power]
enabled = false

[greeter]
[render]
backend = \"gl\"

[look]
not_a_key = 1
",
        );
        assert_eq!(config.keyboard, Keyboard::default());
        assert_eq!(config.sessions, Sessions::default());
        assert!(config.users.show_list);
        assert!(config.power.enabled);
        assert_eq!(config.render, Render::default());
        let named: Vec<_> = ignored
            .iter()
            .map(|entry| (entry.key.as_str(), entry.refusal.clone()))
            .collect();
        assert_eq!(
            named,
            vec![
                ("greeter", OverlayRefusal::GreeterScope),
                ("keyboard", OverlayRefusal::GreeterScope),
                ("look.not_a_key", OverlayRefusal::NotOverlayable),
                ("power", OverlayRefusal::GreeterScope),
                ("render", OverlayRefusal::GreeterScope),
                ("sessions", OverlayRefusal::GreeterScope),
                ("users", OverlayRefusal::GreeterScope),
            ]
        );
        assert_eq!(
            ignored[1].to_string(),
            "user config: ignoring keyboard: greeter-scope — the greeter never reads the user file"
        );
    }

    #[test]
    fn overlay_names_a_whitelisted_key_of_the_wrong_type() {
        let (config, ignored) = overlay("[lock.transition]\nfrost_in_ms = \"soon\"\n");
        assert_eq!(config.lock.transition.frost_in_ms, 150);
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].key, "lock.transition.frost_in_ms");
        assert!(matches!(ignored[0].refusal, OverlayRefusal::Invalid(_)));
    }

    #[test]
    fn overlay_absent_leaves_system_config_untouched() {
        let system = TempFile::new("overlay-system", SYSTEM);
        let expected = parse(SYSTEM).unwrap();
        assert_eq!(
            load_layered_from(system.path(), None),
            (expected.clone(), Vec::new())
        );
        assert_eq!(
            load_layered_from(
                system.path(),
                Some(Path::new("/nonexistent/vigil-user.toml"))
            ),
            (expected, Vec::new())
        );
    }

    #[test]
    fn layered_merges_user_file_over_system() {
        let system = TempFile::new("layered-system", SYSTEM);
        let user = TempFile::new(
            "layered-user",
            "[look]\nclock_format = \"%S\"\n\n[lock]\ngrace_secs = 86400\n",
        );
        let (config, ignored) = load_layered_from(system.path(), Some(user.path()));
        assert_eq!(config.look.clock_format, "%S");
        assert_eq!(config.lock.grace_secs, 0);
        assert_eq!(config.lock.warning.duration_ms, 20_000);
        assert_eq!(
            ignored,
            vec![IgnoredOverlayKey {
                key: "lock.grace_secs".into(),
                refusal: OverlayRefusal::SecurityFloor,
            }]
        );
    }

    #[test]
    fn layered_ignores_an_unparsable_user_file() {
        let system = TempFile::new("broken-system", SYSTEM);
        let user = TempFile::new("broken-user", "[lock\ngrace_secs = ");
        assert_eq!(
            load_layered_from(system.path(), Some(user.path())),
            (parse(SYSTEM).unwrap(), Vec::new())
        );
    }

    #[test]
    fn a_non_regular_overlay_path_is_refused_not_opened() {
        let system = TempFile::new("nonregular-system", SYSTEM);
        let expected = parse(SYSTEM).unwrap();
        let dir =
            std::env::temp_dir().join(format!("vigil-overlay-dir-{}.toml", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_overlay(&dir), Err("not a regular file".into()));
        assert_eq!(
            load_layered_from(system.path(), Some(&dir)),
            (expected.clone(), Vec::new())
        );
        std::fs::remove_dir(&dir).unwrap();

        let link =
            std::env::temp_dir().join(format!("vigil-overlay-link-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/dev/null", &link).unwrap();
        assert_eq!(read_overlay(&link), Err("not a regular file".into()));
        assert_eq!(
            load_layered_from(system.path(), Some(&link)),
            (expected, Vec::new())
        );
        std::fs::remove_file(&link).unwrap();
    }

    #[test]
    fn an_oversize_overlay_is_refused() {
        let system = TempFile::new("oversize-system", SYSTEM);
        let user = TempFile::new(
            "oversize-user",
            &format!(
                "# {}\n[look]\nclock_format = \"%S\"\n",
                "x".repeat(MAX_OVERLAY_BYTES as usize)
            ),
        );
        assert!(read_overlay(user.path()).is_err());
        assert_eq!(
            load_layered_from(system.path(), Some(user.path())),
            (parse(SYSTEM).unwrap(), Vec::new())
        );
    }

    #[test]
    fn explicit_config_path_bypasses_the_overlay() {
        let path = TempFile::new(
            "explicit",
            "[look]\nclock_format = \"%S\"\n\n[lock]\ngrace_secs = 300\n",
        );
        let (config, ignored) = Config::load_layered_reporting(Some(path.path()));
        assert_eq!(config.look.clock_format, "%S");
        assert_eq!(config.lock.grace_secs, 300);
        assert_eq!(ignored, Vec::new());
    }

    #[test]
    fn state_round_trip_with_escapes() {
        let tmp =
            std::env::temp_dir().join(format!("vigil-state-test-{}.toml", std::process::id()));
        let state = State {
            user: "al\"ice".into(),
            session: "Test DE".into(),
            ..State::default()
        };
        state.store(&tmp);
        assert_eq!(State::load(&tmp), Some(state));
        std::fs::remove_file(tmp).unwrap();
    }

    #[test]
    fn state_round_trip_keeps_autologin_fields() {
        let tmp =
            std::env::temp_dir().join(format!("vigil-state-autologin-{}.toml", std::process::id()));
        let state = State {
            user: "alice".into(),
            session: "Test DE".into(),
            auto_started_at: Some(1_700_000_000),
            auto_streak: 2,
            unprompted_user: "al\"ice".into(),
            first_question: Vec::new(),
        };
        state.store(&tmp);
        assert_eq!(State::load(&tmp), Some(state));
        std::fs::remove_file(tmp).unwrap();
    }

    #[test]
    fn state_round_trip_keeps_first_questions_character_for_character() {
        let tmp =
            std::env::temp_dir().join(format!("vigil-state-questions-{}.toml", std::process::id()));
        let question = |user: &str, text: &str, secret| FirstQuestion {
            user: user.into(),
            text: text.into(),
            secret,
        };
        let state = State {
            user: "alice".into(),
            session: "Test DE".into(),
            auto_started_at: Some(1_700_000_000),
            auto_streak: 1,
            unprompted_user: "carol".into(),
            first_question: vec![
                question("alice", "Security token PIN: ", true),
                question(
                    "b\"ob",
                    "Code for\n\tC:\\ \"b\u{1b}[31mob\u{7f}\u{85}\":\r\n",
                    false,
                ),
                question("dave", "Mot de passe\u{a0}: 密码", true),
            ],
        };
        state.store(&tmp);
        assert_eq!(State::load(&tmp), Some(state));
        std::fs::remove_file(tmp).unwrap();
    }

    #[test]
    fn state_without_autologin_fields_loads_with_defaults() {
        let tmp =
            std::env::temp_dir().join(format!("vigil-state-legacy-{}.toml", std::process::id()));
        std::fs::write(&tmp, "user = \"alice\"\nsession = \"Test DE\"\n").unwrap();
        let state = State::load(&tmp).unwrap();
        assert_eq!(state.user, "alice");
        assert_eq!(state.auto_started_at, None);
        assert_eq!(state.auto_streak, 0);
        assert_eq!(state.unprompted_user, "");
        assert_eq!(state.first_question, Vec::new());
        std::fs::remove_file(tmp).unwrap();
    }

    #[test]
    fn a_bad_first_question_entry_is_skipped_and_the_rest_of_the_state_kept() {
        let tmp =
            std::env::temp_dir().join(format!("vigil-state-badq-{}.toml", std::process::id()));
        std::fs::write(
            &tmp,
            "user = \"alice\"\nunprompted_user = \"alice\"\n\n\
             [[first_question]]\nuser = \"bob\"\ntext = \"PIN: \"\n\n\
             [[first_question]]\nuser = \"carol\"\ntext = \"PIN: \"\nsecret = true\n",
        )
        .unwrap();
        let state = State::load(&tmp).unwrap();
        assert_eq!(state.user, "alice");
        assert_eq!(state.unprompted_user, "alice");
        assert_eq!(
            state.first_question,
            vec![FirstQuestion {
                user: "carol".into(),
                text: "PIN: ".into(),
                secret: true,
            }]
        );
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
