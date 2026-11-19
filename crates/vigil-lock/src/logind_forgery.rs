use std::fmt::Debug;
use std::fs::Permissions;
use std::future::poll_fn;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use vigil_config::Lock;
use vigil_core::{AppearanceEvent, ColorScheme, LoginEvent};
use vigil_flow::{FlowCmd, FlowEvent, FlowPhase, LockFlow, LockOutcome, Now};
use vigil_login::{AppearanceWatcher, LoginSession};
use vigil_unlock_token::{Moment, Owners, WINDOW, issue, render};
use zbus::export::futures_core::Stream;
use zbus::fdo::DBusProxy;
use zbus::{Connection, Message, MessageStream, OwnedObjectPath, Variant, block_on};

const PRIVATE_BUS: &str = "VIGIL_TEST_PRIVATE_BUS";
const BUS_DIR_PREFIX: &str = "vigil-test-bus-";
const LOGIN1: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
const SESSION_AUTO: &str = "/org/freedesktop/login1/session/auto";
const SESSION_ID: &str = "c1";
const SESSION_PATH: &str = "/org/freedesktop/login1/session/c1";
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.Settings";
const WAIT: Duration = Duration::from_secs(10);

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

struct PrivateBus {
    daemon: Child,
    dir: PathBuf,
    address: String,
}

impl PrivateBus {
    fn start() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "{BUS_DIR_PREFIX}{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("create the private bus directory");
        let config = dir.join("bus.conf");
        let socket = dir.join("bus");
        std::fs::write(
            &config,
            BUS_CONFIG.replace("SOCKET", socket.to_str().expect("utf-8 temp dir")),
        )
        .expect("write the private bus config");
        let mut daemon = Command::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address")
            .arg(format!("--config-file={}", config.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("dbus-daemon is required to test logind signal authentication");
        let mut address = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(daemon.stdout.take().expect("daemon stdout")),
            &mut address,
        )
        .expect("read the private bus address");
        let address = address.trim().to_string();
        assert!(
            address.starts_with("unix:path="),
            "unexpected address {address:?}"
        );
        Self {
            daemon,
            dir,
            address,
        }
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn private_bus_in_this_process() -> Option<String> {
    let address = std::env::var(PRIVATE_BUS).ok()?;
    let socket = address
        .strip_prefix("unix:path=")
        .and_then(|rest| rest.split(',').next())
        .map(Path::new)
        .expect("private bus address is a unix path");
    let dir = socket.parent().expect("socket has a directory");
    assert!(
        dir.starts_with(std::env::temp_dir())
            && dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(BUS_DIR_PREFIX)),
        "{address} is not a bus this test started"
    );
    for var in ["DBUS_SYSTEM_BUS_ADDRESS", "DBUS_SESSION_BUS_ADDRESS"] {
        assert_eq!(
            std::env::var(var).as_deref(),
            Ok(address.as_str()),
            "{var} must name the private test bus"
        );
    }
    Some(address)
}

fn on_private_bus(test: &str, scenario: fn(&str)) {
    if let Some(address) = private_bus_in_this_process() {
        scenario(&address);
        return;
    }
    let bus = PrivateBus::start();
    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, rest)| rest);
    let output = Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", &format!("{module}::{test}"), "--test-threads=1"])
        .arg("--nocapture")
        .env(PRIVATE_BUS, &bus.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", &bus.address)
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .output()
        .expect("run the scenario in a child process");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{test} on a private bus:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct FakeSession;

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl FakeSession {
    #[zbus(property)]
    fn id(&self) -> String {
        SESSION_ID.to_string()
    }
}

struct FakeManager;

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl FakeManager {
    fn get_session(&self, id: String) -> zbus::fdo::Result<OwnedObjectPath> {
        if id != SESSION_ID {
            return Err(zbus::fdo::Error::Failed(format!("no session {id}")));
        }
        Ok(OwnedObjectPath::try_from(SESSION_PATH).expect("valid path"))
    }
}

fn connect(address: &str) -> Connection {
    block_on(zbus::connection::Builder::address(address).build()).expect("connect to the test bus")
}

fn logind(address: &str) -> Connection {
    block_on(
        zbus::connection::Builder::address(address)
            .serve_at(SESSION_AUTO, FakeSession)
            .serve_at(MANAGER_PATH, FakeManager)
            .name(LOGIN1)
            .build(),
    )
    .expect("own org.freedesktop.login1 on the test bus")
}

fn unique(conn: &Connection) -> String {
    conn.unique_name().expect("bus connection").to_string()
}

fn send(conn: &Connection, to: Option<&str>, event: LoginEvent) {
    let sent = match event {
        LoginEvent::Lock => {
            block_on(conn.emit_signal(to, SESSION_PATH, SESSION_IFACE, "Lock", &()))
        }
        LoginEvent::Unlock => {
            block_on(conn.emit_signal(to, SESSION_PATH, SESSION_IFACE, "Unlock", &()))
        }
        LoginEvent::PrepareForSleep(sleeping) => block_on(conn.emit_signal(
            to,
            MANAGER_PATH,
            MANAGER_IFACE,
            "PrepareForSleep",
            &sleeping,
        )),
    };
    sent.expect("emit a signal");
}

fn send_scheme(conn: &Connection, to: Option<&str>, value: u32) {
    block_on(conn.emit_signal(
        to,
        PORTAL_PATH,
        PORTAL_IFACE,
        "SettingChanged",
        &(
            "org.freedesktop.appearance",
            "color-scheme",
            Variant::from(value),
        ),
    ))
    .expect("emit SettingChanged");
}

fn round_trip(conn: &Connection) {
    block_on(async { DBusProxy::new(conn).await?.get_id().await }).expect("bus round trip");
}

fn forge(from: &Connection, send: impl Fn(Option<&str>)) {
    let me = unique(from);
    let names =
        block_on(async { DBusProxy::new(from).await?.list_names().await }).expect("list bus names");
    for name in names
        .iter()
        .map(|name| name.as_str())
        .filter(|name| name.starts_with(':') && *name != me)
    {
        send(Some(name));
    }
    send(None);
    round_trip(from);
}

fn until<T: PartialEq + Debug>(rx: &Receiver<T>, wanted: T) -> Vec<T> {
    let mut seen = Vec::new();
    loop {
        match rx.recv_timeout(WAIT) {
            Ok(event) if event == wanted => {
                seen.push(event);
                return seen;
            }
            Ok(event) => seen.push(event),
            Err(e) => panic!("{wanted:?} never arrived ({e}); saw {seen:?}"),
        }
    }
}

fn prove_live<T: PartialEq + Debug + Copy>(
    rx: &Receiver<T>,
    emit: impl Fn(T),
    probe: T,
    control: T,
) {
    let live = (0..200).any(|_| {
        emit(probe);
        rx.recv_timeout(Duration::from_millis(50))
            .is_ok_and(|event| event == probe)
    });
    assert!(live, "the subscription never delivered a genuine {probe:?}");
    emit(control);
    until(rx, control);
}

fn subscribed_session(owner: &Connection) -> (LoginSession, Receiver<LoginEvent>) {
    let session = LoginSession::connect().expect("vigil-login reaches the fake logind");
    let (tx, rx) = mpsc::channel();
    session.spawn_signals(tx);
    prove_live(
        &rx,
        |event| send(owner, None, event),
        LoginEvent::Lock,
        LoginEvent::Unlock,
    );
    (session, rx)
}

fn at(ms: u64) -> Now {
    Now::at(Duration::from_millis(ms))
}

fn locked_flow() -> LockFlow {
    let lock = Lock::default();
    let (mut flow, _) = LockFlow::new(at(0), &lock, Some(0));
    let ramp = lock.transition.in_ms();
    flow.step(at(ramp), FlowEvent::Tick);
    let cmds = flow.step(at(ramp + 10), FlowEvent::LockConfirmed);
    assert!(cmds.contains(&FlowCmd::StartAuth), "{cmds:?}");
    assert_eq!(flow.phase(), FlowPhase::Locked);
    flow
}

fn warning_flow() -> LockFlow {
    let (flow, _) = LockFlow::new(at(0), &Lock::default(), Some(60_000));
    assert_eq!(flow.phase(), FlowPhase::PreLock);
    flow
}

fn feed(
    session: &LoginSession,
    flow: &mut LockFlow,
    events: &[LoginEvent],
    ms: u64,
) -> Vec<FlowCmd> {
    events
        .iter()
        .filter_map(|event| super::login_flow_event(*event, Some(session.id())))
        .flat_map(|event| flow.step(at(ms), event))
        .collect()
}

fn unlocks(cmds: &[FlowCmd]) -> bool {
    cmds.iter().any(|cmd| {
        matches!(
            cmd,
            FlowCmd::DetachAuth
                | FlowCmd::ReleaseSessionLock
                | FlowCmd::CreateRevealOverlays
                | FlowCmd::SetLockedHint(false)
                | FlowCmd::Exit(LockOutcome::Unlocked)
        )
    })
}

fn assert_still_locked(flow: &LockFlow, events: &[LoginEvent], cmds: &[FlowCmd]) {
    assert!(
        !unlocks(cmds) && flow.phase() == FlowPhase::Locked,
        "a signal vigil must ignore changed the lock: \
         events {events:?}, phase {:?}, commands {cmds:?}",
        flow.phase()
    );
    assert_eq!(
        events,
        [LoginEvent::Lock],
        "only the genuine marker arrives"
    );
}

fn forged_unlock_while_locked(address: &str) {
    let owner = logind(address);
    let (session, rx) = subscribed_session(&owner);
    let mut flow = locked_flow();
    let attacker = connect(address);
    forge(&attacker, |to| send(&attacker, to, LoginEvent::Unlock));
    send(&owner, None, LoginEvent::Lock);
    let events = until(&rx, LoginEvent::Lock);
    let cmds = feed(&session, &mut flow, &events, 1_000);
    assert_still_locked(&flow, &events, &cmds);
}

#[test]
fn forged_unlock_leaves_a_locked_session_locked() {
    on_private_bus(
        "forged_unlock_leaves_a_locked_session_locked",
        forged_unlock_while_locked,
    );
}

fn forged_unlock_during_warning(address: &str) {
    let owner = logind(address);
    let (session, rx) = subscribed_session(&owner);
    let mut flow = warning_flow();
    let attacker = connect(address);
    forge(&attacker, |to| send(&attacker, to, LoginEvent::Unlock));
    send(&owner, None, LoginEvent::Lock);
    let events = until(&rx, LoginEvent::Lock);
    let mut cmds = feed(&session, &mut flow, &events, 100);
    cmds.extend(flow.step(at(200), FlowEvent::LockConfirmed));
    assert_still_locked(&flow, &events, &cmds);
    assert!(cmds.contains(&FlowCmd::StartAuth), "{cmds:?}");
}

#[test]
fn forged_unlock_during_the_warning_is_not_honoured_at_commit() {
    on_private_bus(
        "forged_unlock_during_the_warning_is_not_honoured_at_commit",
        forged_unlock_during_warning,
    );
}

fn previous_owner(address: &str) {
    let first = logind(address);
    let (session, rx) = subscribed_session(&first);
    let mut flow = locked_flow();
    let second = connect(address);
    block_on(first.release_name(LOGIN1)).expect("release login1");
    block_on(second.request_name(LOGIN1)).expect("take login1 over");
    forge(&first, |to| send(&first, to, LoginEvent::Unlock));
    send(&second, None, LoginEvent::Lock);
    let events = until(&rx, LoginEvent::Lock);
    let cmds = feed(&session, &mut flow, &events, 1_000);
    assert_still_locked(&flow, &events, &cmds);
    send(&second, None, LoginEvent::Unlock);
    assert_eq!(until(&rx, LoginEvent::Unlock), [LoginEvent::Unlock]);
}

#[test]
fn a_replaced_logind_owner_is_no_longer_trusted() {
    on_private_bus(
        "a_replaced_logind_owner_is_no_longer_trusted",
        previous_owner,
    );
}

fn no_owner(address: &str) {
    let first = logind(address);
    let (session, rx) = subscribed_session(&first);
    let mut flow = locked_flow();
    block_on(first.release_name(LOGIN1)).expect("release login1");
    let attacker = connect(address);
    forge(&first, |to| send(&first, to, LoginEvent::Unlock));
    forge(&attacker, |to| send(&attacker, to, LoginEvent::Unlock));
    let second = connect(address);
    block_on(second.request_name(LOGIN1)).expect("take login1");
    send(&second, None, LoginEvent::Lock);
    let events = until(&rx, LoginEvent::Lock);
    let cmds = feed(&session, &mut flow, &events, 1_000);
    assert_still_locked(&flow, &events, &cmds);
}

#[test]
fn with_no_logind_owner_every_unlock_is_rejected() {
    on_private_bus("with_no_logind_owner_every_unlock_is_rejected", no_owner);
}

fn other_session(address: &str) {
    let owner = logind(address);
    let (session, rx) = subscribed_session(&owner);
    let mut flow = locked_flow();
    block_on(owner.emit_signal(
        None::<&str>,
        "/org/freedesktop/login1/session/c2",
        SESSION_IFACE,
        "Unlock",
        &(),
    ))
    .expect("emit Unlock for another session");
    send(&owner, None, LoginEvent::Lock);
    let events = until(&rx, LoginEvent::Lock);
    let cmds = feed(&session, &mut flow, &events, 1_000);
    assert_still_locked(&flow, &events, &cmds);
}

#[test]
fn unlock_for_another_session_leaves_this_one_locked() {
    on_private_bus(
        "unlock_for_another_session_leaves_this_one_locked",
        other_session,
    );
}

fn witness(address: &str) -> (Connection, Receiver<Message>) {
    let conn = connect(address);
    let stream = MessageStream::from(&conn);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            let mut stream = pin!(stream);
            while let Some(Ok(message)) = poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                if tx.send(message).is_err() {
                    return;
                }
            }
        });
    });
    (conn, rx)
}

fn forged_owner_change(address: &str) {
    let owner = logind(address);
    let (session, rx) = subscribed_session(&owner);
    let mut flow = locked_flow();
    let (_witness, delivered) = witness(address);
    let attacker = connect(address);
    let me = unique(&attacker);
    let real = unique(&owner);
    forge(&attacker, |to| {
        block_on(attacker.emit_signal(
            to,
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
            &(LOGIN1, real.as_str(), me.as_str()),
        ))
        .expect("emit NameOwnerChanged");
    });
    forge(&attacker, |to| send(&attacker, to, LoginEvent::Unlock));
    send(&owner, None, LoginEvent::Lock);
    let events = until(&rx, LoginEvent::Lock);
    let cmds = feed(&session, &mut flow, &events, 1_000);
    let bus_delivered_it = std::iter::from_fn(|| delivered.recv_timeout(WAIT).ok()).any(|m| {
        let header = m.header();
        header
            .member()
            .is_some_and(|member| member == "NameOwnerChanged")
            && header.sender().is_some_and(|sender| sender.as_str() == me)
    });
    assert!(
        bus_delivered_it,
        "the bus refused the forged NameOwnerChanged, so this test proves nothing"
    );
    assert_still_locked(&flow, &events, &cmds);
}

#[test]
fn a_forged_name_owner_change_does_not_hand_over_trust() {
    on_private_bus(
        "a_forged_name_owner_change_does_not_hand_over_trust",
        forged_owner_change,
    );
}

fn forged_sleep(address: &str) {
    let owner = logind(address);
    let session = LoginSession::connect().expect("vigil-login reaches the fake logind");
    let (tx, rx) = mpsc::channel();
    session.spawn_sleep_signals(tx);
    prove_live(
        &rx,
        |event| send(&owner, None, event),
        LoginEvent::PrepareForSleep(false),
        LoginEvent::PrepareForSleep(true),
    );
    let attacker = connect(address);
    forge(&attacker, |to| {
        send(&attacker, to, LoginEvent::PrepareForSleep(true))
    });
    send(&owner, None, LoginEvent::PrepareForSleep(false));
    assert_eq!(
        until(&rx, LoginEvent::PrepareForSleep(false)),
        [LoginEvent::PrepareForSleep(false)],
        "a PrepareForSleep from a sender that is not logind reached the lock"
    );
}

#[test]
fn forged_prepare_for_sleep_is_ignored() {
    on_private_bus("forged_prepare_for_sleep_is_ignored", forged_sleep);
}

fn forged_setting(address: &str) {
    let portal = connect(address);
    block_on(portal.request_name(PORTAL)).expect("own the portal name");
    let watcher = AppearanceWatcher::connect().expect("session bus");
    let (tx, rx) = mpsc::channel();
    watcher.spawn_signals(tx);
    let scheme = |raw| AppearanceEvent::Scheme(ColorScheme::from_portal(raw));
    prove_live(
        &rx,
        |event| {
            let raw = match event {
                AppearanceEvent::Scheme(ColorScheme::Dark) => 1,
                _ => 2,
            };
            send_scheme(&portal, None, raw);
        },
        scheme(1),
        scheme(2),
    );
    let attacker = connect(address);
    forge(&attacker, |to| send_scheme(&attacker, to, 1));
    send_scheme(&portal, None, 0);
    assert_eq!(
        until(&rx, scheme(0)),
        [scheme(0)],
        "a SettingChanged from a sender that is not the portal reached the lock"
    );
}

#[test]
fn forged_portal_setting_is_ignored() {
    on_private_bus("forged_portal_setting_is_ignored", forged_setting);
}

struct TokenDir {
    base: PathBuf,
    path: PathBuf,
    elsewhere: PathBuf,
}

impl TokenDir {
    // Each scenario runs in a process of its own, so the trust set here is the only one.
    fn trusting(owners: Owners) -> Self {
        let base = std::env::temp_dir().join(format!("vigil-test-unlock-{}", std::process::id()));
        let path = base.join("unlock");
        let elsewhere = base.join("elsewhere");
        let _ = std::fs::remove_dir_all(&base);
        for dir in [&base, &path, &elsewhere] {
            std::fs::create_dir(dir).expect("create a token directory");
            std::fs::set_permissions(dir, Permissions::from_mode(0o755))
                .expect("chmod a token directory");
        }
        super::TEST_TRUST
            .set((path.clone(), owners))
            .expect("one trust per scenario process");
        Self {
            base,
            path,
            elsewhere,
        }
    }

    fn plant(&self, name: &str, content: &str) {
        let file = self.path.join(name);
        std::fs::write(&file, content).expect("plant a token");
        std::fs::set_permissions(&file, Permissions::from_mode(0o644)).expect("chmod a token");
    }
}

impl Drop for TokenDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn mine() -> Owners {
    Owners::all(me())
}

fn now() -> Moment {
    Moment::now().expect("this boot's id and clock")
}

fn unlock_with_token(
    address: &str,
    trusted: Owners,
    plant: impl FnOnce(&TokenDir),
) -> (LockFlow, Vec<FlowCmd>) {
    let owner = logind(address);
    let (session, rx) = subscribed_session(&owner);
    let mut flow = locked_flow();
    let tokens = TokenDir::trusting(trusted);
    plant(&tokens);
    send(&owner, None, LoginEvent::Unlock);
    let events = until(&rx, LoginEvent::Unlock);
    assert_eq!(
        events,
        [LoginEvent::Unlock],
        "logind's Unlock arrives alone"
    );
    let cmds = feed(&session, &mut flow, &events, 1_000);
    (flow, cmds)
}

fn assert_refused(flow: &LockFlow, cmds: &[FlowCmd]) {
    assert!(
        !unlocks(cmds) && flow.phase() == FlowPhase::Locked,
        "an Unlock root did not ask for changed the lock: phase {:?}, commands {cmds:?}",
        flow.phase()
    );
}

fn refused_with(address: &str, trusted: Owners, plant: impl FnOnce(&TokenDir)) {
    let (flow, cmds) = unlock_with_token(address, trusted, plant);
    assert_refused(&flow, &cmds);
}

fn owner_unlock_without_token(address: &str) {
    refused_with(address, mine(), |_| {});
}

#[test]
fn the_owners_unlock_without_a_root_token_is_refused() {
    on_private_bus(
        "the_owners_unlock_without_a_root_token_is_refused",
        owner_unlock_without_token,
    );
}

fn owner_unlock_during_warning(address: &str) {
    let owner = logind(address);
    let (session, rx) = subscribed_session(&owner);
    let mut flow = warning_flow();
    let _tokens = TokenDir::trusting(mine());
    send(&owner, None, LoginEvent::Unlock);
    send(&owner, None, LoginEvent::Lock);
    let events = until(&rx, LoginEvent::Lock);
    assert_eq!(events, [LoginEvent::Unlock, LoginEvent::Lock]);
    let mut cmds = feed(&session, &mut flow, &events, 100);
    cmds.extend(flow.step(at(200), FlowEvent::LockConfirmed));
    assert_refused(&flow, &cmds);
    assert!(cmds.contains(&FlowCmd::StartAuth), "{cmds:?}");
}

#[test]
fn the_owners_unlock_during_the_warning_is_refused() {
    on_private_bus(
        "the_owners_unlock_during_the_warning_is_refused",
        owner_unlock_during_warning,
    );
}

fn user_owned_token(address: &str) {
    let trusted = Owners {
        token: me().wrapping_add(1),
        ..mine()
    };
    refused_with(address, trusted, |tokens| {
        issue(&tokens.path, mine(), SESSION_ID, &now()).expect("issue as the user");
    });
}

#[test]
fn a_token_the_user_owns_is_refused() {
    on_private_bus("a_token_the_user_owns_is_refused", user_owned_token);
}

fn user_owned_dir(address: &str) {
    let trusted = Owners {
        dir: me().wrapping_add(1),
        ..mine()
    };
    refused_with(address, trusted, |tokens| {
        issue(&tokens.path, mine(), SESSION_ID, &now()).expect("issue as the user");
    });
}

#[test]
fn a_root_token_in_a_directory_the_user_owns_is_refused() {
    on_private_bus(
        "a_root_token_in_a_directory_the_user_owns_is_refused",
        user_owned_dir,
    );
}

fn stale_token(address: &str) {
    refused_with(address, mine(), |tokens| {
        let now = now();
        let then = Moment {
            since_boot: now.since_boot - WINDOW - Duration::from_secs(1),
            ..now
        };
        issue(&tokens.path, mine(), SESSION_ID, &then).expect("issue a stale token");
    });
}

#[test]
fn a_stale_token_is_refused() {
    on_private_bus("a_stale_token_is_refused", stale_token);
}

fn other_session_token(address: &str) {
    refused_with(address, mine(), |tokens| {
        tokens.plant(SESSION_ID, &render("c2", &now()));
        issue(&tokens.path, mine(), "c2", &now()).expect("issue for c2");
    });
}

#[test]
fn a_token_for_another_session_is_refused() {
    on_private_bus(
        "a_token_for_another_session_is_refused",
        other_session_token,
    );
}

fn other_boot_token(address: &str) {
    refused_with(address, mine(), |tokens| {
        let now = now();
        let boot = if now.boot.starts_with('0') { "1" } else { "0" };
        let other = Moment {
            boot: format!("{boot}{}", &now.boot[1..]),
            ..now
        };
        tokens.plant(SESSION_ID, &render(SESSION_ID, &other));
    });
}

#[test]
fn a_token_from_another_boot_is_refused() {
    on_private_bus("a_token_from_another_boot_is_refused", other_boot_token);
}

fn symlinked_token(address: &str) {
    refused_with(address, mine(), |tokens| {
        issue(&tokens.elsewhere, mine(), SESSION_ID, &now()).expect("issue elsewhere");
        std::os::unix::fs::symlink(
            tokens.elsewhere.join(SESSION_ID),
            tokens.path.join(SESSION_ID),
        )
        .expect("link the token in");
    });
}

#[test]
fn a_symlinked_token_is_refused() {
    on_private_bus("a_symlinked_token_is_refused", symlinked_token);
}

fn writable_dir_token(address: &str) {
    refused_with(address, mine(), |tokens| {
        issue(&tokens.path, mine(), SESSION_ID, &now()).expect("issue");
        std::fs::set_permissions(&tokens.path, Permissions::from_mode(0o777))
            .expect("open the directory to everyone");
    });
}

#[test]
fn a_token_in_a_directory_others_can_write_is_refused() {
    on_private_bus(
        "a_token_in_a_directory_others_can_write_is_refused",
        writable_dir_token,
    );
}

fn root_token(address: &str) {
    let (_flow, cmds) = unlock_with_token(address, mine(), |tokens| {
        issue(&tokens.path, mine(), SESSION_ID, &now()).expect("issue as root");
    });
    assert!(
        unlocks(&cmds),
        "logind's Unlock with root's token must unlock: {cmds:?}"
    );
}

#[test]
fn logind_unlock_with_a_root_token_unlocks() {
    on_private_bus("logind_unlock_with_a_root_token_unlocks", root_token);
}
