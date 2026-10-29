//! The owner's hard line: what winhands refuses no matter what else is allowed.
//!
//! Port of `winhands/destructive.py`. This is the one place that list lives. The rule the owner
//! set on 2026-09-24 is: "nothing off limits except what can destroy the machine or steal
//! credentials". So this module is NOT a general safety policy. It does not care about untidy, rude
//! or expensive actions; the everyday guards (`only_apps`, the agent-host refusal, the
//! human-takeover pause, the password-field guard) are separate. What is below is the short list of
//! actions that leave the machine unbootable, unrecoverable, unprotected, or that hand somebody
//! else the owner's credentials.
//!
//! Five categories ([`Category`]):
//!
//! 1. OS destruction: deleting/renaming/moving/overwriting anything under `%SystemRoot%`
//!    (`C:\Windows`, System32, SysWOW64), the EFI or boot partitions, or a drive root; format,
//!    diskpart, bcdedit, takeown/icacls on system paths, `reg delete` of the HKLM\SYSTEM or
//!    HKLM\SOFTWARE\Microsoft\Windows NT hives.
//! 2. Killing recovery: vssadmin/wmic shadow-copy deletion, disabling System Restore, wbadmin delete.
//! 3. Turning off protection: Defender real-time protection, the firewall, UAC (`EnableLUA`), BitLocker.
//! 4. Credential theft: SAM / SECURITY / SYSTEM hives, NTDS.dit, browser Login Data and cookie
//!    databases, LSASS memory.
//! 5. UAC consent: clicking Yes/Allow on a consent.exe or credential prompt.
//!
//! The macOS equivalents of 1 to 4 live in the `mac` submodule (`destructive/mac.rs`) and are checked
//! by the same entry points: `rm -r` of `/System`, `~` and the other roots, `diskutil erase*`,
//! `dd` onto `/dev/disk*`, `tmutil delete`, `csrutil disable`, `security dump-keychain`, keychain,
//! browser and SSH-key theft, and so on. Consent on the Mac is handled in `wh-mac` (roles.rs).
//!
//! These refusals are ALWAYS on: whether or not `only_apps` is set, whether or not the machine has
//! been acknowledged, and for every front end (MCP, the `wh` command line, a raw HTTP call).
//!
//! The opt-in: a caller that really means it sends `allow_destructive: true` on that one call. It is
//! per call, never sticky, and it is the only thing that turns any of this off. Python kept the flag
//! in a thread-local the daemon set before each call; here it is an explicit [`Policy`] value built
//! from each call's own arguments ([`Policy::take_from_args`]), so it cannot leak into the next call
//! or across async tasks.
//!
//! Enforced where the action happens, not only on launch strings: `open`/`open_app` launch text, the
//! text the tool types (it types into terminals), `file_dialog` paths, and pointer actions landing on
//! a UAC consent window.

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::backend::{Backend, Point};

mod mac;

/// The argument name of the per-call opt-in.
pub const OPT_IN: &str = "allow_destructive";

/// Process names of the Windows elevation and credential prompts (Python `CONSENT_PROCESSES`).
pub const CONSENT_PROCESSES: [&str; 3] = ["consent.exe", "credentialuibroker.exe", "lsaiso.exe"];

/// Why an action is on the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// Leaves the machine unbootable.
    OsDestruction,
    /// Destroys the only copies that could undo a mistake.
    KillingRecovery,
    /// Leaves the machine defenceless.
    TurningOffProtection,
    /// Hands somebody else the owner's credentials.
    CredentialTheft,
    /// Approves an elevation prompt the person never saw.
    UacConsent,
    /// Approves a Hekaton step on the person's behalf (2026-10-29). Never lifted by a flag or an
    /// approval: only the person answers an approval.
    SelfApproval,
}

impl Category {
    /// The label printed in a refusal, as in Python.
    pub fn label(self) -> &'static str {
        match self {
            Category::OsDestruction => "OS destruction",
            Category::KillingRecovery => "killing recovery",
            Category::TurningOffProtection => "turning off protection",
            Category::CredentialTheft => "credential theft",
            Category::UacConsent => "UAC consent",
            Category::SelfApproval => "self-approval",
        }
    }

    /// What doing it could cost, in words for the refusal (Python `CONSEQUENCE`).
    pub fn consequence(self) -> &'static str {
        match self {
            Category::OsDestruction => "leave this machine unbootable",
            Category::KillingRecovery => "destroy the only copies that could undo a mistake",
            Category::TurningOffProtection => "leave this machine defenceless",
            Category::CredentialTheft => "hand somebody else the owner's credentials",
            Category::UacConsent => "approve an elevation prompt the person never saw",
            Category::SelfApproval => "approve a step that only the owner may approve",
        }
    }
}

/// Raised instead of doing something on the list above (Python `DestructiveAction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructiveAction {
    /// Which of the five categories.
    pub category: Category,
    /// The full refusal text, which always names [`OPT_IN`].
    pub message: String,
}

impl fmt::Display for DestructiveAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DestructiveAction {}

fn refuse(what: &str, category: Category, where_: &str) -> DestructiveAction {
    DestructiveAction {
        category,
        message: format!(
            "refused: {what} ({}). winhands refuses this whether or not an allowlist is set, \
             because it can {}. Seen in {where_}. If that is genuinely what you want, send the same \
             call again with {OPT_IN}=true, which turns this off for that one call.",
            category.label(),
            category.consequence()
        ),
    }
}

const ALL_CATEGORIES: [Category; 5] = [
    Category::OsDestruction,
    Category::KillingRecovery,
    Category::TurningOffProtection,
    Category::CredentialTheft,
    Category::UacConsent,
];

/// A refusal this module wrote, read back: its category, what was refused and where it was seen.
/// `None` for any other text. The text is the one place the refusal survives once it became a
/// step error, so the approval path ([`crate::approval`]) reads it here, next to [`refuse`].
pub fn parse_refusal(message: &str) -> Option<(Category, &str, &str)> {
    let rest = message.strip_prefix("refused: ")?;
    let (category, what, after) = ALL_CATEGORIES.iter().find_map(|c| {
        let mark = format!(" ({}). winhands refuses this", c.label());
        rest.find(&mark)
            .map(|at| (*c, &rest[..at], &rest[at + mark.len()..]))
    })?;
    let seen = after.find(". Seen in ")? + ". Seen in ".len();
    let tail = format!(
        ". If that is genuinely what you want, send the same call again with {OPT_IN}=true"
    );
    let end = after[seen..].find(&tail)? + seen;
    Some((category, what, &after[seen..end]))
}

// ---------------------------------------------------------------- paths

// A path is "system" when a whole component matches, never a substring: a user folder called
// "system32-notes" or "my-windows-backup" is ordinary and must stay allowed.
static DRIVE_ROOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]:\\?(?:\*(?:\.\*)?)?$").expect("DRIVE_ROOT"));
static WINDOWS_ROOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]:[\\/]windows([\\/]|$)").expect("WINDOWS_ROOT"));
static ENV_SYSTEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^%(systemroot|windir)%").expect("ENV_SYSTEM"));
static DRIVE_LETTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]:$").expect("DRIVE_LETTER"));

const SYSTEM_COMPONENTS: [&str; 6] = [
    "system32",
    "syswow64",
    "winsxs",
    "$windows.~bt",
    "efi",
    "boot",
];
// The EFI system partition and the boot folder are only special at the root of a drive.
const ROOT_ONLY_COMPONENTS: [&str; 2] = ["efi", "boot"];

fn normalize_path(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_lowercase()
}

/// True for `%SystemRoot%` and below, System32/SysWOW64 anywhere, `\EFI` or `\boot` at a drive
/// root, and a bare drive root itself. Python `is_system_path`.
pub fn is_system_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let p = normalize_path(path);
    if ENV_SYSTEM.is_match(&p) || DRIVE_ROOT.is_match(&p) || WINDOWS_ROOT.is_match(&p) {
        return true;
    }
    let parts: Vec<&str> = p.split('\\').filter(|c| !c.is_empty()).collect();
    let rooted = parts
        .first()
        .is_some_and(|first| DRIVE_LETTER.is_match(first));
    for (i, comp) in parts.iter().enumerate() {
        if ROOT_ONLY_COMPONENTS.contains(comp) {
            if rooted && i == 1 {
                return true;
            }
            continue;
        }
        if SYSTEM_COMPONENTS.contains(comp) {
            return true;
        }
    }
    false
}

// Files whose contents ARE the credentials.
static CREDENTIAL_FILES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let windows = [
        r"\\ntds\.dit\b",
        r"\\config\\(sam|security|system)\b",
        r"\\user data\\[^\\]*\\?(login data|cookies|web data)\b",
        r"\\(login data|cookies)( for account)?\b.*\\user data\\",
        r"\\logins\.json\b|\\key[34]\.db\b", // Firefox
    ];
    // The macOS stores (Keychains, Application Support browser folders, Safari cookies). They are
    // matched on the same backslash-normalised text, so they sit in the same list.
    windows
        .iter()
        .map(|p| p.to_string())
        .chain(mac::CREDENTIAL_FILES.iter().cloned())
        .map(|p| Regex::new(&p).expect("CREDENTIAL_FILES"))
        .collect()
});

/// True for the files whose contents are credentials (NTDS.dit, SAM/SECURITY/SYSTEM hives, browser
/// Login Data and cookie databases, Firefox logins). Python `is_credential_path`.
pub fn is_credential_path(path: &str) -> bool {
    let p = normalize_path(path);
    CREDENTIAL_FILES.iter().any(|rx| rx.is_match(&p))
}

// ---------------------------------------------------------------- commands
//
// Every pattern below is anchored at a command position: the start of a line, or just after a
// shell separator (; & && | ( ) or a `cmd /c` / `powershell -Command` prefix. That is what keeps
// the word "format" inside a sentence, or "diskpart" typed into a chat message, from tripping it:
// a document is not a command line.

const CMD_START: &str = r"(?:^|[\n;&|(`]|\bcmd(?:\.exe)?\s+/\w+\s+|\bpowershell(?:\.exe)?\s+(?:-\w+\s+)*|\bstart\s+|\bsudo\s+|\brunas\s+[^\n]*?\s)\s*";

const DESTRUCTIVE_VERBS: &str = r"(?:del|erase|rd|rmdir|move|ren|rename|copy|xcopy|robocopy|attrib|cacls|icacls|takeown|remove-item|ri|rm|move-item|rename-item|set-content|out-file|clear-content|new-item)";

/// One refusal rule: a command pattern, the words for the refusal, and the category.
struct Rule {
    what: &'static str,
    category: Category,
    rx: Regex,
}

fn rule(what: &'static str, category: Category, pattern: &str, flags: &str) -> Rule {
    let rx = Regex::new(&format!("(?{flags}){pattern}")).expect("command rule");
    Rule { what, category, rx }
}

static COMMAND_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    use Category::{CredentialTheft, KillingRecovery, OsDestruction, TurningOffProtection};
    let at_start = |tail: &str| format!("{CMD_START}{tail}");
    let mut rules = vec![
        // 1. OS destruction
        rule(
            "a format command",
            OsDestruction,
            &at_start(r"format(?:\.com)?\s+(?:[a-z]:|/(?:fs|q|x)\b)"),
            "i",
        ),
        rule(
            "a diskpart command",
            OsDestruction,
            &at_start(r"diskpart\b"),
            "i",
        ),
        rule(
            "a bcdedit command",
            OsDestruction,
            &at_start(r"bcdedit\b"),
            "i",
        ),
        rule(
            "a registry delete of a system hive",
            OsDestruction,
            r#"\breg(?:\.exe)?\s+delete\s+"?hk(?:lm|ey_local_machine)\\(?:system|software\\microsoft\\windows nt)"#,
            "i",
        ),
        rule(
            "a PowerShell delete of a system registry hive",
            OsDestruction,
            r"\bremove-item(?:property)?\b[^\n]*hklm:\\(?:system|software\\microsoft\\windows nt)",
            "i",
        ),
        // 2. Killing recovery
        rule(
            "a shadow-copy deletion",
            KillingRecovery,
            r"\bvssadmin(?:\.exe)?\s+(?:delete|resize)\s+shadow",
            "i",
        ),
        rule(
            "a shadow-copy deletion",
            KillingRecovery,
            r"\bwmic\b[^\n]*\bshadowcopy\b[^\n]*\bdelete\b",
            "i",
        ),
        rule(
            "a shadow-copy deletion",
            KillingRecovery,
            r"\bget-wmiobject\b[^\n]*win32_shadowcopy[^\n]*\bdelete\b",
            "i",
        ),
        rule(
            "disabling System Restore",
            KillingRecovery,
            r"\bdisable-computerrestore\b|\bsrclient\b[^\n]*disable|\bnet\s+stop\s+(?:vss|swprv)\b",
            "i",
        ),
        rule(
            "a backup deletion",
            KillingRecovery,
            r"\bwbadmin(?:\.exe)?\s+delete\b",
            "i",
        ),
        // 3. Turning off protection
        rule(
            "disabling Defender real-time protection",
            TurningOffProtection,
            r"\bset-mppreference\b[^\n]*-disable(?:realtimemonitoring|antispyware|behaviormonitoring)\b[^\n]*(?:\$true|1)?",
            "i",
        ),
        rule(
            "stopping Defender",
            TurningOffProtection,
            r#"\b(?:sc(?:\.exe)?\s+(?:stop|config)|net\s+stop)\s+"?(?:windefend|wdnissvc|sense)\b"#,
            "i",
        ),
        rule(
            "turning the firewall off",
            TurningOffProtection,
            r"\bnetsh\b[^\n]*(?:adv)?firewall\b[^\n]*\bstate\s+off\b",
            "i",
        ),
        rule(
            "turning the firewall off",
            TurningOffProtection,
            r"\bset-netfirewallprofile\b[^\n]*-enabled\s+(?:false|0)\b",
            "i",
        ),
        rule(
            "turning UAC off",
            TurningOffProtection,
            r"\benablelua\b[^\n]*(?:/d\s*0|\s0\s*$|\s-value\s*0)",
            "im",
        ),
        rule(
            "turning BitLocker off",
            TurningOffProtection,
            r"\bmanage-bde\b[^\n]*-(?:off|disable)\b|\bdisable-bitlocker\b",
            "i",
        ),
        // 4. Credential theft
        rule(
            "a dump of a credential hive",
            CredentialTheft,
            r#"\breg(?:\.exe)?\s+save\s+"?hk(?:lm|ey_local_machine)\\(?:sam|security|system)\b"#,
            "i",
        ),
        rule(
            "a copy of the AD database",
            CredentialTheft,
            r"\bntdsutil\b|\bntds\.dit\b",
            "i",
        ),
        rule(
            "an LSASS memory dump",
            CredentialTheft,
            r"\blsass(?:\.exe)?\b[^\n]*\b(?:dump|minidump|procdump|-ma\b)|\b(?:procdump|comsvcs\.dll[^\n]*minidump|out-minidump)\b[^\n]*lsass",
            "i",
        ),
        rule(
            "a known credential-dumping tool",
            CredentialTheft,
            r"\bmimikatz\b|\bsekurlsa::|\blsadump::",
            "i",
        ),
    ];
    rules.extend(mac::rules());
    rules
});

/// A destructive verb at a command position followed by its arguments (group `rest`).
static VERB_AT_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i){CMD_START}{DESTRUCTIVE_VERBS}\b(?P<rest>[^\n]*)"
    ))
    .expect("verb")
});
static ARGUMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""[^"]+"|\S+"#).expect("arg"));

/// Typed or launched text that would answer a Hekaton approval: the `approve` command, or an HTTP
/// call to the daemon's `/approve` route (2026-10-29). A model that typed it into a terminal would be
/// approving its own step, so no flag and no approval lifts this.
static SELF_APPROVAL: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#"(?i)\b(?:winhands|hekaton)(?:\.exe)?["']?\s+approve\b"#,
        r"(?i)\b(?:curl|wget|invoke-webrequest|invoke-restmethod|iwr|irm|xh|httpie)\b[^\n]*/approve\b",
        r"(?i)(?:127\.0\.0\.1|localhost|\[::1\]|0\.0\.0\.0)(?::\d+)?/approve\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("self-approval rule"))
    .collect()
});

/// `approve CODE` through any path or variable (`"$BIN" approve K7QM`): the word and a live-looking
/// code (see [`crate::approval::is_code`]), so "approve 2026 budget" is left alone.
static APPROVE_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bapprove["']?\s+["']?([a-z0-9]{4})\b"#).expect("approve code")
});

fn answers_an_approval(text: &str) -> bool {
    SELF_APPROVAL.iter().any(|rx| rx.is_match(text))
        || APPROVE_CODE
            .captures_iter(text)
            .any(|c| crate::approval::is_code(&crate::approval::normalize_code(&c[1])))
}

/// The refusal for [`answers_an_approval`]. It does not offer [`OPT_IN`], since that does not lift it.
fn refuse_self_approval(where_: &str) -> DestructiveAction {
    DestructiveAction {
        category: Category::SelfApproval,
        message: format!(
            "refused: a command that answers a Hekaton approval ({}). Only the owner approves a step, \
             by answering its card or running the approve command himself; nothing lifts this. \
             Seen in {where_}.",
            Category::SelfApproval.label()
        ),
    }
}

/// A short fingerprint of a refusal's text, so [`Policy::only`] stays `Copy`.
fn fingerprint(message: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    message.hash(&mut h);
    h.finish()
}

/// The per-call switch. `Policy::default()` refuses everything on the list; the only way to turn it
/// off is the call's own `allow_destructive: true`, for that call alone, or the person's approval of
/// one refusal ([`Policy::only`]), for that step alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    allowed: bool,
    /// The fingerprint of the one refusal the person approved; every other rule stays on.
    granted: Option<u64>,
}

impl Policy {
    /// Every rule on. The state every call starts from.
    pub const REFUSE: Policy = Policy {
        allowed: false,
        granted: None,
    };

    /// Rules off, for the one call this value is built for.
    pub const fn allow() -> Self {
        Policy {
            allowed: true,
            granted: None,
        }
    }

    /// Every rule on except the one that wrote `refusal` (the exact text, so the same rule seen in
    /// another place, or another rule, is still refused). For the rerun of a step the person
    /// approved (see [`crate::approval`]).
    pub fn only(refusal: &str) -> Self {
        Policy {
            allowed: false,
            granted: Some(fingerprint(refusal)),
        }
    }

    /// Refuse, unless this is the one refusal the person approved.
    fn deny(self, what: &str, category: Category, where_: &str) -> Result<(), DestructiveAction> {
        let refusal = refuse(what, category, where_);
        if self.granted == Some(fingerprint(&refusal.message)) {
            return Ok(());
        }
        Err(refusal)
    }

    /// Pop `allow_destructive` off a call's arguments (so it is never forwarded) and build the
    /// policy for that call. Python: `destructive.set_allowed(bool(args.pop(OPT_IN, False)))`.
    pub fn take_from_args(args: &mut Map<String, Value>) -> Self {
        let allowed = args
            .remove(OPT_IN)
            .is_some_and(|v| crate::config::truthy(&v));
        Policy {
            allowed,
            granted: None,
        }
    }

    /// Is the opt-in set for this call?
    pub fn is_allowed(self) -> bool {
        self.allowed
    }

    /// Refuse a command line, typed or launched, that matches the list. Cheap: plain regex.
    /// `where_` names the surface for the message ("the launch string", "the text to type", ...).
    /// Python `check_command`.
    pub fn check_command(self, text: &str, where_: &str) -> Result<(), DestructiveAction> {
        if text.is_empty() {
            return Ok(());
        }
        if answers_an_approval(text) {
            return Err(refuse_self_approval(where_));
        }
        if self.allowed {
            return Ok(());
        }
        for r in COMMAND_RULES.iter() {
            if r.rx.is_match(text) {
                self.deny(r.what, r.category, where_)?;
            }
        }
        // A destructive verb aimed at a system path, e.g. `del C:\Windows\System32\*` or
        // `Remove-Item %SystemRoot%\... -Recurse`. The verb has to be at a command position and
        // the path has to be a real system path by component, so "notes about del in
        // system32-notes" is left alone.
        for caps in VERB_AT_COMMAND.captures_iter(text) {
            let rest = caps.name("rest").map_or("", |m| m.as_str());
            for arg in ARGUMENT.find_iter(rest) {
                let arg = arg.as_str().trim_matches('"');
                if arg.starts_with('-') || arg.starts_with('/') {
                    continue;
                }
                if is_system_path(arg) {
                    self.deny(
                        "a write to a Windows system path",
                        Category::OsDestruction,
                        where_,
                    )?;
                }
                if is_credential_path(arg) {
                    self.deny(
                        "a copy of a credential store",
                        Category::CredentialTheft,
                        where_,
                    )?;
                }
            }
        }
        // The macOS half: rm/mv of a whole root or the OS trees or Time Machine backups, readers and
        // copiers aimed at a key or credential store (see `destructive/mac.rs`).
        if let Some((what, category)) = mac::check_args(text) {
            return Err(refuse(what, category, where_));
        }
        if is_credential_path(text) {
            self.deny("a credential store path", Category::CredentialTheft, where_)?;
        }
        Ok(())
    }

    /// Refuse saving or opening into a system path or a credential store (file dialogs).
    /// Python `check_path`.
    pub fn check_path(self, path: &str, where_: &str) -> Result<(), DestructiveAction> {
        if self.allowed || path.is_empty() {
            return Ok(());
        }
        if is_system_path(path) {
            self.deny(
                "a file path inside a Windows system location",
                Category::OsDestruction,
                where_,
            )?;
        }
        if let Some((what, category)) = mac::check_path(path) {
            return Err(refuse(what, category, where_));
        }
        if is_credential_path(path) {
            self.deny("a credential store path", Category::CredentialTheft, where_)?;
        }
        Ok(())
    }

    /// Text the tool is about to type somewhere. It types into terminals, so a command typed into
    /// one is the same action as launching it. Python `check_text`.
    pub fn check_text(self, text: &str, where_: &str) -> Result<(), DestructiveAction> {
        self.check_command(text, where_)
    }

    /// A click on the UAC consent dialog, or on a credential prompt, approves elevation on the
    /// owner's behalf. Only the person at the machine gets to do that. Python
    /// `check_consent_window`.
    pub fn check_consent_window(
        self,
        process_name: &str,
        where_: &str,
    ) -> Result<(), DestructiveAction> {
        if self.allowed || process_name.is_empty() {
            return Ok(());
        }
        if CONSENT_PROCESSES.contains(&process_name.to_lowercase().as_str()) {
            self.deny(
                &format!("a click on the Windows elevation or credential prompt ({process_name})"),
                Category::UacConsent,
                where_,
            )?;
        }
        Ok(())
    }

    /// The pointer-action half of the consent rule: look at the top-level window under `point` and
    /// refuse when it belongs to a consent or credential process. A point with no window, or a
    /// window whose process cannot be read, is not refused here (Python `computer._host_at`).
    pub fn check_point(
        self,
        backend: &dyn Backend,
        point: Point,
        where_: &str,
    ) -> Result<(), DestructiveAction> {
        let Ok(Some(window)) = backend.window_at_point(point) else {
            return Ok(());
        };
        let app = backend
            .window_info(window)
            .map(|w| w.app)
            .unwrap_or_default();
        self.check_consent_window(&app, where_)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;
    use crate::backend::{Rect, WindowInfo};

    const OFF: Policy = Policy::REFUSE;

    /// Ported helper `refuses(text, category)`.
    fn refuses(text: &str, category: Option<Category>) -> String {
        let err = OFF
            .check_command(text, "a test")
            .expect_err(&format!("should refuse: {text}"));
        assert!(err.message.contains(OPT_IN), "{}", err.message);
        if let Some(c) = category {
            assert_eq!(err.category, c, "{text}");
            assert!(err.message.contains(c.label()), "{}", err.message);
        }
        err.message
    }

    /// Ported helper `allows(text)`.
    fn allows(text: &str) {
        if let Err(e) = OFF.check_command(text, "a test") {
            panic!("should allow {text:?}: {e}");
        }
    }

    // ---------------------------------------------------------------- 1. OS destruction

    #[test]
    fn os_destruction_is_refused() {
        for cmd in [
            r"del C:\Windows\System32\drivers\etc\hosts",
            r"rmdir /s /q C:\Windows\System32",
            r#"Remove-Item "%SystemRoot%\System32\winload.efi" -Force"#,
            r"move C:\Windows\explorer.exe D:\junk",
            r"takeown /f C:\Windows\System32\config",
            r"icacls C:\Windows /grant everyone:F",
            r"format D: /fs:ntfs /q",
            r"diskpart /s wipe.txt",
            r"bcdedit /delete {current}",
            r#"reg delete "HKLM\SYSTEM\CurrentControlSet\Services" /f"#,
            r"reg delete HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion /f",
            r"del C:\*.*",
            r"Remove-Item C:\EFI -Recurse",
        ] {
            refuses(cmd, Some(Category::OsDestruction));
        }
    }

    #[test]
    fn ordinary_work_is_allowed() {
        for cmd in [
            r"del C:\Users\jon\system32-notes\draft.txt",
            r"Remove-Item C:\Users\jon\Documents\windows-notes.md",
            r"move C:\projects\boot\readme.md C:\projects\archive\\",
            r"del C:\Users\jon\Downloads\installer.exe",
            "The disk needs a format before we can use it.",
            "I told him the format of the report was wrong.",
            "Type 'format' into the search box to find the paragraph tool.",
            "diskpartners.example.com is the vendor",
            r"copy report.docx C:\Users\jon\Documents\\",
        ] {
            allows(cmd);
        }
    }

    #[test]
    fn system_path_matching_is_by_component() {
        assert!(is_system_path(r"C:\Windows\System32\cmd.exe"));
        assert!(is_system_path(r"c:/windows/syswow64"));
        assert!(is_system_path(r"%SystemRoot%\System32"));
        assert!(is_system_path("C:\\"));
        assert!(is_system_path(r"D:\EFI\Microsoft\Boot"));
        assert!(!is_system_path(r"C:\Users\jon\system32-notes"));
        assert!(!is_system_path(r"C:\Users\jon\Documents\boot"));
        assert!(!is_system_path(r"C:\Users\jon\windows-backup\file.txt"));
        assert!(!is_system_path(r"C:\Users\jon"));
    }

    // ---------------------------------------------------------------- 2. killing recovery

    #[test]
    fn killing_recovery_is_refused() {
        for cmd in [
            "vssadmin delete shadows /all /quiet",
            "vssadmin resize shadowstorage /for=c: /maxsize=1MB",
            "wmic shadowcopy delete",
            "Disable-ComputerRestore -Drive C:\\",
            "wbadmin delete catalog -quiet",
            "net stop vss",
        ] {
            refuses(cmd, Some(Category::KillingRecovery));
        }
    }

    #[test]
    fn recovery_reads_are_allowed() {
        for cmd in [
            "vssadmin list shadows",
            "wbadmin start backup -backupTarget:D:",
            "Enable-ComputerRestore -Drive C:\\",
            "The backup deleted itself overnight, apparently.",
        ] {
            allows(cmd);
        }
    }

    // ---------------------------------------------------------------- 3. turning off protection

    #[test]
    fn turning_off_protection_is_refused() {
        for cmd in [
            "Set-MpPreference -DisableRealtimeMonitoring $true",
            "sc stop WinDefend",
            "net stop windefend",
            "netsh advfirewall set allprofiles state off",
            "Set-NetFirewallProfile -Profile Domain,Public -Enabled False",
            r"reg add HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System /v EnableLUA /t REG_DWORD /d 0 /f",
            "manage-bde -off C:",
            "Disable-BitLocker -MountPoint C:",
        ] {
            refuses(cmd, Some(Category::TurningOffProtection));
        }
    }

    #[test]
    fn protection_reads_and_tuning_are_allowed() {
        for cmd in [
            "Get-MpComputerStatus",
            "netsh advfirewall show allprofiles",
            "manage-bde -status C:",
            "Set-MpPreference -ScanAvgCPULoadFactor 20",
        ] {
            allows(cmd);
        }
    }

    // ---------------------------------------------------------------- 4. credential theft

    #[test]
    fn credential_theft_is_refused() {
        for cmd in [
            r"reg save HKLM\SAM sam.hive",
            r"reg save HKLM\SECURITY sec.hive",
            r"copy C:\Windows\NTDS\ntds.dit D:\out\\",
            r#"copy "C:\Users\jon\AppData\Local\Google\Chrome\User Data\Default\Login Data" C:\Users\jon\out.db"#,
            "procdump -ma lsass.exe lsass.dmp",
            "rundll32 comsvcs.dll MiniDump 640 lsass.dmp full",
            "mimikatz # sekurlsa::logonpasswords",
        ] {
            refuses(cmd, Some(Category::CredentialTheft));
        }
    }

    #[test]
    fn credential_near_misses_are_allowed() {
        for cmd in [
            "Get-Process lsass",
            r"copy C:\Users\jon\Documents\login-notes.txt D:\backup\\",
            "The login data for the demo account is in 1Password.",
        ] {
            allows(cmd);
        }
    }

    #[test]
    fn credential_paths() {
        assert!(is_credential_path(r"C:\Windows\NTDS\ntds.dit"));
        assert!(is_credential_path(r"C:\Windows\System32\config\SAM"));
        assert!(!is_credential_path(
            r"C:\Users\jon\Documents\cookies-recipe.txt"
        ));
    }

    // ---------------------------------------------------------------- 5. UAC consent windows

    #[test]
    fn clicking_a_consent_prompt_is_refused() {
        let err = OFF
            .check_consent_window("consent.exe", "a test")
            .unwrap_err();
        assert!(err.message.contains("consent.exe"), "{}", err.message);
        assert_eq!(err.category, Category::UacConsent);
        assert!(
            OFF.check_consent_window("CredentialUIBroker.exe", "a test")
                .is_err()
        );
        assert!(OFF.check_consent_window("notepad.exe", "a test").is_ok());
    }

    /// Ported from `test_pointer_actions_check_the_window_under_them`: the window under the point
    /// is looked up through the Backend, and only a consent process is refused.
    #[test]
    fn pointer_actions_check_the_window_under_them() {
        let backend = FakeBackend::new();
        let consent = backend.add_window(WindowInfo::basic(
            "Do you want to allow this app?",
            "consent.exe",
            11,
            Rect::new(0, 0, 500, 400),
        ));
        let point = Point { x: 100, y: 100 };
        backend.set_window_at_point(Some(consent));
        let err = OFF
            .check_point(&backend, point, "the window under the pointer")
            .unwrap_err();
        assert!(err.message.contains("consent.exe"), "{}", err.message);

        backend.set_window_at_point(None);
        assert!(
            OFF.check_point(&backend, point, "the window under the pointer")
                .is_ok()
        );

        let notepad = backend.add_window(WindowInfo::basic(
            "a - Notepad",
            "notepad.exe",
            12,
            Rect::new(0, 0, 500, 400),
        ));
        backend.set_window_at_point(Some(notepad));
        assert!(
            OFF.check_point(&backend, point, "the window under the pointer")
                .is_ok()
        );
    }

    // ---------------------------------------------------------------- where it is enforced

    #[test]
    fn the_typed_text_path_is_guarded() {
        // computer._type_text calls check_text before sending a single key.
        let backend = FakeBackend::new();
        let opts = crate::input::TypeOptions::new(500);
        let err = crate::input::type_text(&backend, OFF, "vssadmin delete shadows /all", &opts)
            .unwrap_err();
        assert!(matches!(err, crate::input::TypeError::Refused(_)));
        assert!(
            backend.typed_text().is_empty(),
            "nothing may be typed after a refusal"
        );
    }

    #[test]
    fn the_launch_path_is_guarded() {
        // core.do_open calls check_command on the whole launch string before anything starts.
        assert!(
            OFF.check_command("cmd /c format D: /q", "the launch string")
                .is_err()
        );
        assert!(
            OFF.check_command("notepad.exe C:\\Users\\jon\\notes.txt", "the launch string")
                .is_ok()
        );
    }

    #[test]
    fn the_file_dialog_path_is_guarded() {
        assert!(
            OFF.check_path(r"C:\Windows\System32\drivers\etc\hosts", "a test")
                .is_err()
        );
        assert!(
            OFF.check_path(r"C:\Users\jon\Documents\notes.txt", "a test")
                .is_ok()
        );
    }

    #[test]
    fn the_opt_in_turns_it_off_for_one_call_only() {
        let on = Policy::allow();
        assert!(
            on.check_command("vssadmin delete shadows /all", "a test")
                .is_ok()
        );
        assert!(on.check_path(r"C:\Windows\System32\x", "a test").is_ok());
        assert!(on.check_consent_window("consent.exe", "a test").is_ok());
        // The daemon builds the policy from each call's own arguments, so the next call starts
        // from "on" again.
        let mut next_call = Map::new();
        let policy = Policy::take_from_args(&mut next_call);
        assert!(!policy.is_allowed());
        assert!(
            policy
                .check_command("vssadmin delete shadows /all", "a test")
                .is_err()
        );
    }

    #[test]
    fn refusals_apply_with_no_allowlist_set() {
        // No only_apps in sight: the rules never consult one.
        assert!(
            OFF.check_command("Disable-BitLocker -MountPoint C:", "a test")
                .is_err()
        );
    }

    #[test]
    fn the_daemon_reads_the_opt_in_off_the_call() {
        let mut args: Map<String, Value> =
            serde_json::from_str(r#"{"allow_destructive": true, "x": 1}"#).unwrap();
        let policy = Policy::take_from_args(&mut args);
        assert!(policy.is_allowed());
        assert!(
            !args.contains_key(OPT_IN),
            "the flag is not forwarded to the handler"
        );
        assert_eq!(args["x"], 1);
        let mut none = Map::new();
        assert!(!Policy::take_from_args(&mut none).is_allowed());
        let mut falsy: Map<String, Value> =
            serde_json::from_str(r#"{"allow_destructive": false}"#).unwrap();
        assert!(!Policy::take_from_args(&mut falsy).is_allowed());
    }

    // ---------------------------------------------------------------- extra, beyond the Python tests

    #[test]
    fn every_refusal_names_the_surface_the_consequence_and_the_way_out() {
        let msg = refuses(
            "vssadmin delete shadows /all",
            Some(Category::KillingRecovery),
        );
        assert!(
            msg.starts_with("refused: a shadow-copy deletion (killing recovery)."),
            "{msg}"
        );
        assert!(msg.contains("Seen in a test."), "{msg}");
        assert!(
            msg.contains("destroy the only copies that could undo a mistake"),
            "{msg}"
        );
        assert!(msg.contains("allow_destructive=true"), "{msg}");
    }

    #[test]
    fn the_command_position_anchor_follows_shell_separators_and_wrappers() {
        refuses("echo hi; diskpart /s x.txt", Some(Category::OsDestruction));
        refuses("echo hi && bcdedit /set x", Some(Category::OsDestruction));
        refuses(
            "powershell -NoProfile format D: /q",
            Some(Category::OsDestruction),
        );
        refuses("cmd /c format D: /q", Some(Category::OsDestruction));
        refuses(
            "line one\nformat C: /fs:ntfs",
            Some(Category::OsDestruction),
        );
    }

    #[test]
    fn empty_text_and_unrelated_text_pass() {
        assert!(OFF.check_command("", "a test").is_ok());
        assert!(OFF.check_path("", "a test").is_ok());
        assert!(OFF.check_consent_window("", "a test").is_ok());
        allows("hello world");
    }

    #[test]
    fn quoted_and_slashed_arguments_are_normalised() {
        assert!(is_system_path("\"C:\\Windows\\System32\""));
        assert!(is_system_path(r"C:"));
        assert!(is_system_path(r"C:\*"));
        assert!(is_system_path(r"c:/windows"));
        assert!(!is_system_path(""));
        assert!(is_credential_path(
            r#""C:/Users/jon/AppData/Local/Google/Chrome/User Data/Default/Cookies""#
        ));
        assert!(is_credential_path(
            r"C:\Users\jon\AppData\Roaming\Mozilla\Firefox\logins.json"
        ));
        assert!(is_credential_path(r"C:\x\key4.db"));
    }

    #[test]
    fn only_a_whole_drive_letter_roots_the_efi_and_boot_rule() {
        assert!(is_system_path(r"C:\boot\bcd"));
        assert!(!is_system_path(r"\\server\share\boot"));
        assert!(!is_system_path(r"boot"));
    }

    #[test]
    fn a_consent_process_name_is_matched_case_insensitively() {
        assert!(OFF.check_consent_window("CONSENT.EXE", "x").is_err());
        assert!(OFF.check_consent_window("lsaiso.exe", "x").is_err());
    }

    #[test]
    fn every_refusal_reads_back_as_its_category_what_and_where() {
        for c in ALL_CATEGORIES {
            let r = refuse("a click on the prompt (consent.exe)", c, "the text to type");
            assert_eq!(
                parse_refusal(&r.message),
                Some((c, "a click on the prompt (consent.exe)", "the text to type"))
            );
        }
        let real = OFF
            .check_text("format c: /q", "the text to type")
            .unwrap_err();
        let (c, _, at) = parse_refusal(&real.message).unwrap();
        assert_eq!((c, at), (real.category, "the text to type"));
        assert_eq!(parse_refusal("refused: something else"), None);
        assert_eq!(parse_refusal("paused by user hotkey"), None);
    }

    #[test]
    fn typing_an_approval_is_refused_whatever_the_flags_say_and_cannot_be_approved() {
        for text in [
            "hekaton approve K7QM",
            "winhands approve k7qm\n",
            "~/Applications/winhands.app/Contents/MacOS/winhands approve K7QM",
            "\"$WINHANDS_BIN\" approve K7QM",
            "winhands.exe approve",
            "curl -X POST -H 'Authorization: x' http://127.0.0.1:47811/approve -d '{}'",
            "Invoke-RestMethod -Method Post -Uri http://localhost:47811/approve",
            "wget --post-data='code=K7QM' 127.0.0.1:47811/approve",
        ] {
            for policy in [OFF, Policy::allow(), Policy::only("anything")] {
                let err = policy.check_text(text, "the text to type").expect_err(text);
                assert_eq!(err.category, Category::SelfApproval, "{text}");
                assert!(!err.message.contains(OPT_IN), "{}", err.message);
                // not a refusal the approval path can turn into a question
                assert_eq!(parse_refusal(&err.message), None);
            }
        }
        for text in [
            "I approve 2026 budget",
            "please approve this",
            "approve the merge",
            "https://github.com/x/y/pull/1/approve",
            "hekaton deny K7QM",
        ] {
            assert!(OFF.check_text(text, "the text to type").is_ok(), "{text}");
        }
    }

    #[test]
    fn an_approved_refusal_lifts_only_that_rule_in_that_place() {
        let first = OFF
            .check_text("format c: /q", "the text to type")
            .unwrap_err();
        let only = Policy::only(&first.message);
        assert!(!only.is_allowed());
        assert!(only.check_text("format c: /q", "the text to type").is_ok());
        // the same rule seen somewhere else, and another rule, are still refused
        assert!(
            only.check_command("format c: /q", "the launch string")
                .is_err()
        );
        let other = only
            .check_text("format c: /q && diskpart", "the text to type")
            .unwrap_err();
        assert!(other.message.contains("diskpart"), "{}", other.message);
        assert!(
            only.check_text("vssadmin delete shadows /all", "the text to type")
                .is_err()
        );
        assert!(
            only.check_path(r"C:\Windows\System32\x.dll", "the save path")
                .is_err()
        );
    }
}
