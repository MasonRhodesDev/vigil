#![forbid(unsafe_code)]

use std::future::poll_fn;
use std::pin::pin;
use std::sync::mpsc::Sender;
use vigil_core::{AppearanceEvent, ColorScheme, LoginEvent, accent_from_portal};
use zbus::export::futures_core::Stream;
use zbus::proxy::SignalStream;
use zbus::{Connection, Message, OwnedObjectPath, OwnedVariant, Proxy, block_on};

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
                vigil_log::say!(
                    "{component}: logind unavailable ({e}); no logind hints or signals"
                );
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
            vigil_log::say!("vigil-lock: SetLockedHint({locked}): {e}");
        }
    }

    pub fn spawn_signals(&self, tx: Sender<LoginEvent>) {
        Source {
            bus: Bus::System,
            name: DEST,
            path: self.path.to_string(),
            interface: SESSION_IFACE,
            member: None,
        }
        .forward(tx, |message| match message.header().member()?.as_str() {
            "Lock" => Some(LoginEvent::Lock),
            "Unlock" => Some(LoginEvent::Unlock),
            _ => None,
        });
    }

    pub fn spawn_sleep_signals(&self, tx: Sender<LoginEvent>) {
        Source {
            bus: Bus::System,
            name: DEST,
            path: MANAGER_PATH.to_string(),
            interface: MANAGER_IFACE,
            member: Some("PrepareForSleep"),
        }
        .forward(tx, |message| {
            message
                .body()
                .deserialize::<bool>()
                .ok()
                .map(LoginEvent::PrepareForSleep)
        });
    }
}

#[derive(Clone, Copy)]
enum Bus {
    System,
    Session,
}

impl Bus {
    async fn connect(self) -> zbus::Result<Connection> {
        match self {
            Self::System => Connection::system().await,
            Self::Session => Connection::session().await,
        }
    }
}

struct Source {
    bus: Bus,
    name: &'static str,
    path: String,
    interface: &'static str,
    member: Option<&'static str>,
}

impl Source {
    // A Proxy stream yields only messages from the current unique owner of `name`. A MatchRule
    // naming a well-known sender matches every sender, so a forged unicast signal would pass.
    async fn signals(&self) -> zbus::Result<SignalStream<'static>> {
        let conn = self.bus.connect().await?;
        let proxy = Proxy::new(&conn, self.name, self.path.as_str(), self.interface).await?;
        match self.member {
            Some(member) => proxy.receive_signal(member).await,
            None => proxy.receive_all_signals().await,
        }
    }

    fn forward<T: Send + 'static>(
        self,
        tx: Sender<T>,
        decode: impl Fn(&Message) -> Option<T> + Send + 'static,
    ) {
        std::thread::spawn(move || {
            block_on(async move {
                let signals = match self.signals().await {
                    Ok(signals) => signals,
                    Err(e) => {
                        vigil_log::say!(
                            "vigil: {} signals from {}: {e}",
                            self.interface,
                            self.name
                        );
                        return;
                    }
                };
                let mut signals = pin!(signals);
                while let Some(message) = next_message(signals.as_mut()).await {
                    if let Some(event) = decode(&message)
                        && tx.send(event).is_err()
                    {
                        return;
                    }
                }
            });
        });
    }
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
                vigil_log::say!(
                    "vigil-lock: settings portal unavailable ({e}); theme defaults kept"
                );
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
        Source {
            bus: Bus::Session,
            name: PORTAL_DEST,
            path: PORTAL_PATH.to_string(),
            interface: PORTAL_IFACE,
            member: Some("SettingChanged"),
        }
        .forward(tx, |message| {
            let (namespace, key, value) = message
                .body()
                .deserialize::<(String, String, OwnedVariant)>()
                .ok()?;
            if namespace != APPEARANCE_NS {
                return None;
            }
            match key.as_str() {
                "color-scheme" => u32::try_from(portal_inner(value))
                    .ok()
                    .map(|raw| AppearanceEvent::Scheme(ColorScheme::from_portal(raw))),
                "accent-color" => <(f64, f64, f64)>::try_from(portal_inner(value))
                    .ok()
                    .map(|rgb| AppearanceEvent::Accent(accent_from_portal(rgb))),
                _ => None,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn logind_session_resolves_read_only() {
        let Some(session) = LoginSession::connect() else {
            vigil_log::say!("skipped: no system bus or no logind session");
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
            vigil_log::say!("skipped: no session bus");
            return;
        };
        let (tx, rx) = mpsc::channel();
        watcher.read_initial(&tx);
        drop(tx);
        while rx.try_recv().is_ok() {}
        let (tx, _rx) = mpsc::channel();
        watcher.spawn_signals(tx);
    }
}
