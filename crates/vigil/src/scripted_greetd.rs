use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use greetd_ipc::codec::SyncCodec;
use greetd_ipc::{Request, Response};

pub struct Greetd {
    dir: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Greetd {
    pub fn scripted(name: &str, replies: Vec<Response>) -> Option<Self> {
        let dir = std::env::temp_dir().join(format!("vigil-greetd-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("greetd.sock")).and_then(|listener| {
            let probe = UnixStream::connect(dir.join("greetd.sock"))?;
            listener.accept()?;
            drop(probe);
            Ok(listener)
        });
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                std::fs::remove_dir_all(&dir).unwrap();
                assert!(
                    std::env::var_os("CI").is_none(),
                    "unix sockets are refused, so this test would pass without running: {error}"
                );
                return None;
            }
            Err(error) => panic!("open the scripted greetd socket: {error}"),
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let thread = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            for reply in replies {
                let Ok(request) = Request::read_from(&mut stream) else {
                    return;
                };
                seen.lock().unwrap().push(match request {
                    Request::CreateSession { username } => format!("create {username}"),
                    Request::PostAuthMessageResponse { response } => {
                        format!("answer {}", response.as_deref().unwrap_or("<none>"))
                    }
                    Request::StartSession { .. } => "start".into(),
                    Request::CancelSession => "cancel".into(),
                });
                if reply.write_to(&mut stream).is_err() {
                    return;
                }
            }
        });
        Some(Self {
            dir,
            requests,
            thread: Some(thread),
        })
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("greetd.sock")
    }

    pub fn state_file(&self) -> PathBuf {
        self.dir.join("state.toml")
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn finish(mut self) -> Vec<String> {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        self.requests.lock().unwrap().clone()
    }
}
