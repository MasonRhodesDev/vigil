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
    pub fn connect() -> Option<Self> {
        Self::connect_for("vigil-lock")
    }

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
        let auto = Proxy::new(&conn, DEST, SESSION_AUTO, SESSION_IFACE).await?;
        let id: String = auto.get_property("Id").await?;
        let manager = Proxy::new(&conn, DEST, MANAGER_PATH, MANAGER_IFACE).await?;
        let path: OwnedObjectPath = manager.call("GetSession", &(id,)).await?;
        Ok(Self { conn, path })
    }

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

    pub fn spawn_signals(&self, tx: Sender<LoginEvent>) {
        let path = self.path.clone();
        std::thread::spawn(move || {
            block_on(async move {
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

async fn signal_stream(
    connect: impl std::future::Future<Output = zbus::Result<Connection>>,
    rule: zbus::MatchRule<'static>,
    max: usize,
) -> zbus::Result<MessageStream> {
    let conn = connect.await?;
    MessageStream::for_match_rule(rule, &conn, std::num::NonZeroUsize::new(max)).await
}

async fn next_message<S: Stream + Unpin>(mut stream: std::pin::Pin<&mut S>) -> Option<S::Item> {
    poll_fn(|cx| stream.as_mut().poll_next(cx)).await
}

pub struct AppearanceWatcher {
    conn: Connection,
}

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
        let (tx, _rx) = mpsc::channel();
        session.spawn_signals(tx.clone());
        session.spawn_sleep_signals(tx);
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
        while rx.try_recv().is_ok() {}
        let (tx, _rx) = mpsc::channel();
        watcher.spawn_signals(tx);
    }

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
