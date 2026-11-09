//! `hekaton mcp`: the MCP stdio server (Python `shim.py`).
//!
//! Thin and non-elevated. It advertises ONE tool, `computer`, exactly as the Python shim does, forwards
//! each call to the daemon over HTTP (which keeps the elevated-daemon split on Windows), and turns the
//! answer back into MCP content. It starts the daemon on demand when nothing answers and retries once.
//! A reaper thread makes it leave when its host is gone or has replaced it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekaton_core::approval::{CARD_WAIT, Offer, card_message};
use hekaton_core::mcp::{
    self, Block, Reply, ShimDefaults, TOOL_NAME, build_payload, reply_and_approval, timeout_message,
};
use hekaton_core::simple::{self as simple_mode, Profile, Simple};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequestParams,
    ElicitationAction, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::{ElicitationMode, Peer, RequestContext, ServiceError};
use rmcp::{ErrorData as McpError, RoleServer, ServiceExt};
use serde_json::{Map, Value, json};

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
        hekaton_core::tool_schema::served_description(),
        Arc::new(match hekaton_core::tool_schema::served_schema() {
            Value::Object(m) => m,
            _ => mcp::input_schema(),
        }),
    )
}

/// One trip to the daemon: the answer as MCP content, and the approval the daemon offered when a
/// step stopped for a yes from the owner.
pub struct Exchange {
    /// The answer, exactly as it has always been (a stopped step reads as its refusal).
    pub reply: Reply,
    /// A single-use token and the card sentence, only when the call said it could show a card.
    pub offer: Option<Offer>,
}

impl Exchange {
    fn plain(reply: Reply) -> Self {
        Self { reply, offer: None }
    }
}

/// The daemon, as far as the card loop cares: send one payload, get one [`Exchange`] back.
pub trait Daemon: Send + Sync + 'static {
    /// One request to `/computer` with its start-on-demand and retry, blocking until it is answered.
    fn exchange(&self, payload: &Value) -> Exchange;

    /// Record a card or its answer in the audit log. Only called on the approval path.
    fn note(&self, _action: &str, _args: &Value, _ok: bool) {}
}

impl Daemon for DaemonLink {
    fn exchange(&self, payload: &Value) -> Exchange {
        match self.post(TOOL_NAME, payload) {
            // a stopped step is a failed call, so the daemon answers 400: read the offer anyway
            Ok(reply) => {
                let (reply, offer) = reply_and_approval(TOOL_NAME, reply.status, &reply.body);
                Exchange {
                    reply,
                    offer: offer.as_ref().and_then(Offer::from_json),
                }
            }
            Err(Failure::Timeout) => Exchange::plain(Reply::error(timeout_message(TOOL_NAME))),
            Err(Failure::NoDaemon) => Exchange::plain(Reply::error(format!(
                "hekaton daemon unreachable and could not be started automatically. Start it with: {}.",
                self.start_hint()
            ))),
            Err(Failure::Io(why)) => {
                Exchange::plain(Reply::error(format!("hekaton {TOOL_NAME} failed: {why}")))
            }
        }
    }

    fn note(&self, action: &str, args: &Value, ok: bool) {
        hekaton_core::audit::log_action(self.paths(), action, args, "", ok);
    }
}

/// One MCP call to the daemon and back: the payload the Python shim builds, the request with its
/// start-on-demand and one retry, and the answer as MCP content. Never asks for a card.
pub fn forward(link: &DaemonLink, defaults: &ShimDefaults, args: &Map<String, Value>) -> Reply {
    match build_payload(args, defaults) {
        Ok(payload) => link.exchange(&payload).reply,
        Err(e) => Reply::error(format!("Error executing tool {TOOL_NAME}: {e}")),
    }
}

// ------------------------------------------------------------------------------ approval cards

/// How the owner answered a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Accepted the form with "Allow".
    Allow,
    /// Chose "Deny", or declined or cancelled the form.
    Deny,
    /// Nobody answered in [`CARD_WAIT`].
    TimedOut,
    /// The client could not show it (an error, or it went away).
    Unavailable,
}

impl Decision {
    /// The word for the log.
    pub fn word(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::TimedOut => "timeout",
            Decision::Unavailable => "unavailable",
        }
    }
}

/// The card's form: one choice, Deny until the owner picks otherwise. MCP form mode allows only flat
/// primitive fields, so this is a single string with two allowed values.
pub fn card_schema() -> Value {
    json!({
        "type": "object",
        "title": "Hekaton needs your OK",
        "properties": {
            "decision": {
                "type": "string",
                "title": "Allow this one action?",
                "description": "Allow runs only this exact action, once.",
                "enum": ["Allow", "Deny"],
                "default": "Deny"
            }
        },
        "required": ["decision"]
    })
}

/// The `elicitation/create` params for a card.
pub fn card_params(offer: &Offer) -> Option<ElicitRequestParams> {
    serde_json::from_value(json!({
        "mode": "form",
        "message": card_message(&offer.summary),
        "requestedSchema": card_schema(),
    }))
    .ok()
}

/// The owner's answer from the elicitation result: only an accepted form that says "Allow" is a yes.
pub fn decision_from(action: &ElicitationAction, content: Option<&Value>) -> Decision {
    match action {
        ElicitationAction::Accept => {
            let said = content
                .and_then(|c| c.get("decision"))
                .and_then(Value::as_str);
            if said == Some("Allow") {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }
        _ => Decision::Deny,
    }
}

/// Something that can put a card in front of the owner.
pub trait Asker: Send + Sync {
    /// Show `offer` and wait for the answer.
    fn ask(&self, offer: &Offer) -> impl std::future::Future<Output = Decision> + Send;
}

/// Asks through the MCP client that is calling: `elicitation/create`, form mode.
pub struct PeerAsker(pub Peer<RoleServer>);

impl PeerAsker {
    /// Has the client declared the elicitation capability (form mode)?
    pub fn supported(peer: &Peer<RoleServer>) -> bool {
        peer.supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
    }
}

impl Asker for PeerAsker {
    async fn ask(&self, offer: &Offer) -> Decision {
        let Some(params) = card_params(offer) else {
            return Decision::Unavailable;
        };
        match self
            .0
            .create_elicitation_with_timeout(params, Some(CARD_WAIT))
            .await
        {
            Ok(result) => decision_from(&result.action, result.content.as_ref()),
            Err(ServiceError::Timeout { .. }) => Decision::TimedOut,
            Err(_) => Decision::Unavailable,
        }
    }
}

/// Cards one call may show. A batch with this many risky steps is not a batch to wave through.
const MAX_CARDS_PER_CALL: usize = 8;

/// What the model reads after the owner said no.
pub const DECLINED_NOTE: &str = "The owner declined this in the approval card, so it was not done. Do not retry it or look for another way to do it; if it still matters, ask them in chat.";

/// What the model reads when the card went unanswered.
pub const TIMED_OUT_NOTE: &str = "The approval card got no answer within 120 s, so this was not done. Do not retry it; ask the owner in chat if it matters.";

/// One call, with approvals. `payload` goes to the daemon marked `approvable`: "card" when `asker`
/// can show one, else "chat" (a refusal that the owner can lift says "Needs the owner's OK" with a code). When
/// the daemon offers a card, the owner is asked; on Allow the same payload goes again with the token and
/// the daemon carries on from that step. Every other outcome leaves the first answer, the plain
/// refusal, as the result (plus a line telling the model not to retry when the owner said no).
pub async fn run_call<A: Asker>(
    daemon: &Arc<dyn Daemon>,
    asker: Option<&A>,
    mut payload: Value,
) -> Reply {
    if let Some(map) = payload.as_object_mut() {
        let mode = if asker.is_some() { "card" } else { "chat" };
        map.insert("approvable".into(), Value::from(mode));
    }
    let mut shown = 0;
    loop {
        let sent = payload.clone();
        let d = Arc::clone(daemon);
        // the daemon call blocks for as long as the action takes; keep the MCP loop free
        let exchange = tokio::task::spawn_blocking(move || guarded_exchange(|| d.exchange(&sent)))
            .await
            .unwrap_or_else(|e| Exchange::plain(Reply::error(format!("hekaton shim failed: {e}"))));
        let (Some(asker), Some(offer)) = (asker, exchange.offer.as_ref()) else {
            return exchange.reply;
        };
        if shown >= MAX_CARDS_PER_CALL {
            return exchange.reply;
        }
        shown += 1;
        let about = json!({"kind": offer.kind, "step": offer.step, "card": offer.log_summary});
        daemon.note("approval_card", &about, true);
        let asked = Instant::now();
        let decision = asker.ask(offer).await;
        let answer = json!({
            "kind": offer.kind,
            "step": offer.step,
            "answer": decision.word(),
            "waited_ms": u64::try_from(asked.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
        daemon.note("approval_answer", &answer, decision == Decision::Allow);
        match decision {
            Decision::Allow => {
                if let Some(map) = payload.as_object_mut() {
                    map.insert("approval_token".into(), Value::from(offer.token.clone()));
                }
            }
            Decision::Deny => return with_note(exchange.reply, DECLINED_NOTE),
            Decision::TimedOut => return with_note(exchange.reply, TIMED_OUT_NOTE),
            Decision::Unavailable => return exchange.reply,
        }
    }
}

fn guarded_exchange(call: impl FnOnce() -> Exchange) -> Exchange {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(e) => e,
        Err(payload) => Exchange::plain(Reply::error(format!(
            "hekaton shim failed: internal error: {}",
            hekaton_core::daemon::panic_message(payload.as_ref())
        ))),
    }
}

fn with_note(mut reply: Reply, note: &str) -> Reply {
    reply.blocks.push(Block::Text(note.to_string()));
    reply
}

/// The payload for a call, or the error reply when an argument has the wrong type. A panic building
/// it is an error reply too.
fn guarded_payload(defaults: &ShimDefaults, args: &Map<String, Value>) -> Result<Value, Reply> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_payload(args, defaults)
    })) {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(e)) => Err(Reply::error(format!(
            "Error executing tool {TOOL_NAME}: {e}"
        ))),
        Err(payload) => Err(Reply::error(format!(
            "hekaton shim failed: internal error: {}",
            hekaton_core::daemon::panic_message(payload.as_ref())
        ))),
    }
}

/// Run one tool call, turning a panic into an error reply so the MCP server keeps answering. Nothing
/// in `forward` is meant to panic; this is the net under it (and under `build_payload`).
pub fn guarded(call: impl FnOnce() -> Reply) -> Reply {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(reply) => reply,
        Err(payload) => Reply::error(format!(
            "hekaton shim failed: internal error: {}",
            hekaton_core::daemon::panic_message(payload.as_ref())
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

/// One simple-mode call: translate it, run it through the same guarded path as a `computer` call
/// ([`run_call`]: approval, `only_apps`, audit and the owner card all unchanged), and shape the reply.
/// An acting call is followed by a `read`, so its reply carries a fresh numbered list.
pub async fn run_simple<A: Asker>(
    daemon: &Arc<dyn Daemon>,
    asker: Option<&A>,
    simple: &Simple,
    defaults: &ShimDefaults,
    session_id: &str,
    name: &str,
    args: &Map<String, Value>,
) -> Reply {
    let call = match simple.prepare(name, args) {
        Ok(call) => call,
        Err(reply) => return reply,
    };
    let payload = |mut m: Map<String, Value>| {
        m.entry("session_id")
            .or_insert_with(|| Value::String(session_id.to_string()));
        guarded_payload(defaults, &m)
    };
    let main = match payload(call.args.clone()) {
        Ok(p) => p,
        Err(reply) => return reply,
    };
    let reply = run_call(daemon, asker, main).await;
    simple.remember(&call, &reply);
    match simple.finish(&call, reply) {
        Ok(done) => done,
        Err(outcome) => {
            let list = match payload(simple_mode::list_call()) {
                Ok(p) => run_call(daemon, asker, p).await,
                Err(reply) => reply,
            };
            simple.with_list(&call, &outcome, &list)
        }
    }
}

/// The simple tools as the MCP tool list.
pub fn simple_tools(profile: Profile) -> Vec<Tool> {
    profile
        .definitions()
        .into_iter()
        .filter_map(|d| {
            let schema = d.get("inputSchema")?.as_object()?.clone();
            Some(Tool::new(
                d.get("name")?.as_str()?.to_string(),
                d.get("description")?.as_str()?.to_string(),
                Arc::new(schema),
            ))
        })
        .collect()
}

/// The server handler.
pub struct Shim {
    simple: Option<Simple>,
    daemon: Arc<dyn Daemon>,
    defaults: ShimDefaults,
    activity: Activity,
    session_id: String,
    calls: Arc<tokio::sync::Semaphore>,
}

impl Shim {
    /// A shim forwarding over `link`, adding `defaults` to every call.
    pub fn new(link: Arc<DaemonLink>, defaults: ShimDefaults, activity: Activity) -> Self {
        Self::over(link, defaults, activity)
    }

    /// A shim over any [`Daemon`].
    pub fn over(daemon: Arc<dyn Daemon>, defaults: ShimDefaults, activity: Activity) -> Self {
        let unique = hekaton_core::token::generate_token().unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
                .to_string()
        });
        Self {
            simple: None,
            daemon,
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

    /// Serve `profile`'s tools (the full profile is the default and changes nothing).
    pub fn with_profile(mut self, profile: Profile) -> Self {
        self.simple = profile.is_simple().then(|| Simple::new(profile));
        self
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
        Ok(ListToolsResult::with_all_items(match &self.simple {
            Some(simple) => simple_tools(simple.profile()),
            None => vec![tool_definition()],
        }))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.activity.touch();
        let reply = if let Some(simple) = &self.simple {
            match self.calls.clone().try_acquire_owned() {
                Err(_) => Reply::error(
                    "Hands is busy: four calls are already running; request was not executed",
                ),
                Ok(_permit) => {
                    let args = request.arguments.unwrap_or_default();
                    let (daemon, defaults, id) = (&self.daemon, &self.defaults, &self.session_id);
                    if PeerAsker::supported(&context.peer) {
                        let asker = PeerAsker(context.peer.clone());
                        run_simple(
                            daemon,
                            Some(&asker),
                            simple,
                            defaults,
                            id,
                            &request.name,
                            &args,
                        )
                        .await
                    } else {
                        run_simple::<PeerAsker>(
                            daemon,
                            None,
                            simple,
                            defaults,
                            id,
                            &request.name,
                            &args,
                        )
                        .await
                    }
                }
            }
        } else if request.name != TOOL_NAME {
            Reply::error(format!("Unknown tool: {}", request.name))
        } else {
            let args = self.session_args(request.arguments.unwrap_or_default());
            match (
                self.calls.clone().try_acquire_owned(),
                guarded_payload(&self.defaults, &args),
            ) {
                (Err(_), _) => Reply::error(
                    "Hands is busy: four calls are already running; request was not executed",
                ),
                (Ok(_), Err(reply)) => reply,
                (Ok(_permit), Ok(payload)) => {
                    // A client that declared elicitation gets a card instead of a flat refusal for
                    // the steps the owner can approve; any other client gets a chat code.
                    if PeerAsker::supported(&context.peer) {
                        let asker = PeerAsker(context.peer.clone());
                        run_call(&self.daemon, Some(&asker), payload).await
                    } else {
                        run_call::<PeerAsker>(&self.daemon, None, payload).await
                    }
                }
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
        .name("hekaton-reaper".into())
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
/// to read it from (an OS hekaton cannot drive).
pub fn reaper_for(
    backend: Arc<dyn hekaton_core::backend::Backend>,
    cfg: &hekaton_core::config::Config,
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
    use hekaton_core::paths::Paths;
    use serde_json::json;

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
            Paths::in_dir(std::env::temp_dir().join("hk-shim-none")),
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
            Some(hekaton_core::tool_schema::served_description())
        );
        assert_eq!(
            Value::Object((*t.input_schema).clone()),
            hekaton_core::tool_schema::served_schema()
        );
    }

    #[test]
    fn serialised_the_tool_matches_the_python_list_tools_entry() {
        let ours = serde_json::to_value(tool_definition()).unwrap();
        // 2026-10-28: the definition left the Python one behind (Windows/macOS, sessions, budgets);
        // tests/parity/python_list_tools.json stays as history.
        assert_eq!(
            ours,
            hekaton_core::tool_schema::tool_definition(),
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

    // ------------------------------------------------------------------ approval cards

    use std::collections::VecDeque;

    const REFUSAL: &str = "refused: \"Send\" looks irreversible (send/submit/pay/delete/etc.); pass allow_irreversible=true if the user really wants this";

    fn test_offer() -> Offer {
        Offer {
            token: "tok1".into(),
            summary: "Click \"Send\" in Mail".into(),
            log_summary: "Click \"Send\" in Mail".into(),
            kind: "irreversible_button".into(),
            step: 1,
        }
    }

    /// A daemon that stops with a card offer until the payload carries the token.
    #[derive(Default)]
    struct FakeDaemon {
        payloads: Mutex<Vec<Value>>,
        notes: Mutex<Vec<(String, Value)>>,
    }

    impl Daemon for FakeDaemon {
        fn exchange(&self, payload: &Value) -> Exchange {
            self.payloads.lock().unwrap().push(payload.clone());
            if payload.get("approval_token").and_then(Value::as_str) == Some("tok1") {
                Exchange::plain(Reply {
                    blocks: vec![Block::Text("clicked Send".into())],
                    is_error: false,
                })
            } else {
                let offer = (payload.get("approvable").and_then(Value::as_str) == Some("card"))
                    .then(test_offer);
                Exchange {
                    reply: Reply {
                        blocks: vec![Block::Text(REFUSAL.into())],
                        is_error: true,
                    },
                    offer,
                }
            }
        }

        fn note(&self, action: &str, args: &Value, _ok: bool) {
            self.notes
                .lock()
                .unwrap()
                .push((action.to_string(), args.clone()));
        }
    }

    struct FixedAsker(Mutex<VecDeque<Decision>>, Mutex<Vec<String>>);

    impl FixedAsker {
        fn saying(d: &[Decision]) -> Self {
            Self(Mutex::new(d.iter().copied().collect()), Mutex::default())
        }
    }

    impl Asker for FixedAsker {
        async fn ask(&self, offer: &Offer) -> Decision {
            self.1.lock().unwrap().push(offer.summary.clone());
            self.0.lock().unwrap().pop_front().unwrap_or(Decision::Deny)
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn texts(r: &Reply) -> Vec<String> {
        r.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.clone()),
                Block::Image { .. } => None,
            })
            .collect()
    }

    fn click_send() -> Value {
        json!({"action": "click_text", "text": "Send"})
    }

    #[test]
    fn allow_resends_with_the_token_and_logs_card_and_answer() {
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let asker = FixedAsker::saying(&[Decision::Allow]);
        let r = rt().block_on(run_call(&daemon, Some(&asker), click_send()));
        assert_eq!(texts(&r), ["clicked Send"]);
        let sent = fake.payloads.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["approvable"], "card");
        assert!(sent[0].get("approval_token").is_none());
        assert_eq!(sent[1]["approval_token"], "tok1");
        assert_eq!(sent[1]["text"], "Send");
        assert_eq!(
            asker.1.lock().unwrap().as_slice(),
            ["Click \"Send\" in Mail"]
        );
        let notes = fake.notes.lock().unwrap().clone();
        assert_eq!(notes[0].0, "approval_card");
        assert_eq!(notes[1].0, "approval_answer");
        assert_eq!(notes[1].1["answer"], "allow");
    }

    #[test]
    fn deny_and_timeout_keep_the_refusal_and_add_a_do_not_retry_note() {
        for (d, note) in [
            (Decision::Deny, DECLINED_NOTE),
            (Decision::TimedOut, TIMED_OUT_NOTE),
        ] {
            let fake = Arc::new(FakeDaemon::default());
            let daemon: Arc<dyn Daemon> = fake.clone();
            let asker = FixedAsker::saying(&[d]);
            let r = rt().block_on(run_call(&daemon, Some(&asker), click_send()));
            assert_eq!(texts(&r), [REFUSAL, note], "{d:?}");
            assert!(r.is_error);
            assert_eq!(fake.payloads.lock().unwrap().len(), 1, "no resend");
            assert_eq!(fake.notes.lock().unwrap()[1].1["answer"], d.word());
        }
    }

    #[test]
    fn an_unavailable_card_leaves_the_refusal_exactly_as_it_was() {
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let asker = FixedAsker::saying(&[Decision::Unavailable]);
        let r = rt().block_on(run_call(&daemon, Some(&asker), click_send()));
        assert_eq!(texts(&r), [REFUSAL]);
        assert_eq!(fake.payloads.lock().unwrap().len(), 1);
    }

    #[test]
    fn without_an_asker_the_call_asks_for_a_chat_code_and_shows_no_card() {
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let r = rt().block_on(run_call::<FixedAsker>(&daemon, None, click_send()));
        assert_eq!(texts(&r), [REFUSAL]);
        assert_eq!(fake.payloads.lock().unwrap()[0]["approvable"], "chat");
        assert!(fake.notes.lock().unwrap().is_empty());
    }

    #[test]
    fn a_batch_stops_asking_after_the_card_cap() {
        // a daemon that offers a card on every trip, token or not
        struct Always(Mutex<usize>);
        impl Daemon for Always {
            fn exchange(&self, _payload: &Value) -> Exchange {
                *self.0.lock().unwrap() += 1;
                Exchange {
                    reply: Reply::error(REFUSAL),
                    offer: Some(test_offer()),
                }
            }
        }
        let always = Arc::new(Always(Mutex::new(0)));
        let daemon: Arc<dyn Daemon> = always.clone();
        let asker = FixedAsker::saying(&[Decision::Allow; 20]);
        let r = rt().block_on(run_call(&daemon, Some(&asker), click_send()));
        assert_eq!(texts(&r), [REFUSAL]);
        assert_eq!(asker.1.lock().unwrap().len(), MAX_CARDS_PER_CALL);
        assert_eq!(*always.0.lock().unwrap(), MAX_CARDS_PER_CALL + 1);
    }

    #[test]
    fn only_an_accepted_allow_is_a_yes() {
        let allow = json!({"decision": "Allow"});
        let deny = json!({"decision": "Deny"});
        assert_eq!(
            decision_from(&ElicitationAction::Accept, Some(&allow)),
            Decision::Allow
        );
        assert_eq!(
            decision_from(&ElicitationAction::Accept, Some(&deny)),
            Decision::Deny
        );
        assert_eq!(
            decision_from(&ElicitationAction::Accept, None),
            Decision::Deny
        );
        assert_eq!(
            decision_from(&ElicitationAction::Decline, Some(&allow)),
            Decision::Deny
        );
        assert_eq!(
            decision_from(&ElicitationAction::Cancel, Some(&allow)),
            Decision::Deny
        );
    }

    #[test]
    fn the_card_is_a_form_with_an_allow_deny_choice_defaulting_to_deny() {
        let p = serde_json::to_value(card_params(&test_offer()).unwrap()).unwrap();
        assert_eq!(p["mode"], "form");
        assert_eq!(p["message"], "Hekaton wants to: Click \"Send\" in Mail");
        assert_eq!(
            p["requestedSchema"]["properties"]["decision"]["enum"],
            json!(["Allow", "Deny"])
        );
        assert_eq!(
            p["requestedSchema"]["properties"]["decision"]["default"],
            "Deny"
        );
    }

    // A real MCP client on the other end of a pipe: it declares (or not) the elicitation capability,
    // answers the card the way the test says, and reports what the tool call returned.
    struct Run {
        result: Value,
        payloads: Vec<Value>,
        cards: Vec<Value>,
    }

    fn drive(capabilities: Value, answer: Option<Value>) -> Run {
        drive_args(capabilities, answer, click_send())
    }

    fn drive_args(capabilities: Value, answer: Option<Value>, arguments: Value) -> Run {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let shim = Shim::over(daemon, defaults(), Activity::new());
        let (cli, srv) = tokio::io::duplex(1 << 16);
        let (srv_r, srv_w) = tokio::io::split(srv);
        let (cli_r, mut cli_w) = tokio::io::split(cli);
        let (result, cards) = rt().block_on(async move {
            let running = tokio::spawn(async move { shim.serve((srv_r, srv_w)).await });
            let mut lines = BufReader::new(cli_r).lines();
            let send = |v: Value| {
                let mut b = v.to_string().into_bytes();
                b.push(b'\n');
                b
            };
            let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": capabilities,
                "clientInfo": {"name": "fake", "version": "0"}}});
            cli_w.write_all(&send(init)).await.unwrap();
            let _ = lines.next_line().await.unwrap().unwrap();
            cli_w
                .write_all(&send(
                    json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                ))
                .await
                .unwrap();
            let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "computer", "arguments": arguments}});
            cli_w.write_all(&send(call)).await.unwrap();
            let mut cards = Vec::new();
            let result = loop {
                let line = lines.next_line().await.unwrap().expect("server closed");
                let v: Value = serde_json::from_str(&line).unwrap();
                if v["method"] == "elicitation/create" {
                    cards.push(v["params"].clone());
                    let reply = match &answer {
                        Some(a) => json!({"jsonrpc": "2.0", "id": v["id"], "result": a}),
                        None => json!({"jsonrpc": "2.0", "id": v["id"],
                                       "error": {"code": -32601, "message": "no"}}),
                    };
                    cli_w.write_all(&send(reply)).await.unwrap();
                } else if v["id"] == 2 {
                    break v["result"].clone();
                }
            };
            drop(cli_w);
            running.abort();
            (result, cards)
        });
        let payloads = fake.payloads.lock().unwrap().clone();
        Run {
            result,
            payloads,
            cards,
        }
    }

    fn result_texts(v: &Value) -> Vec<String> {
        v["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_client_with_elicitation_gets_a_card_and_allow_carries_on() {
        let r = drive(
            json!({"elicitation": {"form": {}}}),
            Some(json!({"action": "accept", "content": {"decision": "Allow"}})),
        );
        assert_eq!(r.cards.len(), 1);
        assert_eq!(r.cards[0]["mode"], "form");
        assert_eq!(
            r.cards[0]["message"],
            "Hekaton wants to: Click \"Send\" in Mail"
        );
        assert_eq!(result_texts(&r.result), ["clicked Send"]);
        assert_eq!(r.payloads.len(), 2);
        assert_eq!(r.payloads[1]["approval_token"], "tok1");
    }

    #[test]
    fn a_declined_card_returns_the_refusal_and_the_do_not_retry_note() {
        for answer in [
            json!({"action": "decline"}),
            json!({"action": "cancel"}),
            json!({"action": "accept", "content": {"decision": "Deny"}}),
        ] {
            let r = drive(json!({"elicitation": {"form": {}}}), Some(answer));
            assert_eq!(r.cards.len(), 1);
            assert_eq!(result_texts(&r.result), [REFUSAL, DECLINED_NOTE]);
            assert_eq!(r.result["isError"], true);
            assert_eq!(r.payloads.len(), 1, "no resend");
        }
    }

    #[test]
    fn a_card_the_client_cannot_show_leaves_the_plain_refusal() {
        let r = drive(json!({"elicitation": {"form": {}}}), None);
        assert_eq!(r.cards.len(), 1);
        assert_eq!(result_texts(&r.result), [REFUSAL]);
        assert_eq!(r.payloads.len(), 1);
    }

    #[test]
    fn a_client_without_elicitation_gets_no_card_and_a_chat_code_call() {
        let r = drive(json!({}), None);
        assert!(r.cards.is_empty());
        assert_eq!(result_texts(&r.result), [REFUSAL]);
        assert_eq!(r.payloads[0]["approvable"], "chat");
        assert!(
            r.payloads[0]["session_id"]
                .as_str()
                .unwrap()
                .starts_with("hands-")
        );
    }

    #[test]
    fn the_approval_code_reaches_the_daemon_and_a_models_token_does_not() {
        let r = drive_args(
            json!({}),
            None,
            json!({"action": "click_text", "text": "Send", "approval_code": "K7QM",
                   "approval_token": "tok1", "approvable": "card"}),
        );
        assert_eq!(r.payloads[0]["approval_code"], "K7QM");
        assert!(r.payloads[0].get("approval_token").is_none());
        assert_eq!(r.payloads[0]["approvable"], "chat");
        // and inside a batch it still rides top level
        let r = drive_args(
            json!({}),
            None,
            json!({"actions": [{"action": "click_text", "text": "Send"}], "approval_code": "K7QM"}),
        );
        assert_eq!(r.payloads[0]["approval_code"], "K7QM");
    }

    // ------------------------------------------------------------------ simple mode

    /// A daemon that refuses a press of "Send" until it sees approval_code K7QM, does everything
    /// else, and answers a `read` with a three-item list.
    #[derive(Default)]
    struct SimpleDaemon {
        payloads: Mutex<Vec<Value>>,
    }

    impl Daemon for SimpleDaemon {
        fn exchange(&self, payload: &Value) -> Exchange {
            self.payloads.lock().unwrap().push(payload.clone());
            let action = payload["action"].as_str().unwrap_or("");
            let text = payload["text"].as_str().unwrap_or("");
            let said = match action {
                "read" => {
                    "window Notes (Notes)\n1 Button \"Save\"\n2 Edit \"Body\"\n3 Button \"Send\""
                        .to_string()
                }
                "click_text" if text == "Send" && payload.get("approval_code").is_none() => {
                    return Exchange::plain(Reply::error(
                        "Needs the owner's OK. Show them this in chat: \"Hekaton wants to: Click Send\". Ask them to run \"hekaton approve K7QM\" then repeat the call with approval_code \"K7QM\".",
                    ));
                }
                "press" if text == "99" => {
                    return Exchange::plain(Reply::error(
                        "no element 99 in the last read of that window; read it first",
                    ));
                }
                "press" => format!("pressed {text} Button 'Save'\nscreen settled in 90 ms"),
                _ => format!("did {action}\nscreen settled in 90 ms"),
            };
            Exchange::plain(Reply {
                blocks: vec![
                    Block::Text(said),
                    Block::Image {
                        data: "AAAA".into(),
                        mime: "image/png".into(),
                    },
                ],
                is_error: false,
            })
        }
    }

    fn run_simple_tool(
        daemon: &Arc<SimpleDaemon>,
        simple: &Simple,
        name: &str,
        args: Value,
    ) -> Reply {
        let d: Arc<dyn Daemon> = daemon.clone();
        rt().block_on(run_simple::<FixedAsker>(
            &d,
            None,
            simple,
            &defaults(),
            "hands-test",
            name,
            args.as_object().unwrap(),
        ))
    }

    fn actions_sent(daemon: &SimpleDaemon) -> Vec<String> {
        daemon
            .payloads
            .lock()
            .unwrap()
            .iter()
            .map(|p| p["action"].as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn an_acting_call_answers_with_one_line_and_a_fresh_list_and_no_picture() {
        let daemon = Arc::new(SimpleDaemon::default());
        let simple = Simple::new(Profile::Simple);
        let r = run_simple_tool(&daemon, &simple, "press", json!({"target": "2"}));
        assert!(!r.is_error);
        assert!(
            r.blocks.iter().all(|b| matches!(b, Block::Text(_))),
            "no image"
        );
        assert_eq!(
            texts(&r),
            [
                "pressed 2 Button 'Save'\nwindow Notes (Notes)\n1 Button \"Save\"\n2 Edit \"Body\"\n3 Button \"Send\""
            ]
        );
        assert_eq!(actions_sent(&daemon), ["press", "read"]);
        let sent = daemon.payloads.lock().unwrap().clone();
        assert_eq!(sent[0]["shot"], false);
        assert_eq!(
            sent[0]["approvable"], "chat",
            "same guarded path as computer"
        );
        assert_eq!(sent[1]["limit"], 40);
        assert!(
            sent[0]["session_id"]
                .as_str()
                .unwrap()
                .starts_with("hands-")
        );
    }

    #[test]
    fn look_is_the_list_alone() {
        let daemon = Arc::new(SimpleDaemon::default());
        let simple = Simple::new(Profile::Simple);
        let r = run_simple_tool(&daemon, &simple, "look", json!({"filter": "save"}));
        assert!(!r.is_error);
        assert!(texts(&r)[0].starts_with("window Notes (Notes)\n1 Button"));
        assert_eq!(actions_sent(&daemon), ["read"]);
        assert_eq!(daemon.payloads.lock().unwrap()[0]["filter"], "save");
    }

    #[test]
    fn a_failed_action_is_the_error_alone_with_a_next_line() {
        let daemon = Arc::new(SimpleDaemon::default());
        let simple = Simple::new(Profile::Simple);
        let r = run_simple_tool(&daemon, &simple, "press", json!({"target": 99}));
        assert!(r.is_error);
        let t = &texts(&r)[0];
        assert!(t.starts_with("no element 99"), "{t}");
        assert!(
            t.lines().last().unwrap().starts_with("Next: call look"),
            "{t}"
        );
        assert!(!t.contains("window Notes"), "no list after a failure");
        assert_eq!(actions_sent(&daemon), ["press"], "no read was sent");
    }

    #[test]
    fn approve_resends_the_refused_call_with_the_code_and_says_when_nothing_waits() {
        let daemon = Arc::new(SimpleDaemon::default());
        let simple = Simple::new(Profile::Simple);
        let none = run_simple_tool(&daemon, &simple, "approve", json!({"code": "K7QM"}));
        assert!(none.is_error);
        assert!(texts(&none)[0].contains("Nothing is waiting"));
        assert!(
            daemon.payloads.lock().unwrap().is_empty(),
            "nothing was sent"
        );

        let refused = run_simple_tool(&daemon, &simple, "press", json!({"target": "Send"}));
        assert!(refused.is_error);
        assert!(texts(&refused)[0].contains("Needs the owner's OK"));
        let ok = run_simple_tool(&daemon, &simple, "approve", json!({"code": "K7QM"}));
        assert!(!ok.is_error, "{ok:?}");
        let sent = daemon.payloads.lock().unwrap().clone();
        let again = &sent[1];
        assert_eq!(
            (again["action"].as_str(), again["text"].as_str()),
            (Some("click_text"), Some("Send"))
        );
        assert_eq!(again["approval_code"], "K7QM");
        assert!(texts(&ok)[0].contains("3 Button \"Send\""), "list follows");
        // used up
        let twice = run_simple_tool(&daemon, &simple, "approve", json!({"code": "K7QM"}));
        assert!(twice.is_error);
    }

    #[test]
    fn simple_profiles_list_their_tools_and_the_full_profile_is_unchanged() {
        let names = |p: Profile| -> Vec<String> {
            simple_tools(p)
                .into_iter()
                .map(|t| t.name.to_string())
                .collect()
        };
        assert_eq!(names(Profile::Simple).len(), 9);
        assert_eq!(names(Profile::SimpleVision).len(), 11);
        assert_eq!(names(Profile::Simple)[0], "look");
        assert!(simple_tools(Profile::Full).is_empty());
        let shim = Shim::new(Arc::new(dead_link()), defaults(), Activity::new());
        assert!(shim.simple.is_none());
        assert!(shim.with_profile(Profile::Full).simple.is_none());
    }
}
