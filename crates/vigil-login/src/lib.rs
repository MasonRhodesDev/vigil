//! logind glue for vigil-lock (DESIGN.md §12): SetLockedHint, and Lock/Unlock/PrepareForSleep signals on a worker thread (the vigil-pam pattern — the event loop is calloop-driven and must never block on D-Bus). Transport only, no policy. D-Bus being unavailable must never affect locking: connect() returns None and every call becomes a no-op.

use std::sync::mpsc::Sender;
use vigil_core::{AppearanceEvent, ColorScheme, LoginEvent, accent_from_portal};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const DEST: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
/// logind resolves this alias to the caller's session — and, for a process
/// in the user manager's scope (hypridle spawns us there, so GetSessionByPID
/// fails), to the user's display session. Verified on the reference machine.
const SESSION_AUTO: &str = "/org/freedesktop/login1/session/auto";
const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NS: &str = "org.freedesktop.appearance";

pub struct LoginSession {
    conn: Connection,
    path: OwnedObjectPath,
}

impl LoginSession {
    /// Connect and resolve our session's real object path. `None` (logged
    /// once) if anything fails — the locker then runs exactly as before.
    pub fn connect() -> Option<Self> {
        Self::connect_for("vigil-lock")
    }

    /// As [`connect`](Self::connect), naming the caller in the failure log —
    /// the greeter wants the sleep signals too, and "vigil-lock" in the
    /// greeter's journal is a lie that costs someone an hour.
    pub fn connect_for(component: &str) -> Option<Self> {
        match Self::try_connect() {
            Ok(session) => Some(session),
            Err(e) => {
                eprintln!("{component}: logind unavailable ({e}); no logind hints or signals");
                None
            }
        }
    }

    fn try_connect() -> Result<Self, zbus::Error> {
        let conn = Connection::system()?;
        // The alias answers property reads but signals arrive on the real
        // path, so resolve it: Id from the alias, then Manager.GetSession.
        let auto = Proxy::new(&conn, DEST, SESSION_AUTO, SESSION_IFACE)?;
        let id: String = auto.get_property("Id")?;
        let manager = Proxy::new(&conn, DEST, MANAGER_PATH, MANAGER_IFACE)?;
        let path: OwnedObjectPath = manager.call("GetSession", &(id,))?;
        Ok(Self { conn, path })
    }

    /// Best-effort `SetLockedHint`; failures log and are otherwise ignored.
    pub fn set_locked_hint(&self, locked: bool) {
        let result = Proxy::new(&self.conn, DEST, self.path.as_str(), SESSION_IFACE)
            .and_then(|p| p.call::<_, _, ()>("SetLockedHint", &(locked,)));
        if let Err(e) = result {
            eprintln!("vigil-lock: SetLockedHint({locked}): {e}");
        }
    }

    /// Spawn the signal thread. It owns a clone of the connection and sends
    /// [`LoginEvent`]s until the receiver is dropped.
    pub fn spawn_signals(&self, tx: Sender<LoginEvent>) {
        let conn = self.conn.clone();
        let path = self.path.clone();
        std::thread::spawn(move || {
            // One rule for every login1 signal; dispatch below. Session
            // signals are filtered to OUR path — another session's Unlock
            // must never unlock this screen.
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(DEST)
                .and_then(|b| b.interface(SESSION_IFACE))
                .map(|b| b.build());
            let session_rule = match rule {
                Ok(rule) => rule,
                Err(e) => {
                    eprintln!("vigil-lock: logind match rule: {e}");
                    return;
                }
            };
            let iter = match MessageIterator::for_match_rule(session_rule, &conn, Some(8)) {
                Ok(iter) => iter,
                Err(e) => {
                    eprintln!("vigil-lock: logind signals: {e}");
                    return;
                }
            };
            for message in iter.flatten() {
                let header = message.header();
                if header.path().map(|p| p.as_str()) != Some(path.as_str()) {
                    continue;
                }
                let event = match header.member().map(|m| m.as_str()) {
                    Some("Lock") => LoginEvent::Lock,
                    Some("Unlock") => LoginEvent::Unlock,
                    _ => continue,
                };
                if tx.send(event).is_err() {
                    return;
                }
            }
        });
    }

    /// Spawn a second thread for `Manager.PrepareForSleep` (different
    /// interface and path, so a separate match rule and iterator).
    pub fn spawn_sleep_signals(&self, tx: Sender<LoginEvent>) {
        let conn = self.conn.clone();
        std::thread::spawn(move || {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(DEST)
                .and_then(|b| b.interface(MANAGER_IFACE))
                .and_then(|b| b.member("PrepareForSleep"))
                .map(|b| b.build());
            let Ok(rule) = rule else { return };
            let Ok(iter) = MessageIterator::for_match_rule(rule, &conn, Some(4)) else {
                return;
            };
            for message in iter.flatten() {
                let Ok(sleeping) = message.body().deserialize::<bool>() else {
                    continue;
                };
                if tx.send(LoginEvent::PrepareForSleep(sleeping)).is_err() {
                    return;
                }
            }
        });
    }
}

/// Settings-portal reader for `org.freedesktop.appearance` (issues #15/#16).
/// Session bus, unlike [`LoginSession`]. The portal being absent is normal —
/// there is none at the greeter, and a bare session may not run one — so
/// every failure logs once and leaves the theme on its own defaults.
pub struct AppearanceWatcher {
    conn: Connection,
}

/// The portal returns `v`; some backends nest one more variant. Unwrap at
/// most one level so both shapes deserialize.
fn portal_inner(value: OwnedValue) -> OwnedValue {
    match value.downcast_ref::<zbus::zvariant::Value>() {
        Ok(inner) => OwnedValue::try_from(inner).unwrap_or(value),
        Err(_) => value,
    }
}

impl AppearanceWatcher {
    pub fn connect() -> Option<Self> {
        match Connection::session() {
            Ok(conn) => Some(Self { conn }),
            Err(e) => {
                eprintln!("vigil-lock: settings portal unavailable ({e}); theme defaults kept");
                None
            }
        }
    }

    /// Read both keys once and send them. Missing keys are silent: the
    /// portal answers with an error for keys its backend does not provide.
    pub fn read_initial(&self, tx: &Sender<AppearanceEvent>) {
        let Ok(proxy) = Proxy::new(&self.conn, PORTAL_DEST, PORTAL_PATH, PORTAL_IFACE) else {
            return;
        };
        if let Ok(value) =
            proxy.call::<_, _, OwnedValue>("ReadOne", &(APPEARANCE_NS, "color-scheme"))
            && let Ok(raw) = u32::try_from(portal_inner(value))
        {
            let _ = tx.send(AppearanceEvent::Scheme(ColorScheme::from_portal(raw)));
        }
        if let Ok(value) =
            proxy.call::<_, _, OwnedValue>("ReadOne", &(APPEARANCE_NS, "accent-color"))
            && let Ok(rgb) = <(f64, f64, f64)>::try_from(portal_inner(value))
        {
            let _ = tx.send(AppearanceEvent::Accent(accent_from_portal(rgb)));
        }
    }

    /// Watch `SettingChanged` so `lmtt switch` retints a running lock.
    pub fn spawn_signals(&self, tx: Sender<AppearanceEvent>) {
        let conn = self.conn.clone();
        std::thread::spawn(move || {
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(PORTAL_DEST)
                .and_then(|b| b.interface(PORTAL_IFACE))
                .and_then(|b| b.member("SettingChanged"))
                .map(|b| b.build());
            let Ok(rule) = rule else { return };
            let Ok(iter) = MessageIterator::for_match_rule(rule, &conn, Some(8)) else {
                return;
            };
            for message in iter.flatten() {
                let Ok((namespace, key, value)) =
                    message.body().deserialize::<(String, String, OwnedValue)>()
                else {
                    continue;
                };
                if namespace != APPEARANCE_NS {
                    continue;
                }
                let event = match key.as_str() {
                    "color-scheme" => u32::try_from(portal_inner(value))
                        .ok()
                        .map(|raw| AppearanceEvent::Scheme(ColorScheme::from_portal(raw))),
                    "accent-color" => <(f64, f64, f64)>::try_from(portal_inner(value))
                        .ok()
                        .map(|rgb| AppearanceEvent::Accent(accent_from_portal(rgb))),
                    _ => None,
                };
                if let Some(event) = event
                    && tx.send(event).is_err()
                {
                    return;
                }
            }
        });
    }
}
