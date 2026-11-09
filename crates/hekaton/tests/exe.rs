//! The real `hekaton` executable: the MCP server over stdio (tool listing against the Python
//! server's own `list_tools`, errors when no daemon can be started), and on Windows a real daemon on a
//! test port with its data directory in a temp folder (passive routes only: it never sends input and
//! never touches the real token file or port 47811).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_hekaton")
}

struct Sandbox {
    dir: PathBuf,
    port: u16,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hk-exe-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Sandbox { dir, port }
    }

    /// Environment that points the child at this sandbox and nowhere else.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(exe());
        cmd.args(args);
        for (key, _) in std::env::vars() {
            if key.starts_with("HEKATON_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("LOCALAPPDATA", &self.dir)
            .env("HEKATON_PORT", self.port.to_string())
            .env("HEKATON_IDLE_TIMEOUT_SECONDS", "120");
        cmd
    }

    fn data(&self) -> PathBuf {
        self.dir.join("hekaton")
    }

    #[cfg(windows)]
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<hekaton_lib::client::Reply> {
        let token = std::fs::read_to_string(self.data().join("token")).unwrap_or_default();
        let bytes = body.map(|b| serde_json::to_vec(b).unwrap());
        hekaton_lib::client::request(
            "127.0.0.1",
            self.port,
            method,
            path,
            token.trim(),
            bytes.as_deref(),
            hekaton_lib::client::Timeouts::for_host("127.0.0.1", Duration::from_secs(20)),
        )
        .ok()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            // stop a daemon a failed test left behind
            let _ = self.request("POST", "/shutdown", Some(&json!({})));
            thread::sleep(Duration::from_millis(200));
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// An MCP client over the child's stdin and stdout.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Mcp {
    fn spawn(mut cmd: Command) -> Mcp {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start hekaton mcp");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Mcp {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// The response with this id, skipping notifications.
    fn recv(&self, id: u64, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response with id {id} within {limit:?}"));
            let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn handshake(&mut self) -> Value {
        self.send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "hekaton-test", "version": "0"}}}),
        );
        let init = self.recv(1, Duration::from_secs(20));
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }

    fn call(&mut self, id: u64, args: Value, limit: Duration) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                         "params": {"name": "computer", "arguments": args}}));
        self.recv(id, limit)
    }

    /// Hang up and wait for the server to exit by itself.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        None
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// 2026-10-28: the core definition, no longer the Python one (kept in tests/parity as history).
fn python_tool() -> Value {
    hekaton_core::tool_schema::tool_definition()
}

#[test]
fn mcp_serves_exactly_the_tool_the_python_server_serves() {
    let sb = Sandbox::new("list");
    let mut mcp = Mcp::spawn(sb.command(&["mcp"]));
    let init = mcp.handshake();
    assert_eq!(init["result"]["serverInfo"]["name"], "hekaton", "{init}");
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "{init}"
    );
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = mcp.recv(2, Duration::from_secs(20));
    let tools = listed["result"]["tools"].as_array().expect("a tools array");
    assert_eq!(tools.len(), 1, "{listed}");
    assert_eq!(
        tools[0],
        python_tool(),
        "name, description and inputSchema match the Python list_tools"
    );
    assert_eq!(
        mcp.finish(),
        Some(0),
        "the server exits cleanly when the client hangs up"
    );
}

#[test]
fn mcp_says_so_when_no_daemon_answers_and_none_can_be_started() {
    let sb = Sandbox::new("nodaemon");
    let mut cmd = sb.command(&["mcp"]);
    cmd.env(
        "HEKATON_DAEMON_CMD",
        "definitely-not-a-hekaton-program daemon",
    )
    .env("HEKATON_DAEMON_WAIT_S", "1");
    let mut mcp = Mcp::spawn(cmd);
    mcp.handshake();
    let started = Instant::now();
    let reply = mcp.call(2, json!({"action": "screenshot"}), Duration::from_secs(30));
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("unreachable") && text.contains("definitely-not-a-hekaton-program daemon"),
        "{text}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    mcp.finish();
}

#[test]
fn mcp_refuses_an_unknown_tool_and_a_bad_argument_with_a_message() {
    let sb = Sandbox::new("badcall");
    let mut cmd = sb.command(&["mcp"]);
    cmd.env(
        "HEKATON_DAEMON_CMD",
        "definitely-not-a-hekaton-program daemon",
    )
    .env("HEKATON_DAEMON_WAIT_S", "1");
    let mut mcp = Mcp::spawn(cmd);
    mcp.handshake();
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "nope", "arguments": {}}}));
    let reply = mcp.recv(2, Duration::from_secs(10));
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool")
    );
    let reply = mcp.call(
        3,
        json!({"coordinate": "not a list"}),
        Duration::from_secs(10),
    );
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("coordinate")
    );
    mcp.finish();
}

#[test]
fn the_command_line_answers_without_a_daemon() {
    let sb = Sandbox::new("cli");
    let run = |args: &[&str]| {
        let out = sb.command(args).stdin(Stdio::null()).output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };
    let version = run(&["--version"]).1;
    assert!(version.starts_with(env!("CARGO_PKG_VERSION")));
    assert!(version.contains("source="));
    assert!(version.contains("protocol=1"));
    let (code, text) = run(&["status"]);
    assert_eq!(code, Some(1));
    assert!(text.starts_with("hekaton: stopped"), "{text}");
    let (code, text) = run(&["acknowledge"]);
    assert_eq!(code, Some(1), "not a terminal: nothing recorded");
    assert!(
        text.contains("has to be typed by the person at this machine"),
        "{text}"
    );
    assert!(!sb.data().join("acknowledged.json").exists());
    let (code, _) = run(&["frobnicate"]);
    assert_eq!(code, Some(2));
    let (code, text) = run(&["hk", "help"]);
    assert_eq!(code, Some(0));
    assert!(text.contains("hk recipe list"));
}

#[cfg(windows)]
fn wait_for_health(sb: &Sandbox, limit: Duration) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(r) = sb.request("GET", "/health", None)
            && r.status == 200
        {
            return Some(start.elapsed());
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

#[cfg(windows)]
#[test]
fn a_real_daemon_serves_passive_routes_refuses_input_and_stops_on_request() {
    let sb = Sandbox::new("daemon");
    let mut child = sb
        .command(&["daemon", "--no-hotkey"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let up = wait_for_health(&sb, Duration::from_secs(20)).expect("the daemon came up");
    eprintln!("cold start to healthy: {up:?}");
    let health: Value =
        serde_json::from_slice(&sb.request("GET", "/health", None).unwrap().body).unwrap();
    assert_eq!(health["impl"], "hekaton-rs");
    assert_eq!(health["backend"], "windows");
    assert_eq!(health["hotkey"], Value::Null, "--no-hotkey");
    // the token file and the pid file are in the sandbox, not in the real data directory
    assert_eq!(
        std::fs::read_to_string(sb.data().join("token"))
            .unwrap()
            .len(),
        64
    );
    assert!(sb.data().join("daemon.pid").exists());
    // a passive read works
    let listed = sb
        .request(
            "POST",
            "/computer",
            Some(&json!({"action": "list_windows"})),
        )
        .unwrap();
    assert_eq!(
        listed.status,
        200,
        "{}",
        String::from_utf8_lossy(&listed.body)
    );
    // nothing that acts runs: this sandbox has no acknowledgement
    let refused = sb
        .request(
            "POST",
            "/computer",
            Some(&json!({"action": "left_click", "coordinate": [1, 1]})),
        )
        .unwrap();
    assert_eq!(refused.status, 400);
    assert!(String::from_utf8_lossy(&refused.body).contains("hekaton acknowledge"));
    // a second daemon on the same port leaves the first one's token alone and exits 3
    let token_before = std::fs::read_to_string(sb.data().join("token")).unwrap();
    let second = sb
        .command(&["daemon", "--no-hotkey"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        second.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(sb.data().join("token")).unwrap(),
        token_before
    );
    // stop it
    assert_eq!(
        sb.request("POST", "/shutdown", Some(&json!({})))
            .unwrap()
            .status,
        200
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit after /shutdown"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(status.code(), Some(0));
    assert!(
        !sb.data().join("daemon.pid").exists(),
        "the pid file is removed on the way out"
    );
    // and it starts again on the same port straight away (an idle exit, then the next call)
    let mut again = sb
        .command(&["daemon", "--no-hotkey"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        wait_for_health(&sb, Duration::from_secs(20)).is_some(),
        "restarted on the same port"
    );
    assert_eq!(
        sb.request("POST", "/shutdown", Some(&json!({})))
            .unwrap()
            .status,
        200
    );
    assert_eq!(again.wait().unwrap().code(), Some(0));
}

#[cfg(windows)]
#[test]
fn the_shim_starts_the_daemon_on_demand_and_the_call_goes_through() {
    let sb = Sandbox::new("ondemand");
    let mut cmd = sb.command(&["mcp"]);
    cmd.env(
        "HEKATON_DAEMON_CMD",
        format!("\"{}\" daemon --no-hotkey", exe()),
    );
    let mut mcp = Mcp::spawn(cmd);
    mcp.handshake();
    assert!(
        sb.request("GET", "/health", None).is_none(),
        "no daemon yet"
    );
    let started = Instant::now();
    let reply = mcp.call(
        2,
        json!({"action": "list_windows"}),
        Duration::from_secs(40),
    );
    eprintln!(
        "first call including the daemon start: {:?}",
        started.elapsed()
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert!(reply["result"]["content"][0]["type"] == "text", "{reply}");
    assert!(
        sb.request("GET", "/health", None).is_some(),
        "the daemon is up now"
    );
    // the next call reuses it
    let again = mcp.call(
        3,
        json!({"action": "cursor_position"}),
        Duration::from_secs(20),
    );
    assert_eq!(again["result"]["isError"], false, "{again}");
    assert!(
        std::fs::read_dir(sb.data().join("shims"))
            .map(|d| d.count())
            .unwrap_or(0)
            >= 1,
        "the shim registered itself"
    );
    assert_eq!(mcp.finish(), Some(0));
    let left = std::fs::read_dir(sb.data().join("shims"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "the shim removed its record on the way out");
}

#[cfg(windows)]
fn hidden_exe() -> &'static str {
    env!("CARGO_BIN_EXE_hekatond")
}

/// The PE optional header's Subsystem field: 2 is the Windows GUI subsystem (no console), 3 is console.
#[cfg(windows)]
fn pe_subsystem(path: &str) -> u16 {
    let bytes = std::fs::read(path).unwrap();
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe..pe + 4], b"PE\0\0", "{path} is not a PE file");
    let optional = pe + 24;
    u16::from_le_bytes(bytes[optional + 68..optional + 70].try_into().unwrap())
}

#[cfg(windows)]
#[test]
fn hekatond_has_no_console_and_hekaton_keeps_one() {
    assert_eq!(pe_subsystem(hidden_exe()), 2, "hekatond is a GUI program");
    assert_eq!(pe_subsystem(exe()), 3, "hekaton stays a console program");
}

#[cfg(windows)]
#[test]
fn hekatond_runs_the_daemon_and_writes_its_messages_to_the_log() {
    let sb = Sandbox::new("hidden");
    let mut cmd = Command::new(hidden_exe());
    cmd.arg("--no-hotkey");
    for (key, _) in std::env::vars() {
        if key.starts_with("HEKATON_") {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd
        .env("LOCALAPPDATA", &sb.dir)
        .env("HEKATON_PORT", sb.port.to_string())
        .env("HEKATON_IDLE_TIMEOUT_SECONDS", "120")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_health(&sb, Duration::from_secs(20)).expect("hekatond came up");
    let health: Value =
        serde_json::from_slice(&sb.request("GET", "/health", None).unwrap().body).unwrap();
    assert_eq!(health["impl"], "hekaton-rs");
    assert_eq!(health["hotkey"], Value::Null, "--no-hotkey");
    assert!(sb.data().join("daemon.pid").exists());
    let log = std::fs::read_to_string(sb.data().join("daemon.log")).unwrap_or_default();
    assert!(log.contains("hekaton listening on 127.0.0.1"), "{log}");
    // an ordinary request works and the daemon stops on request, exit code 0
    let r = sb
        .request(
            "POST",
            "/computer",
            Some(&json!({"action": "list_windows"})),
        )
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(
        sb.request("POST", "/shutdown", Some(&json!({})))
            .unwrap()
            .status,
        200
    );
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(!sb.data().join("daemon.pid").exists(), "pid file removed");
}

#[cfg(windows)]
#[test]
fn a_second_hekatond_on_the_same_port_logs_why_it_left() {
    let sb = Sandbox::new("hidden2");
    let _blocker = std::net::TcpListener::bind(("127.0.0.1", sb.port)).unwrap();
    let mut cmd = Command::new(hidden_exe());
    cmd.arg("--no-hotkey");
    for (key, _) in std::env::vars() {
        if key.starts_with("HEKATON_") {
            cmd.env_remove(key);
        }
    }
    let status = cmd
        .env("LOCALAPPDATA", &sb.dir)
        .env("HEKATON_PORT", sb.port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(3), "port taken");
    let log = std::fs::read_to_string(sb.data().join("daemon.log")).unwrap_or_default();
    assert!(log.contains("cannot bind"), "{log}");
    assert!(
        !sb.data().join("token").exists(),
        "a daemon that cannot bind leaves the token alone"
    );
}

#[cfg(windows)]
#[test]
fn hekaton_start_can_start_hekatond_and_status_names_it_by_default() {
    let sb = Sandbox::new("startd");
    // by default a stopped daemon would be started with the hekatond.exe that sits next to hekaton.exe
    let out = sb.command(&["status"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("start method:") && text.contains("hekatond.exe"),
        "{text}"
    );
    assert!(
        !text.contains("hekaton.exe\" daemon"),
        "not the console build: {text}"
    );
    // started through --daemon-cmd with the test-only flag, it comes up and hekaton stop ends it
    // (capturing the output is part of the test: a daemon that inherited the pipe would keep `output()`
    // waiting until its 120 s idle timeout)
    let cmdline = format!("\"{}\" --no-hotkey", hidden_exe());
    let began = Instant::now();
    let out = sb
        .command(&["start", "--daemon-cmd", &cmdline])
        .output()
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(60),
        "the started daemon held the caller's output pipe open: {:?}",
        began.elapsed()
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("hekaton: running"), "{text}");
    assert!(sb.request("GET", "/health", None).is_some());
    let out = sb.command(&["stop"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("stop requested"));
    thread::sleep(Duration::from_millis(300));
    assert!(sb.request("GET", "/health", None).is_none());
}

fn listed_names(mut cmd: Command) -> Vec<String> {
    cmd.env(
        "HEKATON_DAEMON_CMD",
        "definitely-not-a-hekaton-program daemon",
    );
    let mut mcp = Mcp::spawn(cmd);
    mcp.handshake();
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let listed = mcp.recv(2, Duration::from_secs(20));
    let names = listed["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(mcp.finish(), Some(0));
    names
}

#[test]
fn mcp_profiles_choose_the_tool_list_and_the_flag_beats_the_environment() {
    let nine = [
        "look",
        "read_text",
        "press",
        "type",
        "key",
        "open",
        "scroll",
        "wait",
        "approve",
    ];
    let sb = Sandbox::new("profiles");
    assert_eq!(listed_names(sb.command(&["mcp"])), ["computer"]);
    assert_eq!(
        listed_names(sb.command(&["mcp", "--profile", "full"])),
        ["computer"]
    );
    assert_eq!(
        listed_names(sb.command(&["mcp", "--profile", "simple"])),
        nine
    );
    let vision = listed_names(sb.command(&["mcp", "--profile=simple-vision"]));
    assert_eq!(vision[..9], nine);
    assert_eq!(vision[9..], ["screenshot", "click_at"]);
    let mut by_env = sb.command(&["mcp"]);
    by_env.env("HEKATON_MCP_PROFILE", "simple");
    assert_eq!(listed_names(by_env), nine);
    let mut flag_wins = sb.command(&["mcp", "--profile", "full"]);
    flag_wins.env("HEKATON_MCP_PROFILE", "simple");
    assert_eq!(listed_names(flag_wins), ["computer"]);
}

#[test]
fn an_unknown_profile_exits_2_with_the_usage_line() {
    let sb = Sandbox::new("badprofile");
    for mut cmd in [
        sb.command(&["mcp", "--profile", "huge"]),
        {
            let mut c = sb.command(&["mcp"]);
            c.env("HEKATON_MCP_PROFILE", "huge");
            c
        },
        sb.command(&["mcp", "--profile"]),
    ] {
        let out = cmd.stdin(Stdio::null()).output().expect("run hekaton mcp");
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("usage: hekaton mcp [--profile full|simple|simple-vision]"),
            "{err}"
        );
    }
}
