//! The process table, through `sysinfo` (libproc underneath).

use std::sync::Mutex;

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use wh_core::backend::ProcessInfo;

/// A reusable `sysinfo` handle: refreshing an existing `System` is cheaper than building one.
pub(crate) struct Procs {
    sys: Mutex<System>,
}

impl Default for Procs {
    fn default() -> Self {
        Self {
            sys: Mutex::new(System::new()),
        }
    }
}

fn kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet)
}

fn to_info(p: &Process) -> ProcessInfo {
    let exe_path = p.exe().map(|e| e.display().to_string());
    // The path carries the full file name; the kernel's process name is cut to 15 characters.
    let exe_name = p
        .exe()
        .and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
    ProcessInfo {
        pid: p.pid().as_u32(),
        parent_pid: p.parent().map_or(0, Pid::as_u32),
        exe_name,
        exe_path,
        started_at: Some(p.start_time()),
        elevated: false,
    }
}

impl Procs {
    /// Every process this account can see.
    pub(crate) fn table(&self) -> Vec<ProcessInfo> {
        let mut sys = self.sys.lock().unwrap_or_else(|e| e.into_inner());
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind());
        sys.processes().values().map(to_info).collect()
    }

    /// One process, or `None` when it is gone.
    pub(crate) fn one(&self, pid: u32) -> Option<ProcessInfo> {
        let mut sys = self.sys.lock().unwrap_or_else(|e| e.into_inner());
        let id = Pid::from_u32(pid);
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[id]), true, kind());
        sys.process(id).map(to_info)
    }
}
