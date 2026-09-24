//! A one-request HTTP server, hand-rolled over [`std::net::TcpListener`].
//!
//! No `tiny_http`, no `wiremock`: the whole point of these tests is to assert
//! on the *exact bytes* the backend puts on the wire — request line, header
//! casing, body — against what `engine/heard/tts/*.py` sends. A framework that
//! parses the request into its own model would hide precisely the differences
//! worth catching, and would be one more dependency for a crate whose entire
//! HTTP surface is one POST.
//!
//! Bound to `127.0.0.1:0`, so every test gets its own ephemeral port and the
//! suite runs in parallel without a fixed port to collide on. Nothing here
//! reaches the network: `api.elevenlabs.io` is never resolved, and every credential in these tests is a fake like `sk_test_fake`.

#![allow(dead_code)] // each test binary uses a different subset

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

/// One request, as the fake server saw it.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    /// e.g. `POST /v1/synth HTTP/1.1`.
    pub request_line: String,
    /// Header names lowercased, values as sent.
    pub headers: Vec<(String, String)>,
    /// The body bytes.
    pub body: Vec<u8>,
}

impl CapturedRequest {
    /// The HTTP method.
    pub fn method(&self) -> &str {
        self.request_line.split(' ').next().unwrap_or("")
    }

    /// The request target, including any query string.
    pub fn target(&self) -> &str {
        self.request_line.split(' ').nth(1).unwrap_or("")
    }

    /// The path, without the query string.
    pub fn path(&self) -> &str {
        self.target().split('?').next().unwrap_or("")
    }

    /// The query string, without the `?`.
    pub fn query(&self) -> &str {
        self.target().split_once('?').map_or("", |(_, q)| q)
    }

    /// A header's value, looked up case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == want)
            .map(|(_, v)| v.as_str())
    }

    /// The body as UTF-8.
    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body parsed as JSON.
    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body should be JSON")
    }
}

/// What the fake server answers with.
pub struct Reply {
    /// HTTP status code.
    pub status: u16,
    /// The `Content-Type` header value.
    pub content_type: &'static str,
    /// The response body.
    pub body: Vec<u8>,
}

impl Reply {
    /// `200 OK` with `audio/mpeg` — a successful synth.
    pub fn audio(bytes: &[u8]) -> Self {
        Self {
            status: 200,
            content_type: "audio/mpeg",
            body: bytes.to_vec(),
        }
    }

    /// A non-2xx with a JSON body, the shape an HTTP backend uses for errors.
    pub fn json_error(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }

    /// A non-2xx with a body that is not JSON at all — Cloudflare's HTML
    /// error pages, for one.
    pub fn text_error(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/html",
            body: body.as_bytes().to_vec(),
        }
    }
}

/// A server that accepts exactly one request, answers it, and stops.
pub struct FakeServer {
    base_url: String,
    rx: mpsc::Receiver<CapturedRequest>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FakeServer {
    /// Start one, answering the single request it gets with `reply`.
    pub fn start(reply: Reply) -> Self {
        Self::start_with(move |_req| reply)
    }

    /// Start one whose reply may depend on the request.
    pub fn start_with<F>(responder: F) -> Self
    where
        F: FnOnce(&CapturedRequest) -> Reply + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let Some(req) = read_request(&stream) else {
                return;
            };
            let reply = responder(&req);
            let _ = tx.send(req);
            let _ = write_reply(&stream, &reply);
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            rx,
            handle: Some(handle),
        }
    }

    /// Start one that accepts a connection and then drops it without replying
    /// — the transport-failure case (`URLError` on the Python side).
    pub fn start_hanging_up() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let (_tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = read_request(&stream);
                drop(stream); // close without a response
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            rx,
            handle: Some(handle),
        }
    }

    /// The `http://127.0.0.1:PORT` base to point a backend at.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The request the server saw. Panics if none arrived.
    pub fn captured(mut self) -> CapturedRequest {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.rx
            .try_recv()
            .expect("the backend should have sent exactly one request")
    }
}

fn read_request(stream: &TcpStream) -> Option<CapturedRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let request_line = request_line.trim_end().to_string();
    if request_line.is_empty() {
        return None;
    }

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    Some(CapturedRequest {
        request_line,
        headers,
        body,
    })
}

fn write_reply(mut stream: &TcpStream, reply: &Reply) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        402 => "Payment Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
