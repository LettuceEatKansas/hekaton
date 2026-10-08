//! Shims that leave when their host is gone or has replaced them (Python `shim.py`: `_host_alive`,
//! `_register_shim`, `_has_newer_sibling`, `_superseded`, `_reaper_tick`).
//!
//! A host that outlives its sessions (Hermes: `hermes serve`, whose cron opens a fresh MCP session twice
//! a day) never dies, so a shim whose host merely stays alive would live for ever. A shim therefore
//! also leaves when it has had no MCP request for `shim_idle_s` AND a newer shim exists under the same
//! spawner (the process that started it), which is proof its session was replaced. A lone idle shim
//! is never touched, and a shim never leaves on a detection failure.
//!
//! The decisions are pure functions over a process table and a directory of small JSON records
//! (`<pid>.json`, the same files the Python shim writes), so they are tested without processes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use wh_core::backend::ProcessInfo;

/// How many ancestors are looked through for the host (`range(6)`).
const HOST_DEPTH: usize = 6;

/// A snapshot of the process table, by pid.
pub struct ProcTable {
    by_pid: HashMap<u32, ProcessInfo>,
}

impl ProcTable {
    /// Index a backend's `process_table()`.
    pub fn new(list: Vec<ProcessInfo>) -> Self {
        Self {
            by_pid: list.into_iter().map(|p| (p.pid, p)).collect(),
        }
    }

    fn contains(&self, pid: u32) -> bool {
        self.by_pid.contains_key(&pid)
    }

    fn parent(&self, pid: u32) -> u32 {
        self.by_pid.get(&pid).map_or(0, |p| p.parent_pid)
    }

    fn name(&self, pid: u32) -> String {
        self.by_pid
            .get(&pid)
            .map(|p| p.exe_name.to_lowercase())
            .unwrap_or_default()
    }

    fn image(&self, pid: u32) -> String {
        self.by_pid
            .get(&pid)
            .and_then(|p| p.exe_path.as_deref())
            .map(normcase)
            .unwrap_or_default()
    }

    /// The creation time the backend reports for `pid`, 0 when unknown.
    pub fn created_of(&self, pid: u32) -> u64 {
        self.by_pid
            .get(&pid)
            .and_then(|p| p.started_at)
            .unwrap_or(0)
    }
}

/// A path as it is compared: lowercase, one kind of slash (`os.path.normcase`).
pub fn normcase(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

/// A process that only stands between the shim and its host: a python interpreter or this same
/// executable (a wrapper that re-launches itself).
fn is_launcher(name: &str, own_name: &str) -> bool {
    name.starts_with("python") || (!own_name.is_empty() && name == own_name)
}

/// The pid of the program that started this shim (Claude, Hermes, ...): the first ancestor that is not
/// a launcher. `Some(0)` when the parent chain is gone, `None` when it cannot be told.
pub fn host_pid(table: &ProcTable, me: u32, own_name: &str) -> Option<u32> {
    let mut pid = me;
    for _ in 0..HOST_DEPTH {
        let parent = table.parent(pid);
        if parent == 0 || !table.contains(parent) {
            return Some(0); // the parent no longer exists
        }
        if !is_launcher(&table.name(parent), own_name) {
            return Some(parent);
        }
        pid = parent;
    }
    None
}

/// True while the program that started this shim still runs. Cannot tell counts as alive: a shim is
/// never killed on a detection failure.
pub fn host_alive(table: &ProcTable, me: u32, own_name: &str) -> bool {
    host_pid(table, me, own_name) != Some(0)
}

/// The pid of the process that started this shim's launcher (Claude, Hermes' python, ...): when the
/// parent is this same executable, one level further up; otherwise the parent itself. 0 when it is
/// gone. Never matches across two shims unless they truly share it.
pub fn spawner_pid(table: &ProcTable, me: u32, own_image: &str) -> u32 {
    let parent = table.parent(me);
    if parent == 0 || !table.contains(parent) {
        return 0;
    }
    if !own_image.is_empty() && table.image(parent) == own_image {
        let grand = table.parent(parent);
        return if grand != 0 && table.contains(grand) {
            grand
        } else {
            0
        };
    }
    parent
}

/// One shim's record in the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct ShimRecord {
    /// The shim's pid.
    pub pid: u32,
    /// Its spawner (0 unknown).
    pub spawner: u32,
    /// Its creation time in the backend's unit, to tell a recycled pid from the shim.
    pub created: u64,
    /// When it started, seconds since the Unix epoch.
    pub started: f64,
}

impl ShimRecord {
    fn to_json(&self) -> Value {
        json!({"pid": self.pid, "spawner": self.spawner, "created": self.created, "started": self.started})
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            pid: u32::try_from(v.get("pid")?.as_u64()?).ok()?,
            spawner: v.get("spawner").and_then(Value::as_u64).unwrap_or(0) as u32,
            created: v.get("created").and_then(Value::as_u64).unwrap_or(0),
            started: v.get("started").and_then(Value::as_f64).unwrap_or(0.0),
        })
    }
}

/// The directory of shim records.
#[derive(Clone)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// A registry in `dir` (`%LOCALAPPDATA%\winhands\shims`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn file(&self, pid: u32) -> PathBuf {
        self.dir.join(format!("{pid}.json"))
    }

    /// Leave a small record so a newer shim under the same host can tell this one is its elder. Best
    /// effort: with no record this shim is never seen as a sibling, which only means it is never the
    /// one chosen to leave and never counted as the newer one.
    pub fn register(&self, record: &ShimRecord) {
        if std::fs::create_dir_all(&self.dir).is_ok() {
            let _ = std::fs::write(self.file(record.pid), record.to_json().to_string());
        }
    }

    /// Remove this shim's record.
    pub fn unregister(&self, pid: u32) {
        let _ = std::fs::remove_file(self.file(pid));
    }

    /// This shim's own record, if it left one.
    pub fn record(&self, pid: u32) -> Option<ShimRecord> {
        let text = std::fs::read_to_string(self.file(pid)).ok()?;
        ShimRecord::from_json(&serde_json::from_str(&text).ok()?)
    }

    /// Is another live shim under the same spawner newer than this one? Records of dead shims (pid
    /// gone, or reused by a different process) are deleted on the way.
    pub fn has_newer_sibling(&self, me: u32, spawner: u32, table: &ProcTable) -> bool {
        let Some(mine) = self.record(me) else {
            return false; // not registered: cannot order ourselves against anyone
        };
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return false;
        };
        let mut newer = false;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(rec) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| ShimRecord::from_json(&v))
            else {
                continue;
            };
            if rec.pid == me {
                continue;
            }
            let now_created = table.created_of(rec.pid);
            let alive = table.contains(rec.pid)
                && (rec.created == 0 || now_created == 0 || now_created == rec.created);
            if !alive {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if rec.spawner == spawner && rec.started > mine.started {
                newer = true;
            }
        }
        newer
    }
}

/// Everything the reaper needs to decide.
pub struct Reaper {
    /// The records.
    pub registry: Registry,
    /// This shim's pid.
    pub me: u32,
    /// This executable's file name, lowercased.
    pub own_name: String,
    /// This executable's full path, normalised.
    pub own_image: String,
    /// `shim_idle_s`; 0 or less turns the idle rule off.
    pub idle_limit: Duration,
}

impl Reaper {
    /// Idle for longer than the limit AND a newer shim exists under the same spawner. The idle test
    /// costs nothing and runs first; the scan only runs once idle.
    pub fn superseded(&self, table: &ProcTable, idle: Duration) -> bool {
        if self.idle_limit.is_zero() || idle < self.idle_limit {
            return false;
        }
        let spawner = spawner_pid(table, self.me, &self.own_image);
        spawner != 0 && self.registry.has_newer_sibling(self.me, spawner, table)
    }

    /// True when this shim should exit now.
    pub fn should_exit(&self, table: &ProcTable, idle: Duration) -> bool {
        !host_alive(table, self.me, &self.own_name) || self.superseded(table, idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent: u32, name: &str, path: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: parent,
            exe_name: name.to_string(),
            exe_path: (!path.is_empty()).then(|| path.to_string()),
            started_at: Some(5000 + u64::from(pid)),
            elevated: false,
        }
    }

    const OWN: &str = "c:\\tools\\winhands.exe";

    /// Shim 1000 <- stub 900 (same exe) <- host 800 (claude.exe) <- 1.
    fn table() -> Vec<ProcessInfo> {
        vec![
            proc(1000, 900, "winhands.exe", OWN),
            proc(900, 800, "winhands.exe", "C:\\Tools\\winhands.exe"),
            proc(800, 1, "claude.exe", "c:\\apps\\claude.exe"),
            proc(1, 0, "system", ""),
        ]
    }

    struct Rig {
        dir: PathBuf,
        reaper: Reaper,
        table: Vec<ProcessInfo>,
    }

    impl Rig {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("wh-shims-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self {
                reaper: Reaper {
                    registry: Registry::new(&dir),
                    me: 1000,
                    own_name: "winhands.exe".into(),
                    own_image: OWN.into(),
                    idle_limit: Duration::from_secs(300),
                },
                dir,
                table: table(),
            }
        }

        fn t(&self) -> ProcTable {
            ProcTable::new(self.table.clone())
        }

        fn register_me(&self, spawner: u32) {
            self.reaper.registry.register(&ShimRecord {
                pid: 1000,
                spawner,
                created: 6000,
                started: 10_000.0,
            });
        }

        fn sibling(&mut self, pid: u32, spawner: u32, started: f64, alive: bool) {
            self.reaper.registry.register(&ShimRecord {
                pid,
                spawner,
                created: 5000 + u64::from(pid),
                started,
            });
            if alive {
                self.table.push(proc(pid, 900, "winhands.exe", OWN));
            }
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // Ported from tests/test_20261008_speed_fixes.py (W6).

    #[test]
    fn the_spawner_skips_the_launcher_stub() {
        let rig = Rig::new();
        assert_eq!(spawner_pid(&rig.t(), 1000, OWN), 800);
    }

    #[test]
    fn without_a_stub_the_spawner_is_the_parent() {
        let rig = Rig::new();
        assert_eq!(spawner_pid(&rig.t(), 1000, "c:\\somewhere\\else.exe"), 900);
    }

    #[test]
    fn a_gone_spawner_is_zero() {
        let mut rig = Rig::new();
        rig.table.retain(|p| p.pid != 900);
        assert_eq!(spawner_pid(&rig.t(), 1000, OWN), 0);
    }

    #[test]
    fn the_host_skips_python_and_our_own_exe() {
        let mut rig = Rig::new();
        assert_eq!(host_pid(&rig.t(), 1000, "winhands.exe"), Some(800));
        rig.table[1] = proc(900, 800, "python.exe", "");
        assert_eq!(host_pid(&rig.t(), 1000, "winhands.exe"), Some(800));
        rig.table.retain(|p| p.pid != 800);
        assert_eq!(host_pid(&rig.t(), 1000, "winhands.exe"), Some(0));
        assert!(!host_alive(&rig.t(), 1000, "winhands.exe"));
    }

    #[test]
    fn a_chain_of_launchers_that_never_ends_cannot_be_told() {
        let table: Vec<ProcessInfo> = (1..=9)
            .map(|pid| proc(pid, pid + 1, "winhands.exe", ""))
            .chain([
                proc(10, 11, "winhands.exe", ""),
                proc(11, 12, "winhands.exe", ""),
                proc(12, 13, "winhands.exe", ""),
            ])
            .collect();
        let t = ProcTable::new(table);
        assert_eq!(host_pid(&t, 1, "winhands.exe"), None);
        assert!(
            host_alive(&t, 1, "winhands.exe"),
            "cannot tell counts as alive"
        );
    }

    #[test]
    fn an_idle_shim_with_a_newer_sibling_under_the_same_spawner_leaves() {
        let mut rig = Rig::new();
        rig.register_me(800);
        rig.sibling(2000, 800, 10_060.0, true);
        assert!(rig.reaper.superseded(&rig.t(), Duration::from_secs(400)));
        assert!(rig.reaper.should_exit(&rig.t(), Duration::from_secs(400)));
    }

    #[test]
    fn a_busy_shim_stays() {
        let mut rig = Rig::new();
        rig.register_me(800);
        rig.sibling(2000, 800, 10_060.0, true);
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(200)));
        assert!(!rig.reaper.should_exit(&rig.t(), Duration::from_secs(200)));
    }

    #[test]
    fn a_lone_idle_shim_stays() {
        let rig = Rig::new();
        rig.register_me(800);
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(10_000)));
    }

    #[test]
    fn an_idle_shim_with_only_older_siblings_stays() {
        let mut rig = Rig::new();
        rig.sibling(500, 800, 6_400.0, true);
        rig.register_me(800);
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(10_000)));
    }

    #[test]
    fn a_sibling_under_another_spawner_does_not_count() {
        let mut rig = Rig::new();
        rig.register_me(800);
        rig.sibling(2000, 801, 10_060.0, true);
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(10_000)));
    }

    #[test]
    fn a_dead_sibling_does_not_count_and_its_record_is_removed() {
        let mut rig = Rig::new();
        rig.register_me(800);
        rig.sibling(2000, 800, 10_060.0, false);
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(10_000)));
        assert!(!rig.dir.join("2000.json").exists());
    }

    #[test]
    fn a_recycled_pid_does_not_count() {
        let mut rig = Rig::new();
        rig.register_me(800);
        rig.sibling(2000, 800, 10_060.0, true);
        for p in &mut rig.table {
            if p.pid == 2000 {
                p.started_at = Some(99); // a different process now owns the pid
            }
        }
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(10_000)));
        assert!(!rig.dir.join("2000.json").exists());
    }

    #[test]
    fn an_unregistered_shim_never_leaves() {
        let mut rig = Rig::new();
        rig.sibling(2000, 800, 10_060.0, true);
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(10_000)));
    }

    #[test]
    fn an_idle_limit_of_zero_turns_the_rule_off() {
        let mut rig = Rig::new();
        rig.reaper.idle_limit = Duration::ZERO;
        rig.register_me(800);
        rig.sibling(2000, 800, 10_060.0, true);
        assert!(!rig.reaper.superseded(&rig.t(), Duration::from_secs(10_000)));
    }

    #[test]
    fn a_shim_whose_host_died_leaves_at_once() {
        let mut rig = Rig::new();
        rig.table.retain(|p| p.pid != 800);
        assert!(rig.reaper.should_exit(&rig.t(), Duration::ZERO));
    }

    #[test]
    fn records_round_trip_and_unregister_removes_them() {
        let rig = Rig::new();
        rig.register_me(800);
        let rec = rig.reaper.registry.record(1000).unwrap();
        assert_eq!((rec.pid, rec.spawner, rec.created), (1000, 800, 6000));
        rig.reaper.registry.unregister(1000);
        assert!(rig.reaper.registry.record(1000).is_none());
    }

    #[test]
    fn normcase_folds_case_and_slashes() {
        assert_eq!(normcase("C:/Tools/WinHands.EXE"), "c:\\tools\\winhands.exe");
    }
}
