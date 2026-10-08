//! The HTTP daemon's brain (Python `winhands/daemon.py`), without the sockets.
//!
//! [`Daemon::handle`] takes one request (method, path, the `X-Winhands-Token` header, the body, who is
//! on the other end) and returns the status, content type and body to send back. The `winhands` binary
//! puts an HTTP server round it; because nothing here touches a socket, every route, status code and
//! error text is tested on every OS against the fake backend.
//!
//! What it speaks, so the Python shim and `wh` work against it unchanged:
//!
//! | Route | Behaviour |
//! |---|---|
//! | `GET /health` | token required; `elevated`, `paused`, `version`, `hotkey`, `idle_*`, `remote_*`, `caller_guard`, `ocr` |
//! | `POST /computer` | [`Computer::execute`]: pause, acknowledgement, rate cap, one action at a time, audit |
//! | `POST /batch` | up to 50 `{tool, args}` members, `stop_on_error`, `step N tool: ...` lines |
//! | `POST /shutdown` | `{"ok": true, "result": "shutting down"}`, then the server stops |
//! | `POST /pause`, `/resume` | new in the Rust build: set or clear the pause flag |
//! | `windows`, `snapshot`, `files`, `screenshot` | read routes, answered from the core directly |
//! | `click`, `type`, `key`, `scroll`, `drag`, `hover`, `open` | adapters onto `computer` actions, see [`legacy`] |
//!
//! Status codes: 401 `{"error": "unauthorized"}` without the token (every POST, and `/health`), 404
//! `{"error": "not found"}` for any other GET, 400 `{"error": "bad json: ..."}` for an unparsable body,
//! 200 for a call that ran (a failed step inside a batch is still 200 with `failed_step`), 400
//! `{"ok": false, "error": ...}` for a refused or failed call.

pub mod legacy;
pub mod warm;

use std::cell::OnceCell;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::audit::log_action;
use crate::backend::HotkeyChord;
use crate::caller::{Caller, identify};
use crate::config::truthy;
use crate::keys::Key;
use crate::orchestrate::Computer;
use crate::state::RateLimiter;
use crate::textutil::py_str;
use crate::{ack, token};

pub use warm::{OCR_ACTIONS, WARM_WAIT, WarmGate, run_warmup};

/// The version `/health` reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Most members one `/batch` call may carry.
pub const BATCH_LIMIT: usize = 50;

/// One request, as the HTTP layer parsed it.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// `GET`, `POST`, ...
    pub method: &'a str,
    /// The request target, e.g. `/computer`.
    pub path: &'a str,
    /// The `X-Winhands-Token` header, if sent.
    pub token: Option<&'a str>,
    /// The body bytes (empty when there is none).
    pub body: &'a [u8],
    /// The client's address, for finding which process is calling.
    pub peer: Option<SocketAddr>,
}

/// What to send back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status.
    pub status: u16,
    /// `application/json` or `image/png`.
    pub content_type: &'static str,
    /// The body.
    pub body: Vec<u8>,
    /// Stop the server after this response is on the wire (`/shutdown`).
    pub shutdown: bool,
}

impl Response {
    fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_default(),
            shutdown: false,
        }
    }

    fn png(bytes: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "image/png",
            body: bytes,
            shutdown: false,
        }
    }

    /// The body as JSON, for tests and clients inside this process.
    pub fn json_body(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// What one tool call produced.
enum Tool {
    Json(Value),
    Png(Vec<u8>),
}

impl Tool {
    fn ok(&self) -> bool {
        match self {
            Tool::Json(v) => v.get("ok").and_then(Value::as_bool).unwrap_or(false),
            Tool::Png(_) => true,
        }
    }

    /// The Python outcome dict: a PNG is `{"ok": true, "content": [image]}`.
    fn into_value(self) -> Value {
        match self {
            Tool::Json(v) => v,
            Tool::Png(bytes) => json!({
                "ok": true,
                "content": [{
                    "type": "image",
                    "data": crate::imaging::base64(&bytes),
                    "mimeType": "image/png",
                }],
            }),
        }
    }
}

/// The process on the other end of this request, looked up the first time a call needs it. Never
/// global: two agents driving the same daemon each get their own answer.
struct PeerCaller<'a> {
    daemon: &'a Daemon,
    peer: Option<SocketAddr>,
    found: OnceCell<Caller>,
}

impl PeerCaller<'_> {
    fn get(&self) -> &Caller {
        self.found.get_or_init(|| {
            let (port, host) = match self.peer {
                Some(addr) => (addr.port(), addr.ip().to_string()),
                None => (0, String::new()),
            };
            // remote_caller() is true when the owner set the flag OR the daemon is bound off loopback:
            // then the pid on the other end of the socket is a relay at best, so it is not trusted.
            identify(
                self.daemon.computer.backend(),
                port,
                self.daemon.port,
                &host,
                self.daemon.computer.config().remote_caller(),
            )
        })
    }
}

/// The daemon: a [`Computer`], the token, and the little state around them.
pub struct Daemon {
    computer: Arc<Computer>,
    token: String,
    port: u16,
    hotkey: Mutex<Option<String>>,
    warm: Arc<WarmGate>,
    rate: RateLimiter,
}

impl Daemon {
    /// A daemon over `computer` that answers to `token`, listening on `port` (the port is how the
    /// caller's process is found in the TCP table).
    pub fn new(computer: Arc<Computer>, token: impl Into<String>, port: u16) -> Self {
        Self {
            computer,
            token: token.into(),
            port,
            hotkey: Mutex::new(None),
            warm: Arc::new(WarmGate::new()),
            rate: RateLimiter::new(),
        }
    }

    /// Use `gate` as the OCR warm-up gate.
    pub fn with_warm_gate(mut self, gate: Arc<WarmGate>) -> Self {
        self.warm = gate;
        self
    }

    /// The tool this daemon serves.
    pub fn computer(&self) -> &Arc<Computer> {
        &self.computer
    }

    /// The OCR warm-up gate (the start-up code begins it and a thread finishes it).
    pub fn warm_gate(&self) -> &Arc<WarmGate> {
        &self.warm
    }

    /// Record which kill hotkey got registered (`/health` reports it).
    pub fn set_hotkey(&self, name: Option<String>) {
        *self.hotkey.lock().unwrap_or_else(|e| e.into_inner()) = name;
    }

    /// The kill hotkey in force, if one registered.
    pub fn hotkey(&self) -> Option<String> {
        self.hotkey
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Has the daemon been idle for its whole timeout? Python `_idle_expired`.
    pub fn idle_expired(&self) -> bool {
        self.computer
            .idle_timer()
            .expired(self.computer.clock.as_ref())
    }

    /// Register the kill hotkey with the backend. Pressing it flips the pause flag. Returns the chord
    /// that registered, `None` when the OS refused every candidate (the daemon runs without one).
    pub fn install_kill_hotkey(&self) -> Option<String> {
        let computer = self.computer.clone();
        let on_press: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let paused = computer.pause_flag().toggle();
            eprintln!("winhands: kill hotkey toggled, paused={paused}");
        });
        let name = self
            .computer
            .backend()
            .register_kill_hotkey(&hotkey_candidates(), on_press)
            .ok()
            .flatten();
        self.set_hotkey(name.clone());
        name
    }

    // ------------------------------------------------------------------------------ routing

    /// Answer one request.
    pub fn handle(&self, req: &Request<'_>) -> Response {
        match req.method {
            "GET" => self.get(req),
            "POST" => self.post(req),
            _ => Response::json(501, &json!({"error": "unsupported method"})),
        }
    }

    /// Does this `X-Winhands-Token` header value open the daemon? The HTTP layer asks before it reads a
    /// body, so a caller without the token cannot make the daemon buffer anything.
    pub fn is_authorised(&self, presented: Option<&str>) -> bool {
        token::verify(presented.unwrap_or(""), &self.token)
    }

    fn authorised(&self, req: &Request<'_>) -> bool {
        self.is_authorised(req.token)
    }

    fn unauthorised() -> Response {
        Response::json(401, &json!({"error": "unauthorized"}))
    }

    fn get(&self, req: &Request<'_>) -> Response {
        if req.path == "/health" {
            if !self.authorised(req) {
                return Self::unauthorised();
            }
            return Response::json(200, &self.health());
        }
        Response::json(404, &json!({"error": "not found"}))
    }

    fn post(&self, req: &Request<'_>) -> Response {
        if !self.authorised(req) {
            return Self::unauthorised();
        }
        let who = PeerCaller {
            daemon: self,
            peer: req.peer,
            found: OnceCell::new(),
        };
        self.touch();
        let path = req.path.trim_matches('/');
        let args = match parse_body(req.body) {
            Ok(v) => v,
            Err(e) => return Response::json(400, &json!({"error": format!("bad json: {e}")})),
        };
        let response = match path {
            // An authenticated stop, so an updated build can be swapped in without an admin shell.
            // The daemon may be elevated, so an ordinary shell cannot kill it. Same trust as every
            // other route: the caller must hold the per-run token.
            "shutdown" => {
                let mut r = Response::json(200, &json!({"ok": true, "result": "shutting down"}));
                r.shutdown = true;
                r
            }
            "pause" | "resume" => {
                let pause = path == "pause";
                self.computer.pause_flag().set(pause);
                Response::json(
                    200,
                    &json!({"ok": true, "result": if pause { "paused" } else { "resumed" }}),
                )
            }
            "batch" => self.handle_batch(&args, &who),
            _ => {
                let tool = self.execute_tool(path, &args, &who);
                match tool {
                    Tool::Png(bytes) if path == "screenshot" => Response::png(bytes),
                    other => {
                        let ok = other.ok();
                        Response::json(if ok { 200 } else { 400 }, &other.into_value())
                    }
                }
            }
        };
        self.touch(); // a long call counts as activity until it ends, not just when it began
        response
    }

    fn touch(&self) {
        self.computer
            .idle_timer()
            .touch(self.computer.clock.as_ref());
    }

    fn health(&self) -> Value {
        let cfg = self.computer.config();
        let backend = self.computer.backend();
        let clock = self.computer.clock.as_ref();
        let remote = cfg.remote_caller();
        let elevated = backend
            .own_process()
            .ok()
            .flatten()
            .is_some_and(|p| p.elevated);
        json!({
            "elevated": elevated,
            "paused": self.computer.pause_flag().is_paused(),
            "version": VERSION,
            "hotkey": self.hotkey(),
            "idle_remaining_seconds": self.computer.idle_timer().remaining(clock),
            "idle_timeout_seconds": cfg.idle_timeout_seconds(),
            // An operator has to be able to see, without guessing, that the caller-relative guard is
            // off on this daemon.
            "remote_caller": remote,
            "remote_bind": cfg.bind_is_remote(),
            "caller_guard": if remote { "protect_processes only" } else { "caller-relative" },
            "ocr": backend.ocr_engine_name(),
            // new in the Rust build; a client that reads the fields above ignores these
            "backend": backend.name(),
            "impl": "winhands-rs",
        })
    }

    // ------------------------------------------------------------------------------ batch

    /// `POST /batch`: several tool calls in one request (Python `_handle_batch`).
    fn handle_batch(&self, args: &Value, who: &PeerCaller<'_>) -> Response {
        let Some(actions) = args.get("actions").and_then(Value::as_array) else {
            return Response::json(
                400,
                &json!({"ok": false, "error": "actions must be a list"}),
            );
        };
        if actions.len() > BATCH_LIMIT {
            return Response::json(
                400,
                &json!({"ok": false, "error": "batch limit is 50 actions; no actions sent"}),
            );
        }
        let stop_on_error = args.get("stop_on_error").is_none_or(truthy);
        let mut lines: Vec<String> = Vec::new();
        let mut content: Vec<Value> = Vec::new();
        let mut failed = false;
        for (index, item) in actions.iter().enumerate() {
            let (tool, outcome) = match item.as_object() {
                Some(member) if member.get("tool").and_then(Value::as_str) != Some("batch") => {
                    let tool = member
                        .get("tool")
                        .map_or_else(|| "None".to_string(), py_str);
                    let args = member.get("args").cloned().unwrap_or_else(|| json!({}));
                    let outcome = self.execute_tool(&tool, &args, who).into_value();
                    (tool, outcome)
                }
                _ => (
                    "invalid".to_string(),
                    json!({
                        "ok": false,
                        "error": "batch members must be tools; nested batch not allowed"
                    }),
                ),
            };
            let ok = outcome.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let detail = if ok {
                outcome
                    .get("result")
                    .map_or_else(|| "ok".to_string(), py_str)
            } else {
                format!(
                    "error: {}",
                    outcome
                        .get("error")
                        .map_or_else(|| "failed".to_string(), py_str)
                )
            };
            let line = format!("step {} {tool}: {detail}", index + 1);
            content.push(json!({"type": "text", "text": line}));
            lines.push(line);
            // Preserve earlier images and partial results even when a later step fails.
            if let Some(Value::Array(blocks)) = outcome.get("content") {
                content.extend(blocks.iter().cloned());
            }
            if !ok {
                failed = true;
                if stop_on_error {
                    break;
                }
            }
        }
        let mut payload = Map::new();
        payload.insert("ok".into(), Value::Bool(!failed));
        payload.insert("results".into(), json!(lines));
        payload.insert("content".into(), Value::Array(content));
        if failed {
            payload.insert(
                "error".into(),
                Value::from(
                    "batch partially executed or failed; inspect step results before retrying",
                ),
            );
        }
        Response::json(if failed { 400 } else { 200 }, &Value::Object(payload))
    }

    // ------------------------------------------------------------------------------ one tool

    /// One execution path for direct calls and batches. Never retries input. Python
    /// `_execute_tool`.
    fn execute_tool(&self, path: &str, args: &Value, who: &PeerCaller<'_>) -> Tool {
        self.touch();
        let outcome = if path == "computer" {
            // OCR actions that arrive while the start-up warm-up still runs wait for it
            self.warm.wait_for(args);
            Ok(Tool::Json(self.computer.execute(args, who.get()).to_json()))
        } else {
            self.run_route(path, args, who)
        };
        match outcome {
            Ok(tool) => tool,
            Err(error) => Tool::Json(self.failed(path, args, &error)),
        }
    }

    /// A refused or failed non-`computer` call: logged, counted, and worded for the caller.
    fn failed(&self, path: &str, args: &Value, error: &str) -> Value {
        log_action(self.computer.config().paths(), path, args, error, false);
        let error = self
            .computer
            .failure_counter()
            .note_failure(path, args, error);
        json!({"ok": false, "error": error})
    }

    fn ok_text(&self, path: &str, args: &Value, text: String) -> Tool {
        log_action(self.computer.config().paths(), path, args, &text, true);
        Tool::Json(json!({"ok": true, "result": text}))
    }

    /// The routes other than `computer`: the read routes and the legacy input routes.
    fn run_route(&self, path: &str, args: &Value, who: &PeerCaller<'_>) -> Result<Tool, String> {
        let Value::Object(map) = args else {
            return Err("tool args must be an object".into());
        };
        let c = &self.computer;
        if c.pause_flag().is_paused() && !legacy::READ_ONLY_ROUTES.contains(&path) {
            return Err("paused by user hotkey".into());
        }
        ack::check_acknowledged(c.config(), path, args).map_err(|e| e.0)?;
        if !self.rate.check(c.clock.as_ref()) {
            return Err("rate limit exceeded".into());
        }
        let step_err = |e: crate::orchestrate::StepError| e.message().to_string();
        let str_arg = |key: &str| map.get(key).and_then(Value::as_str);
        let int_arg = |key: &str, default: i64| {
            map.get(key)
                .and_then(crate::config::to_i64)
                .unwrap_or(default)
        };
        match path {
            "windows" => {
                let text = c.windows_text(false).map_err(step_err)?;
                let text = c.repeat_note().nudge(c.clock.as_ref(), path, args, &text);
                Ok(self.ok_text(path, args, text))
            }
            "snapshot" => {
                let _one_at_a_time = c.tool_lock().lock().unwrap_or_else(|e| e.into_inner());
                let coords = map.get("coords").is_some_and(truthy);
                let text = c
                    .read_snapshot(
                        str_arg("window"),
                        int_arg("max_items", 40),
                        str_arg("filter").unwrap_or(""),
                        coords,
                        true,
                    )
                    .map_err(step_err)?;
                let text = c.repeat_note().nudge(c.clock.as_ref(), path, args, &text);
                Ok(self.ok_text(path, args, text))
            }
            "files" => {
                let folder = str_arg("path").ok_or("missing required argument 'path'")?;
                let text = c
                    .list_files(
                        folder,
                        str_arg("sort").unwrap_or("modified"),
                        map.get("desc").is_none_or(truthy),
                        int_arg("limit", 20),
                        str_arg("pattern").unwrap_or(""),
                    )
                    .map_err(step_err)?;
                let text = c.repeat_note().nudge(c.clock.as_ref(), path, args, &text);
                Ok(self.ok_text(path, args, text))
            }
            "screenshot" => {
                let _one_at_a_time = c.tool_lock().lock().unwrap_or_else(|e| e.into_inner());
                let png = c.display_png().map_err(step_err)?;
                log_action(c.config().paths(), path, args, "screenshot", true);
                Ok(Tool::Png(png))
            }
            p if legacy::is_input_route(p) => {
                let displays = c.backend().displays().map_err(|e| e.to_string())?;
                let payload = legacy::to_computer(p, map, &displays)?;
                // Computer::execute does the audit entry, one action at a time, and the failure
                // fields; its answer is the route's answer.
                Ok(Tool::Json(c.execute(&payload, who.get()).to_json()))
            }
            other => Err(format!("unknown action: {other}")),
        }
    }
}

/// Parse a request body: empty is `{}`, anything else must be JSON (Python `_read_body`).
fn parse_body(body: &[u8]) -> Result<Value, serde_json::Error> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body)
}

/// The kill-hotkey chords, in the order they are tried (Python `HOTKEY_CANDIDATES`). The first one
/// the OS accepts wins. Ctrl+Alt+Shift+K, the originally specified combo, is deliberately not in the
/// list: on 2026-09-17 it was found already owned system-wide by another running app, so a working
/// fallback chain replaces it.
pub fn hotkey_candidates() -> Vec<HotkeyChord> {
    let chord = |name: &str, keys: &[Key]| HotkeyChord {
        name: name.to_string(),
        keys: keys.to_vec(),
    };
    vec![
        chord(
            "ctrl+alt+shift+f12",
            &[Key::Ctrl, Key::Alt, Key::Shift, Key::F(12)],
        ),
        chord(
            "ctrl+alt+shift+pause",
            &[Key::Ctrl, Key::Alt, Key::Shift, Key::Pause],
        ),
        chord(
            "ctrl+shift+win+k",
            &[Key::Ctrl, Key::Shift, Key::Meta, Key::Char('k')],
        ),
    ]
}

#[cfg(test)]
mod tests;
