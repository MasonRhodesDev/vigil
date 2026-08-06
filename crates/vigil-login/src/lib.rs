//! logind glue for vigil-lock (DESIGN.md §12): SetLockedHint, and Lock/Unlock/PrepareForSleep signals on a worker thread (the vigil-pam pattern — the event loop is calloop-driven and must never block on D-Bus). Transport only, no policy. D-Bus being unavailable must never affect locking: connect() returns None and every call becomes a no-op.

use std::sync::mpsc::Sender;
use vigil_core::LoginEvent;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::OwnedObjectPath;

const DEST: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
/// logind resolves this alias to the caller's session — and, for a process
/// in the user manager's scope (hypridle spawns us there, so GetSessionByPID
/// fails), to the user's display session. Verified on the reference machine.
const SESSION_AUTO: &str = "/org/freedesktop/login1/session/auto";

pub struct LoginSession {
    conn: Connection,
    path: OwnedObjectPath,
}

impl LoginSession {
    /// Connect and resolve our session's real object path. `None` (logged
    /// once) if anything fails — the locker then runs exactly as before.
    pub fn connect() -> Option<Self> {
        match Self::try_connect() {
            Ok(session) => Some(session),
            Err(e) => {
                eprintln!("vigil-lock: logind unavailable ({e}); no lock hint or signals");
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
