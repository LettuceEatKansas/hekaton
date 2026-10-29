//! `winhands mcp`: the MCP stdio server (Python `winhands/shim.py`).
//!
//! Thin and non-elevated. It advertises ONE tool, `computer`, exactly as the Python shim does, forwards
//! each call to the daemon over HTTP (which keeps the elevated-daemon split on Windows), and turns the
//! answer back into MCP content. It starts the daemon on demand when nothing answers and retries once.
//! A reaper thread makes it leave when its host is gone or has replaced it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServiceExt};
use serde_json::{Map, Value};
use wh_core::mcp::{
    self, Block, Reply, ShimDefaults, TOOL_NAME, build_payload,
    reply_from_daemon, timeout_message,
};

use crate::link::{DaemonLink, Failure};
use crate::registry::{ProcTable, Reaper, Registry, ShimRecord, normcase, spawner_pid};

/// When the shim last handled an MCP request (a long call counts as activity until it ends).
#[derive(Clone)]
pub struct Activity(Arc<Mutex<Instant>>);

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    /// Starts at launch.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    /// A request arrived or finished.
    pub fn touch(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Time since the last request.
    pub fn idle(&self) -> Duration {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).elapsed()
    }
}

/// The one tool, as `list_tools` serves it.
pub fn tool_definition() -> Tool {
    Tool::new(
        TOOL_NAME,
        wh_core::tool_schema::served_description(),
        Arc::new(match wh_core::tool_schema::served_schema() {
            Value::Object(m) => m,
            _ => mcp::input_schema(),
        }),
    )
}

/// One MCP call to the daemon and back: the payload the Python shim builds, the request with its
/// start-on-demand and one retry, and the answer as MCP content.
pub fn forward(link: &DaemonLink, defaults: &ShimDefaults, args: &Map<String, Value>) -> Reply {
    let payload = match build_payload(args, defaults) {
        Ok(p) => p,
        Err(e) => return Reply::error(format!("Error executing tool {TOOL_NAME}: {e}")),
    };
    match link.post(TOOL_NAME, &payload) {
        Ok(reply) => reply_from_daemon(TOOL_NAME, reply.status, &reply.body),
        Err(Failure::Timeout) => Reply::error(timeout_message(TOOL_NAME)),
        Err(Failure::NoDaemon) => Reply::error(format!(
            "winhands daemon unreachable and could not be started automatically. Start it with: {}.",
            link.start_hint()
        )),
        Err(Failure::Io(why)) => Reply::error(format!("winhands {TOOL_NAME} failed: {why}")),
    }
}

/// Run one tool call, turning a panic into an error reply so the MCP server keeps answering. Nothing
/// in `forward` is meant to panic; this is the net under it (and under `build_payload`).
pub fn guarded(call: impl FnOnce() -> Reply) -> Reply {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(reply) => reply,
        Err(payload) => Reply::error(format!(
            "winhands shim failed: internal error: {}",
            wh_core::daemon::panic_message(payload.as_ref())
        )),
    }
}

fn to_result(reply: Reply) -> CallToolResult {
    let blocks = reply
        .blocks
        .into_iter()
        .map(|b| match b {
            Block::Text(t) => ContentBlock::text(t),
            Block::Image { data, mime } => ContentBlock::image(data, mime),
        })
        .collect();
    if reply.is_error {
        CallToolResult::error(blocks)
    } else {
        CallToolResult::success(blocks)
    }
}

/// The server handler.
pub struct Shim {
    link: Arc<DaemonLink>,
    defaults: ShimDefaults,
    activity: Activity,
    session_id: String,
    calls: Arc<tokio::sync::Semaphore>,
}

impl Shim {
    /// A shim forwarding over `link`, adding `defaults` to every call.
    pub fn new(link: Arc<DaemonLink>, defaults: ShimDefaults, activity: Activity) -> Self {
        let unique = wh_core::token::generate_token().unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
                .to_string()
        });
        Self {
            link,
            defaults,
            activity,
            session_id: format!(
                "hands-{}-{}",
                std::process::id(),
                &unique[..unique.len().min(32)]
            ),
            calls: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }

    fn session_args(&self, mut args: Map<String, Value>) -> Map<String, Value> {
        args.entry("session_id")
            .or_insert_with(|| Value::String(self.session_id.clone()));
        args
    }
}

impl ServerHandler for Shim {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(mcp::SERVER_NAME, env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.activity.touch();
        Ok(ListToolsResult::with_all_items(vec![tool_definition()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.activity.touch();
        let reply = if request.name != TOOL_NAME {
            Reply::error(format!("Unknown tool: {}", request.name))
        } else {
            let link = self.link.clone();
            let defaults = self.defaults.clone();
            let args = self.session_args(request.arguments.unwrap_or_default());
            let permit = self.calls.clone().try_acquire_owned();
            // the daemon call blocks for as long as the action takes; keep the MCP loop free
            match permit {
                Ok(permit) => tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    guarded(|| forward(&link, &defaults, &args))
                })
                .await
                .unwrap_or_else(|e| Reply::error(format!("winhands shim failed: {e}"))),
                Err(_) => Reply::error(
                    "Hands is busy: four calls are already running; request was not executed",
                ),
            }
        };
        self.activity.touch();
        Ok(to_result(reply).into())
    }
}

/// Everything the reaper thread needs, resolved once at start-up.
pub struct ReaperSetup {
    /// The decision logic and the records.
    pub reaper: Reaper,
    /// Reads the live process table.
    pub table: Box<dyn Fn() -> Option<ProcTable> + Send>,
    /// Shared with the handler.
    pub activity: Activity,
}

/// How often the reaper looks (`time.sleep(30)`).
pub const REAPER_PERIOD: Duration = Duration::from_secs(30);

/// Removes this shim's record when it exits normally (the reaper removes it when it exits abnormally).
pub struct Cleanup {
    registry: Registry,
    pid: u32,
}

impl Cleanup {
    /// Remove the record.
    pub fn run(&self) {
        self.registry.unregister(self.pid);
    }
}

/// Register this shim and start the reaper thread. Best effort: any failure only means this shim is
/// never the one chosen to leave.
pub fn start_reaper(setup: ReaperSetup) -> Cleanup {
    let ReaperSetup {
        reaper,
        table,
        activity,
    } = setup;
    let cleanup = Cleanup {
        registry: reaper.registry.clone(),
        pid: reaper.me,
    };
    if let Some(t) = table() {
        let created = t.created_of(reaper.me);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        reaper.registry.register(&ShimRecord {
            pid: reaper.me,
            spawner: spawner_pid(&t, reaper.me, &reaper.own_image),
            created,
            started,
        });
    }
    let _ = std::thread::Builder::new()
        .name("winhands-reaper".into())
        .spawn(move || {
            loop {
                std::thread::sleep(REAPER_PERIOD);
                let Some(t) = table() else {
                    continue; // never leave on a detection failure
                };
                if reaper.should_exit(&t, activity.idle()) {
                    reaper.registry.unregister(reaper.me);
                    std::process::exit(0);
                }
            }
        });
    cleanup
}

/// Build the reaper for this process over `backend`'s process table. `None` when there is no backend
/// to read it from (an OS winhands cannot drive).
pub fn reaper_for(
    backend: Arc<dyn wh_core::backend::Backend>,
    cfg: &wh_core::config::Config,
    activity: Activity,
) -> Option<ReaperSetup> {
    let exe = std::env::current_exe().ok()?;
    let own_name = exe.file_name()?.to_string_lossy().to_lowercase();
    let own_image = normcase(&exe.to_string_lossy());
    let reaper = Reaper {
        registry: Registry::new(cfg.paths().shims_dir()),
        me: std::process::id(),
        own_name,
        own_image,
        idle_limit: Duration::from_secs(u64::try_from(cfg.shim_idle_s()).unwrap_or(0)),
    };
    Some(ReaperSetup {
        reaper,
        table: Box::new(move || backend.process_table().ok().map(ProcTable::new)),
        activity,
    })
}

/// Serve MCP on stdin and stdout until the client hangs up.
pub fn serve_stdio(shim: Shim) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    runtime.block_on(async {
        let running = shim
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| format!("MCP start-up failed: {e}"))?;
        running
            .waiting()
            .await
            .map(|_| ())
            .map_err(|e| format!("MCP server stopped: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wh_core::paths::Paths;

    fn dead_link() -> DaemonLink {
        // a port nothing listens on, and no way to start anything
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        DaemonLink::new(
            "127.0.0.1",
            port,
            Paths::in_dir(std::env::temp_dir().join("wh-shim-none")),
            None,
            Duration::from_secs(1),
        )
    }

    fn defaults() -> ShimDefaults {
        ShimDefaults {
            coords: "pixels".into(),
            only_apps: Vec::new(),
        }
    }

    #[test]
    fn shim_sessions_are_stable_isolated_and_allow_explicit_override() {
        let first = Shim::new(Arc::new(dead_link()), defaults(), Activity::new());
        let second = Shim::new(Arc::new(dead_link()), defaults(), Activity::new());
        let a = first.session_args(Map::new());
        assert_eq!(a, first.session_args(Map::new()));
        assert_ne!(
            a["session_id"],
            second.session_args(Map::new())["session_id"]
        );
        let explicit = json!({"session_id": "operator-session", "action": "screenshot"});
        assert_eq!(
            first.session_args(explicit.as_object().unwrap().clone()),
            explicit.as_object().unwrap().clone()
        );
    }

    #[test]
    fn the_tool_is_the_python_tool() {
        let t = tool_definition();
        assert_eq!(t.name, "computer");
        assert_eq!(
            t.description.as_deref(),
            Some(wh_core::tool_schema::served_description())
        );
        assert_eq!(
            Value::Object((*t.input_schema).clone()),
            wh_core::tool_schema::served_schema()
        );
    }

    #[test]
    fn serialised_the_tool_matches_the_python_list_tools_entry() {
        let ours = serde_json::to_value(tool_definition()).unwrap();
        // 2026-10-28: the definition left the Python one behind (Windows/macOS, sessions, budgets);
        // tests/parity/python_list_tools.json stays as history.
        assert_eq!(
            ours,
            wh_core::tool_schema::tool_definition(),
            "name, description and inputSchema, and nothing else"
        );
    }

    #[test]
    fn a_bad_argument_is_an_error_reply_not_a_panic() {
        let r = forward(
            &dead_link(),
            &defaults(),
            json!({"coordinate": "no"}).as_object().unwrap(),
        );
        assert!(r.is_error);
        let Block::Text(t) = &r.blocks[0] else {
            panic!("text")
        };
        assert!(
            t.starts_with("Error executing tool computer:") && t.contains("coordinate"),
            "{t}"
        );
    }

    #[test]
    fn no_daemon_and_no_way_to_start_one_is_a_clear_error() {
        let r = forward(
            &dead_link(),
            &defaults(),
            json!({"action": "screenshot"}).as_object().unwrap(),
        );
        assert!(r.is_error);
        let Block::Text(t) = &r.blocks[0] else {
            panic!("text")
        };
        assert!(
            t.contains("unreachable") && t.contains("Start it with:"),
            "{t}"
        );
    }

    #[test]
    fn activity_measures_idle_time_and_touch_resets_it() {
        let a = Activity::new();
        std::thread::sleep(Duration::from_millis(30));
        assert!(a.idle() >= Duration::from_millis(25));
        a.touch();
        assert!(a.idle() < Duration::from_millis(25));
    }

    #[test]
    fn replies_become_mcp_results_with_the_error_flag() {
        let ok = serde_json::to_value(to_result(Reply {
            blocks: vec![
                Block::Text("hi".into()),
                Block::Image {
                    data: "AAAA".into(),
                    mime: "image/png".into(),
                },
            ],
            is_error: false,
        }))
        .unwrap();
        assert_eq!(ok["content"][0], json!({"type": "text", "text": "hi"}));
        assert_eq!(
            ok["content"][1],
            json!({"type": "image", "data": "AAAA", "mimeType": "image/png"})
        );
        assert_eq!(ok["isError"], false);
        let bad = serde_json::to_value(to_result(Reply::error("no"))).unwrap();
        assert_eq!(bad["isError"], true);
    }
}
