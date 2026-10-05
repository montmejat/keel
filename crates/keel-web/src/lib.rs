//! `keel web`: a page in the browser over the control API. The same API
//! `keel top` and `keel-mcp` use, through a small HTTP server written on
//! `std::net`, like keel's other protocols.
//!
//! - `GET /` is the page, one file.
//! - `GET /api/events` is the stream (server-sent events: plain HTTP, no
//!   handshake, and the browser reconnects by itself): `status`, `logs` and
//!   `latency`, as the control API's `subscribe` sends them.
//! - `GET /api/dataflows` and `GET /api/actions` read.
//! - `POST /api/approve`, `deny`, `restart` and `stop` act, as a person.
//!
//! What it must not become is a way for another website to approve what an
//! agent asked: the page is what stands between an agent and the machine. So
//! it listens on the loopback address, refuses requests whose `Host` isn't
//! this server's (DNS rebinding), and wants a random token, printed in the URL
//! at start. Reads take it as a query (an `EventSource` can't set headers);
//! a `POST` takes it as a header only, which a page on another origin can't
//! send without a preflight this server doesn't answer.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use keel_daemon::control::{self, Client, NodeState, Reply};
use serde_json::{json, Value};

const PAGE: &str = include_str!("index.html");

/// Longest request line or header line we read.
const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7600";

struct Server {
    token: String,
    addr: SocketAddr,
    /// The daemon to talk to, or the only one running.
    pid: Option<u32>,
}

/// Serves until killed. Prints the URL to open, token included.
pub fn run(listen: &str, pid: Option<u32>) -> io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        eprintln!("keel web: {addr} is not a loopback address: anyone who can reach it and has the token can act as you");
    }
    let server = Arc::new(Server { token: new_token()?, addr, pid });
    println!("keel web: http://{addr}/#{}", server.token);
    for stream in listener.incoming().flatten() {
        let server = server.clone();
        std::thread::spawn(move || {
            let _ = serve(stream, &server);
        });
    }
    Ok(())
}

fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    host: Option<String>,
    token_header: Option<String>,
}

fn read_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    if reader.take(MAX_LINE).read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(line.trim_end().to_owned())
}

fn parse(stream: &TcpStream) -> io::Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream);
    let first = read_line(&mut reader)?;
    let mut parts = first.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::other("not an HTTP request"));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = (query.split('&'))
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let (mut host, mut token_header) = (None, None);
    for _ in 0..MAX_HEADERS {
        let line = read_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "host" => host = Some(value.trim().to_owned()),
                "x-keel-token" => token_header = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    stream.set_read_timeout(None)?;
    Ok(Request { method: method.to_owned(), path: path.to_owned(), query, host, token_header })
}

/// `%41` and `+`, as a query carries them.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Equal, without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn json_reply(stream: &mut TcpStream, status: &str, value: Value) -> io::Result<()> {
    respond(stream, status, "application/json", &value.to_string())
}

fn serve(mut stream: TcpStream, server: &Server) -> io::Result<()> {
    let request = parse(&stream)?;
    // Only this server's own name: a page on another site that points its own
    // name at this address (DNS rebinding) sends that name as Host.
    if server.addr.ip().is_loopback() {
        let port = server.addr.port();
        let ok = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
        if !request.host.as_ref().is_some_and(|h| ok.contains(h)) {
            return respond(&mut stream, "421 Misdirected Request", "text/plain", "wrong host\n");
        }
    }
    if request.path == "/" && request.method == "GET" {
        // The page is the same for everyone, and holds no secret.
        return respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE);
    }
    if !request.path.starts_with("/api/") {
        return respond(&mut stream, "404 Not Found", "text/plain", "not found\n");
    }
    let acting = request.method == "POST";
    let token = match acting {
        true => request.token_header.clone(),
        false => request.query.iter().find(|(k, _)| k == "token").map(|(_, v)| v.clone()),
    };
    if !token.is_some_and(|t| same(&t, &server.token)) {
        return json_reply(&mut stream, "401 Unauthorized", json!({"error": "open the URL `keel web` printed, token included"}));
    }
    if !acting && request.method != "GET" {
        return json_reply(&mut stream, "405 Method Not Allowed", json!({"error": "method not allowed"}));
    }
    let arg = |name: &str| request.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let outcome = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/events") => return events(&mut stream, server),
        ("GET", "/api/dataflows") => Ok(dataflows()),
        ("GET", "/api/actions") => actions(server),
        ("POST", "/api/approve") => decide(server, arg("id"), true),
        ("POST", "/api/deny") => decide(server, arg("id"), false),
        ("POST", "/api/restart") => with_client(server, |c| match arg("node") {
            Some(node) => c.restart(node).map(|r| outcome(&r)),
            None => Err(io::Error::other("`node` is required")),
        }),
        ("POST", "/api/stop") => with_client(server, |c| c.stop().map(|_| json!({"outcome": "stopping"}))),
        _ => return json_reply(&mut stream, "404 Not Found", json!({"error": "no such endpoint"})),
    };
    match outcome {
        Ok(value) => json_reply(&mut stream, "200 OK", value),
        Err(e) => json_reply(&mut stream, "400 Bad Request", json!({"error": e.to_string()})),
    }
}

fn outcome(reply: &Reply) -> Value {
    let text = match reply {
        Reply::Stopping => "stopping".to_owned(),
        Reply::Restarted(node) => format!("`{node}` was killed and starts again"),
        Reply::Updating(nodes) => format!("replacing {}", nodes.join(", ")),
        other => format!("{other:?}"),
    };
    json!({"outcome": text})
}

fn with_client(server: &Server, f: impl FnOnce(&mut Client) -> io::Result<Value>) -> io::Result<Value> {
    let pid = control::pick(server.pid).map_err(io::Error::other)?;
    f(&mut Client::connect(pid)?)
}

fn actions(server: &Server) -> io::Result<Value> {
    with_client(server, |c| Ok(serde_json::to_value(c.actions()?)?))
}

fn decide(server: &Server, id: Option<&str>, approve: bool) -> io::Result<Value> {
    let id: u64 = id.and_then(|i| i.parse().ok()).ok_or_else(|| io::Error::other("`id` is required"))?;
    with_client(server, |c| match approve {
        true => c.approve(id).map(|r| outcome(&r)),
        false => c.deny(id).map(|_| json!({"outcome": "denied"})),
    })
}

fn dataflows() -> Value {
    let rows: Vec<Value> = (control::running().into_iter())
        .map(|(pid, status)| {
            let running = status.nodes.iter().filter(|n| n.state == NodeState::Running).count();
            json!({"pid": pid, "uptime_ms": status.uptime_ms, "nodes_running": running,
                   "nodes": status.nodes.len(), "machine": status.machine,
                   "coordinator": status.coordinator, "dataflow": status.dataflow})
        })
        .collect();
    json!(rows)
}

/// The control API's stream, as server-sent events: until the daemon is gone
/// or the browser is.
fn events(stream: &mut TcpStream, server: &Server) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nretry: 1000\n\n"
    )?;
    let subscription = control::pick(server.pid)
        .map_err(io::Error::other)
        .and_then(|pid| Client::connect(pid)?.subscribe(Duration::from_millis(250)));
    let mut subscription = match subscription {
        Ok(s) => s,
        Err(e) => {
            // Nothing to show yet: the browser asks again in a second.
            return write!(stream, "event: waiting\ndata: {}\n\n", json!({"reason": e.to_string()}));
        }
    };
    loop {
        let value = serde_json::to_value(subscription.next_event()?)?;
        let Some((name, data)) = value.as_object().and_then(|o| o.iter().next()) else { continue };
        write!(stream, "event: {name}\ndata: {data}\n\n")?;
    }
}
