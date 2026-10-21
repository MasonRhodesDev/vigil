//! logind glue for vigil-lock (DESIGN.md §12): SetLockedHint, and Lock/Unlock/PrepareForSleep signals on a worker thread (the vigil-pam pattern — the event loop is calloop-driven and must never block on D-Bus). Transport only, no policy. D-Bus being unavailable must never affect locking: connect() returns None and every call becomes a no-op.

use std::future::poll_fn;
use std::pin::pin;
use std::sync::mpsc::Sender;
use vigil_core::{AppearanceEvent, ColorScheme, LoginEvent, accent_from_portal};
use zbus::export::futures_core::Stream;
use zbus::{Connection, MessageStream, OwnedObjectPath, OwnedVariant, Proxy, block_on};

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
        block_on(Self::try_connect_async())
    }

    async fn try_connect_async() -> Result<Self, zbus::Error> {
        let conn = Connection::system().await?;
        // The alias answers property reads but signals arrive on the real
        // path, so resolve it: Id from the alias, then Manager.GetSession.
        let auto = Proxy::new(&conn, DEST, SESSION_AUTO, SESSION_IFACE).await?;
        let id: String = auto.get_property("Id").await?;
        let manager = Proxy::new(&conn, DEST, MANAGER_PATH, MANAGER_IFACE).await?;
        let path: OwnedObjectPath = manager.call("GetSession", &(id,)).await?;
        Ok(Self { conn, path })
    }

    /// Best-effort `SetLockedHint`; failures log and are otherwise ignored.
    pub fn set_locked_hint(&self, locked: bool) {
        let result = block_on(async {
            Proxy::new(&self.conn, DEST, self.path.as_str(), SESSION_IFACE)
                .await?
                .call::<_, _, ()>("SetLockedHint", &(locked,))
                .await
        });
        if let Err(e) = result {
            eprintln!("vigil-lock: SetLockedHint({locked}): {e}");
        }
    }

    /// Spawn the signal thread. It owns its own system-bus connection (zbus
    /// 6 drives a connection from the thread that blocks on it, so the
    /// worker must not share the caller's) and sends [`LoginEvent`]s until
    /// the receiver is dropped.
    pub fn spawn_signals(&self, tx: Sender<LoginEvent>) {
        let path = self.path.clone();
        std::thread::spawn(move || {
            block_on(async move {
                // One rule for every login1 signal; dispatch below. Session
                // signals are filtered to OUR path, because another
                // session's Unlock must never unlock this screen.
                let rule = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .sender(DEST)
                    .interface(SESSION_IFACE)
                    .build();
                let session_rule = match rule {
                    Ok(rule) => rule,
                    Err(e) => {
                        eprintln!("vigil-lock: logind match rule: {e}");
                        return;
                    }
                };
                let stream = match signal_stream(Connection::system(), session_rule, 8).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        eprintln!("vigil-lock: logind signals: {e}");
                        return;
                    }
                };
                let mut stream = pin!(stream);
                while let Some(message) = next_message(stream.as_mut()).await {
                    let Ok(message) = message else { continue };
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
        });
    }

    /// Spawn a second thread for `Manager.PrepareForSleep` (different
    /// interface and path, so a separate match rule, stream and connection).
    pub fn spawn_sleep_signals(&self, tx: Sender<LoginEvent>) {
        std::thread::spawn(move || {
            block_on(async move {
                let rule = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .sender(DEST)
                    .interface(MANAGER_IFACE)
                    .member("PrepareForSleep")
                    .build();
                let Ok(rule) = rule else { return };
                let Ok(stream) = signal_stream(Connection::system(), rule, 4).await else {
                    return;
                };
                let mut stream = pin!(stream);
                while let Some(message) = next_message(stream.as_mut()).await {
                    let Ok(message) = message else { continue };
                    let Ok(sleeping) = message.body().deserialize::<bool>() else {
                        continue;
                    };
                    if tx.send(LoginEvent::PrepareForSleep(sleeping)).is_err() {
                        return;
                    }
                }
            });
        });
    }
}

/// Subscribe to `rule` on a freshly built connection, queueing at most `max`
/// messages. The stream holds its connection alive.
async fn signal_stream(
    connect: impl std::future::Future<Output = zbus::Result<Connection>>,
    rule: zbus::MatchRule<'static>,
    max: usize,
) -> zbus::Result<MessageStream> {
    let conn = connect.await?;
    MessageStream::for_match_rule(rule, &conn, std::num::NonZeroUsize::new(max)).await
}

/// `StreamExt::next` without a futures-util dependency.
async fn next_message<S: Stream + Unpin>(mut stream: std::pin::Pin<&mut S>) -> Option<S::Item> {
    poll_fn(|cx| stream.as_mut().poll_next(cx)).await
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
fn portal_inner(value: OwnedVariant) -> OwnedVariant {
    match value.downcast_ref::<zbus::Variant>() {
        Ok(inner) => OwnedVariant::try_from(inner).unwrap_or(value),
        Err(_) => value,
    }
}

impl AppearanceWatcher {
    pub fn connect() -> Option<Self> {
        match block_on(Connection::session()) {
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
        let Ok(proxy) = block_on(Proxy::new(
            &self.conn,
            PORTAL_DEST,
            PORTAL_PATH,
            PORTAL_IFACE,
        )) else {
            return;
        };
        if let Ok(value) =
            block_on(proxy.call::<_, _, OwnedVariant>("ReadOne", &(APPEARANCE_NS, "color-scheme")))
            && let Ok(raw) = u32::try_from(portal_inner(value))
        {
            let _ = tx.send(AppearanceEvent::Scheme(ColorScheme::from_portal(raw)));
        }
        if let Ok(value) =
            block_on(proxy.call::<_, _, OwnedVariant>("ReadOne", &(APPEARANCE_NS, "accent-color")))
            && let Ok(rgb) = <(f64, f64, f64)>::try_from(portal_inner(value))
        {
            let _ = tx.send(AppearanceEvent::Accent(accent_from_portal(rgb)));
        }
    }

    /// Watch `SettingChanged` so `lmtt switch` retints a running lock. The
    /// worker opens its own session-bus connection (see
    /// [`LoginSession::spawn_signals`]).
    pub fn spawn_signals(&self, tx: Sender<AppearanceEvent>) {
        std::thread::spawn(move || {
            block_on(async move {
                let rule = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .sender(PORTAL_DEST)
                    .interface(PORTAL_IFACE)
                    .member("SettingChanged")
                    .build();
                let Ok(rule) = rule else { return };
                let Ok(stream) = signal_stream(Connection::session(), rule, 8).await else {
                    return;
                };
                let mut stream = pin!(stream);
                while let Some(message) = next_message(stream.as_mut()).await {
                    let Ok(message) = message else { continue };
                    let Ok((namespace, key, value)) = message
                        .body()
                        .deserialize::<(String, String, OwnedVariant)>()
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
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Read-only against the real buses; skips where there is no bus.
    #[test]
    fn logind_session_resolves_read_only() {
        let Some(session) = LoginSession::connect() else {
            eprintln!("skipped: no system bus or no logind session");
            return;
        };
        assert!(
            session
                .path
                .as_str()
                .starts_with("/org/freedesktop/login1/session/")
        );
        // Subscribing only adds a match rule; nothing here changes state.
        let (tx, _rx) = mpsc::channel();
        session.spawn_signals(tx.clone());
        session.spawn_sleep_signals(tx);
        // The caller's connection must stay usable beside the workers.
        let id: String = block_on(async {
            Proxy::new(&session.conn, DEST, session.path.as_str(), SESSION_IFACE)
                .await?
                .get_property("Id")
                .await
        })
        .unwrap();
        assert!(!id.is_empty());
    }

    #[test]
    fn portal_read_initial_is_harmless() {
        let Some(watcher) = AppearanceWatcher::connect() else {
            eprintln!("skipped: no session bus");
            return;
        };
        let (tx, rx) = mpsc::channel();
        watcher.read_initial(&tx);
        drop(tx);
        // Whatever the portal answered (possibly nothing) must parse.
        while rx.try_recv().is_ok() {}
        let (tx, _rx) = mpsc::channel();
        watcher.spawn_signals(tx);
    }

    /// The worker pattern (own connection, blocking on a stream from a plain
    /// thread) must deliver a signal sent by a different connection.
    #[test]
    fn worker_thread_receives_signal_from_other_connection() {
        let Ok(sender) = block_on(Connection::session()) else {
            eprintln!("skipped: no session bus");
            return;
        };
        let (tx, rx) = mpsc::channel::<String>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            block_on(async move {
                let rule = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .interface("org.vigil.LoginTest")
                    .member("Ping")
                    .build()
                    .unwrap();
                let stream = signal_stream(Connection::session(), rule, 4).await.unwrap();
                ready_tx.send(()).unwrap();
                let mut stream = pin!(stream);
                if let Some(Ok(m)) = next_message(stream.as_mut()).await {
                    let body: String = m.body().deserialize().unwrap();
                    tx.send(body).unwrap();
                }
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        block_on(sender.emit_signal(
            None::<&str>,
            "/org/vigil/LoginTest",
            "org.vigil.LoginTest",
            "Ping",
            &("pong".to_string(),),
        ))
        .unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "pong");
    }
}
