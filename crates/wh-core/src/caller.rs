//! Who is driving winhands right now. Port of `winhands/caller.py` plus `core.agent_host_refusal`.
//!
//! The daemon is a local HTTP server, so the process on the other end of the socket is the process
//! that is actually calling it. The backend names that process ([`Backend::peer_pid`]) and walks its
//! parents ([`Backend::process_table`], [`Backend::process_info`]). This exists so winhands can refuse
//! the one mistake the old hardcoded name list was really guarding against: clicking or typing into the
//! agent, terminal or editor that is driving it. Before 2026-09-24 that was a fixed list of process
//! names, which meant winhands could never be used to work in a terminal or in VS Code at all. Now the
//! refusal is caller-relative: only the caller's own windows and its ancestors' windows are off limits.
//!
//! Identification is done once per connection (the daemon keeps a [`Caller`] per connection) and never
//! per action, because the guard runs on every action. A [`Caller`] is passed into each call
//! explicitly: Python kept it in a thread-local, which is the same thing one connection per thread
//! gave it.

use std::collections::{BTreeSet, HashMap};

use crate::backend::{Backend, WindowRef};
use crate::config::Config;

/// How many ancestors a chain may hold.
///
/// 2026-09-24 live check: python <- python <- bash <- bash <- bash <- claude.exe used all six slots and
/// stopped before the app's other processes, so a keystroke into the agent's own window was allowed.
/// Real chains nest deeper than the design assumed.
pub const MAX_ANCESTORS: usize = 16;

/// Hosts that say nothing about which app is driving.
///
/// An app runs as several processes (an Electron agent has a UI process and the one that spawned the
/// shim). Matching a window by pid alone therefore misses the app's OTHER windows, so a name in the
/// chain protects that app's windows too. Generic hosts are excluded: refusing every python or shell
/// window would be the old blanket ban again. Terminals and editors are excluded on purpose (owner's
/// call 2026-09-24): they must stay usable as ordinary apps. Their own windows are still protected when
/// they drive winhands, because one of their processes owns every one of its windows, so the pid match
/// covers it.
pub const GENERIC_HOSTS: [&str; 30] = [
    "python.exe",
    "pythonw.exe",
    "bash.exe",
    "sh.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "conhost.exe",
    "node.exe",
    "winhands.exe",
    "py.exe",
    "uv.exe",
    "git.exe",
    "sudo.exe",
    "wsl.exe",
    "env.exe",
    "svchost.exe",
    "explorer.exe",
    "windowsterminal.exe",
    "openconsole.exe",
    "wt.exe",
    "code.exe",
    "code - insiders.exe",
    "devenv.exe",
    "idea64.exe",
    "pycharm64.exe",
    "cursor.exe",
    "alacritty.exe",
    "wezterm-gui.exe",
    "hyper.exe",
];

/// Processes that only RELAY a connection. When the process table names one of these as the owner of
/// the client socket, the real driver is on the far side of the relay and winhands cannot see it: a
/// netsh portproxy is owned by svchost.exe, a WSL connection surfaces as wslhost.exe / wslrelay.exe.
/// Treating the relay as the caller is worse than knowing nothing, because the guard would then
/// believe it knows who is calling and protect the wrong windows.
pub const PROXY_HOSTS: [&str; 5] = [
    "svchost.exe",
    "wslhost.exe",
    "wslrelay.exe",
    "wslservice.exe",
    "netsh.exe",
];

/// The fixed list the guard falls back to when the caller cannot be identified at all
/// (`core.AGENT_HOST_PROCESSES`). Never fail open: an unidentifiable caller is never allowed to type
/// into an agent window.
pub const AGENT_HOST_PROCESSES: [&str; 5] = [
    "claude.exe",
    "hermes.exe",
    "codex.exe",
    "windowsterminal.exe",
    "code.exe",
];

/// Did this connection genuinely arrive on loopback? Anything else, including an empty or unreadable
/// address, is not loopback: the safe answer when in doubt is "no".
pub fn is_loopback_peer(peer_host: &str) -> bool {
    let host = peer_host.trim().to_lowercase();
    if host.is_empty() {
        return false;
    }
    let host = host.strip_prefix("::ffff:").unwrap_or(&host);
    matches!(host, "127.0.0.1" | "::1")
}

/// The process driving winhands on one connection, plus its ancestors, nearest first. An empty chain
/// means the lookup failed; callers must then fall back to a name list, never to allowing everything.
///
/// `remote` records WHY the chain may be empty: the caller is not a local process at all (a remote
/// bind or a port proxy), so no lookup could have been right. The refusal message says so, and the
/// guard falls back to `protect_processes` only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caller {
    chain: Vec<(u32, String)>,
    remote: bool,
}

impl Caller {
    /// A caller from a chain of `(pid, exe name)` pairs, nearest first. Names are lowercased.
    pub fn new(chain: Vec<(u32, String)>, remote: bool) -> Self {
        Self {
            chain: chain
                .into_iter()
                .map(|(pid, name)| (pid, name.to_lowercase()))
                .collect(),
            remote,
        }
    }

    /// Nobody could be identified (Python `UNKNOWN`).
    pub fn unknown() -> Self {
        Self::default()
    }

    /// The caller is not a local process (Python `REMOTE`).
    pub fn remote() -> Self {
        Self {
            chain: Vec::new(),
            remote: true,
        }
    }

    /// Was the calling process identified?
    pub fn known(&self) -> bool {
        !self.chain.is_empty()
    }

    /// Is the caller declared remote?
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// The `(pid, name)` chain, nearest first.
    pub fn chain(&self) -> &[(u32, String)] {
        &self.chain
    }

    /// Every pid in the chain.
    pub fn pids(&self) -> BTreeSet<u32> {
        self.chain.iter().map(|(pid, _)| *pid).collect()
    }

    /// Names in the chain that identify an app, not a generic host.
    pub fn app_names(&self) -> BTreeSet<String> {
        self.chain
            .iter()
            .map(|(_, name)| name.to_lowercase())
            .filter(|name| !GENERIC_HOSTS.contains(&name.as_str()))
            .collect()
    }

    /// How `pid` relates to this caller, in words for a refusal message, or "" if unrelated.
    pub fn role(&self, pid: u32, name: &str) -> String {
        for (index, (owner, _)) in self.chain.iter().enumerate() {
            if *owner == pid {
                if index == 0 {
                    return "the process that is calling winhands right now".to_string();
                }
                let (first_pid, first_name) = &self.chain[0];
                return format!(
                    "a parent of the process calling winhands (pid {first_pid} {first_name})"
                );
            }
        }
        if !name.is_empty() && self.app_names().contains(&name.to_lowercase()) {
            return format!(
                "another process of {name}, the app driving winhands (one app runs several processes)"
            );
        }
        String::new()
    }

    /// `python.exe (pid 10) <- claude.exe (pid 12)`, or `unknown`.
    pub fn describe(&self) -> String {
        if self.chain.is_empty() {
            return "unknown".to_string();
        }
        self.chain
            .iter()
            .map(|(pid, name)| format!("{name} (pid {pid})"))
            .collect::<Vec<_>>()
            .join(" <- ")
    }
}

/// `[(pid, exe name), ...]` for a process and its ancestors, nearest first. Stops at
/// [`MAX_ANCESTORS`], on a pid it has already seen (cycle guard), and on a parent that started after its
/// child (a recycled pid, not a real ancestor). Python `process_chain`.
pub fn process_chain(backend: &dyn Backend, pid: u32) -> Vec<(u32, String)> {
    if pid == 0 {
        return Vec::new();
    }
    let Ok(table) = backend.process_table() else {
        return Vec::new();
    };
    let by_pid: HashMap<u32, (u32, String)> = table
        .into_iter()
        .map(|p| (p.pid, (p.parent_pid, p.exe_name.to_lowercase())))
        .collect();
    let created = |pid: u32| -> u64 {
        backend
            .process_info(pid)
            .ok()
            .flatten()
            .and_then(|p| p.started_at)
            .unwrap_or(0)
    };
    let mut chain: Vec<(u32, String)> = Vec::new();
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut current = pid;
    while current != 0 && !seen.contains(&current) && chain.len() < MAX_ANCESTORS {
        seen.insert(current);
        let (parent, name) = by_pid.get(&current).cloned().unwrap_or((0, String::new()));
        if name.is_empty() {
            break;
        }
        chain.push((current, name));
        if parent == 0 || seen.contains(&parent) {
            break;
        }
        let (child_created, parent_created) = (created(current), created(parent));
        if child_created != 0 && parent_created != 0 && parent_created > child_created {
            break;
        }
        current = parent;
    }
    chain
}

/// Identify the process on the other end of a connection. Returns an unknown caller on any failure.
///
/// `remote` is `config.remote_caller()`: the owner (or a non-loopback bind) has said the caller is not
/// a local process. Then a resolved pid is trusted only when the connection genuinely arrived on
/// loopback AND the resolved process is not a relay; anything else returns a remote caller, which the
/// guard reads as "caller-relative protection is off". Refusing to trust beats trusting wrongly.
///
/// A relay process ([`PROXY_HOSTS`]) is never the caller, remote flag or not: the driver is on the far
/// side of it. Python `identify`.
pub fn identify(
    backend: &dyn Backend,
    peer_port: u16,
    server_port: u16,
    peer_host: &str,
    remote: bool,
) -> Caller {
    let fallback = || {
        if remote {
            Caller::remote()
        } else {
            Caller::unknown()
        }
    };
    if remote && !is_loopback_peer(peer_host) {
        return Caller::remote();
    }
    let pid = if peer_port == 0 {
        0
    } else {
        match backend.peer_pid(peer_port, server_port) {
            Ok(pid) => pid.unwrap_or(0),
            Err(_) => return fallback(),
        }
    };
    let chain = process_chain(backend, pid);
    if let Some((_, first)) = chain.first()
        && PROXY_HOSTS.contains(&first.as_str())
    {
        return fallback();
    }
    if remote && chain.is_empty() {
        return Caller::remote();
    }
    Caller::new(chain, remote)
}

/// Why this window must not be clicked or typed into, or "" when it may be.
///
/// Three reasons, in order:
///
/// 1. its process is in the owner's permanent `protect_processes` config list;
/// 2. it belongs to the process driving winhands right now, or to one of that process's ancestors;
/// 3. the caller could not be identified at all, and the process is on the old fixed list
///    [`AGENT_HOST_PROCESSES`]. Never fail open: an unidentifiable caller keeps the old behavior.
///
/// Remote callers are the exception to 3. When the caller is not a local process, the local TCP table
/// cannot name it, so there is nothing caller-relative left to enforce and the fixed name list would
/// only ban the owner's terminals and editors for no protection. Reason 1 still applies and the
/// refusal says the caller is remote. Python `core.agent_host_refusal`.
pub fn agent_host_refusal(
    backend: &dyn Backend,
    cfg: &Config,
    who: &Caller,
    window: WindowRef,
) -> String {
    let Ok(root) = backend.root_window(window) else {
        return String::new();
    };
    let Ok(info) = backend.window_info(root) else {
        return String::new();
    };
    let name = if info.app.is_empty() {
        "?".to_string()
    } else {
        info.app.to_lowercase()
    };
    if cfg.protected_processes().contains(&name) {
        let note = if who.is_remote() {
            "the caller is remote, so the caller-relative guard is off and protect_processes is \
             the only list left. "
        } else {
            ""
        };
        return format!(
            "that window belongs to {name}, which is in winhands' protect_processes list \
             (config.json / WINHANDS_PROTECT_PROCESSES). {note}It is refused whoever is calling"
        );
    }
    if who.known() {
        let pid = info.pid;
        let role = if pid != 0 {
            who.role(pid, &name)
        } else {
            String::new()
        };
        if !role.is_empty() {
            return format!(
                "that window belongs to {name} (pid {pid}), {role}. winhands never clicks or types \
                 into the process driving it, or into that process's parents (caller chain: {}). \
                 Act on another window instead",
                who.describe()
            );
        }
        return String::new();
    }
    if who.is_remote() {
        // The caller is not a local process, so no pid on this machine is the driver. Only
        // protect_processes (checked above) refuses anything now; see SECURITY.md "Remote mode".
        return String::new();
    }
    if AGENT_HOST_PROCESSES.contains(&name.as_str()) {
        let mut names = AGENT_HOST_PROCESSES.to_vec();
        names.sort_unstable();
        return format!(
            "that window belongs to {name}, and winhands could not work out which process is calling \
             it, so it fell back to its fixed list of agent, terminal and editor process names ({}) \
             rather than allow the window. Pass window=... for a different app, or fix the caller \
             lookup",
            names.join(", ")
        );
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::backend::{ProcessInfo, Rect, WindowInfo};
    use crate::env::MapEnv;
    use crate::paths::Paths;
    use crate::testutil::TempDir;

    fn caller(pairs: &[(u32, &str)]) -> Caller {
        Caller::new(
            pairs.iter().map(|(p, n)| (*p, n.to_string())).collect(),
            false,
        )
    }

    fn proc(pid: u32, parent: u32, name: &str, started: Option<u64>) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: parent,
            exe_name: name.to_string(),
            exe_path: None,
            started_at: started,
            elevated: false,
        }
    }

    fn cfg_with(env: MapEnv) -> (Config, TempDir) {
        let tmp = TempDir::new("caller");
        (Config::new(Paths::in_dir(tmp.path()), env), tmp)
    }

    /// One window per `(pid, exe)`, handles 1, 2, ... in order; returns the backend and the handles.
    fn desktop(owners: &[(u32, &str)]) -> (FakeBackend, Vec<WindowRef>) {
        let backend = FakeBackend::new();
        let handles = owners
            .iter()
            .map(|(pid, app)| {
                backend.add_window(WindowInfo::basic(
                    "a window",
                    app,
                    *pid,
                    Rect::from_xywh(0, 0, 10, 10),
                ))
            })
            .collect();
        (backend, handles)
    }

    // ---- ported from tests/test_20260924_caller_guard.py ----

    fn claude_chain() -> Caller {
        caller(&[(100, "claude.exe"), (200, "windowsterminal.exe")])
    }

    #[test]
    fn the_callers_own_window_is_refused() {
        let (backend, w) = desktop(&[(100, "claude.exe")]);
        let (cfg, _t) = cfg_with(MapEnv::new());
        let reason = agent_host_refusal(&backend, &cfg, &claude_chain(), w[0]);
        assert!(reason.contains("claude.exe"), "{reason}");
        assert!(reason.contains("calling winhands right now"), "{reason}");
    }

    #[test]
    fn the_callers_parent_window_is_refused() {
        let (backend, w) = desktop(&[(200, "windowsterminal.exe")]);
        let (cfg, _t) = cfg_with(MapEnv::new());
        let reason = agent_host_refusal(&backend, &cfg, &claude_chain(), w[0]);
        assert!(reason.contains("windowsterminal.exe"), "{reason}");
        assert!(
            reason.contains("parent of the process calling winhands"),
            "{reason}"
        );
        assert!(reason.contains("pid 100"), "{reason}");
    }

    #[test]
    fn an_unrelated_terminal_or_editor_window_is_allowed() {
        // The whole point of the change: a terminal or editor window that is not driving winhands is
        // work the owner wants back.
        let (backend, w) = desktop(&[
            (900, "windowsterminal.exe"),
            (901, "code.exe"),
            (902, "claude.exe"),
        ]);
        let (cfg, _t) = cfg_with(MapEnv::new());
        let who = claude_chain();
        assert_eq!(agent_host_refusal(&backend, &cfg, &who, w[0]), "");
        assert_eq!(agent_host_refusal(&backend, &cfg, &who, w[1]), "");
        // ... but another process of the app that IS driving this call is refused: an app runs several
        // processes, and on 2026-09-24 that hole let a keystroke into the agent's own window.
        let reason = agent_host_refusal(&backend, &cfg, &who, w[2]);
        assert!(reason.contains("another process of claude.exe"), "{reason}");
    }

    #[test]
    fn unknown_caller_falls_back_to_the_name_list_and_says_so() {
        let (backend, w) = desktop(&[(900, "windowsterminal.exe"), (901, "notepad.exe")]);
        let (cfg, _t) = cfg_with(MapEnv::new());
        let who = Caller::unknown();
        let reason = agent_host_refusal(&backend, &cfg, &who, w[0]);
        assert!(
            reason.contains("could not work out which process is calling"),
            "{reason}"
        );
        assert!(reason.contains("windowsterminal.exe"), "{reason}");
        // the fallback is the old list, not "refuse everything"
        assert_eq!(agent_host_refusal(&backend, &cfg, &who, w[1]), "");
    }

    #[test]
    fn config_protected_processes_refuse_whoever_is_calling() {
        let (backend, w) = desktop(&[(900, "keepass.exe")]);
        let (cfg, _t) = cfg_with(MapEnv::new().with("WINHANDS_PROTECT_PROCESSES", "keepass.exe"));
        // a known, unrelated caller: still refused
        assert!(
            agent_host_refusal(&backend, &cfg, &claude_chain(), w[0]).contains("protect_processes")
        );
        assert!(
            agent_host_refusal(&backend, &cfg, &Caller::unknown(), w[0])
                .contains("protect_processes")
        );
    }

    #[test]
    fn a_window_that_cannot_be_read_is_never_refused() {
        let backend = FakeBackend::new();
        let (cfg, _t) = cfg_with(MapEnv::new());
        let ghost = WindowRef::from_raw(31337);
        assert_eq!(
            agent_host_refusal(&backend, &cfg, &Caller::unknown(), ghost),
            ""
        );
    }

    #[test]
    fn process_chain_walks_up_and_stops() {
        let backend = FakeBackend::new();
        backend.set_processes(vec![
            proc(10, 11, "Python.exe", Some(30)),
            proc(11, 12, "bash.exe", Some(20)),
            proc(12, 1, "claude.exe", Some(10)),
            proc(1, 0, "System", Some(1)),
        ]);
        let chain = process_chain(&backend, 10);
        let names: Vec<&str> = chain.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, ["python.exe", "bash.exe", "claude.exe", "system"]);
        assert!(chain.len() <= MAX_ANCESTORS);
        assert_eq!(process_chain(&backend, 0), Vec::new());
        // a pid the table does not know ends the walk at once
        assert_eq!(process_chain(&backend, 999), Vec::new());
    }

    #[test]
    fn process_chain_has_a_cycle_guard_and_a_depth_cap() {
        let backend = FakeBackend::new();
        backend.set_processes(vec![proc(1, 2, "a.exe", None), proc(2, 1, "b.exe", None)]);
        let chain = process_chain(&backend, 1);
        assert_eq!(chain.len(), 2);
        let long: Vec<ProcessInfo> = (1..=40u32)
            .map(|pid| proc(pid, pid + 1, "n.exe", None))
            .collect();
        backend.set_processes(long);
        assert_eq!(process_chain(&backend, 1).len(), MAX_ANCESTORS);
    }

    #[test]
    fn a_parent_created_after_its_child_is_a_recycled_pid() {
        let backend = FakeBackend::new();
        backend.set_processes(vec![
            proc(10, 11, "child.exe", Some(100)),
            proc(11, 12, "recycled.exe", Some(500)),
            proc(12, 0, "root.exe", Some(1)),
        ]);
        let chain = process_chain(&backend, 10);
        assert_eq!(chain, vec![(10, "child.exe".to_string())]);
    }

    #[test]
    fn the_caller_lookup_names_the_peer_process() {
        let backend = FakeBackend::new();
        backend.set_processes(vec![proc(42, 0, "claude.exe", None)]);
        backend.set_peer(50000, 47811, 42);
        let who = identify(&backend, 50000, 47811, "127.0.0.1", false);
        assert!(who.known() && who.chain()[0].0 == 42);
        assert_eq!(
            who.role(42, ""),
            "the process that is calling winhands right now"
        );
        assert_eq!(who.role(42 + 100_000, ""), "");
        // no port, no table row: unknown, never an error
        assert!(!identify(&backend, 0, 47811, "127.0.0.1", false).known());
        assert!(!identify(&backend, 50001, 47811, "127.0.0.1", false).known());
    }

    #[test]
    fn unknown_caller_is_the_safe_default() {
        let who = Caller::unknown();
        assert!(!who.known() && who.pids().is_empty() && !who.is_remote());
        assert_eq!(who.role(1234, ""), "");
        assert_eq!(who.describe(), "unknown");
    }

    // ---- ported from tests/test_20260924_remote_caller.py ----

    fn proxied(proxy: &str, remote: bool, host: &str) -> Caller {
        let backend = FakeBackend::new();
        backend.set_processes(vec![
            proc(4242, 10, proxy, None),
            proc(10, 0, "services.exe", None),
        ]);
        backend.set_peer(50000, 47811, 4242);
        identify(&backend, 50000, 47811, host, remote)
    }

    #[test]
    fn a_relay_process_is_treated_as_unknown_not_as_the_caller() {
        for proxy in PROXY_HOSTS {
            let who = proxied(proxy, false, "127.0.0.1");
            assert!(
                !who.known(),
                "{proxy} relays a connection, it never drives winhands"
            );
            assert!(who.pids().is_empty() && who.app_names().is_empty());
        }
    }

    #[test]
    fn a_proxied_caller_does_not_protect_the_relays_windows() {
        // The dangerous case in one line: before, svchost.exe was "the caller", so svchost windows
        // were refused and the actual driver's windows were fair game while the guard felt satisfied.
        let who = proxied("svchost.exe", false, "127.0.0.1");
        let (backend, w) = desktop(&[(4242, "svchost.exe"), (900, "notepad.exe")]);
        let (cfg, _t) = cfg_with(MapEnv::new());
        assert!(
            !agent_host_refusal(&backend, &cfg, &who, w[0]).contains("calling winhands right now")
        );
        assert_eq!(agent_host_refusal(&backend, &cfg, &who, w[1]), "");
    }

    #[test]
    fn a_real_local_caller_is_still_identified() {
        let backend = FakeBackend::new();
        backend.set_processes(vec![proc(100, 0, "claude.exe", None)]);
        backend.set_peer(50000, 47811, 100);
        let who = identify(&backend, 50000, 47811, "127.0.0.1", false);
        assert!(who.known() && !who.is_remote());
        assert_eq!(who.pids(), BTreeSet::from([100]));
    }

    #[test]
    fn remote_caller_refuses_to_trust_a_non_loopback_connection() {
        let backend = FakeBackend::new();
        backend.set_processes(vec![proc(100, 0, "claude.exe", None)]);
        backend.set_peer(50000, 47811, 100);
        let who = identify(&backend, 50000, 47811, "10.0.0.9", true);
        assert!(
            who.is_remote() && !who.known(),
            "a pid resolved for an off-machine peer is a coincidence"
        );
        // an empty or unreadable peer address is not loopback either
        assert!(identify(&backend, 50000, 47811, "", true).is_remote());
    }

    #[test]
    fn remote_caller_still_trusts_a_genuine_loopback_non_proxy_caller() {
        // remote_caller is forced on by a remote bind, and that daemon may still be driven locally.
        let backend = FakeBackend::new();
        backend.set_processes(vec![proc(100, 0, "claude.exe", None)]);
        backend.set_peer(50000, 47811, 100);
        for host in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            let who = identify(&backend, 50000, 47811, host, true);
            assert!(who.known(), "{host}");
            assert_eq!(who.pids(), BTreeSet::from([100]));
        }
    }

    #[test]
    fn remote_caller_does_not_trust_a_loopback_relay() {
        // A netsh portproxy delivers the connection FROM loopback. Loopback alone is not enough.
        let who = proxied("svchost.exe", true, "127.0.0.1");
        assert!(who.is_remote() && !who.known());
    }

    #[test]
    fn remote_caller_means_no_caller_relative_refusals() {
        let (backend, w) = desktop(&[
            (900, "windowsterminal.exe"),
            (901, "code.exe"),
            (902, "claude.exe"),
        ]);
        let (cfg, _t) = cfg_with(MapEnv::new());
        // The old fixed list is NOT used: it would protect nothing real and ban the owner's tools.
        for h in w {
            assert_eq!(agent_host_refusal(&backend, &cfg, &Caller::remote(), h), "");
        }
    }

    #[test]
    fn protect_processes_still_applies_in_remote_mode_and_says_the_caller_is_remote() {
        let (backend, w) = desktop(&[(900, "keepass.exe")]);
        let (cfg, _t) = cfg_with(MapEnv::new().with("WINHANDS_PROTECT_PROCESSES", "keepass.exe"));
        let reason = agent_host_refusal(&backend, &cfg, &Caller::remote(), w[0]);
        assert!(reason.contains("protect_processes"), "{reason}");
        assert!(reason.contains("caller is remote"), "{reason}");
        assert!(reason.contains("caller-relative guard is off"), "{reason}");
        assert!(reason.contains("keepass.exe"), "{reason}");
    }

    #[test]
    fn a_locally_identified_caller_is_still_refused_in_remote_mode() {
        let (backend, w) = desktop(&[(100, "claude.exe")]);
        let (cfg, _t) = cfg_with(MapEnv::new());
        let who = Caller::new(vec![(100, "claude.exe".to_string())], true);
        assert!(
            agent_host_refusal(&backend, &cfg, &who, w[0]).contains("calling winhands right now")
        );
    }

    // ---- ported from tests/test_caller_scope.py ----

    #[test]
    fn app_sibling_process_is_refused() {
        let who = caller(&[(10, "python.exe"), (11, "bash.exe"), (12, "claude.exe")]);
        // a different process of the same app, e.g. the agent's UI window
        assert!(!who.role(99, "claude.exe").is_empty());
        // unrelated apps stay allowed, including editors and terminals
        assert_eq!(who.role(98, "Code.exe"), "");
        assert_eq!(who.role(97, "WindowsTerminal.exe"), "");
    }

    #[test]
    fn generic_hosts_do_not_protect_every_window() {
        let who = caller(&[(10, "python.exe"), (11, "bash.exe")]);
        assert_eq!(who.role(99, "python.exe"), "");
        assert_eq!(who.role(99, "bash.exe"), "");
    }

    #[test]
    fn pid_match_still_wins() {
        let who = caller(&[(10, "python.exe"), (12, "claude.exe")]);
        assert!(
            who.role(10, "python.exe")
                .contains("calling winhands right now")
        );
        assert!(who.role(12, "claude.exe").contains("parent"));
    }

    #[test]
    fn chain_depth_allows_real_nesting() {
        const { assert!(MAX_ANCESTORS >= 12) };
    }

    #[test]
    fn loopback_peers_are_recognised_conservatively() {
        assert!(is_loopback_peer("127.0.0.1") && is_loopback_peer(" ::1 "));
        assert!(is_loopback_peer("::FFFF:127.0.0.1"));
        assert!(
            !is_loopback_peer("")
                && !is_loopback_peer("10.0.0.9")
                && !is_loopback_peer("localhost")
        );
    }
}
