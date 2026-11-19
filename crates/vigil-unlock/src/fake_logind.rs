use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use vigil_unlock_token::{Refusal, admit};
use zbus::connection::Builder;

use super::*;

const BUS_CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <listen>unix:path=SOCKET</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow user="*"/>
    <allow own="*"/>
    <allow send_type="method_call"/>
    <allow send_type="signal"/>
    <allow send_requested_reply="true" send_type="method_return"/>
    <allow send_requested_reply="true" send_type="error"/>
    <allow receive_type="method_call"/>
    <allow receive_type="method_return"/>
    <allow receive_type="error"/>
    <allow receive_type="signal"/>
  </policy>
</busconfig>
"#;

struct Scratch {
    dir: PathBuf,
    daemon: Child,
    address: String,
}

impl Scratch {
    fn start(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("vigil-unlock-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("unlock")).expect("create the scratch directory");
        std::fs::set_permissions(dir.join("unlock"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod the token directory");
        let config = dir.join("bus.conf");
        let socket = dir.join("bus");
        std::fs::write(
            &config,
            BUS_CONFIG.replace("SOCKET", socket.to_str().expect("utf-8 temp dir")),
        )
        .expect("write the bus config");
        let mut daemon = Command::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address")
            .arg(format!("--config-file={}", config.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is required to test vigil-unlock against a fake logind");
        let mut address = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(daemon.stdout.take().expect("daemon stdout")),
            &mut address,
        )
        .expect("read the bus address");
        Self {
            dir,
            daemon,
            address: address.trim().to_string(),
        }
    }

    fn tokens(&self) -> PathBuf {
        self.dir.join("unlock")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Seen = Vec<(String, Result<(), Refusal>)>;
type Calls = Arc<Mutex<Seen>>;

struct Manager {
    tokens: PathBuf,
    owner: u32,
    refuses: &'static str,
    calls: Calls,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn list_sessions(&self) -> Vec<Listed> {
        [("c1", "alice"), ("c2", "alice"), ("3", "bob")]
            .into_iter()
            .map(|(id, user)| {
                (
                    id.to_string(),
                    1000,
                    user.to_string(),
                    "seat0".to_string(),
                    session_path(id),
                )
            })
            .collect()
    }

    fn unlock_session(&self, id: String) -> zbus::fdo::Result<()> {
        let at = Moment::now().expect("this boot");
        let seen = admit(&self.tokens, self.owner, &id, &at);
        self.calls.lock().expect("calls").push((id.clone(), seen));
        if id == self.refuses {
            return Err(zbus::fdo::Error::AccessDenied("refused".into()));
        }
        Ok(())
    }
}

struct Session {
    id: &'static str,
    refuses: &'static str,
    calls: Calls,
}

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    #[zbus(property)]
    fn locked_hint(&self) -> bool {
        self.id == self.refuses
            || !self
                .calls
                .lock()
                .expect("calls")
                .iter()
                .any(|(id, seen)| id == self.id && seen.is_ok())
    }
}

fn session_path(id: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("/org/freedesktop/login1/session/{id}")).expect("valid path")
}

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn run_against(name: &str, target: &str, refuses: &'static str) -> (Result<bool, String>, Seen) {
    let scratch = Scratch::start(name);
    let calls = Calls::default();
    let mut builder = Builder::address(scratch.address.as_str()).serve_at(
        MANAGER_PATH,
        Manager {
            tokens: scratch.tokens(),
            owner: me(),
            refuses,
            calls: calls.clone(),
        },
    );
    for id in ["c1", "c2", "3"] {
        builder = builder.serve_at(
            session_path(id),
            Session {
                id,
                refuses,
                calls: calls.clone(),
            },
        );
    }
    let _logind = block_on(builder.name(LOGIN1).build()).expect("own login1");
    let root =
        block_on(Builder::address(scratch.address.as_str()).build()).expect("connect as root");
    let outcome = block_on(unlock(&root, target, &scratch.tokens(), me()));
    let left: Vec<_> = std::fs::read_dir(scratch.tokens())
        .expect("list the token directory")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert!(left.is_empty(), "tokens left behind: {left:?}");
    let calls = calls.lock().expect("calls").clone();
    (outcome, calls)
}

#[test]
fn logind_sees_root_s_token_when_asked_and_no_token_outlives_the_run() {
    let (outcome, calls) = run_against("session", "c1", "");
    assert_eq!(outcome, Ok(true));
    assert_eq!(calls, [("c1".to_string(), Ok(()))]);
}

#[test]
fn a_user_names_each_session_and_a_refused_one_still_loses_its_token() {
    let (outcome, calls) = run_against("user", "alice", "c2");
    assert_eq!(outcome, Ok(false), "c2 stays locked");
    assert_eq!(
        calls,
        [("c1".to_string(), Ok(())), ("c2".to_string(), Ok(()))]
    );
}

#[test]
fn an_unknown_target_writes_nothing() {
    let (outcome, calls) = run_against("unknown", "carol", "");
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(calls.is_empty());
}
