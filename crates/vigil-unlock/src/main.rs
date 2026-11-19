#![forbid(unsafe_code)]

use std::path::Path;
use std::time::Duration;

use vigil_log::{out, say};
use vigil_unlock_token::{DIR, Moment, ROOT};
use zbus::{Connection, OwnedObjectPath, Proxy, block_on};

const LOGIN1: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
const USAGE: &str = "usage: vigil-unlock <session-id|user>";
const HELP: &str = "Unlocks a session that vigil-lock holds. Run it as root.\n\
A session id names one session. A user name names every session of that user.\n\
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
    match block_on(unlock(target)) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            say!("vigil-unlock: {e}");
            1
        }
    }
}

fn choose<'a>(sessions: &'a [Listed], target: &str) -> Vec<&'a Listed> {
    let by_id: Vec<_> = sessions.iter().filter(|s| s.0 == target).collect();
    if !by_id.is_empty() {
        return by_id;
    }
    sessions.iter().filter(|s| s.2 == target).collect()
}

async fn unlock(target: &str) -> Result<bool, String> {
    let bus = |e: zbus::Error| format!("logind: {e}");
    let conn = Connection::system().await.map_err(bus)?;
    let manager = Proxy::new(&conn, LOGIN1, MANAGER_PATH, MANAGER_IFACE)
        .await
        .map_err(bus)?;
    let sessions: Vec<Listed> = manager.call("ListSessions", &()).await.map_err(bus)?;
    let chosen = choose(&sessions, target);
    if chosen.is_empty() {
        return Err(format!("no session or user named {target:?}"));
    }
    let at = Moment::now().map_err(|e| format!("this boot: {e}"))?;
    let dir = Path::new(DIR);
    vigil_unlock_token::prepare(dir).map_err(|e| format!("{DIR}: {e}"))?;
    let wanted = chosen.len();
    let mut issued = Vec::new();
    for (id, _, user, _, path) in chosen {
        if let Err(e) = vigil_unlock_token::issue(dir, ROOT, id, &at) {
            say!("vigil-unlock: session {id} ({user}): token: {e}");
            continue;
        }
        match manager.call::<_, _, ()>("UnlockSession", &(id,)).await {
            Ok(()) => issued.push((id, user, path)),
            Err(e) => say!("vigil-unlock: session {id} ({user}): UnlockSession: {e}"),
        }
    }
    std::thread::sleep(SETTLE);
    let mut unlocked = issued.len() == wanted;
    for (id, user, path) in issued {
        if let Err(e) = vigil_unlock_token::revoke(dir, id) {
            say!("vigil-unlock: session {id}: remove the token: {e}");
        }
        let locked = match Proxy::new(&conn, LOGIN1, path.as_str(), SESSION_IFACE).await {
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
                .iter()
                .map(|s| s.0.as_str())
                .collect()
        };
        assert_eq!(ids("c1"), ["c1"]);
        assert_eq!(ids("alice"), ["2", "c1"]);
        assert_eq!(ids("bob"), ["3"]);
        assert!(ids("carol").is_empty());
    }
}
