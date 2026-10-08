//! A small HTTP/1.1 client for the daemon on loopback, standard library only.
//!
//! The shim and the `wh` command line each send a handful of JSON requests to one local port, so a
//! full HTTP stack is not worth its size. What matters is telling the failures apart the way the
//! Python clients do: a refused or unreachable connection means "no daemon, start one and retry once"
//! because the request never reached it; a timeout means "outcome unknown, do not retry"; any other
//! I/O error after the connection was made is reported as it is.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// How long to wait for a TCP connection to a daemon on this machine. A listening loopback port
/// accepts in well under a millisecond, and a CLOSED one takes about two seconds to say so on Windows
/// (the stack retries the SYN), so waiting any longer than this only slows down finding out that
/// nothing is running.
pub const LOOPBACK_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

/// How long to wait for a TCP connection to a daemon on another machine.
pub const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Time limits for one request.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// For making the connection.
    pub connect: Duration,
    /// For every read and write after it.
    pub io: Duration,
}

impl Timeouts {
    /// The connect limit for `host`: short for this machine, longer for another.
    pub fn for_host(host: &str, io: Duration) -> Self {
        let local = wh_core::config::LOOPBACK_ADDRESSES.contains(&host.to_lowercase().as_str());
        Self {
            connect: if local {
                LOOPBACK_CONNECT_TIMEOUT
            } else {
                REMOTE_CONNECT_TIMEOUT
            },
            io,
        }
    }
}

/// A daemon's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// The `Content-Type` header, "" when there was none.
    pub content_type: String,
    /// The body.
    pub body: Vec<u8>,
}

/// Why a request did not get an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The connection could not be made: nothing is listening (or the host is unreachable). The
    /// request was not sent, so retrying after starting the daemon cannot repeat an action.
    Unreachable(String),
    /// The daemon did not answer in time. The request may still be running.
    Timeout,
    /// The connection broke or the answer was not HTTP.
    Io(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unreachable(why) => write!(f, "connection failed: {why}"),
            ClientError::Timeout => f.write_str("timed out"),
            ClientError::Io(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ClientError {}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn io_error(e: io::Error) -> ClientError {
    if is_timeout(&e) {
        ClientError::Timeout
    } else {
        ClientError::Io(e.to_string())
    }
}

/// One read, retried when a signal cuts it short (`EINTR`, which Linux can raise on a socket with a
/// read timeout), so a signal landing in the process is not reported as a broken connection.
fn read_some(stream: &mut impl Read, buf: &mut [u8]) -> Result<usize, ClientError> {
    loop {
        match stream.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other.map_err(io_error),
        }
    }
}

/// One request, one answer, one connection. `token` is sent as `X-Winhands-Token`; `body` makes it a
/// JSON body with its length. `timeouts` bound the connection and then every read and write.
pub fn request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&[u8]>,
    timeouts: Timeouts,
) -> Result<Reply, ClientError> {
    let mut stream = connect(host, port, timeouts.connect)?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(timeouts.io))
        .and_then(|()| stream.set_write_timeout(Some(timeouts.io)))
        .map_err(io_error)?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nX-Winhands-Token: {token}\r\n\
         Content-Type: application/json\r\nConnection: close\r\n"
    );
    if let Some(b) = body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    if let Some(b) = body {
        out.extend_from_slice(b);
    }
    stream.write_all(&out).map_err(io_error)?;
    stream.flush().map_err(io_error)?;
    read_reply(&mut stream)
}

fn connect(host: &str, port: u16, limit: Duration) -> Result<TcpStream, ClientError> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| ClientError::Unreachable(e.to_string()))?;
    let mut last = String::from("no address");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, limit) {
            Ok(s) => return Ok(s),
            Err(e) => last = e.to_string(),
        }
    }
    Err(ClientError::Unreachable(last))
}

/// Read a whole HTTP response: status line, headers, then the body by `Content-Length`, by chunks, or
/// to the end of the stream.
fn read_reply(stream: &mut impl Read) -> Result<Reply, ClientError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        let n = read_some(stream, &mut chunk)?;
        if n == 0 {
            return Err(ClientError::Io(
                "the daemon closed the connection before answering".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ClientError::Io(format!("not an HTTP answer: {status_line:?}")))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut content_type = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "content-type" => content_type = value.to_string(),
            _ => {}
        }
    }
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    if chunked {
        body = read_chunked(stream, body)?;
    } else if let Some(want) = content_length {
        while body.len() < want {
            let n = read_some(stream, &mut chunk)?;
            if n == 0 {
                return Err(ClientError::Io(
                    "the daemon closed the connection mid-answer".into(),
                ));
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(want);
    } else {
        loop {
            let n = read_some(stream, &mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    Ok(Reply {
        status,
        content_type,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_chunked(stream: &mut impl Read, mut pending: Vec<u8>) -> Result<Vec<u8>, ClientError> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // the size line
        let line_end = loop {
            if let Some(i) = find(&pending, b"\r\n") {
                break i;
            }
            let n = read_some(stream, &mut chunk)?;
            if n == 0 {
                return Err(ClientError::Io("truncated chunked answer".into()));
            }
            pending.extend_from_slice(&chunk[..n]);
        };
        let size_text = String::from_utf8_lossy(&pending[..line_end]).to_string();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| ClientError::Io(format!("bad chunk size {size_text:?}")))?;
        pending.drain(..line_end + 2);
        if size == 0 {
            return Ok(out);
        }
        while pending.len() < size + 2 {
            let n = read_some(stream, &mut chunk)?;
            if n == 0 {
                return Err(ClientError::Io("truncated chunked answer".into()));
            }
            pending.extend_from_slice(&chunk[..n]);
        }
        out.extend_from_slice(&pending[..size]);
        pending.drain(..size + 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn quick(io_ms: u64) -> Timeouts {
        Timeouts {
            connect: Duration::from_millis(300),
            io: Duration::from_millis(io_ms),
        }
    }

    fn parse(raw: &[u8]) -> Result<Reply, ClientError> {
        read_reply(&mut Cursor::new(raw.to_vec()))
    }

    #[test]
    fn a_content_length_answer_is_read_exactly() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}EXTRA")
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type, "application/json");
        assert_eq!(r.body, b"{\"a\":1}");
    }

    #[test]
    fn an_answer_without_a_length_is_read_to_the_end() {
        let r = parse(b"HTTP/1.0 401 Unauthorized\r\n\r\nplain").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (401, &b"plain"[..]));
    }

    #[test]
    fn a_chunked_answer_is_joined() {
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4\r\ndefg\r\n0\r\n\r\n")
            .unwrap();
        assert_eq!(r.body, b"abcdefg");
    }

    #[test]
    fn a_truncated_answer_is_an_io_error_not_a_hang() {
        assert!(matches!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort"),
            Err(ClientError::Io(_))
        ));
        assert!(matches!(parse(b"HTTP/1.1 200"), Err(ClientError::Io(_))));
        assert!(matches!(parse(b"garbage\r\n\r\n"), Err(ClientError::Io(_))));
    }

    #[test]
    fn nothing_listening_is_unreachable() {
        // bind then drop to find a port that is free right now
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let t = std::time::Instant::now();
        let err = request("127.0.0.1", port, "GET", "/health", "t", None, quick(1000)).unwrap_err();
        assert!(matches!(err, ClientError::Unreachable(_)), "{err:?}");
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "a closed port must not cost the two seconds Windows takes to refuse: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn a_silent_server_is_a_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let keep = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(600));
            drop(s);
        });
        let err = request(
            "127.0.0.1",
            port,
            "POST",
            "/computer",
            "t",
            Some(b"{}"),
            quick(150),
        )
        .unwrap_err();
        assert_eq!(err, ClientError::Timeout);
        keep.join().unwrap();
    }

    #[test]
    fn the_request_carries_the_token_the_length_and_the_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !String::from_utf8_lossy(&got).contains("{\"x\":1}") {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        let r = request(
            "127.0.0.1",
            port,
            "POST",
            "/computer",
            "secret",
            Some(b"{\"x\":1}"),
            quick(2000),
        )
        .unwrap();
        assert_eq!(r.body, b"ok");
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /computer HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("X-Winhands-Token: secret\r\n"));
        assert!(seen.contains("Content-Length: 7\r\n"));
        assert!(seen.contains("Connection: close\r\n"));
        assert!(seen.ends_with("{\"x\":1}"));
    }
}
