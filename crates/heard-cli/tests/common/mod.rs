//! Shared fixtures: an isolated environment, a fake daemon that records
//! frames, and a loopback HTTP server for model downloads.
//!
//! Safety: every command runs with a temp `HOME` and `HEARD_CLI_HOME`,
//! provider keys removed, no autostart, and no audio.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Provider credentials named explicitly; [`is_secret_env`] also removes
/// every `*_API_KEY` / `*_TOKEN` variable the test process inherited.
pub const SECRET_ENV: [&str; 2] = ["ANTHROPIC_API_KEY", "ELEVENLABS_API_KEY"];

pub fn is_secret_env(name: &str) -> bool {
    SECRET_ENV.contains(&name) || name.ends_with("_API_KEY") || name.ends_with("_TOKEN")
}

/// Remove every credential-shaped variable from a command's environment.
pub fn scrub_secrets(c: &mut std::process::Command) {
    for k in SECRET_ENV {
        c.env_remove(k);
    }
    for (k, _) in std::env::vars_os() {
        if k.to_str().is_some_and(is_secret_env) {
            c.env_remove(k);
        }
    }
}

pub struct Env {
    pub root: tempfile::TempDir,
    pub home: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Self {
        // Short base so the socket path stays under the 104-byte sun_path limit.
        let root = tempfile::Builder::new()
            .prefix("hcli")
            .tempdir_in("/tmp")
            .unwrap();
        let home = tempfile::Builder::new()
            .prefix("hhome")
            .tempdir_in("/tmp")
            .unwrap();
        Env { root, home }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn config_yaml(&self) -> String {
        std::fs::read_to_string(self.root().join("config.yaml")).unwrap_or_default()
    }

    pub fn socket(&self) -> PathBuf {
        self.root().join("daemon.sock")
    }

    pub fn std_command(&self) -> std::process::Command {
        let mut c = std::process::Command::new(assert_cmd::cargo::cargo_bin!("heard"));
        self.apply(&mut c);
        c
    }

    pub fn apply(&self, c: &mut std::process::Command) {
        c.env("HEARD_CLI_HOME", self.root())
            .env("HOME", self.home.path())
            .env("NO_COLOR", "1")
            .env("HEARD_NO_AUTOSTART", "1")
            .env_remove("HEARD_TEST_AUDIO")
            .env_remove("HEARD_CLI_MODELS_URL")
            .env_remove("HEARD_CLI_MODELS_MANIFEST")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME");
        scrub_secrets(c);
    }

    pub fn cmd(&self) -> assert_cmd::Command {
        assert_cmd::Command::from_std(self.std_command())
    }

    pub fn write_history(&self, lines: &[Value]) {
        let mut s = String::new();
        for l in lines {
            s.push_str(&l.to_string());
            s.push('\n');
        }
        std::fs::create_dir_all(self.root()).unwrap();
        std::fs::write(self.root().join("history.jsonl"), s).unwrap();
    }
}

/// A Unix-socket listener that records every frame and answers the
/// request-shaped ones.
pub struct FakeDaemon {
    pub frames: Arc<Mutex<Vec<Value>>>,
}

impl FakeDaemon {
    pub fn start(socket: &Path, status: Value) -> Self {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket).unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let rec = frames.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut buf = Vec::new();
                let _ = conn.read_to_end(&mut buf);
                if buf.is_empty() {
                    continue; // a liveness probe (connect + close)
                }
                let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
                    continue;
                };
                let reply = match v.get("cmd").and_then(Value::as_str) {
                    Some("status") => Some(status.clone()),
                    Some("mute_session") | Some("unmute_session") => Some(serde_json::json!({
                        "ok": true,
                        "session_id": v["session_id"],
                    })),
                    _ => None,
                };
                if let Some(r) = reply {
                    let _ = conn.write_all(r.to_string().as_bytes());
                    let _ = conn.shutdown(Shutdown::Write);
                }
                rec.lock().unwrap().push(v);
            }
        });
        FakeDaemon { frames }
    }

    /// Frames other than `status`, waiting briefly for stragglers.
    pub fn wait_frames(&self, want: usize) -> Vec<Value> {
        let t0 = Instant::now();
        loop {
            let f: Vec<Value> = self
                .frames
                .lock()
                .unwrap()
                .iter()
                .filter(|v| v.get("cmd").and_then(Value::as_str) != Some("status"))
                .cloned()
                .collect();
            if f.len() >= want || t0.elapsed() > Duration::from_secs(3) {
                return f;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

pub fn status_reply(sessions: Value) -> Value {
    serde_json::json!({
        "alive": true, "backend": "KokoroTTS", "persona": "jarvis",
        "narrate_tools": true, "muted": false, "last_error": null,
        "account_usage": null, "speaking": false, "queued": 0,
        "active_sessions": sessions, "router_mode": "solo",
        "agent_states": [], "langgraph_runs": [], "recap": "",
        "mission_agents": [], "pending_count": 0,
        "awaiting_resume_intent": false, "pending_update": null
    })
}

/// Loopback HTTP file server. Records each request's `Range` header ("" if
/// none). `honor_range=false` always answers 200 with the whole body.
pub struct FileServer {
    pub base_url: String,
    pub ranges: Arc<Mutex<Vec<String>>>,
}

impl FileServer {
    pub fn start(files: HashMap<String, Vec<u8>>, honor_range: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let rec = ranges.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut range = String::new();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("range:") {
                        range = v.trim().to_string();
                    }
                }
                rec.lock().unwrap().push(range.clone());
                let name = path.rsplit('/').next().unwrap_or("").to_string();
                let Some(body) = files.get(&name) else {
                    let _ = conn.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                let start = range
                    .strip_prefix("bytes=")
                    .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
                    .filter(|_| honor_range);
                let resp = match start {
                    Some(s) if s < body.len() => {
                        let part = &body[s..];
                        let mut r = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                            part.len(), s, body.len() - 1, body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(part);
                        r
                    }
                    Some(_) => "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_bytes().to_vec(),
                    None => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                };
                let _ = conn.write_all(&resp);
                let _ = conn.shutdown(Shutdown::Both);
            }
        });
        FileServer {
            base_url: format!("http://127.0.0.1:{port}/files/"),
            ranges,
        }
    }
}

/// Deterministic fake model bytes.
pub fn fake_bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
