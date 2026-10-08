//! `winhands daemon`: the HTTP server round [`wh_core::daemon::Daemon`] and the process start-up
//! (Python `daemon.main`).
//!
//! The server is synchronous: one accept loop, one thread per request, like the Python
//! `ThreadingHTTPServer`, so a slow call (a long OCR, a long wait) never blocks `/health` or
//! `/shutdown`. Anything that can move focus or send input takes the core's tool lock, so two agents
//! never interleave mouse and keyboard input.

use std::io::{self, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tiny_http::{Header, Response, Server};
use wh_core::backend::Backend;
use wh_core::clock::SystemClock;
use wh_core::config::Config;
use wh_core::daemon::{Daemon, Request, WarmGate, run_warmup};
use wh_core::orchestrate::Computer;
use wh_core::state::IDLE_POLL;
use wh_core::token;

/// A request body larger than this is cut off (a screenshot-sized JSON call is a few KB).
const MAX_BODY: u64 = 64 * 1024 * 1024;

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
fn answer(daemon: &Daemon, mut request: tiny_http::Request) -> bool {
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
            .take(MAX_BODY)
            .read_to_end(&mut body)
            .is_err()
    {
        // a client that hangs up mid-request is not worth a stack trace
        return false;
    }
    let reply = daemon.handle(&Request {
        method: &method,
        path: &path,
        token: token.as_deref(),
        body: &body,
        peer,
    });
    let shutdown = reply.shutdown;
    let mut response = Response::from_data(reply.body).with_status_code(reply.status);
    if let Some(h) = header("Content-Type", reply.content_type) {
        response = response.with_header(h);
    }
    // the client may be gone already (a killed shim, a timed-out call)
    let _ = request.respond(response);
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
            break;
        }
        let (daemon, server, stop) = (daemon.clone(), server.clone(), stop.clone());
        thread::Builder::new()
            .name("winhands-request".into())
            .spawn(move || {
                if answer(&daemon, request) {
                    stop.store(true, Ordering::SeqCst);
                    server.unblock();
                }
            })?;
    }
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
