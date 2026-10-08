//! Processes: names, elevation, creation time, the process table and the owner of a TCP
//! connection (Python `core.py:_process_name`, `_is_elevated`, `caller.py:_process_table`,
//! `_created_at`, `pid_for_peer`).

use std::mem::size_of;

use wh_core::backend::{BackendResult, ProcessInfo};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_BASIC_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use crate::util::{OwnedHandle, exe_basename, from_wide, os_err};

/// `ERROR_INSUFFICIENT_BUFFER`.
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

fn open_limited(pid: u32) -> Option<OwnedHandle> {
    // SAFETY: OpenProcess takes plain values; the returned handle is owned and closed by OwnedHandle.
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .ok()
        .map(OwnedHandle::new)
}

/// The full image path of a process, when this account may read it.
pub(crate) fn image_path(pid: u32) -> Option<String> {
    let h = open_limited(pid)?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` has room for `len` UTF-16 units and `len` is updated to the number written.
    unsafe {
        QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// The executable file name of a process (`notepad.exe`), when this account may read it.
pub(crate) fn process_name(pid: u32) -> Option<String> {
    image_path(pid).map(|p| exe_basename(&p).to_string())
}

fn elevated_handle(h: HANDLE) -> bool {
    let mut token = HANDLE::default();
    // SAFETY: `h` is a live process handle; on success `token` is a handle we close below.
    if unsafe { OpenProcessToken(h, TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let token = OwnedHandle::new(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: the buffer is a TOKEN_ELEVATION and its exact size is passed.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some(std::ptr::from_mut(&mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    ok.is_ok() && elevation.TokenIsElevated != 0
}

/// Does the process run with an elevated token? False when it cannot be read (Python `_is_elevated`).
pub(crate) fn is_elevated(pid: u32) -> bool {
    open_limited(pid).is_some_and(|h| elevated_handle(h.0))
}

/// The process creation time as a FILETIME in 100 ns units, `None` when unreadable.
pub(crate) fn created_at(h: HANDLE) -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the four FILETIME out-parameters are live locals.
    unsafe { GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    let t = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    (t != 0).then_some(t)
}

/// The parent pid straight from the kernel (`ProcessBasicInformation`), `None` when it cannot say.
fn parent_of(h: HANDLE) -> Option<u32> {
    let mut info = PROCESS_BASIC_INFORMATION::default();
    let mut returned = 0u32;
    // SAFETY: the buffer is a PROCESS_BASIC_INFORMATION and its exact size is passed.
    let status = unsafe {
        NtQueryInformationProcess(
            h,
            ProcessBasicInformation,
            std::ptr::from_mut(&mut info).cast(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut returned,
        )
    };
    if status.0 < 0 {
        return None;
    }
    u32::try_from(info.InheritedFromUniqueProcessId).ok()
}

/// Every process in a Toolhelp snapshot: pid, parent pid, exe name. The cheap fields only; the
/// slow ones (path, creation time, elevation) are `process_info`'s.
pub(crate) fn process_table() -> BackendResult<Vec<ProcessInfo>> {
    // SAFETY: a snapshot of processes needs no other arguments; the handle is owned below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| os_err("could not snapshot the process list", &e))?;
    let snap = OwnedHandle::new(snap);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: `entry.dwSize` is set and `snap` is a live snapshot handle.
    let mut more = unsafe { Process32FirstW(snap.0, &mut entry) }.is_ok();
    while more {
        out.push(ProcessInfo {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            exe_name: from_wide(&entry.szExeFile),
            exe_path: None,
            started_at: None,
            elevated: false,
        });
        // SAFETY: same entry and snapshot as above.
        more = unsafe { Process32NextW(snap.0, &mut entry) }.is_ok();
    }
    Ok(out)
}

/// One process in full. Falls back to the snapshot's name and parent for a process this account
/// may not open (system services), with no creation time.
pub(crate) fn process_info(pid: u32) -> BackendResult<Option<ProcessInfo>> {
    if let Some(h) = open_limited(pid) {
        let path = {
            let mut buf = vec![0u16; 1024];
            let mut len = buf.len() as u32;
            // SAFETY: `buf` has room for `len` UTF-16 units and `len` is updated to the number written.
            unsafe {
                QueryFullProcessImageNameW(
                    h.0,
                    PROCESS_NAME_WIN32,
                    PWSTR(buf.as_mut_ptr()),
                    &mut len,
                )
            }
            .ok()
            .map(|()| String::from_utf16_lossy(&buf[..len as usize]))
        };
        let parent = parent_of(h.0);
        if let Some(path) = path {
            let parent_pid = match parent {
                Some(p) => p,
                None => snapshot_entry(pid)?.map_or(0, |e| e.parent_pid),
            };
            return Ok(Some(ProcessInfo {
                pid,
                parent_pid,
                exe_name: exe_basename(&path).to_string(),
                started_at: created_at(h.0),
                elevated: elevated_handle(h.0),
                exe_path: Some(path),
            }));
        }
    }
    snapshot_entry(pid)
}

fn snapshot_entry(pid: u32) -> BackendResult<Option<ProcessInfo>> {
    Ok(process_table()?.into_iter().find(|p| p.pid == pid))
}

// ------------------------------------------------------------------------------------ TCP owner

/// A port field of the TCP tables: network byte order in the low 16 bits of a DWORD.
pub(crate) fn tcp_port(raw: u32) -> u16 {
    u16::from_be((raw & 0xFFFF) as u16)
}

/// Rows of one address family as (local port, remote port, owning pid).
fn tcp_rows(family: u32) -> Option<Vec<(u16, u16, u32)>> {
    let mut size = 0u32;
    // Two tries: the table can grow between asking for its size and reading it.
    for _ in 0..4 {
        let mut buf = vec![0u8; size as usize];
        let ptr = (size != 0).then(|| buf.as_mut_ptr().cast());
        // SAFETY: `buf` is `size` bytes (or null with size 0, the documented way to ask for the size).
        let rc = unsafe {
            GetExtendedTcpTable(ptr, &mut size, false, family, TCP_TABLE_OWNER_PID_ALL, 0)
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            size = size.saturating_add(4096);
            continue;
        }
        if rc != 0 || size == 0 {
            return None;
        }
        return Some(parse_tcp_rows(&buf, family));
    }
    None
}

/// Walk a `MIB_TCPTABLE_OWNER_PID` / `MIB_TCP6TABLE_OWNER_PID` buffer.
fn parse_tcp_rows(buf: &[u8], family: u32) -> Vec<(u16, u16, u32)> {
    let Some(count_bytes) = buf.get(..4) else {
        return Vec::new();
    };
    let count = u32::from_le_bytes([
        count_bytes[0],
        count_bytes[1],
        count_bytes[2],
        count_bytes[3],
    ]) as usize;
    let row_size = if family == u32::from(AF_INET.0) {
        size_of::<MIB_TCPROW_OWNER_PID>()
    } else {
        size_of::<MIB_TCP6ROW_OWNER_PID>()
    };
    let word = |at: usize| -> Option<u32> {
        let b = buf.get(at..at + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut rows = Vec::new();
    for i in 0..count {
        let base = 4 + i * row_size;
        // IPv4 row: state, local addr, local port, remote addr, remote port, pid.
        // IPv6 row: local addr[16], scope, local port, remote addr[16], scope, remote port, state, pid.
        let fields = if family == u32::from(AF_INET.0) {
            (word(base + 8), word(base + 16), word(base + 20))
        } else {
            (word(base + 20), word(base + 44), word(base + 52))
        };
        if let (Some(local), Some(remote), Some(pid)) = fields {
            rows.push((tcp_port(local), tcp_port(remote), pid));
        }
    }
    rows
}

/// The pid that owns the client end of a connection into this daemon (Python `pid_for_peer`).
pub(crate) fn peer_pid(peer_port: u16, server_port: u16) -> BackendResult<Option<u32>> {
    if peer_port == 0 {
        return Ok(None);
    }
    for family in [AF_INET, AF_INET6] {
        let Some(rows) = tcp_rows(u32::from(family.0)) else {
            continue;
        };
        if let Some((_, _, pid)) = rows
            .into_iter()
            .find(|(local, remote, _)| *local == peer_port && *remote == server_port)
        {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_ports_are_big_endian_in_the_low_word() {
        // Port 8080 is 0x1F90; the table stores the bytes 1F 90, which a little-endian DWORD reads as 0x901F.
        assert_eq!(tcp_port(0x901F), 8080);
        assert_eq!(tcp_port(0xFFFF_901F), 8080);
        assert_eq!(tcp_port(0), 0);
        assert_eq!(tcp_port(0x5000), 80);
    }

    fn v4_row(local: u16, remote: u16, pid: u32) -> Vec<u8> {
        let mut row = Vec::new();
        row.extend(5u32.to_le_bytes()); // state
        row.extend([127, 0, 0, 1]);
        row.extend(u32::from(local.to_be()).to_le_bytes());
        row.extend([127, 0, 0, 1]);
        row.extend(u32::from(remote.to_be()).to_le_bytes());
        row.extend(pid.to_le_bytes());
        row
    }

    fn v6_row(local: u16, remote: u16, pid: u32) -> Vec<u8> {
        let mut row = vec![0u8; 16];
        row.extend(0u32.to_le_bytes()); // local scope
        row.extend(u32::from(local.to_be()).to_le_bytes());
        row.extend(vec![0u8; 16]);
        row.extend(0u32.to_le_bytes()); // remote scope
        row.extend(u32::from(remote.to_be()).to_le_bytes());
        row.extend(5u32.to_le_bytes()); // state
        row.extend(pid.to_le_bytes());
        row
    }

    #[test]
    fn ipv4_tables_parse_to_port_pairs() {
        let mut buf = 2u32.to_le_bytes().to_vec();
        buf.extend(v4_row(50123, 7071, 4242));
        buf.extend(v4_row(7071, 50123, 99));
        assert_eq!(size_of::<MIB_TCPROW_OWNER_PID>(), 24);
        let rows = parse_tcp_rows(&buf, u32::from(AF_INET.0));
        assert_eq!(rows, [(50123, 7071, 4242), (7071, 50123, 99)]);
    }

    #[test]
    fn ipv6_tables_parse_to_port_pairs() {
        let mut buf = 1u32.to_le_bytes().to_vec();
        buf.extend(v6_row(51000, 7071, 1234));
        assert_eq!(size_of::<MIB_TCP6ROW_OWNER_PID>(), 56);
        let rows = parse_tcp_rows(&buf, u32::from(AF_INET6.0));
        assert_eq!(rows, [(51000, 7071, 1234)]);
    }

    #[test]
    fn a_short_table_yields_the_rows_it_has() {
        assert!(parse_tcp_rows(&[], u32::from(AF_INET.0)).is_empty());
        let mut buf = 3u32.to_le_bytes().to_vec();
        buf.extend(v4_row(1, 2, 3));
        assert_eq!(parse_tcp_rows(&buf, u32::from(AF_INET.0)).len(), 1);
    }

    #[test]
    fn port_zero_never_matches() {
        assert_eq!(peer_pid(0, 7071).unwrap(), None);
    }
}
