//! Approvals: the "ask the person" path for steps the safety rules would otherwise refuse flat.
//!
//! Two refusals can be put to the person instead of ending the step: the irreversible-button guard
//! (a click on Send, Buy, Delete and the like under `only_apps`) and the destructive hard line
//! ([`crate::destructive`]). When the MCP server marks a call as able to ask (`approvable`), the first
//! such refusal of the call becomes a [`Pending`] step, bound to the exact call, the exact step and the
//! exact target. The daemon turns it into one of two things:
//!
//! * an [`Offer`] (`approvable: "card"`): a single-use token and a short sentence. The MCP server shows
//!   the sentence as an Allow/Deny card (MCP elicitation) and, on Allow, sends the same call again with
//!   the token. The token never reaches the model.
//! * a chat code (`approvable: "chat"`, a client that cannot show a card): four characters with at
//!   least one digit. The answer tells the model to show the person the sentence and wait. The person
//!   runs `hekaton approve CODE` in his own terminal, which tells the daemon (the command refuses
//!   when its stdin is not a terminal, so an agent's shell cannot run it, and the hard line refuses
//!   typing it). Only then does the same call repeated with `approval_code` go ahead. Seeing the
//!   code is not enough to use it, so the model cannot approve itself by repeating it.
//!
//! Either way the approval opens one step of one call, once, for a few minutes; a destructive
//! approval lifts only the one rule that refused, for that step. Deny, no answer or a client that
//! cannot ask leaves the refusal exactly as it was. On a call that can ask, the model's own
//! `allow_irreversible` / `allow_destructive` no longer lift anything (they become the question);
//! the open profile never asks. A call without `approvable` (the `wh` command line, scripts, every
//! test that predates this module) sees no change at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::destructive::Category;

/// How long a card token stays valid after the daemon issued it.
pub const TOKEN_TTL: Duration = Duration::from_secs(300);

/// How long the MCP server waits for the person to answer a card.
pub const CARD_WAIT: Duration = Duration::from_secs(120);

/// How long a chat code stays valid after the daemon issued it.
pub const CODE_TTL: Duration = Duration::from_secs(600);

/// The most characters of typed text a card shows.
pub const TYPED_SHOWN: usize = 80;

/// Characters in a chat code.
pub const CODE_LEN: usize = 4;

/// The characters of a chat code: no 0, O, 1 or I. Exactly 32, so one random byte picks one.
const CODE_CHARS: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Offers (and, separately, codes) outstanding at once; the oldest goes first.
const MAX_PENDING: usize = 32;

/// Call arguments that carry the approval itself. They are not part of what an approval is bound to.
pub const CONTROL_KEYS: [&str; 5] = [
    "allow_irreversible",
    "allow_destructive",
    "approvable",
    "approval_token",
    "approval_code",
];

/// What kind of step wants approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A click on a button named send, buy, delete and the like under `only_apps`.
    IrreversibleButton,
    /// One of the destructive-action rules.
    Destructive(Category),
}

impl Kind {
    /// A short stable name, part of what an approval is bound to and of the log line.
    pub fn tag(self) -> String {
        match self {
            Kind::IrreversibleButton => "irreversible-button".into(),
            Kind::Destructive(c) => format!("destructive-{}", c.label().replace(' ', "-")),
        }
    }
}

/// A step that was stopped and could go ahead if the person agrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// What kind of step.
    pub kind: Kind,
    /// The exact target as the guard saw it (a button name, the refusal of a command). Part of the
    /// binding: if the target changed since the person said yes, the step asks again.
    pub subject: String,
    /// What the step does, in plain words: `Click "Send"`. Empty = filled in from the step.
    pub action: String,
    /// The same without anything typed, for the log. Empty = same as `action`.
    pub action_log: String,
    /// Why it was stopped, in a few words: `looks hard to undo`.
    pub why: String,
    /// Where: `in Mail`. Empty when unknown.
    pub place: String,
    /// The full refusal text, exactly what the caller gets when nobody approves.
    pub reason: String,
}

impl Need {
    /// A need with no action text or place yet.
    pub fn new(
        kind: Kind,
        subject: impl Into<String>,
        why: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            subject: subject.into(),
            action: String::new(),
            action_log: String::new(),
            why: why.into(),
            place: String::new(),
            reason: reason.into(),
        }
    }

    /// What an approval is bound to besides the call and the step: the kind and the exact target.
    pub fn key(&self) -> String {
        format!("{}\n{}", self.kind.tag(), self.subject)
    }

    /// The card sentence: `Click "Send" (looks hard to undo) in Mail`.
    pub fn summary(&self) -> String {
        compose(&self.action, &self.why, &self.place)
    }

    /// The same sentence for the log (no typed text).
    pub fn log_summary(&self) -> String {
        let action = if self.action_log.is_empty() {
            &self.action
        } else {
            &self.action_log
        };
        compose(action, &self.why, &self.place)
    }
}

fn compose(action: &str, why: &str, place: &str) -> String {
    let mut out = action.to_string();
    if !why.is_empty() {
        out.push_str(&format!(" ({why})"));
    }
    if !place.is_empty() {
        out.push(' ');
        out.push_str(place);
    }
    out
}

/// How this call can reach the person, as the MCP server said (`approvable`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Nobody to ask: refusals stay refusals (every caller but the MCP server).
    #[default]
    Off,
    /// The client can show an Allow/Deny card.
    Card,
    /// The client cannot show a card: ask in chat with a code.
    Chat,
}

impl Mode {
    /// Read the `approvable` argument: `"card"` (or `true`), `"chat"`, anything else is off.
    pub fn from_arg(v: Option<&Value>) -> Self {
        match v {
            Some(Value::Bool(true)) => Mode::Card,
            Some(Value::String(s)) => match s.as_str() {
                "card" => Mode::Card,
                "chat" => Mode::Chat,
                _ => Mode::Off,
            },
            _ => Mode::Off,
        }
    }
}

/// One call's side of approvals. Cloned with the call's options (a recipe step's narrowed context
/// shares the same cell), never kept past the call.
#[derive(Debug, Clone, Default)]
pub struct CallApproval {
    /// How this call can reach the person.
    pub mode: Mode,
    /// The approval this call resumes on, if any.
    pub resume: Option<Arc<Entry>>,
    /// The need the guard noted when it refused, for the batch loop to pick up. Only written when
    /// `mode` is not off, so a plain call never touches it.
    pub noted: Arc<Mutex<Option<Need>>>,
    /// Only on the one rerun of an approved step: the [`Need::key`] the guard may let through.
    pub granted: Option<String>,
}

impl CallApproval {
    /// Can this call ask, or is it running a step the person already approved?
    pub fn active(&self) -> bool {
        self.mode != Mode::Off || self.granted.is_some()
    }

    /// The guard refused for a reason the person could approve.
    pub fn note(&self, need: Need) {
        *self.noted.lock().unwrap_or_else(|e| e.into_inner()) = Some(need);
    }

    /// The need behind `reason`, if the guard noted one (taken, so it is read once).
    pub fn take_noted(&self, reason: &str) -> Option<Need> {
        let mut noted = self.noted.lock().unwrap_or_else(|e| e.into_inner());
        noted.take().filter(|n| n.reason == reason)
    }
}

/// The offer a call's answer carries when a step is waiting for a yes on a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Single use, valid [`TOKEN_TTL`].
    pub token: String,
    /// The card sentence.
    pub summary: String,
    /// The sentence for the log.
    pub log_summary: String,
    /// [`Kind::tag`].
    pub kind: String,
    /// The 1-based step that is waiting.
    pub step: usize,
}

impl Offer {
    /// The `approval` object of the daemon's answer.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "token": self.token,
            "summary": self.summary,
            "log_summary": self.log_summary,
            "kind": self.kind,
            "step": self.step,
        })
    }

    /// Read the `approval` object back (the MCP server). `None` when it is malformed.
    pub fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            token: s("token").filter(|t| !t.is_empty())?,
            summary: s("summary").filter(|t| !t.is_empty())?,
            log_summary: s("log_summary").unwrap_or_default(),
            kind: s("kind").unwrap_or_default(),
            step: v.get("step").and_then(Value::as_u64).unwrap_or(0) as usize,
        })
    }
}

/// What the batch loop knows when a step stops for approval: the daemon turns it into an [`Offer`] or
/// a chat code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// What is waiting.
    pub need: Need,
    /// [`step_id`] of the waiting step.
    pub step_id: String,
    /// Its 1-based position among the call's steps (a recipe counts as its own steps).
    pub index: usize,
    /// The lines of the steps that already ran.
    pub notes: Vec<String>,
    /// The pictures those steps took.
    pub shots: Vec<String>,
}

/// An issued, unused approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// [`call_id`] of the call it was issued for.
    pub call_id: String,
    /// [`step_id`] of the step that may run.
    pub step_id: String,
    /// Its 1-based position.
    pub index: usize,
    /// [`Need::key`].
    pub key: String,
    /// When it stops being valid (the daemon clock).
    pub expires: Duration,
    /// Lines of the steps that ran before it.
    pub notes: Vec<String>,
    /// Their pictures.
    pub shots: Vec<String>,
}

/// Why a card token could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeError {
    /// Never issued, or already used.
    Unknown,
    /// Older than [`TOKEN_TTL`].
    Expired,
}

impl TakeError {
    /// The refusal text.
    pub fn message(self) -> &'static str {
        match self {
            TakeError::Unknown => {
                "that approval was already used or never existed, so nothing ran. Ask again by \
                 repeating the original call"
            }
            TakeError::Expired => {
                "that approval expired (they last 5 minutes), so nothing ran. Ask again by \
                 repeating the original call"
            }
        }
    }
}

/// Where a chat code stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeState {
    Waiting,
    Approved,
    Denied,
}

#[derive(Debug, Clone)]
struct CodeEntry {
    entry: Entry,
    state: CodeState,
    /// [`Need::log_summary`], for the one line `hekaton approve` prints.
    summary: String,
}

/// Why a chat code could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeError {
    /// Never issued, or already used.
    Unknown,
    /// Older than [`CODE_TTL`].
    Expired,
    /// Issued, but the person has not said yes yet.
    NotYet(String),
    /// The person said no.
    Denied,
}

impl CodeError {
    /// The refusal text.
    pub fn message(&self) -> String {
        match self {
            CodeError::Unknown => "that approval code was already used or never existed, so \
                 nothing ran. Repeat the original call to get a new code"
                .to_string(),
            CodeError::Expired => "that approval code expired (they last 10 minutes), so \
                 nothing ran. Repeat the original call to get a new code"
                .to_string(),
            CodeError::NotYet(code) => {
                format!(
                    "The owner has not approved {code} yet, so nothing ran; wait for their reply"
                )
            }
            CodeError::Denied => "The owner denied this, so nothing ran; do not retry".to_string(),
        }
    }
}

/// Make a code as typed in chat comparable: trimmed and upper case.
pub fn normalize_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

/// Is this shaped like a chat code (right length, right characters, at least one digit)?
pub fn is_code(code: &str) -> bool {
    code.len() == CODE_LEN
        && code.bytes().all(|b| CODE_CHARS.contains(&b))
        && code.bytes().any(|b| b.is_ascii_digit())
}

/// The message a card carries.
pub fn card_message(summary: &str) -> String {
    format!("Hekaton wants to: {summary}")
}

/// The text a caller gets when no card can be shown: the one thing to tell the person, and how to
/// carry on once he answered.
pub fn code_refusal(summary: &str, code: &str) -> String {
    format!(
        "Needs the owner's OK. Show them this in chat: \"{}\". Ask them to run \"hekaton approve {code}\" \
         (or \"hekaton deny {code}\") in their own terminal. After they say they did, repeat this same \
         call with approval_code \"{code}\". Never run or type the approve command yourself.",
        card_message(summary)
    )
}

/// The tokens and chat codes the daemon has issued and nobody has used yet.
#[derive(Debug, Default)]
pub struct Store {
    entries: HashMap<String, Entry>,
    codes: HashMap<String, CodeEntry>,
}

impl Store {
    /// Issue a card token for `entry` (its `expires` is set here). `None` when no randomness is
    /// available, in which case there is simply no card.
    pub fn mint(&mut self, mut entry: Entry, now: Duration) -> Option<String> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).ok()?;
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        self.entries.retain(|_, e| e.expires > now);
        while self.entries.len() >= MAX_PENDING {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.expires)
                .map(|(t, _)| t.clone())?;
            self.entries.remove(&oldest);
        }
        entry.expires = now + TOKEN_TTL;
        self.entries.insert(token.clone(), entry);
        Some(token)
    }

    /// Use a card token: removes it either way, so a second use always fails.
    pub fn take(&mut self, token: &str, now: Duration) -> Result<Entry, TakeError> {
        let entry = self.entries.remove(token).ok_or(TakeError::Unknown)?;
        if entry.expires <= now {
            return Err(TakeError::Expired);
        }
        Ok(entry)
    }

    /// Issue a chat code for `entry`: 4 characters with a digit, waiting for `hekaton approve`.
    /// `None` when no randomness is available.
    pub fn mint_code(&mut self, mut entry: Entry, summary: &str, now: Duration) -> Option<String> {
        self.codes.retain(|_, c| c.entry.expires > now);
        while self.codes.len() >= MAX_PENDING {
            let oldest = self
                .codes
                .iter()
                .min_by_key(|(_, c)| c.entry.expires)
                .map(|(k, _)| k.clone())?;
            self.codes.remove(&oldest);
        }
        let code = loop {
            let mut raw = [0u8; CODE_LEN];
            getrandom::fill(&mut raw).ok()?;
            let code: String = raw
                .iter()
                .map(|b| CODE_CHARS[usize::from(b & 31)] as char)
                .collect();
            // a digit keeps codes apart from words: "allow / deny" once unlocked a code "DENY"
            if is_code(&code) && !self.codes.contains_key(&code) {
                break code;
            }
        };
        entry.expires = now + CODE_TTL;
        self.codes.insert(
            code.clone(),
            CodeEntry {
                entry,
                state: CodeState::Waiting,
                summary: summary.to_string(),
            },
        );
        Some(code)
    }

    /// The person's answer (`hekaton approve|deny`). Returns the log sentence of the step. A code
    /// answers once: a second answer, even the same one, is refused.
    pub fn answer_code(
        &mut self,
        code: &str,
        allow: bool,
        now: Duration,
    ) -> Result<String, CodeError> {
        let code = normalize_code(code);
        let c = self.codes.get_mut(&code).ok_or(CodeError::Unknown)?;
        if c.entry.expires <= now {
            self.codes.remove(&code);
            return Err(CodeError::Expired);
        }
        if c.state != CodeState::Waiting {
            return Err(CodeError::Unknown);
        }
        c.state = if allow {
            CodeState::Approved
        } else {
            CodeState::Denied
        };
        Ok(c.summary.clone())
    }

    /// Use a code on the repeated call. Only an approved code gives its entry, and only once.
    pub fn take_code(&mut self, code: &str, now: Duration) -> Result<Entry, CodeError> {
        let code = normalize_code(code);
        let c = self.codes.get(&code).ok_or(CodeError::Unknown)?;
        if c.entry.expires <= now {
            self.codes.remove(&code);
            return Err(CodeError::Expired);
        }
        match c.state {
            CodeState::Waiting => Err(CodeError::NotYet(code)),
            CodeState::Denied => Err(CodeError::Denied),
            CodeState::Approved => {
                let c = self.codes.remove(&code).ok_or(CodeError::Unknown)?;
                Ok(c.entry)
            }
        }
    }

    /// How many card tokens are outstanding (tests).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// How many chat codes are outstanding (tests).
    pub fn codes_len(&self) -> usize {
        self.codes.len()
    }

    /// No outstanding card tokens.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn short_hash(v: &Value) -> String {
    let digest = Sha256::digest(v.to_string().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn without_control_keys(m: &Map<String, Value>) -> Map<String, Value> {
    let mut m = m.clone();
    for k in CONTROL_KEYS {
        m.remove(k);
    }
    m
}

/// A short stable fingerprint of one step: the step as sent, minus the approval arguments.
pub fn step_id(step: &Map<String, Value>) -> String {
    short_hash(&Value::Object(without_control_keys(step)))
}

/// A short stable fingerprint of a whole call, minus the approval arguments: an approval issued for
/// one call does not open the same step inside a different batch.
pub fn call_id(args: &Map<String, Value>) -> String {
    short_hash(&Value::Object(without_control_keys(args)))
}

/// Cut typed text for a card: one line, [`TYPED_SHOWN`] characters, line breaks shown as `[Enter]`.
pub fn shown(text: &str) -> String {
    let flat = text
        .replace("\r\n", "\n")
        .replace(['\r', '\n'], " [Enter] ");
    let flat = flat.trim();
    if flat.chars().count() <= TYPED_SHOWN {
        flat.to_string()
    } else {
        let head: String = flat.chars().take(TYPED_SHOWN).collect();
        format!("{head}...")
    }
}

fn text_of(step: &Map<String, Value>, key: &str) -> String {
    match step.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn text_of_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What a step does, in plain words, for a card. `redact` hides typed text (for the log).
pub fn describe_step(step: &Map<String, Value>, redact: bool) -> String {
    let action = text_of(step, "action");
    let text = text_of(step, "text");
    let quoted = |s: &str| -> String {
        if redact {
            format!("[{} characters]", s.chars().count())
        } else {
            format!("\"{}\"", shown(s))
        }
    };
    match action.as_str() {
        "type" => format!("Type {}", quoted(&text)),
        "left_click" | "double_click" | "right_click" | "middle_click" | "triple_click" => {
            let verb = match action.as_str() {
                "double_click" => "Double-click",
                "right_click" => "Right-click",
                "middle_click" => "Middle-click",
                "triple_click" => "Triple-click",
                _ => "Click",
            };
            match step.get("coordinate").and_then(Value::as_array) {
                Some(xy) if xy.len() == 2 => format!(
                    "{verb} at ({}, {})",
                    text_of_value(&xy[0]),
                    text_of_value(&xy[1])
                ),
                _ => verb.to_string(),
            }
        }
        "click_text" => format!("Click {}", quoted(&text)),
        "key" => format!("Press {}", shown(&text)),
        "" => "Do one step".to_string(),
        other if text.is_empty() => format!("Run {other}"),
        other => format!("Run {other} with {}", quoted(&text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(step: &str) -> Entry {
        Entry {
            call_id: "c".into(),
            step_id: step.into(),
            index: 1,
            key: "k".into(),
            expires: Duration::ZERO,
            notes: vec![],
            shots: vec![],
        }
    }

    #[test]
    fn a_token_works_once() {
        let mut s = Store::default();
        let t = s.mint(entry("a"), Duration::from_secs(10)).unwrap();
        assert_eq!(t.len(), 32);
        assert!(s.take(&t, Duration::from_secs(11)).is_ok());
        assert_eq!(s.take(&t, Duration::from_secs(12)), Err(TakeError::Unknown));
        assert!(s.is_empty());
    }

    #[test]
    fn a_token_expires_after_five_minutes() {
        let mut s = Store::default();
        let t = s.mint(entry("a"), Duration::from_secs(100)).unwrap();
        assert_eq!(
            s.take(&t, Duration::from_secs(100) + TOKEN_TTL),
            Err(TakeError::Expired)
        );
        let t2 = s.mint(entry("a"), Duration::from_secs(100)).unwrap();
        let just_before = Duration::from_secs(100) + TOKEN_TTL - Duration::from_secs(1);
        assert!(s.take(&t2, just_before).is_ok());
    }

    #[test]
    fn an_invented_token_is_unknown() {
        let mut s = Store::default();
        assert_eq!(s.take("nope", Duration::ZERO), Err(TakeError::Unknown));
    }

    #[test]
    fn the_store_stays_small() {
        let mut s = Store::default();
        for _ in 0..(MAX_PENDING * 2) {
            s.mint(entry("a"), Duration::ZERO).unwrap();
            s.mint_code(entry("a"), "x", Duration::ZERO).unwrap();
        }
        assert!(s.len() <= MAX_PENDING);
        assert!(s.codes_len() <= MAX_PENDING);
    }

    #[test]
    fn the_ids_ignore_the_approval_arguments_only() {
        let a = json!({"action": "left_click", "coordinate": [1, 2], "allow_irreversible": false});
        let b = json!({"action": "left_click", "coordinate": [1, 2], "approval_token": "x",
                       "approval_code": "K7QM", "approvable": "chat", "allow_irreversible": true});
        let c = json!({"action": "left_click", "coordinate": [1, 3]});
        let id = |v: &Value| step_id(v.as_object().unwrap());
        assert_eq!(id(&a), id(&b));
        assert_ne!(id(&a), id(&c));
        let call = |v: &Value| call_id(v.as_object().unwrap());
        assert_eq!(call(&a), call(&b));
        assert_ne!(call(&a), call(&c));
    }

    #[test]
    fn the_mode_comes_from_the_approvable_argument() {
        assert_eq!(Mode::from_arg(None), Mode::Off);
        assert_eq!(Mode::from_arg(Some(&json!(true))), Mode::Card);
        assert_eq!(Mode::from_arg(Some(&json!("card"))), Mode::Card);
        assert_eq!(Mode::from_arg(Some(&json!("chat"))), Mode::Chat);
        assert_eq!(Mode::from_arg(Some(&json!("yes please"))), Mode::Off);
        assert_eq!(Mode::from_arg(Some(&json!(false))), Mode::Off);
    }

    #[test]
    fn typed_text_is_cut_and_line_breaks_are_shown() {
        assert_eq!(shown("hi\nthere"), "hi [Enter] there");
        let long = "x".repeat(200);
        let s = shown(&long);
        assert_eq!(s.chars().count(), TYPED_SHOWN + 3);
        assert!(s.ends_with("..."));
    }

    #[test]
    fn the_card_sentence_has_action_why_and_place() {
        let mut n = Need::new(Kind::IrreversibleButton, "Send", "looks hard to undo", "r");
        n.action = "Click \"Send\"".into();
        n.place = "in Mail".into();
        assert_eq!(n.summary(), "Click \"Send\" (looks hard to undo) in Mail");
    }

    #[test]
    fn the_log_sentence_leaves_typed_text_out() {
        let mut n = Need::new(
            Kind::Destructive(Category::OsDestruction),
            "s",
            "could x",
            "r",
        );
        n.action = "Type \"my secret\"".into();
        n.action_log = "Type [9 characters]".into();
        assert!(n.summary().contains("my secret"));
        assert!(!n.log_summary().contains("my secret"));
    }

    #[test]
    fn describe_step_is_plain() {
        let step = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            describe_step(&step(json!({"action": "type", "text": "hello"})), false),
            "Type \"hello\""
        );
        assert_eq!(
            describe_step(&step(json!({"action": "type", "text": "hello"})), true),
            "Type [5 characters]"
        );
        assert_eq!(
            describe_step(
                &step(json!({"action": "left_click", "coordinate": [10, 20]})),
                false
            ),
            "Click at (10, 20)"
        );
    }

    #[test]
    fn an_offer_survives_json() {
        let o = Offer {
            token: "t".into(),
            summary: "s".into(),
            log_summary: "l".into(),
            kind: "k".into(),
            step: 3,
        };
        assert_eq!(Offer::from_json(&o.to_json()), Some(o));
        assert_eq!(Offer::from_json(&json!({"token": ""})), None);
    }

    #[test]
    fn a_code_is_four_safe_characters_with_a_digit_and_waits_for_a_yes() {
        let mut s = Store::default();
        let now = Duration::from_secs(5);
        for _ in 0..50 {
            let c = s.mint_code(entry("a"), "sum", now).unwrap();
            assert!(is_code(&c), "{c}");
            assert!(c.bytes().any(|b| b.is_ascii_digit()), "{c}");
            assert!(!c.contains(['0', 'O', '1', 'I']), "{c}");
            assert_eq!(s.take_code(&c, now), Err(CodeError::NotYet(c.clone())));
        }
        assert!(!is_code("DENY"), "a word is never a code");
        assert!(is_code("K7QM"));
    }

    #[test]
    fn an_approved_code_works_once_and_in_any_case() {
        let mut s = Store::default();
        let now = Duration::from_secs(5);
        let c = s.mint_code(entry("a"), "Click \"Send\"", now).unwrap();
        let lower = c.to_ascii_lowercase();
        assert_eq!(s.answer_code(&lower, true, now).unwrap(), "Click \"Send\"");
        assert_eq!(
            s.take_code(&format!(" {lower} "), now).unwrap().step_id,
            "a"
        );
        assert_eq!(s.take_code(&c, now), Err(CodeError::Unknown));
        assert_eq!(s.codes_len(), 0);
    }

    #[test]
    fn a_denied_code_stays_denied_and_cannot_be_flipped() {
        let mut s = Store::default();
        let now = Duration::ZERO;
        let c = s.mint_code(entry("a"), "x", now).unwrap();
        s.answer_code(&c, false, now).unwrap();
        assert_eq!(s.take_code(&c, now), Err(CodeError::Denied));
        assert_eq!(s.answer_code(&c, true, now), Err(CodeError::Unknown));
        assert_eq!(s.take_code(&c, now), Err(CodeError::Denied));
    }

    #[test]
    fn a_code_expires_after_ten_minutes_even_when_approved() {
        let mut s = Store::default();
        let now = Duration::from_secs(100);
        let c = s.mint_code(entry("a"), "x", now).unwrap();
        s.answer_code(&c, true, now).unwrap();
        assert_eq!(s.take_code(&c, now + CODE_TTL), Err(CodeError::Expired));
        let c2 = s.mint_code(entry("a"), "x", now).unwrap();
        assert_eq!(
            s.answer_code(&c2, true, now + CODE_TTL),
            Err(CodeError::Expired)
        );
        assert_eq!(s.answer_code("ZZ2Z", true, now), Err(CodeError::Unknown));
    }

    #[test]
    fn the_code_refusal_has_the_card_sentence_and_the_code() {
        let t = code_refusal("Click \"Send\" (looks hard to undo)", "K7QM");
        assert_eq!(
            t,
            "Needs the owner's OK. Show them this in chat: \"Hekaton wants to: Click \"Send\" \
             (looks hard to undo)\". Ask them to run \"hekaton approve K7QM\" (or \"hekaton deny \
             K7QM\") in their own terminal. After they say they did, repeat this same call with \
             approval_code \"K7QM\". Never run or type the approve command yourself."
        );
    }
}
