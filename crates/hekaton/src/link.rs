//! The connection from a client (the MCP shim, `hk`, `hekaton status`) to the daemon: fresh token on
//! every request, one retry when the daemon restarted under it, and an on-demand start with one retry
//! when nothing is listening (Python `shim._call`, `shim._try_start_daemon`, `hk.call`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hekaton_core::paths::Paths;
use hekaton_core::token::read_token;
use serde_json::Value;

use crate::client::{self, ClientError, Reply, Timeouts};
use crate::start::Starter;

/// The MCP shim's per-request timeout (`REQUEST_TIMEOUT_S`).
pub const SHIM_TIMEOUT: Duration = Duration::from_secs(120);

/// `hk`'s per-request timeout.
pub const HK_TIMEOUT: Duration = Duration::from_secs(130);

/// A start attempt is made at most this often, so a burst of failing calls does not spawn a storm.
pub const START_RATE_LIMIT: Duration = Duration::from_secs(30);

/// How long a started daemon gets to answer `/health`.
pub const START_WAIT: Duration = Duration::from_secs(25);

/// How often `/health` is polled while a started daemon comes up.
const START_POLL: Duration = Duration::from_millis(25);

/// How long each of those polls waits for a connection.
const START_POLL_CONNECT: Duration = Duration::from_millis(100);

/// Waits before re-sending a request the daemon turned away unexecuted: its queue was full, or
/// another caller held the desktop. Short and steady (a held desktop frees within a call or two),
/// about 5 s in all.
const BUSY_RETRIES: [Duration; 36] = {
    let mut w = [Duration::from_millis(150); 36];
    w[0] = Duration::from_millis(30);
    w[1] = Duration::from_millis(50);
    w[2] = Duration::from_millis(80);
    w[3] = Duration::from_millis(120);
    w
};

/// A reply that says the request never ran, so re-sending cannot repeat an action: the 503 for a
/// full queue, or an acting call refused while another caller's call held the desktop
/// (2026-10-29 stress: 8 clients typing at once, 7 got "desktop busy" straight back; the daemon
/// still never makes an acting call wait, so input never lands long after it was asked for).
fn turned_away_unexecuted(reply: &Reply) -> bool {
    let body = String::from_utf8_lossy(&reply.body);
    (reply.status == 503 && body.contains("request was not executed"))
        || body.contains("desktop busy; no input sent")
}

/// Why a call produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The daemon did not answer in time; the work may still be running. Never retried.
    Timeout,
    /// Nothing answers and it could not be started (or the start was already tried a moment ago).
    NoDaemon,
    /// The connection broke after it was made.
    Io(String),
}

/// A client's line to one daemon.
pub struct DaemonLink {
    host: String,
    port: u16,
    paths: Paths,
    /// `None` for a daemon on another machine: it is not ours to start.
    starter: Option<Starter>,
    io_timeout: Duration,
    last_start: Mutex<Option<Instant>>,
    notice: Option<String>,
    start_wait: Duration,
}

impl DaemonLink {
    /// A link to `host:port`. The token is read from `paths` on every request.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        paths: Paths,
        starter: Option<Starter>,
        io_timeout: Duration,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            paths,
            starter,
            io_timeout,
            last_start: Mutex::new(None),
            notice: None,
            start_wait: START_WAIT,
        }
    }

    /// How long a started daemon gets to answer `/health` (default 25 s; `daemon_wait_s`).
    pub fn with_start_wait(mut self, wait: Duration) -> Self {
        self.start_wait = wait;
        self
    }

    /// Print `message` on stderr just before a daemon start is attempted (the `hk` command line says
    /// what it is doing; the MCP shim, whose stderr nobody reads, says nothing).
    pub fn with_start_notice(mut self, message: impl Into<String>) -> Self {
        self.notice = Some(message.into());
        self
    }

    /// `http://host:port`, for messages.
    pub fn base(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// The daemon's host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The daemon's port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// How a missing daemon would be started, if it can be.
    pub fn starter(&self) -> Option<&Starter> {
        self.starter.as_ref()
    }

    /// The paths this link reads its token from.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    fn token(&self) -> String {
        read_token(&self.paths)
    }

    fn once(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        token: &str,
    ) -> Result<Reply, ClientError> {
        client::request(
            &self.host,
            self.port,
            method,
            path,
            token,
            body,
            Timeouts::for_host(&self.host, self.io_timeout),
        )
    }

    /// One request with the token as it is on disk right now. When the daemon answers 401 and the
    /// token file changed since the request was built, the daemon restarted between reading the token
    /// and the call and rotated it: send once more with the new one.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Reply, ClientError> {
        let sent = self.token();
        let reply = self.once(method, path, body, &sent)?;
        if reply.status == 401 {
            let now = self.token();
            if !now.is_empty() && now != sent {
                return self.once(method, path, body, &now);
            }
        }
        Ok(reply)
    }

    /// `GET /health` as JSON, `None` unless the daemon answered 200 with an object.
    pub fn health(&self, timeout: Duration) -> Option<Value> {
        self.health_with(Timeouts::for_host(&self.host, timeout))
    }

    fn health_with(&self, timeouts: Timeouts) -> Option<Value> {
        let reply = client::request(
            &self.host,
            self.port,
            "GET",
            "/health",
            &self.token(),
            None,
            timeouts,
        )
        .ok()?;
        if reply.status != 200 {
            return None;
        }
        serde_json::from_slice::<Value>(&reply.body)
            .ok()
            .filter(Value::is_object)
    }

    fn can_try_start(&self) -> bool {
        let mut last = self.last_start.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < START_RATE_LIMIT) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    /// Start the daemon and wait for `/health`. True when it is (already) up. Rate limited; a daemon
    /// on another machine is never started.
    pub fn start_and_wait(&self, wait: Duration) -> bool {
        if self.health(Duration::from_secs(2)).is_some() {
            return true;
        }
        self.start_and_poll(wait)
    }

    /// [`DaemonLink::start_and_wait`] for a caller that has just seen nobody answer.
    fn start_and_poll(&self, wait: Duration) -> bool {
        let Some(starter) = &self.starter else {
            return false;
        };
        if !self.can_try_start() {
            return false;
        }
        if let Some(message) = &self.notice {
            eprintln!("{message}");
        }
        let started = starter.start(&self.paths.daemon_log());
        // A failing start command is also what "already running" looks like while another caller's
        // start is still binding the port, so poll anyway, just not for long.
        let wait = if started.is_err() {
            wait.min(Duration::from_secs(5))
        } else {
            wait
        };
        // While the daemon comes up, a closed port can take two seconds to refuse on Windows, which
        // would make every poll as slow as that. Give up on a connection after 100 ms and ask again.
        let mut poll = Timeouts::for_host(&self.host, Duration::from_secs(2));
        poll.connect = poll.connect.min(START_POLL_CONNECT);
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if self.health_with(poll).is_some() {
                return true;
            }
            std::thread::sleep(START_POLL);
        }
        false
    }

    /// Call the daemon. Nothing listening means it is not running (never started, or it exited after
    /// its idle timeout): start it and retry ONCE. That first try never reached the daemon, so the
    /// retry cannot repeat an action. A timeout means the outcome is unknown: nothing is started and
    /// nothing is retried.
    pub fn call(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Reply, Failure> {
        // 2026-10-28 Mac stress test: 16 parallel screenshots got four 503s straight back to the
        // model. A full queue clears in milliseconds, so wait a little and send again.
        let mut reply = self.call_once(method, path, body);
        for wait in BUSY_RETRIES {
            match &reply {
                Ok(r) if turned_away_unexecuted(r) => {
                    std::thread::sleep(wait);
                    reply = self.call_once(method, path, body);
                }
                _ => break,
            }
        }
        reply
    }

    fn call_once(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Reply, Failure> {
        match self.send(method, path, body) {
            Ok(reply) => return Ok(reply),
            Err(ClientError::Timeout) => return Err(Failure::Timeout),
            Err(ClientError::Io(why)) => return Err(Failure::Io(why)),
            Err(ClientError::Unreachable(_)) => {}
        }
        if self.start_and_poll(self.start_wait) {
            return match self.send(method, path, body) {
                Ok(reply) => Ok(reply),
                Err(ClientError::Timeout) => Err(Failure::Timeout),
                Err(ClientError::Io(why)) => Err(Failure::Io(why)),
                Err(ClientError::Unreachable(_)) => Err(Failure::NoDaemon),
            };
        }
        Err(Failure::NoDaemon)
    }

    /// POST a JSON payload to `/{path}`.
    pub fn post(&self, path: &str, payload: &Value) -> Result<Reply, Failure> {
        let body = serde_json::to_vec(payload).unwrap_or_default();
        self.call("POST", &format!("/{path}"), Some(&body))
    }

    /// What to tell a person when no daemon answers.
    pub fn start_hint(&self) -> String {
        match &self.starter {
            Some(s @ Starter::Task(_)) => {
                format!(
                    "{} (first-time setup: run setup.ps1 as administrator)",
                    s.describe()
                )
            }
            Some(s) => s.describe(),
            None => format!("check HEKATON_CONNECT_HOST ({})", self.base()),
        }
    }
}

#[cfg(test)]
mod busy_tests {
    use super::*;

    fn reply(status: u16, body: &str) -> Reply {
        Reply {
            status,
            content_type: "application/json".into(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn only_an_unexecuted_full_queue_reply_is_retried() {
        assert!(turned_away_unexecuted(&reply(
            503,
            r#"{"ok":false,"error":"request capacity exhausted; request was not executed"}"#
        )));
        assert!(!turned_away_unexecuted(&reply(
            503,
            r#"{"ok":false,"error":"shutting down"}"#
        )));
        assert!(!turned_away_unexecuted(&reply(
            400,
            "request was not executed"
        )));
        assert!(turned_away_unexecuted(&reply(
            200,
            r#"{"ok":false,"error":"desktop busy; no input sent; wait for current request before retrying"}"#
        )));
        assert!(!turned_away_unexecuted(&reply(
            200,
            r#"{"ok":false,"error":"refused: busy"}"#
        )));
    }
}
