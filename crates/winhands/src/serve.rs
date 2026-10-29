//! `winhands daemon`: the HTTP server round [`wh_core::daemon::Daemon`] and the process start-up
//! (Python `daemon.main`).
//!
//! A bounded desktop pool and a separate bounded control pool keep slow desktop work from using
//! every worker. Queues have deadlines: work that has expired never starts sending input.

use std::io::{self, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tiny_http::{Header, Response, Server};
use wh_core::backend::Backend;
use wh_core::clock::SystemClock;
use wh_core::config::Config;
use wh_core::daemon::{Daemon, Request, WarmGate, run_warmup};
use wh_core::orchestrate::Computer;
use wh_core::state::IDLE_POLL;
use wh_core::token;

/// A request body larger than this is cut off (a screenshot-sized JSON call is a few KB).
const MAX_BODY: u64 = 2 * 1024 * 1024;

const REQUEST_WORKERS: usize = 4;
const CONTROL_WORKERS: usize = 2;
const QUEUE_CAPACITY: usize = 8;
const LEGACY_REQUEST_BUDGET: Duration = Duration::from_secs(120);
const MAX_REQUEST_BUDGET: Duration = Duration::from_secs(600);

/// Version of the backwards-compatible Hands HTTP request contract.
pub const PROTOCOL_VERSION: u32 = 1;

/// Build provenance, shared by the CLI and authenticated health response.
pub fn build_info() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "source_commit": env!("HANDS_SOURCE_COMMIT"),
        "source_dirty": env!("HANDS_SOURCE_DIRTY"),
        "protocol_version": PROTOCOL_VERSION,
        "target": env!("HANDS_BUILD_TARGET"),
    })
}

struct QueuedRequest {
    request: tiny_http::Request,
    deadline: Instant,
}

struct Rejection {
    request: tiny_http::Request,
    status: u16,
    message: &'static str,
}

fn rejection_pool() -> io::Result<mpsc::SyncSender<Rejection>> {
    let (sender, receiver) = mpsc::sync_channel::<Rejection>(QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    for n in 0..2 {
        let receiver = receiver.clone();
        thread::Builder::new()
            .name(format!("hands-reject-{n}"))
            .spawn(move || {
                loop {
                    let item = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(item) = item else { break };
                    reject(item.request, item.status, item.message);
                }
            })?;
    }
    Ok(sender)
}

fn reject_later(
    sender: &mpsc::SyncSender<Rejection>,
    request: tiny_http::Request,
    status: u16,
    message: &'static str,
) {
    if let Err(error) = sender.try_send(Rejection {
        request,
        status,
        message,
    }) {
        let item = match error {
            mpsc::TrySendError::Full(item) | mpsc::TrySendError::Disconnected(item) => item,
        };
        // Explicitly abort the socket, without writing or draining an attacker-controlled body.
        item.request.abort();
    }
}

fn control_route(method: &str, path: &str) -> bool {
    (method == "GET" && path == "/health")
        || (method == "POST" && matches!(path.trim_matches('/'), "pause" | "resume" | "shutdown"))
}

fn request_deadline(value: Option<&str>, now: Instant, wall: SystemTime) -> Result<Instant, ()> {
    let budget = match value {
        Some(raw) => {
            let until = raw.parse::<u64>().map_err(|_| ())?;
            let elapsed = wall.duration_since(UNIX_EPOCH).map_err(|_| ())?;
            Duration::from_millis(until).saturating_sub(elapsed)
        }
        None => LEGACY_REQUEST_BUDGET,
    };
    Ok(now + budget.min(MAX_REQUEST_BUDGET))
}

fn reject(request: tiny_http::Request, status: u16, message: &str) {
    let body = if status == 401 {
        serde_json::json!({"error": message})
    } else {
        serde_json::json!({"ok": false, "error": message})
    };
    let response = Response::from_string(body.to_string()).with_status_code(status);
    let response = match header("Content-Type", "application/json") {
        Some(h) => response.with_header(h),
        None => response,
    };
    let _ = request.respond_and_close(response);
}

fn request_pool(
    name: &str,
    workers: usize,
    daemon: &Arc<Daemon>,
    server: &Arc<Server>,
    stop: &Arc<AtomicBool>,
) -> io::Result<mpsc::SyncSender<QueuedRequest>> {
    let (sender, receiver) = mpsc::sync_channel::<QueuedRequest>(QUEUE_CAPACITY);
    let receiver = Arc::new(Mutex::new(receiver));
    for n in 0..workers {
        let (receiver, daemon, server, stop) = (
            receiver.clone(),
            daemon.clone(),
            server.clone(),
            stop.clone(),
        );
        thread::Builder::new()
            .name(format!("hands-{name}-{n}"))
            .spawn(move || {
                loop {
                    let item = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(item) = item else { break };
                    if stop.load(Ordering::SeqCst) {
                        reject(
                            item.request,
                            503,
                            "daemon is stopping; request was not executed",
                        );
                        continue;
                    }
                    if answer(&daemon, item) {
                        stop.store(true, Ordering::SeqCst);
                        server.unblock();
                    }
                }
            })?;
    }
    Ok(sender)
}

/// Exit code: the config asked for a non-loopback bind without both opt-ins.
pub const EXIT_REMOTE_BIND_REFUSED: i32 = 4;

/// Exit code: the port is taken, another daemon is running.
pub const EXIT_PORT_TAKEN: i32 = 3;

/// What `winhands daemon` can be told.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Do not register the kill hotkey (tests, benchmarks).
    pub no_hotkey: bool,
    /// How often the idle thread looks (the daemon's own pace is 15 s).
    pub idle_poll: Duration,
    /// When the process started its own work; the listening line says how long set-up took.
    pub started: Instant,
    /// Write the daemon's messages to this file (appended, rotated at 1 MB) instead of stdout and
    /// stderr. `winhandsd` has no console, so its messages would go nowhere otherwise.
    pub log: Option<PathBuf>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            no_hotkey: false,
            idle_poll: IDLE_POLL,
            started: Instant::now(),
            log: None,
        }
    }
}

impl DaemonOptions {
    /// An ordinary message: stdout, or the log file when there is one.
    pub fn out(&self, line: &str) {
        match &self.log {
            Some(path) => append_log(path, line),
            None => println!("{line}"),
        }
    }

    /// A problem: stderr, or the log file when there is one.
    pub fn err(&self, line: &str) {
        match &self.log {
            Some(path) => append_log(path, line),
            None => eprintln!("{line}"),
        }
    }
}

/// Append one line, stamped with the Unix time in seconds, to the daemon log. Never fails: a log that
/// cannot be written is not worth stopping the daemon for.
pub fn append_log(path: &Path, line: &str) {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    wh_core::audit::rotate(path, 1_000_000);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[{secs}] {line}");
    }
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

/// Answer one request on the connection it arrived on. Returns true when the daemon should stop.
fn answer(daemon: &Daemon, item: QueuedRequest) -> bool {
    let QueuedRequest {
        mut request,
        deadline,
    } = item;
    if Instant::now() >= deadline {
        reject(request, 408, "request deadline expired before execution");
        return false;
    }
    let method = request.method().as_str().to_string();
    let path = request.url().to_string();
    let token = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("X-Winhands-Token"))
        .map(|h| h.value.as_str().to_string());
    let peer = request.remote_addr().copied();
    // A caller without the token gets its 401 without the daemon reading what it sent.
    let mut body = Vec::new();
    if daemon.is_authorised(token.as_deref())
        && request
            .as_reader()
            .take(MAX_BODY + 1)
            .read_to_end(&mut body)
            .is_err()
    {
        // a client that hangs up mid-request is not worth a stack trace
        request.abort();
        return false;
    }
    if body.len() as u64 > MAX_BODY {
        reject(request, 413, "request body exceeds 2 MiB");
        return false;
    }
    if Instant::now() >= deadline {
        reject(
            request,
            408,
            "request deadline expired while reading its body",
        );
        return false;
    }
    let mut reply = daemon.handle_until(
        &Request {
            method: &method,
            path: &path,
            token: token.as_deref(),
            body: &body,
            peer,
        },
        deadline,
    );
    if method == "GET"
        && path == "/health"
        && reply.status == 200
        && let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&reply.body)
        && let Some(object) = value.as_object_mut()
    {
        object.insert("build".into(), build_info());
        if let Ok(body) = serde_json::to_vec(&value) {
            reply.body = body;
        }
    }
    let shutdown = reply.shutdown;
    let mut response = Response::from_data(reply.body).with_status_code(reply.status);
    if let Some(h) = header("Content-Type", reply.content_type) {
        response = response.with_header(h);
    }
    // the client may be gone already (a killed shim, a timed-out call)
    let _ = request.respond_and_close(response);
    shutdown
}

/// Serve `daemon` on `listener` until a `/shutdown` request, the idle timeout, or `stop` being set.
/// Returns when the accept loop has ended; requests still running are abandoned, as in Python.
pub fn serve(
    listener: TcpListener,
    daemon: Arc<Daemon>,
    stop: Arc<AtomicBool>,
    idle_poll: Duration,
) -> io::Result<()> {
    let server = Arc::new(Server::from_listener(listener, None).map_err(io::Error::other)?);
    let requests = request_pool("request", REQUEST_WORKERS, &daemon, &server, &stop)?;
    let controls = request_pool("control", CONTROL_WORKERS, &daemon, &server, &stop)?;
    let rejections = rejection_pool()?;
    {
        // Exit when no request has arrived for the idle timeout. winhands starts on demand and goes
        // away again, because it is not meant to be resident; the shim starts it on the next call.
        let (daemon, server, stop) = (daemon.clone(), server.clone(), stop.clone());
        thread::Builder::new()
            .name("winhands-idle".into())
            .spawn(move || {
                loop {
                    sleep_unless(&stop, idle_poll);
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if daemon.idle_expired() {
                        let secs = daemon.computer().config().idle_timeout_seconds();
                        println!("winhands idle for {secs}s, shutting down");
                        stop.store(true, Ordering::SeqCst);
                        server.unblock();
                        return;
                    }
                }
            })?;
    }
    for request in server.incoming_requests() {
        if stop.load(Ordering::SeqCst) {
            request.abort();
            break;
        }
        let token = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("X-Winhands-Token"))
            .map(|h| h.value.as_str());
        let auth_required = request.method().as_str() == "POST"
            || (request.method().as_str() == "GET" && request.url() == "/health");
        if auth_required && !daemon.is_authorised(token) {
            reject_later(&rejections, request, 401, "unauthorized");
            continue;
        }
        if request.body_length().is_some_and(|n| n as u64 > MAX_BODY) {
            reject_later(&rejections, request, 413, "request body exceeds 2 MiB");
            continue;
        }
        let raw_deadline = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("X-Hands-Deadline-Unix-Ms"))
            .map(|h| h.value.as_str());
        let deadline = match request_deadline(raw_deadline, Instant::now(), SystemTime::now()) {
            Ok(deadline) => deadline,
            Err(()) => {
                reject_later(
                    &rejections,
                    request,
                    400,
                    "invalid X-Hands-Deadline-Unix-Ms header",
                );
                continue;
            }
        };
        let pool = if control_route(request.method().as_str(), request.url()) {
            &controls
        } else {
            &requests
        };
        if let Err(error) = pool.try_send(QueuedRequest { request, deadline }) {
            let item = match error {
                mpsc::TrySendError::Full(item) | mpsc::TrySendError::Disconnected(item) => item,
            };
            reject_later(
                &rejections,
                item.request,
                503,
                "request capacity exhausted; request was not executed",
            );
        }
    }
    stop.store(true, Ordering::SeqCst);
    Ok(())
}

/// Sleep `total`, in short slices, returning early once `stop` is set.
fn sleep_unless(stop: &AtomicBool, total: Duration) {
    let slice = Duration::from_millis(50).min(total);
    let mut left = total;
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let step = slice.min(left);
        thread::sleep(step);
        left = left.saturating_sub(step);
    }
}

/// Bind the loopback port. The port is bound BEFORE the token file is touched: a second daemon used to
/// rewrite the token, then fail to bind because the first was still running, and the live daemon kept
/// the old token, so every call returned "unauthorized" until someone killed everything.
pub fn bind(host: &str, port: u16) -> io::Result<TcpListener> {
    TcpListener::bind((host, port))
}

/// Run the daemon in this process: set up, serve, clean up. Returns the exit code. Python
/// `daemon.main`.
pub fn run_daemon(backend: Arc<dyn Backend>, cfg: Arc<Config>, opts: &DaemonOptions) -> i32 {
    let port = cfg.port();
    let host = match cfg.bind_host() {
        Ok(h) => h,
        Err(refused) => {
            opts.err(&format!("winhands: {refused}"));
            return EXIT_REMOTE_BIND_REFUSED;
        }
    };
    if let Err(e) = backend.init() {
        opts.err(&format!("winhands: backend setup failed: {e}"));
    }
    // Warm the OCR path from the very start: it overlaps the bind, token and hotkey set-up below
    // instead of starting after them (the first read_text used to pay seconds for it).
    let warm = Arc::new(WarmGate::new());
    warm.begin();
    {
        let (warm, backend) = (warm.clone(), backend.clone());
        let _ = thread::Builder::new()
            .name("winhands-warmup".into())
            .spawn(move || run_warmup(&warm, backend.as_ref()));
    }
    let listener = match bind(&host, port) {
        Ok(l) => l,
        Err(e) => {
            opts.err(&format!(
                "winhands: cannot bind {host}:{port} ({e}); another daemon is running, leaving \
                 its token untouched and exiting"
            ));
            return EXIT_PORT_TAKEN;
        }
    };
    let bound_port = listener.local_addr().map_or(port, |a| a.port());
    let paths = cfg.paths().clone();
    let token = match token::write_token(&paths, cfg.env()) {
        Ok(t) => t,
        Err(e) => {
            opts.err(&format!(
                "winhands: cannot write the token file {}: {e}",
                paths.token().display()
            ));
            return 1;
        }
    };
    let remote = cfg.remote_caller();
    let remote_bind = cfg.bind_is_remote();
    let idle_secs = cfg.idle_timeout_seconds();
    let protected = cfg.protected_processes();
    let computer = Arc::new(Computer::new(
        backend.clone(),
        Arc::new(SystemClock::new()),
        cfg,
    ));
    let daemon = Arc::new(Daemon::new(computer, token, bound_port).with_warm_gate(warm));
    write_pid(&paths.pid());
    if !opts.no_hotkey {
        match daemon.install_kill_hotkey() {
            Some(name) => opts.err(&format!("winhands: kill hotkey registered: {name}")),
            None => {
                opts.err("winhands: no kill hotkey could be registered; pause feature disabled")
            }
        }
    }
    if !wh_core::config::LOOPBACK_ADDRESSES.contains(&host.to_lowercase().as_str()) {
        opts.out(&format!(
            "winhands: WARNING listening on {host}, not loopback. Anything that can reach \
             {host}:{bound_port} and read the token can drive this desktop. See SECURITY.md."
        ));
    }
    if remote {
        let why = if remote_bind {
            "bind is not loopback"
        } else {
            "remote_caller is set"
        };
        let names: Vec<String> = protected.into_iter().collect();
        let list = if names.is_empty() {
            "empty".to_string()
        } else {
            names.join(", ")
        };
        opts.out(&format!(
            "winhands: WARNING remote caller mode ({why}). The calling process cannot be \
             identified from this machine, so the caller-relative guard is OFF: winhands will \
             not refuse the windows of whatever is driving it. Only protect_processes ({list}) \
             is enforced. See the \"Remote mode\" section of SECURITY.md."
        ));
    }
    let elevated = backend
        .own_process()
        .ok()
        .flatten()
        .is_some_and(|p| p.elevated);
    opts.out(&format!(
        "winhands listening on {host}:{bound_port} elevated={elevated} idle_shutdown={idle_secs}s (ready in {} ms)",
        opts.started.elapsed().as_millis()
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let result = serve(listener, daemon, stop, opts.idle_poll);
    let _ = std::fs::remove_file(paths.pid());
    match result {
        Ok(()) => 0,
        Err(e) => {
            opts.err(&format!("winhands: server stopped: {e}"));
            1
        }
    }
}

fn write_pid(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, std::process::id().to_string());
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    fn control_lane_contains_only_health_and_control_commands() {
        assert!(control_route("GET", "/health"));
        for path in ["/pause", "/resume", "/shutdown", "/pause/"] {
            assert!(control_route("POST", path));
        }
        assert!(!control_route("POST", "/computer"));
        assert!(!control_route("POST", "/batch"));
        assert!(!control_route("GET", "/pause"));
        assert!(!control_route("POST", "/health"));
    }

    #[test]
    fn deadlines_are_monotonic_bounded_and_legacy_compatible() {
        let now = Instant::now();
        let wall = UNIX_EPOCH + Duration::from_millis(1_000);
        assert_eq!(
            request_deadline(None, now, wall).unwrap(),
            now + LEGACY_REQUEST_BUDGET
        );
        assert_eq!(
            request_deadline(Some("1100"), now, wall).unwrap(),
            now + Duration::from_millis(100)
        );
        assert_eq!(request_deadline(Some("999"), now, wall).unwrap(), now);
        assert_eq!(
            request_deadline(Some("9999999999"), now, wall).unwrap(),
            now + MAX_REQUEST_BUDGET
        );
        assert!(request_deadline(Some("-1"), now, wall).is_err());
        assert!(request_deadline(Some("tomorrow"), now, wall).is_err());
    }

    #[test]
    fn build_identity_reports_protocol_and_source_explicitly() {
        let info = build_info();
        assert_eq!(info["protocol_version"], 1);
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        assert!(!info["source_commit"].as_str().unwrap().is_empty());
    }
}
