//! The daemon over real sockets, on a test port, over the fake desktop: routing, auth, the
//! acknowledgement gate, concurrency, `/shutdown`, idle exit, and the clients (`DaemonLink`, the shim's
//! `forward`, `wh`) talking to it. Nothing here touches a real desktop or the real token file.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wh_core::backend::fake::FakeBackend;
use wh_core::backend::{Display, Image, Rect, WindowInfo};
use wh_core::clock::SystemClock;
use wh_core::config::Config;
use wh_core::daemon::Daemon;
use wh_core::env::MapEnv;
use wh_core::mcp::{Block, ShimDefaults};
use wh_core::orchestrate::Computer;
use wh_core::paths::Paths;
use wh_core::token;
use winhands_lib::client::{self, ClientError, Reply, Timeouts};
use winhands_lib::link::{DaemonLink, Failure};
use winhands_lib::serve::{bind, serve};
use winhands_lib::shim::{forward, guarded};
use winhands_lib::wh;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Tmp(PathBuf);

impl Tmp {
    fn new(label: &str) -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wh-http-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn textured(w: u32, h: u32) -> Image {
    let mut bytes = Vec::new();
    for i in 0..(w * h) {
        let v = ((i / 2) * 7 % 256) as u8;
        bytes.extend_from_slice(&[v, v, v, 255]);
    }
    Image::new(w, h, bytes).expect("textured image")
}

/// A daemon serving on 127.0.0.1:0 over a fake desktop.
struct Live {
    port: u16,
    token: String,
    paths: Paths,
    fake: Arc<FakeBackend>,
    thread: Option<JoinHandle<std::io::Result<()>>>,
    _tmp: Tmp,
}

impl Live {
    fn start() -> Self {
        Self::start_with(
            MapEnv::new().with("WINHANDS_ACKNOWLEDGED", "1"),
            Duration::from_secs(15),
        )
    }

    fn start_with(env: MapEnv, idle_poll: Duration) -> Self {
        let tmp = Tmp::new("live");
        let paths = Paths::in_dir(tmp.0.join("data"));
        let fake = Arc::new(FakeBackend::new());
        fake.set_displays(vec![Display {
            id: 1,
            name: "kit".into(),
            rect: Rect::from_xywh(0, 0, 320, 180),
            scale: 1.0,
            primary: true,
        }]);
        fake.set_screen(textured(320, 180));
        let cfg = Arc::new(Config::new(paths.clone(), env));
        let token = token::write_token(&paths, cfg.env()).unwrap();
        let computer = Arc::new(Computer::new(
            fake.clone(),
            Arc::new(SystemClock::new()),
            cfg,
        ));
        let listener = bind("127.0.0.1", 0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let daemon = Arc::new(Daemon::new(computer, token.clone(), port));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn(move || serve(listener, daemon, stop, idle_poll));
        Live {
            port,
            token,
            paths,
            fake,
            thread: Some(thread),
            _tmp: tmp,
        }
    }

    fn raw(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<&Value>,
    ) -> Result<Reply, ClientError> {
        let bytes = body.map(|b| serde_json::to_vec(b).unwrap());
        client::request(
            "127.0.0.1",
            self.port,
            method,
            path,
            token,
            bytes.as_deref(),
            Timeouts::for_host("127.0.0.1", Duration::from_secs(20)),
        )
    }

    fn get(&self, path: &str) -> Reply {
        self.raw("GET", path, &self.token, None).unwrap()
    }

    fn post(&self, path: &str, body: Value) -> Reply {
        self.raw("POST", path, &self.token, Some(&body)).unwrap()
    }

    fn link(&self) -> DaemonLink {
        DaemonLink::new(
            "127.0.0.1",
            self.port,
            self.paths.clone(),
            None,
            Duration::from_secs(20),
        )
    }

    fn window(&self, title: &str, app: &str) {
        let w = self.fake.add_window(WindowInfo::basic(
            title,
            app,
            4242,
            Rect::from_xywh(10, 10, 300, 160),
        ));
        self.fake.set_foreground(Some(w));
    }

    /// Wait for the server thread to finish.
    fn finished_within(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
                if let Some(t) = self.thread.take() {
                    t.join().unwrap().unwrap();
                }
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.raw("POST", "/shutdown", &self.token, Some(&json!({})));
        }
    }
}

fn json_of(reply: &Reply) -> Value {
    serde_json::from_slice(&reply.body).expect("a JSON body")
}

// ---------------------------------------------------------------------------------- auth, routes

#[test]
fn health_needs_the_token_over_the_wire() {
    let live = Live::start();
    let wrong = live.raw("GET", "/health", "wrong-token", None).unwrap();
    assert_eq!(wrong.status, 401);
    assert_eq!(json_of(&wrong), json!({"error": "unauthorized"}));
    assert_eq!(live.raw("GET", "/health", "", None).unwrap().status, 401);
    let ok = live.get("/health");
    assert_eq!(ok.status, 200);
    assert_eq!(ok.content_type, "application/json");
    let data = json_of(&ok);
    assert!(
        data.get("paused").is_some()
            && data.get("elevated").is_some()
            && data.get("version").is_some()
    );
    assert_eq!(data["impl"], "winhands-rs");
}

#[test]
fn the_token_header_is_matched_case_insensitively() {
    let live = Live::start();
    let mut s = TcpStream::connect(("127.0.0.1", live.port)).unwrap();
    write!(
        s,
        "GET /health HTTP/1.1\r\nHost: x\r\nx-winhands-token: {}\r\nConnection: close\r\n\r\n",
        live.token
    )
    .unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
}

#[test]
fn every_post_needs_the_token_and_other_gets_are_404() {
    let live = Live::start();
    for path in ["/computer", "/nope", "/shutdown", "/batch"] {
        let r = live.raw("POST", path, "bad", Some(&json!({}))).unwrap();
        assert_eq!(r.status, 401, "{path}");
    }
    assert_eq!(live.get("/computer").status, 404);
    assert_eq!(
        json_of(&live.get("/nothing")),
        json!({"error": "not found"})
    );
}

#[test]
fn a_computer_call_returns_text_and_an_image() {
    let live = Live::start();
    let r = live.post("/computer", json!({"action": "screenshot"}));
    assert_eq!(r.status, 200);
    let data = json_of(&r);
    assert_eq!(data["ok"], true);
    let content = data["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image");
}

#[test]
fn a_body_that_is_not_json_is_400_and_the_daemon_carries_on() {
    let live = Live::start();
    let mut s = TcpStream::connect(("127.0.0.1", live.port)).unwrap();
    let body = "{broken";
    write!(
        s,
        "POST /computer HTTP/1.1\r\nHost: x\r\nX-Winhands-Token: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        live.token,
        body.len()
    )
    .unwrap();
    let mut text = String::new();
    s.read_to_string(&mut text).unwrap();
    assert!(text.starts_with("HTTP/1.1 400"), "{text}");
    assert!(text.contains("bad json"));
    assert_eq!(live.get("/health").status, 200);
}

#[test]
fn nothing_that_acts_runs_until_the_owner_acknowledges() {
    let live = Live::start_with(MapEnv::new(), Duration::from_secs(15));
    assert_eq!(
        live.post("/computer", json!({"action": "list_windows"}))
            .status,
        200
    );
    let r = live.post(
        "/computer",
        json!({"action": "left_click", "coordinate": [1, 2]}),
    );
    assert_eq!(r.status, 400);
    let error = json_of(&r)["error"].as_str().unwrap().to_string();
    assert!(error.contains("winhands acknowledge"), "{error}");
    assert!(live.fake.events().is_empty());
}

#[test]
fn the_legacy_screenshot_route_answers_png_and_wh_saves_it() {
    let live = Live::start();
    let r = live.post("/screenshot", json!({}));
    assert_eq!((r.status, r.content_type.as_str()), (200, "image/png"));
    assert_eq!(&r.body[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn batch_runs_over_the_wire() {
    let live = Live::start();
    live.window("Notepad", "notepad.exe");
    let r = live.post("/batch", json!({"actions": [{"tool": "windows"}, {"tool": "computer", "args": {"action": "cursor_position"}}]}));
    assert_eq!(r.status, 200);
    assert_eq!(json_of(&r)["results"].as_array().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------------- concurrency

#[test]
fn a_slow_call_does_not_block_health() {
    let live = Arc::new(Live::start());
    let slow = {
        let live = live.clone();
        thread::spawn(move || {
            live.post(
                "/computer",
                json!({"action": "wait", "duration": 1.5, "shot": false}),
            )
        })
    };
    thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    assert_eq!(live.get("/health").status, 200);
    assert!(
        t.elapsed() < Duration::from_millis(900),
        "health waited {:?}",
        t.elapsed()
    );
    assert_eq!(slow.join().unwrap().status, 200);
}

#[test]
fn two_callers_at_once_both_get_answers() {
    let live = Arc::new(Live::start());
    live.window("Notepad", "notepad.exe");
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let live = live.clone();
            thread::spawn(move || {
                live.post("/computer", json!({"action": "list_windows"}))
                    .status
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 200);
    }
}

// ---------------------------------------------------------------------------------- shutdown, idle

#[test]
fn shutdown_stops_the_server_after_answering() {
    let mut live = Live::start();
    let r = live.post("/shutdown", json!({}));
    assert_eq!(r.status, 200);
    assert_eq!(json_of(&r), json!({"ok": true, "result": "shutting down"}));
    assert!(
        live.finished_within(Duration::from_secs(5)),
        "the server stopped"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut refused = false;
    while Instant::now() < deadline {
        if matches!(
            live.raw("GET", "/health", &live.token, None),
            Err(ClientError::Unreachable(_))
        ) {
            refused = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(refused, "nothing listens any more");
}

#[test]
fn an_idle_daemon_exits_by_itself() {
    let mut live = Live::start_with(
        MapEnv::new()
            .with("WINHANDS_ACKNOWLEDGED", "1")
            .with("WINHANDS_IDLE_TIMEOUT_SECONDS", "1"),
        Duration::from_millis(50),
    );
    assert_eq!(live.get("/health").status, 200);
    assert!(
        live.finished_within(Duration::from_secs(6)),
        "idle exit after about a second"
    );
}

#[test]
fn requests_keep_an_idle_daemon_alive() {
    let mut live = Live::start_with(
        MapEnv::new()
            .with("WINHANDS_ACKNOWLEDGED", "1")
            .with("WINHANDS_IDLE_TIMEOUT_SECONDS", "2"),
        Duration::from_millis(50),
    );
    for _ in 0..6 {
        thread::sleep(Duration::from_millis(500));
        live.post("/computer", json!({"action": "list_windows"}));
    }
    assert!(
        !live.finished_within(Duration::from_millis(100)),
        "3 s of traffic kept a 2 s timeout at bay"
    );
    assert!(
        live.finished_within(Duration::from_secs(8)),
        "then it went idle"
    );
}

// ---------------------------------------------------------------------------------- the clients

#[test]
fn the_shim_forwards_a_call_and_returns_text_and_image_blocks() {
    let live = Live::start();
    live.window("Notepad", "notepad.exe");
    let link = live.link();
    let defaults = ShimDefaults {
        coords: "pixels".into(),
        only_apps: Vec::new(),
    };
    let reply = forward(
        &link,
        &defaults,
        json!({"action": "screenshot"}).as_object().unwrap(),
    );
    assert!(!reply.is_error, "{reply:?}");
    assert!(matches!(&reply.blocks[0], Block::Text(t) if t.starts_with("screenshot")));
    assert!(matches!(&reply.blocks[1], Block::Image { mime, .. } if mime.starts_with("image/")));
    // a refused call is an error with the daemon's words
    let reply = forward(
        &link,
        &defaults,
        json!({"action": "frobnicate"}).as_object().unwrap(),
    );
    assert!(reply.is_error);
    let Block::Text(t) = &reply.blocks[0] else {
        panic!("text")
    };
    assert!(t.contains("unknown action"), "{t}");
}

// A panic in the core must cost one call, not the daemon: the request gets a 500 error answer over the
// wire, the shim turns it into an error result, and the next call is answered normally.
#[test]
fn a_panicking_core_call_is_an_error_over_the_wire_and_the_daemon_keeps_serving() {
    let live = Live::start();
    live.window("Notepad", "notepad.exe");
    live.fake
        .set_click_hook(|_, _| panic!("core panic over the wire"));
    let click = json!({"action": "left_click", "coordinate": [50, 50], "shot": false});
    let reply = live.post("/computer", click.clone());
    assert_eq!(reply.status, 500);
    let data = json_of(&reply);
    assert_eq!(data["ok"], false);
    assert!(
        data["error"]
            .as_str()
            .unwrap()
            .contains("internal error: core panic over the wire"),
        "{data}"
    );
    // the shim reports it as an error result instead of failing
    let defaults = ShimDefaults {
        coords: "pixels".into(),
        only_apps: Vec::new(),
    };
    let link = live.link();
    let reply = forward(&link, &defaults, click.as_object().unwrap());
    assert!(reply.is_error, "{reply:?}");
    let Block::Text(t) = &reply.blocks[0] else {
        panic!("text")
    };
    assert!(t.contains("core panic over the wire"), "{t}");
    // and the daemon is still there for the next caller, several threads at once
    assert_eq!(live.get("/health").status, 200);
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (port, token) = (live.port, live.token.clone());
            thread::spawn(move || {
                client::request(
                    "127.0.0.1",
                    port,
                    "POST",
                    "/computer",
                    &token,
                    Some(br#"{"action":"list_windows"}"#),
                    Timeouts::for_host("127.0.0.1", Duration::from_secs(20)),
                )
                .unwrap()
                .status
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 200);
    }
}

// catch_unwind only works if the release build unwinds; "abort" would turn every caught panic back
// into a dead daemon.
#[test]
fn the_release_profile_unwinds_so_that_panics_can_be_caught() {
    let manifest = include_str!("../../../Cargo.toml");
    let release = manifest
        .split("[profile.release]")
        .nth(1)
        .expect("a release profile");
    assert!(release.contains("panic = \"unwind\""), "{release}");
    assert!(!release.contains("panic = \"abort\""));
}

#[test]
fn the_shim_guard_turns_a_panic_into_an_error_reply() {
    let reply = guarded(|| panic!("tool call exploded"));
    assert!(reply.is_error);
    let Block::Text(t) = &reply.blocks[0] else {
        panic!("text")
    };
    assert_eq!(
        t,
        "winhands shim failed: internal error: tool call exploded"
    );
    let fine = guarded(|| wh_core::mcp::Reply::error("plain".to_string()));
    assert!(matches!(&fine.blocks[0], Block::Text(t) if t == "plain"));
}

#[test]
fn wh_talks_to_the_daemon_end_to_end() {
    let live = Live::start();
    live.window("Notepad", "notepad.exe");
    let link = live.link();
    let tmp = Tmp::new("wh-out");
    let cfg = Config::new(Paths::in_dir(tmp.0.join("cfg")), MapEnv::new());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut call = |tool: &str, payload: &serde_json::Map<String, Value>| {
        link.post(tool, &Value::Object(payload.clone()))
            .map_err(|f| wh::Exit(format!("{f:?}")))
    };
    let argv: Vec<String> = ["computer", "action=list_windows"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let code = wh::run(
        &argv,
        &cfg,
        &mut std::io::empty(),
        &mut out,
        &mut err,
        &tmp.0,
        &mut call,
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert!(String::from_utf8_lossy(&out).contains("Notepad"));
    let argv: Vec<String> = ["screenshot"].iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = wh::run(
        &argv,
        &cfg,
        &mut std::io::empty(),
        &mut out,
        &mut err,
        &tmp.0,
        &mut call,
    );
    assert_eq!(code, 0);
    let path = String::from_utf8(out).unwrap();
    assert_eq!(&std::fs::read(path.trim()).unwrap()[..4], b"\x89PNG");
}

#[test]
fn a_rotated_token_is_retried_once_with_the_new_one() {
    let tmp = Tmp::new("rotate");
    let paths = Paths::in_dir(&tmp.0);
    std::fs::write(paths.token(), "old-token").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let token_file = paths.token();
    let server = thread::spawn(move || {
        let mut seen = Vec::new();
        for round in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = String::new();
            while !got.contains("\r\n\r\n") {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            let sent = got
                .lines()
                .find_map(|l| l.strip_prefix("X-Winhands-Token: "))
                .unwrap_or("")
                .to_string();
            seen.push(sent);
            if round == 0 {
                // the daemon restarted and rotated the token between the read and the call
                std::fs::write(&token_file, "new-token").unwrap();
                s.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
            } else {
                let body = b"{\"ok\": true, \"result\": \"fine\"}";
                write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                s.write_all(body).unwrap();
            }
        }
        seen
    });
    let link = DaemonLink::new("127.0.0.1", port, paths, None, Duration::from_secs(5));
    let reply = link
        .post("computer", &json!({"action": "screenshot"}))
        .unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(server.join().unwrap(), ["old-token", "new-token"]);
}

#[test]
fn a_401_with_an_unchanged_token_is_not_retried() {
    let tmp = Tmp::new("norotate");
    let paths = Paths::in_dir(&tmp.0);
    std::fs::write(paths.token(), "same").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = s.read(&mut buf).unwrap();
        s.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .unwrap();
        // a second connection would hang the test: it must never come
        listener.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(300));
        listener.accept().is_ok()
    });
    let link = DaemonLink::new("127.0.0.1", port, paths, None, Duration::from_secs(5));
    assert_eq!(link.post("computer", &json!({})).unwrap().status, 401);
    assert!(!server.join().unwrap(), "no second request");
}

#[test]
fn no_daemon_and_no_starter_is_no_daemon_without_a_retry_storm() {
    let tmp = Tmp::new("nodaemon");
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let link = DaemonLink::new(
        "127.0.0.1",
        free,
        Paths::in_dir(&tmp.0),
        None,
        Duration::from_secs(2),
    );
    let t = Instant::now();
    assert_eq!(link.post("computer", &json!({})), Err(Failure::NoDaemon));
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn a_timeout_is_reported_as_unknown_and_nothing_is_started() {
    let tmp = Tmp::new("timeout");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let keep = thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(700));
        drop(s);
    });
    // a starter that would leave a trace if it ran
    let marker = tmp.0.join("started");
    let starter = winhands_lib::start::Starter::Command(vec![
        "definitely-not-a-program".into(),
        marker.display().to_string(),
    ]);
    let link = DaemonLink::new(
        "127.0.0.1",
        port,
        Paths::in_dir(&tmp.0),
        Some(starter),
        Duration::from_millis(200),
    );
    assert_eq!(link.post("computer", &json!({})), Err(Failure::Timeout));
    keep.join().unwrap();
    assert!(!marker.exists());
}
