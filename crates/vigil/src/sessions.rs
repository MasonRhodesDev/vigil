//! Session (desktop environment) enumeration: the `.desktop` entries under
//! /usr/share/wayland-sessions and /usr/share/xsessions, the same lists every
//! spec-following greeter offers. Pure parsing, unit-tested; the event loop
//! owns which entry is selected.

use std::path::Path;

/// One launchable session as greetd will receive it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    /// Display name (`Name=`), suffixed with ` (X11)` for xsessions.
    pub name: String,
    /// Command line for greetd `start_session`.
    pub cmd: Vec<String>,
    /// `KEY=value` environment for greetd `start_session`.
    pub env: Vec<String>,
}

/// Enumerate installed sessions: Wayland first, then X11, each sorted by
/// name. X11 entries are listed for spec completeness but untested
/// (DESIGN.md non-goals). Never empty: a login-shell entry is the fallback.
///
/// `VIGIL_SESSION_DIRS` (colon-separated) overrides the standard directories
/// (tests, unusual distros); a directory whose basename is `xsessions`
/// counts as X11.
pub fn enumerate() -> Vec<SessionEntry> {
    let dirs: Vec<(std::path::PathBuf, SessionKind)> = match std::env::var_os("VIGIL_SESSION_DIRS")
    {
        Some(paths) => std::env::split_paths(&paths)
            .map(|dir| {
                let kind = if dir.file_name().is_some_and(|n| n == "xsessions") {
                    SessionKind::X11
                } else {
                    SessionKind::Wayland
                };
                (dir, kind)
            })
            .collect(),
        None => vec![
            ("/usr/share/wayland-sessions".into(), SessionKind::Wayland),
            ("/usr/share/xsessions".into(), SessionKind::X11),
        ],
    };
    let mut sessions = Vec::new();
    for (dir, kind) in dirs {
        let mut batch = read_dir_entries(&dir, kind);
        batch.sort_by(|a, b| a.name.cmp(&b.name));
        sessions.extend(batch);
    }
    if sessions.is_empty() {
        sessions.push(SessionEntry {
            name: "Shell".into(),
            cmd: vec!["/bin/sh".into(), "-l".into()],
            env: Vec::new(),
        });
    }
    sessions
}

#[derive(Clone, Copy, PartialEq)]
enum SessionKind {
    Wayland,
    X11,
}

fn read_dir_entries(dir: &Path, kind: SessionKind) -> Vec<SessionEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "desktop") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(session) = parse_desktop(&content, &stem, kind) {
            sessions.push(session);
        }
    }
    sessions
}

/// Parse the `[Desktop Entry]` group of a session file. Only the keys a
/// greeter needs; unknown keys are ignored per the desktop-entry spec.
fn parse_desktop(content: &str, stem: &str, kind: SessionKind) -> Option<SessionEntry> {
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut try_exec = None;
    let mut desktop_names = None;
    for line in content.lines() {
        let line = line.trim();
        if let Some(group) = line.strip_prefix('[') {
            in_entry = group.trim_end_matches(']') == "Desktop Entry";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" => name = Some(value.to_owned()),
            "Exec" => exec = Some(value.to_owned()),
            "TryExec" => try_exec = Some(value.to_owned()),
            "DesktopNames" => desktop_names = Some(value.to_owned()),
            "Hidden" | "NoDisplay" if value.eq_ignore_ascii_case("true") => return None,
            _ => {}
        }
    }

    let cmd = split_exec(&exec?)?;
    if let Some(try_exec) = try_exec
        && !executable_exists(&try_exec)
    {
        return None;
    }

    let mut name = name.unwrap_or_else(|| stem.to_owned());
    let session_type = match kind {
        SessionKind::Wayland => "wayland",
        SessionKind::X11 => {
            name.push_str(" (X11)");
            "x11"
        }
    };
    let mut env = vec![
        format!("XDG_SESSION_TYPE={session_type}"),
        format!("XDG_SESSION_DESKTOP={stem}"),
    ];
    if let Some(names) = desktop_names {
        // DesktopNames is `;`-separated; XDG_CURRENT_DESKTOP is `:`-separated.
        let joined = names
            .split(';')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(":");
        if !joined.is_empty() {
            env.push(format!("XDG_CURRENT_DESKTOP={joined}"));
        }
    }
    Some(SessionEntry { name, cmd, env })
}

fn executable_exists(program: &str) -> bool {
    let path = Path::new(program);
    if path.is_absolute() {
        return path.exists();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).exists()))
}

/// Split an `Exec=` line into argv per the desktop-entry quoting rules that
/// matter for session files: whitespace splits, double quotes group, `\`
/// escapes inside quotes. Field codes (`%f` etc.) never appear in session
/// entries and are dropped defensively.
fn split_exec(exec: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = exec.chars().peekable();
    let mut quoted = false;
    let mut has_current = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                has_current = true;
            }
            '\\' if quoted => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if has_current {
                    args.push(std::mem::take(&mut current));
                    has_current = false;
                }
            }
            c => {
                current.push(c);
                has_current = true;
            }
        }
    }
    if has_current {
        args.push(current);
    }
    args.retain(|a| !(a.len() == 2 && a.starts_with('%')));
    if args.is_empty() { None } else { Some(args) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Option<SessionEntry> {
        parse_desktop(content, "test-session", SessionKind::Wayland)
    }

    #[test]
    fn parses_a_typical_wayland_session() {
        let session = parse(
            "[Desktop Entry]\nName=Hyprland\nComment=A dynamic compositor\n\
             Exec=Hyprland\nType=Application\nDesktopNames=Hyprland;wlroots\n",
        )
        .unwrap();
        assert_eq!(session.name, "Hyprland");
        assert_eq!(session.cmd, ["Hyprland"]);
        assert_eq!(
            session.env,
            [
                "XDG_SESSION_TYPE=wayland",
                "XDG_SESSION_DESKTOP=test-session",
                "XDG_CURRENT_DESKTOP=Hyprland:wlroots",
            ]
        );
    }

    #[test]
    fn x11_sessions_are_suffixed_and_typed() {
        let session = parse_desktop(
            "[Desktop Entry]\nName=i3\nExec=i3\n",
            "i3",
            SessionKind::X11,
        )
        .unwrap();
        assert_eq!(session.name, "i3 (X11)");
        assert!(session.env.contains(&"XDG_SESSION_TYPE=x11".to_owned()));
    }

    #[test]
    fn hidden_and_execless_entries_are_skipped() {
        assert!(parse("[Desktop Entry]\nName=Gone\nExec=x\nHidden=true\n").is_none());
        assert!(parse("[Desktop Entry]\nName=NoExec\n").is_none());
        assert!(parse("[Desktop Entry]\nName=NoShow\nExec=x\nNoDisplay=true\n").is_none());
    }

    #[test]
    fn keys_outside_desktop_entry_group_are_ignored() {
        let session = parse(
            "[Desktop Entry]\nName=Real\nExec=real-session\n\
             [Other Group]\nExec=evil --nope\n",
        )
        .unwrap();
        assert_eq!(session.cmd, ["real-session"]);
    }

    #[test]
    fn exec_lines_split_with_quotes_and_drop_field_codes() {
        assert_eq!(
            split_exec(r#"env FOO="a b" start-session %f"#).unwrap(),
            ["env", r#"FOO=a b"#, "start-session"]
        );
        assert_eq!(
            split_exec(r#""/opt/My DE/bin/session" --flag"#).unwrap(),
            ["/opt/My DE/bin/session", "--flag"]
        );
        assert!(split_exec("").is_none());
    }

    #[test]
    fn missing_try_exec_binary_hides_the_entry() {
        assert!(
            parse("[Desktop Entry]\nName=Ghost\nExec=ghost\nTryExec=/nonexistent/bin\n").is_none()
        );
        assert!(parse("[Desktop Entry]\nName=Real\nExec=sh\nTryExec=/bin/sh\n").is_some());
    }
}
