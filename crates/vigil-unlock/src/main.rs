#![forbid(unsafe_code)]

use std::path::Path;
use std::time::Duration;

use vigil_log::{out, say};
use vigil_unlock_token::{DIR, Moment, Owners};
use zbus::{Connection, OwnedObjectPath, Proxy, block_on};

const LOGIN1: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
const NOT_SUPPORTED: &str = "org.freedesktop.DBus.Error.NotSupported";
const USAGE: &str = "usage: vigil-unlock <session-id|user>";
const HELP: &str = "Unlocks a session that vigil-lock holds. Run it as root.\n\
A session id names one session. A user name names every session of that user that can lock.\n\
vigil-lock ignores a logind Unlock that root did not ask for, so plain\n\
`loginctl unlock-session` no longer unlocks it.";
// The lock reads the token on its own event loop after logind's signal reaches it, so the
// token stays this long, well inside vigil_unlock_token::WINDOW.
const SETTLE: Duration = Duration::from_secs(2);

type Listed = (String, u32, String, String, OwnedObjectPath);

fn main() {
    vigil_secret::exit_on_panic();
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    let [target] = args else {
        say!("{USAGE}");
        return 2;
    };
    if target == "-h" || target == "--help" {
        out!("{USAGE}\n{HELP}");
        return 0;
    }
    if !rustix::process::geteuid().is_root() {
        say!("vigil-unlock: run as root; vigil-lock obeys an Unlock only when root asked for it");
        return 1;
    }
    let unlocked = block_on(async {
        let conn = Connection::system()
            .await
            .map_err(|e| format!("logind: {e}"))?;
        unlock(&conn, target, Path::new(DIR), Owners::ROOT).await
    });
    match unlocked {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            say!("vigil-unlock: {e}");
            1
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Named {
    Session,
    User,
}

fn choose<'a>(sessions: &'a [Listed], target: &str) -> (Named, Vec<&'a Listed>) {
    let by_id: Vec<_> = sessions.iter().filter(|s| s.0 == target).collect();
    if !by_id.is_empty() {
        return (Named::Session, by_id);
    }
    (
        Named::User,
        sessions.iter().filter(|s| s.2 == target).collect(),
    )
}

// logind locks only user sessions; a user also has a user@.service session of class manager.
// A logind older than CanLock has no such class, so a session it lists can lock.
async fn can_lock(conn: &Connection, path: &OwnedObjectPath) -> bool {
    match Proxy::new(conn, LOGIN1, path.as_str(), SESSION_IFACE).await {
        Ok(session) => session
            .get_property::<bool>("CanLock")
            .await
            .unwrap_or(true),
        Err(_) => true,
    }
}

fn not_supported(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::MethodError(name, _, _) => name.as_str() == NOT_SUPPORTED,
        zbus::Error::FDO(e) => matches!(**e, zbus::fdo::Error::NotSupported(_)),
        _ => false,
    }
}

async fn unlock(
    conn: &Connection,
    target: &str,
    dir: &Path,
    owners: Owners,
) -> Result<bool, String> {
    let bus = |e: zbus::Error| format!("logind: {e}");
    let manager = Proxy::new(conn, LOGIN1, MANAGER_PATH, MANAGER_IFACE)
        .await
        .map_err(bus)?;
    let sessions: Vec<Listed> = manager.call("ListSessions", &()).await.map_err(bus)?;
    let (named, chosen) = choose(&sessions, target);
    if chosen.is_empty() {
        return Err(format!("no session or user named {target:?}"));
    }
    let mut lockable = Vec::new();
    for listed in chosen {
        if can_lock(conn, &listed.4).await {
            lockable.push(listed);
        } else if named == Named::Session {
            return Err(format!(
                "session {target} cannot lock; logind locks only user sessions"
            ));
        }
    }
    if lockable.is_empty() {
        return Err(format!("{target} has no session that can lock"));
    }
    vigil_unlock_token::prepare(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let swept = Moment::now().map_err(|e| format!("this boot: {e}"))?;
    vigil_unlock_token::sweep(dir, owners, &swept)
        .map_err(|e| format!("{}: clear old tokens: {e}", dir.display()))?;
    let mut wanted = lockable.len();
    let mut written = Vec::new();
    let mut asked = Vec::new();
    for (id, _, user, _, path) in lockable {
        let issued = Moment::now().map_err(|e| e.to_string()).and_then(|at| {
            vigil_unlock_token::issue(dir, owners, id, &at).map_err(|e| e.to_string())
        });
        if let Err(e) = issued {
            say!("vigil-unlock: session {id} ({user}): token: {e}");
            continue;
        }
        written.push(id);
        match manager.call::<_, _, ()>("UnlockSession", &(id,)).await {
            Ok(()) => asked.push((id, user, path)),
            Err(e) if named == Named::User && not_supported(&e) => {
                out!("session {id} ({user}): has no lock screen, skipped");
                wanted -= 1;
            }
            Err(e) => say!("vigil-unlock: session {id} ({user}): UnlockSession: {e}"),
        }
    }
    std::thread::sleep(SETTLE);
    for id in written {
        if let Err(e) = vigil_unlock_token::revoke(dir, id) {
            say!("vigil-unlock: session {id}: remove the token: {e}");
        }
    }
    let mut unlocked = wanted > 0 && asked.len() == wanted;
    for (id, user, path) in asked {
        let locked = match Proxy::new(conn, LOGIN1, path.as_str(), SESSION_IFACE).await {
            Ok(session) => session.get_property::<bool>("LockedHint").await.ok(),
            Err(_) => None,
        };
        match locked {
            Some(false) => out!("session {id} ({user}): unlocked"),
            Some(true) => {
                out!("session {id} ({user}): still locked");
                unlocked = false;
            }
            None => {
                out!("session {id} ({user}): lock state unknown");
                unlocked = false;
            }
        }
    }
    Ok(unlocked)
}

#[cfg(test)]
mod fake_logind;

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(id: &str, user: &str) -> Listed {
        (
            id.into(),
            1000,
            user.into(),
            "seat0".into(),
            OwnedObjectPath::try_from(format!("/org/freedesktop/login1/session/{id}"))
                .expect("valid path"),
        )
    }

    #[test]
    fn a_session_id_names_one_session_and_a_user_names_all_of_theirs() {
        let sessions = [
            listed("2", "alice"),
            listed("c1", "alice"),
            listed("3", "bob"),
        ];
        let ids = |target| -> Vec<&str> {
            choose(&sessions, target)
                .1
                .iter()
                .map(|s| s.0.as_str())
                .collect()
        };
        assert_eq!(ids("c1"), ["c1"]);
        assert_eq!(ids("alice"), ["2", "c1"]);
        assert_eq!(ids("bob"), ["3"]);
        assert!(ids("carol").is_empty());
        assert_eq!(choose(&sessions, "c1").0, Named::Session);
        assert_eq!(choose(&sessions, "alice").0, Named::User);
    }
}
