//! Finding and starting programs (Python `core.py:launch_deelevated`, `launch_deelevated_shell`,
//! `_get_deelevated_token`, `_find_explorer_pid`, `_create_process_with_token`, `resolve_shortcut`,
//! `_app_paths_exe`, `find_store_app`, and the launch branches of `do_open`).
//!
//! When the daemon runs elevated, whatever it starts must not inherit that: it clones the
//! interactive user's token from `explorer.exe` (always medium integrity) and starts the program
//! with it, exactly as the Python build did.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use wh_core::backend::{AppTarget, BackendError, BackendResult, LaunchOutcome, LaunchSpec};
use windows::Win32::Foundation::{HANDLE, HWND, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, ImpersonateLoggedOnUser, LUID_AND_ATTRIBUTES,
    LookupPrivilegeValueW, RevertToSelf, SE_IMPERSONATE_NAME, SE_PRIVILEGE_ENABLED,
    SecurityImpersonation, TOKEN_ACCESS_MASK, TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrimary,
};
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IPersistFile, STGM_READ,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    RegGetValueW,
};
use windows::Win32::System::Threading::{
    CREATE_PROCESS_LOGON_FLAGS, CreateProcessW, CreateProcessWithTokenW, GetCurrentProcess,
    GetProcessId, OpenProcess, OpenProcessToken, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    PROCESS_QUERY_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, IShellLinkW, KF_FLAG_DEFAULT,
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, SHGetKnownFolderItem,
    SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, ShellExecuteExW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, SW_SHOWNORMAL,
};
use windows::core::{Interface, PCWSTR, PWSTR, w};

use crate::process;
use crate::resolve::{
    self, Hive, LnkTarget, Sources, app_paths_lookup, apps_folder_arg, build_store_index,
    command_line, list_shortcuts, match_store_app, parse_lnk, parse_pathext, pick_shortcut,
    start_menu_roots, which,
};
use crate::util::{OwnedHandle, ensure_com, from_wide, os_err, wide};

/// `LOGON_WITH_PROFILE`.
const LOGON_WITH_PROFILE: u32 = 1;
/// `MAXIMUM_ALLOWED`.
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;

// ------------------------------------------------------------------------------------ small reads

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `%VAR%` expansion (Python `os.path.expandvars`, the `%` form Windows uses).
pub(crate) fn expand_env(s: &str) -> String {
    let src = wide(s);
    // SAFETY: the first call asks for the needed size, with no output buffer.
    let needed = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), None) } as usize;
    if needed == 0 {
        return s.to_string();
    }
    let mut buf = vec![0u16; needed + 1];
    // SAFETY: `buf` holds `needed + 1` UTF-16 units, more than the call may write.
    let got = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) } as usize;
    if got == 0 {
        return s.to_string();
    }
    from_wide(&buf)
}

/// The default value of an `App Paths` key.
fn registry_default(hive: Hive, subkey: &str) -> Option<String> {
    let root: HKEY = match hive {
        Hive::CurrentUser => HKEY_CURRENT_USER,
        Hive::LocalMachine => HKEY_LOCAL_MACHINE,
    };
    let key = wide(subkey);
    let mut size = 0u32;
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    // SAFETY: the first call asks for the size of the default value, with no output buffer.
    let rc = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR::null(),
            flags,
            None,
            None,
            Some(&mut size),
        )
    };
    if rc.is_err() || size < 2 {
        return None;
    }
    let mut buf = vec![0u16; size as usize / 2 + 1];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf` is `size` bytes and the call reports how many it wrote.
    let rc = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR::null(),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    rc.is_ok().then(|| from_wide(&buf))
}

/// A shortcut's target and arguments by the shell's own parser (`IShellLink`).
fn shell_shortcut(path: &Path) -> Option<LnkTarget> {
    ensure_com();
    // SAFETY: plain COM activation of the shell's shortcut object on a thread with an apartment.
    let link: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ok()?;
    let file: IPersistFile = link.cast().ok()?;
    let name = wide(&path.to_string_lossy());
    // SAFETY: `name` is a NUL-terminated path that outlives the call.
    unsafe { file.Load(PCWSTR(name.as_ptr()), STGM_READ) }.ok()?;
    let mut target = vec![0u16; 1024];
    let mut find = WIN32_FIND_DATAW::default();
    // SAFETY: `target` has room for 1024 UTF-16 units; `find` is a live out-parameter.
    unsafe { link.GetPath(&mut target, &mut find, 0) }.ok()?;
    let mut args = vec![0u16; 4096];
    // SAFETY: `args` has room for 4096 UTF-16 units.
    let _ = unsafe { link.GetArguments(&mut args) };
    Some(LnkTarget {
        target: from_wide(&target),
        args: from_wide(&args),
    })
}

/// A shortcut's target and arguments: the shell's parser first, the file format as a fallback
/// (Python `resolve_shortcut`).
pub(crate) fn resolve_shortcut(path: &Path) -> Option<LnkTarget> {
    shell_shortcut(path)
        .filter(|t| !t.target.is_empty())
        .or_else(|| std::fs::read(path).ok().and_then(|b| parse_lnk(&b)))
}

/// Display names and app ids of everything in the shell's Apps folder (what `Get-StartApps` lists).
fn enumerate_apps_folder() -> Option<Vec<(String, String)>> {
    ensure_com();
    // SAFETY: asks the shell for the virtual Apps folder.
    let folder: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: binds the folder's item enumerator.
    let items: IEnumShellItems = unsafe { folder.BindToHandler(None, &BHID_EnumItems) }.ok()?;
    let mut out = Vec::new();
    loop {
        let mut slot: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        // SAFETY: one slot, and `fetched` reports how many were filled.
        let _ = unsafe { items.Next(&mut slot, Some(&mut fetched)) };
        if fetched == 0 {
            break;
        }
        let Some(item) = slot[0].take() else {
            break;
        };
        let name = display_name(&item, false);
        let id = display_name(&item, true);
        if let (Some(name), Some(id)) = (name, id) {
            out.push((name, id));
        }
    }
    Some(out)
}

fn display_name(item: &IShellItem, parsing: bool) -> Option<String> {
    let kind = if parsing {
        SIGDN_PARENTRELATIVEPARSING
    } else {
        SIGDN_NORMALDISPLAY
    };
    // SAFETY: the returned string is shell memory, copied and freed right here.
    let pw = unsafe { item.GetDisplayName(kind) }.ok()?;
    // SAFETY: `pw` is a valid NUL-terminated string from the shell.
    let text = unsafe { pw.to_string() }.ok();
    // SAFETY: freeing the string the shell allocated for us.
    unsafe { CoTaskMemFree(Some(pw.0.cast())) };
    text
}

// ------------------------------------------------------------------------------------ resolver

/// The machine's own sources of names. The Store list is read once and kept, like Python's.
#[derive(Default)]
pub(crate) struct Machine {
    store: Mutex<Option<Vec<(String, String)>>>,
}

impl Machine {
    fn roots(&self) -> Vec<PathBuf> {
        start_menu_roots(env("ProgramData").as_deref(), env("APPDATA").as_deref())
    }

    fn store_index(&self) -> Vec<(String, String)> {
        let mut guard = self
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            // A failed read is not kept, so a later call may succeed.
            match enumerate_apps_folder() {
                Some(apps) => *guard = Some(build_store_index(&apps)),
                None => return Vec::new(),
            }
        }
        guard.clone().unwrap_or_default()
    }
}

impl Sources for Machine {
    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn which(&self, name: &str) -> Option<String> {
        let path_dirs: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|p| !p.as_os_str().is_empty())
                .collect();
        let pathext = parse_pathext(env("PATHEXT").as_deref());
        let is_file = |p: &Path| p.is_file();
        let hit = which(name, &path_dirs, &pathext, &is_file).or_else(|| {
            (!name.to_lowercase().ends_with(".exe"))
                .then(|| which(&format!("{name}.exe"), &path_dirs, &pathext, &is_file))
                .flatten()
        })?;
        let text = hit.to_string_lossy().into_owned();
        (!text.to_lowercase().ends_with(".lnk")).then_some(text)
    }

    fn app_paths(&self, name: &str) -> Option<String> {
        app_paths_lookup(name, &registry_default, &expand_env, &|p| {
            Path::new(p).exists()
        })
    }

    fn shortcut(&self, name: &str) -> Option<String> {
        let all: Vec<PathBuf> = self
            .roots()
            .iter()
            .flat_map(|r| list_shortcuts(r))
            .collect();
        pick_shortcut(&all, name).map(|p| p.to_string_lossy().into_owned())
    }

    fn shortcut_target(&self, lnk: &str) -> Option<String> {
        let t = resolve_shortcut(Path::new(lnk))?;
        (Path::new(&t.target).exists() && t.target.to_lowercase().ends_with(".exe"))
            .then_some(t.target)
    }

    fn store_app(&self, name: &str) -> Option<String> {
        match_store_app(&self.store_index(), name)
    }
}

/// Resolve a name on this machine (Python `resolve_app_exe` plus the shortcut and Store fallbacks).
pub(crate) fn resolve_app(machine: &Machine, name: &str) -> Option<AppTarget> {
    resolve::resolve_with(name, machine)
}

// ------------------------------------------------------------------------------------ starting

fn own_elevation() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| process::is_elevated(std::process::id()))
}

/// Turn on `SeImpersonatePrivilege` in this process's token, which `CreateProcessWithTokenW` needs.
/// Held by Administrators by default, so an elevated daemon has it, merely disabled.
fn enable_impersonate_privilege() -> BackendResult<()> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle for this process is always valid; `token` receives an owned handle.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }
    .map_err(|e| os_err("could not open this process's token", &e))?;
    let token = OwnedHandle::new(token);
    let mut luid = LUID::default();
    // SAFETY: `luid` is a live out-parameter and the privilege name is a static string.
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_IMPERSONATE_NAME, &mut luid) }
        .map_err(|e| os_err("could not look up SeImpersonatePrivilege", &e))?;
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: one privilege entry, and the token handle has TOKEN_ADJUST_PRIVILEGES.
    unsafe { AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None) }
        .map_err(|e| os_err("could not enable SeImpersonatePrivilege", &e))
}

/// explorer.exe always owns the taskbar window and always runs at the user's normal (medium)
/// integrity level, even when this process is elevated.
fn explorer_pid() -> Option<u32> {
    // SAFETY: a plain window lookup by class name.
    let hwnd: HWND = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }.ok()?;
    let mut pid = 0u32;
    // SAFETY: `pid` is a live out-parameter.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

/// A primary copy of explorer's token: the normal-integrity identity of the signed-in user.
fn deelevated_token() -> BackendResult<OwnedHandle> {
    enable_impersonate_privilege()?;
    let pid = explorer_pid().ok_or_else(|| {
        BackendError::Os(
            "could not find explorer.exe (Shell_TrayWnd) to clone a normal-IL token from".into(),
        )
    })?;
    // SAFETY: opening a process by id for a token query; the handle is owned below.
    let proc = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) }
        .map_err(|e| os_err("could not open explorer.exe", &e))?;
    let proc = OwnedHandle::new(proc);
    let mut token = HANDLE::default();
    // SAFETY: `proc` is a live process handle; `token` receives an owned handle.
    unsafe { OpenProcessToken(proc.0, TOKEN_DUPLICATE, &mut token) }
        .map_err(|e| os_err("could not open explorer's token", &e))?;
    let token = OwnedHandle::new(token);
    let mut dup = HANDLE::default();
    // SAFETY: duplicating into a primary token; `dup` receives an owned handle.
    unsafe {
        DuplicateTokenEx(
            token.0,
            TOKEN_ACCESS_MASK(MAXIMUM_ALLOWED),
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup,
        )
    }
    .map_err(|e| os_err("could not duplicate explorer's token", &e))?;
    Ok(OwnedHandle::new(dup))
}

fn startup_info(desktop: Option<&mut Vec<u16>>) -> STARTUPINFOW {
    let mut si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    if let Some(d) = desktop {
        si.lpDesktop = PWSTR(d.as_mut_ptr());
    }
    si
}

fn finish(pi: PROCESS_INFORMATION) -> u32 {
    drop(OwnedHandle::new(pi.hProcess));
    drop(OwnedHandle::new(pi.hThread));
    pi.dwProcessId
}

/// Start a command line as this process (Python `subprocess.Popen`, without inheriting handles).
fn create_process(cmdline: &str) -> BackendResult<u32> {
    let mut cmd = wide(cmdline);
    let si = startup_info(None);
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `cmd` is a mutable NUL-terminated buffer as CreateProcessW requires; `si` and `pi` are live.
    unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
    }
    .map_err(|e| os_err("CreateProcess failed", &e))?;
    Ok(finish(pi))
}

/// Start a command line with a duplicated token (Python `_create_process_with_token`).
fn create_process_with_token(token: &OwnedHandle, cmdline: &str) -> BackendResult<u32> {
    let mut cmd = wide(cmdline);
    let mut desktop = wide("winsta0\\default");
    let si = startup_info(Some(&mut desktop));
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `cmd` and `desktop` are NUL-terminated buffers that outlive the call; the token is primary.
    unsafe {
        CreateProcessWithTokenW(
            token.0,
            CREATE_PROCESS_LOGON_FLAGS(LOGON_WITH_PROFILE),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
    }
    .map_err(|e| os_err("CreateProcessWithTokenW failed", &e))?;
    Ok(finish(pi))
}

/// Impersonation of a token on this thread, reverted when dropped.
struct Impersonation;

impl Impersonation {
    fn begin(token: &OwnedHandle) -> BackendResult<Self> {
        // SAFETY: `token` is a live impersonation-capable token; reverted by `Drop`.
        unsafe { ImpersonateLoggedOnUser(token.0) }
            .map_err(|e| os_err("could not impersonate the normal-integrity token", &e))?;
        Ok(Self)
    }
}

impl Drop for Impersonation {
    fn drop(&mut self) {
        // SAFETY: ends the impersonation begun in `begin` on this same thread.
        let _ = unsafe { RevertToSelf() };
    }
}

/// `ShellExecute` with the "open" verb (Python `os.startfile`). The process id when the shell says.
fn shell_open(file: &str, params: &str) -> BackendResult<Option<u32>> {
    ensure_com();
    let file_w = wide(file);
    let params_w = wide(params);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file_w.as_ptr()),
        lpParameters: if params.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(params_w.as_ptr())
        },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: the strings are NUL-terminated and outlive the call; `info` is fully initialised.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|e| os_err(&format!("could not open {file}"), &e))?;
    if info.hProcess.is_invalid() {
        return Ok(None);
    }
    let proc = OwnedHandle::new(info.hProcess);
    // SAFETY: `proc` is the process handle the shell returned.
    let pid = unsafe { GetProcessId(proc.0) };
    Ok((pid != 0).then_some(pid))
}

/// Best-effort de-elevated shell open: impersonate the normal-integrity token, then ShellExecute.
/// Weaker than starting the program with the token: some shell handlers re-elevate internally.
fn shell_open_deelevated(file: &str, params: &str) -> BackendResult<Option<u32>> {
    let token = deelevated_token()?;
    let _impersonating = Impersonation::begin(&token)?;
    shell_open(file, params)
}

fn explorer_exe() -> String {
    let root = env("SystemRoot").unwrap_or_else(|| r"C:\Windows".to_string());
    format!(r"{root}\explorer.exe")
}

fn start_executable(path: &str, args: &str, deelevate: bool) -> BackendResult<LaunchOutcome> {
    let cmd = command_line(path, args);
    if deelevate && own_elevation() {
        let token = deelevated_token()?;
        let pid = create_process_with_token(&token, &cmd)?;
        return Ok(LaunchOutcome { pid: Some(pid) });
    }
    // Not elevated: start it directly; if CreateProcess refuses (an app-execution alias, a document
    // type), the shell's own "open" is what `cmd /c start` was in the Python build.
    match create_process(&cmd) {
        Ok(pid) => Ok(LaunchOutcome { pid: Some(pid) }),
        Err(first) => shell_open(path, args)
            .map(|pid| LaunchOutcome { pid })
            .map_err(|_| first),
    }
}

/// Start something (Python `do_open`'s launch branches).
pub(crate) fn launch(spec: &LaunchSpec) -> BackendResult<LaunchOutcome> {
    let deelevate = spec.deelevate && own_elevation();
    match &spec.target {
        AppTarget::Executable(path) => start_executable(path, &spec.args, spec.deelevate),
        AppTarget::Packaged(id) => {
            // The Apps folder opens a packaged app, and Explorer is a plain program to start.
            start_executable(&explorer_exe(), &apps_folder_arg(id), spec.deelevate)
        }
        AppTarget::Shortcut(lnk) => {
            if deelevate {
                // A .lnk launched through the shell kept the daemon's admin rights (Notepad++ came up
                // "[Administrator]"): resolve it to its program and start that de-elevated.
                if let Some(t) = resolve_shortcut(Path::new(lnk))
                    && !t.target.is_empty()
                    && Path::new(&t.target).exists()
                {
                    let args = if spec.args.is_empty() {
                        t.args.clone()
                    } else {
                        spec.args.clone()
                    };
                    if let Ok(out) = start_executable(&t.target, &args, true) {
                        return Ok(out);
                    }
                }
                return shell_open_deelevated(lnk, &spec.args).map(|pid| LaunchOutcome { pid });
            }
            shell_open(lnk, &spec.args).map(|pid| LaunchOutcome { pid })
        }
        AppTarget::Other(target) => {
            let pid = if deelevate {
                shell_open_deelevated(target, &spec.args)?
            } else {
                shell_open(target, &spec.args)?
            };
            Ok(LaunchOutcome { pid })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_variables_expand_the_windows_way() {
        let root = std::env::var("SystemRoot").expect("SystemRoot is always set on Windows");
        assert_eq!(
            expand_env(r"%SystemRoot%\explorer.exe"),
            format!(r"{root}\explorer.exe")
        );
        assert_eq!(expand_env("no variables"), "no variables");
        assert_eq!(expand_env(""), "");
        // An unknown variable is left as typed, which is what Windows does.
        assert_eq!(
            expand_env("%WH_WIN_NO_SUCH_VARIABLE%"),
            "%WH_WIN_NO_SUCH_VARIABLE%"
        );
    }

    #[test]
    fn explorer_is_under_the_system_root() {
        assert!(explorer_exe().to_lowercase().ends_with(r"\explorer.exe"));
        assert!(Path::new(&explorer_exe()).exists());
    }

    #[test]
    #[ignore = "reads this machine's registry"]
    fn app_paths_come_from_the_registry() {
        let machine = Machine::default();
        let found: Vec<(String, String)> = ["brave", "excel", "FreeCAD", "iexplore", "chrome"]
            .iter()
            .filter_map(|n| machine.app_paths(n).map(|p| ((*n).to_string(), p)))
            .collect();
        println!("App Paths hits: {found:?}");
        assert!(!found.is_empty(), "no well-known App Paths entry resolved");
        assert!(
            found
                .iter()
                .all(|(_, p)| Path::new(p).exists() && !p.contains('"'))
        );
        assert_eq!(machine.app_paths("definitely-not-registered-xyzzy"), None);
    }

    #[test]
    #[ignore = "reads this machine's Start Menu"]
    fn the_shell_parser_and_the_file_parser_agree_on_start_menu_shortcuts() {
        let machine = Machine::default();
        let all: Vec<PathBuf> = machine
            .roots()
            .iter()
            .flat_map(|r| list_shortcuts(r))
            .collect();
        let (mut both, mut differ, mut shell_only, mut file_only) = (0, Vec::new(), 0, 0);
        for lnk in &all {
            let shell = shell_shortcut(lnk).filter(|t| !t.target.is_empty());
            let file = std::fs::read(lnk)
                .ok()
                .and_then(|b| parse_lnk(&b))
                .filter(|t| !t.target.is_empty());
            match (shell, file) {
                (Some(s), Some(f)) => {
                    both += 1;
                    if !s.target.eq_ignore_ascii_case(&f.target) || s.args != f.args {
                        differ.push((lnk.clone(), s, f));
                    }
                }
                (Some(_), None) => shell_only += 1,
                (None, Some(_)) => file_only += 1,
                (None, None) => {}
            }
        }
        println!(
            "{} shortcuts: {both} parsed by both ({} differ), {shell_only} only by the shell, {file_only} only from the file",
            all.len(),
            differ.len()
        );
        for (p, s, f) in differ.iter().take(8) {
            println!("  {}\n    shell {s:?}\n    file  {f:?}", p.display());
        }
        assert!(!all.is_empty());
        assert!(both > 0);
        assert!(
            differ.len() * 10 <= both.max(1),
            "the two parsers disagree on too many shortcuts"
        );
    }

    #[test]
    #[ignore = "reads this machine's Start Menu"]
    fn a_real_shortcut_resolves_to_a_program() {
        let machine = Machine::default();
        let lnk = machine
            .shortcut("notepad++")
            .or_else(|| machine.shortcut("brave"));
        let Some(lnk) = lnk else {
            println!("no Notepad++ or Brave shortcut here; nothing to resolve");
            return;
        };
        let target = machine.shortcut_target(&lnk);
        println!("{lnk} -> {target:?}");
        assert!(target.is_some_and(|t| t.to_lowercase().ends_with(".exe")));
    }

    #[test]
    #[ignore = "reads the shell's Apps folder"]
    fn the_apps_folder_lists_packaged_apps() {
        let machine = Machine::default();
        let t = std::time::Instant::now();
        let paint = machine.store_app("paint");
        let first = t.elapsed();
        let t = std::time::Instant::now();
        let again = machine.store_app("snipping");
        let cached = t.elapsed();
        println!(
            "Apps folder read took {first:?}; a cached lookup {cached:?}: paint {paint:?}, snipping {again:?}"
        );
        assert!(paint.is_some_and(|id| id.contains('!')));
        assert!(cached < std::time::Duration::from_millis(50));
    }
}
