//! The desktop side of the browser bridge.
//!
//! Shape: Chrome spawns our own binary as a **Native Messaging host** (`--native-messaging-host`),
//! which relays between Chrome's stdio protocol and a **Unix domain socket** owned by the running
//! app. That gives us:
//!
//! - **no listening TCP port**, so no web page can reach it (the spike's loopback HTTP server was
//!   reachable by any page on the machine),
//! - **no polling** — everything is a blocking read woken by an actual message,
//! - **no extra process to install or manage**: the host is this same binary with a flag.
//!
//! Silence is refusal. A request that is not answered within its deadline returns
//! [`BridgeError::NoResponse`], never "probably fine".

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
// Windows 10 has Unix domain sockets too - the same file-permission-guarded socket, the same
// shape. `uds_windows` is the standard library's API for them, which std only offers on Unix.
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub enum BridgeError {
    NotConnected,
    NoResponse,
    Transport(String),
}

pub fn socket_path() -> Option<PathBuf> {
    hvtt_core::paths::data_dir().map(|d| d.join("bridge.sock"))
}

#[derive(Default)]
struct Inner {
    writer: Option<UnixStream>,
    pending: HashMap<u64, Sender<Value>>,
    next_id: u64,
    connected: bool,
}

#[derive(Clone)]
pub struct Bridge {
    inner: Arc<Mutex<Inner>>,
}

impl Bridge {
    pub fn new() -> Self {
        Bridge { inner: Arc::new(Mutex::new(Inner { next_id: 1, ..Default::default() })) }
    }

    pub fn is_connected(&self) -> bool {
        self.inner.lock().unwrap().connected
    }

    /// Listen for the native-messaging host to connect. One connection at a time is enough:
    /// the host is spawned per browser, and v1 supports a single pinned destination.
    pub fn serve(&self) -> std::io::Result<()> {
        let path = socket_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no app data directory")
        })?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;

        let me = self.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let read_side = match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                {
                    let mut inner = me.inner.lock().unwrap();
                    inner.writer = Some(stream);
                    inner.connected = true;
                }
                let me2 = me.clone();
                std::thread::spawn(move || {
                    let reader = BufReader::new(read_side);
                    for line in reader.lines().map_while(Result::ok) {
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
                            let tx = me2.inner.lock().unwrap().pending.remove(&id);
                            if let Some(tx) = tx {
                                let _ = tx.send(v);
                            }
                        }
                    }
                    // The browser went away. Every pinned destination in it is gone with it.
                    let mut inner = me2.inner.lock().unwrap();
                    inner.connected = false;
                    inner.writer = None;
                    inner.pending.clear();
                });
            }
        });
        Ok(())
    }

    /// Send a request and wait for its reply. A timeout is a refusal.
    pub fn request(&self, cmd: &str, text: Option<String>, timeout: Duration)
        -> Result<Value, BridgeError>
    {
        let (id, rx) = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.connected {
                return Err(BridgeError::NotConnected);
            }
            let id = inner.next_id;
            inner.next_id += 1;
            let (tx, rx): (Sender<Value>, Receiver<Value>) = channel();
            inner.pending.insert(id, tx);

            let req = Request { id, cmd: cmd.to_string(), text };
            let line = serde_json::to_string(&req)
                .map_err(|e| BridgeError::Transport(e.to_string()))?;
            let writer = inner.writer.as_mut().ok_or(BridgeError::NotConnected)?;
            writer
                .write_all(format!("{line}\n").as_bytes())
                .and_then(|_| writer.flush())
                .map_err(|e| BridgeError::Transport(e.to_string()))?;
            (id, rx)
        };

        match rx.recv_timeout(timeout) {
            Ok(v) => Ok(v),
            Err(_) => {
                self.inner.lock().unwrap().pending.remove(&id);
                Err(BridgeError::NoResponse)
            }
        }
    }
}

/// Native-messaging host mode: relay Chrome's stdio protocol to the app's socket.
///
/// Chrome frames each message as a little-endian u32 length followed by JSON. This process does
/// nothing else — no parsing of the payload, no policy. All decisions live in the app.
pub fn run_native_messaging_host() -> ! {
    use std::io::{Read, Write as _};

    let Some(path) = socket_path() else { std::process::exit(2) };
    let Ok(sock) = UnixStream::connect(&path) else {
        // The app is not running. Exiting cleanly makes the extension see a closed port, which
        // it reports as "no desktop app", not as an error.
        std::process::exit(3)
    };

    // socket -> Chrome
    let mut to_chrome = sock.try_clone().expect("clone socket");
    std::thread::spawn(move || {
        let reader = BufReader::new(to_chrome.try_clone().expect("clone"));
        let mut out = std::io::stdout();
        for line in reader.lines().map_while(Result::ok) {
            let bytes = line.as_bytes();
            if out.write_all(&(bytes.len() as u32).to_le_bytes()).is_err()
                || out.write_all(bytes).is_err()
                || out.flush().is_err()
            {
                break;
            }
        }
        let _ = to_chrome.flush();
        std::process::exit(0);
    });

    // Chrome -> socket
    let mut from_chrome = sock;
    let mut stdin = std::io::stdin();
    loop {
        let mut len = [0u8; 4];
        if stdin.read_exact(&mut len).is_err() {
            break;
        }
        let n = u32::from_le_bytes(len) as usize;
        if n == 0 || n > 64 * 1024 * 1024 {
            break;
        }
        let mut buf = vec![0u8; n];
        if stdin.read_exact(&mut buf).is_err() {
            break;
        }
        buf.push(b'\n');
        if from_chrome.write_all(&buf).is_err() {
            break;
        }
        let _ = from_chrome.flush();
    }
    std::process::exit(0)
}
