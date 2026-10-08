//! Recipes: saved, parameterized batches of `computer` steps. Port of `winhands/recipes.py`
//! (2026-10-08, PLAN-fewer-turns item 6).
//!
//! A recipe is DATA: a JSON file naming the steps to run and the parameters they take. It is never
//! code. There is no eval, no scripting and no expression language here. A recipe cannot do anything a
//! model could not already do by sending the same steps in an `actions` batch, and it gets no extra
//! rights: every step goes through the same `run_many` loop, so the acknowledgement gate, the
//! destructive refusals, `only_apps`, the caller-window refusal, the human-input pause and the audit
//! log all apply exactly as they do to a hand-written batch.
//!
//! The format is the Python build's, so a recipe file written for either build loads in both:
//!
//! ```json
//! {"name": "save_as",
//!  "description": "Save the front document under a full path",
//!  "params": {"path": {"required": true, "description": "full file path"},
//!             "n":    {"default": "3"}},
//!  "only_apps": ["notepad.exe"],
//!  "steps": [{"action": "key", "text": "ctrl+s"},
//!            {"action": "file_dialog", "text": "{path}", "expect": {"gone_text": "File name"}}]}
//! ```
//!
//! * `{param}` inside any string of a step is replaced by that param's value, in one pass (a value is
//!   never scanned again, so a value holding braces cannot reach another param). A placeholder with no
//!   matching param is an error, at save time and again at run time.
//! * `on_fail: "skip"` carries on after the step fails; `when: {text|gone_text|title}` runs the step
//!   only if that holds right now. No loops and no variables beyond params.
//! * `only_apps` narrows the call's allowlist for this recipe's steps and never widens it.
//! * A recipe cannot set `allow_irreversible`, `allow_destructive` or `only_apps` on a step, and cannot
//!   run or save recipes: per-call authority belongs to the call that runs a recipe.
//!
//! Where recipes live: the bundled ones are compiled into the binary from `crates/wh-core/recipes/`,
//! the owner's own sit in `%LOCALAPPDATA%\winhands\recipes\` ([`Paths::recipes_dir`]). A user recipe
//! overrides a bundled one of the same name.
//!
//! The three actions are `recipe_list`, `recipe` (expanded in place inside `run_many`, see
//! [`expand`]) and `recipe_save` (writes to the user folder only).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::actions::ACTIONS_SORTED;
use crate::config::truthy;
use crate::orchestrate::{flag, normalize_only_app};
use crate::textutil::{py_repr, py_str, py_value_repr};

/// The actions that belong to this module. A recipe step may be none of them.
pub const ACTIONS: [&str; 3] = ["recipe", "recipe_list", "recipe_save"];

/// Most steps a recipe may have.
pub const MAX_STEPS: usize = 40;
/// Most params a recipe may take.
pub const MAX_PARAMS: usize = 12;
/// Largest recipe file, in bytes.
pub const MAX_BYTES: usize = 64 * 1024;
/// Longest value a param may be given, in characters.
pub const MAX_PARAM_CHARS: usize = 20000;

const FIELDS: [&str; 5] = ["name", "description", "params", "only_apps", "steps"];
/// Per-call authority belongs to the call that runs a recipe, never to a stored file.
const RESERVED_STEP_KEYS: [&str; 7] = [
    "allow_irreversible",
    "allow_destructive",
    "only_apps",
    "actions",
    "coords",
    "display",
    "shot_long",
];
const WHEN_KEYS: [&str; 3] = ["text", "gone_text", "title"];

/// The recipes that ship in the binary: file stem and JSON text.
pub const BUNDLED: [(&str, &str); 6] = [
    ("capture_page", include_str!("../recipes/capture_page.json")),
    ("close_app", include_str!("../recipes/close_app.json")),
    (
        "explorer_newest",
        include_str!("../recipes/explorer_newest.json"),
    ),
    ("note", include_str!("../recipes/note.json")),
    ("open_url", include_str!("../recipes/open_url.json")),
    ("save_as", include_str!("../recipes/save_as.json")),
];

static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,39}$").expect("NAME_RE"));
static PARAM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,39}$").expect("PARAM_RE"));
static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("PLACEHOLDER"));

// ---------------------------------------------------------------------------------------- types

/// Where a recipe came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Compiled into the binary.
    Bundled,
    /// The owner's own, in the user folder.
    User,
}

/// One param a recipe takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The caller must give it.
    pub required: bool,
    /// The value used when the caller gives none (`""` for an optional param with no default);
    /// `None` for a required one.
    pub default: Option<String>,
    /// One line for the model.
    pub description: String,
}

/// A validated recipe.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    /// Lowercase name, also the file stem.
    pub name: String,
    /// One line.
    pub description: String,
    /// Params in the order the file lists them.
    pub params: Vec<(String, Param)>,
    /// Process names the steps are limited to (narrows the call's allowlist, never widens it).
    pub only_apps: Vec<String>,
    /// The steps, plain JSON.
    pub steps: Vec<Value>,
    /// Bundled or the owner's.
    pub source: Source,
}

impl Recipe {
    fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|(n, _)| n == name).map(|(_, p)| p)
    }

    /// `name(p, q=9, r?)`: required params by name, optional ones with their default or a `?`.
    pub fn signature(&self) -> String {
        let parts: Vec<String> = self
            .params
            .iter()
            .map(|(name, spec)| {
                if spec.required {
                    name.clone()
                } else {
                    match spec.default.as_deref() {
                        Some(d) if !d.is_empty() => format!("{name}={d}"),
                        _ => format!("{name}?"),
                    }
                }
            })
            .collect();
        format!("{}({})", self.name, parts.join(", "))
    }

    /// Params no step uses, in the order the file lists them.
    pub fn unused_params(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        for step in &self.steps {
            collect_placeholders(step, &mut seen);
        }
        self.params
            .iter()
            .filter(|(n, _)| !seen.contains(n))
            .map(|(n, _)| n.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------------- validation

/// The recipe name in normal form, or the reason it is not a name.
pub fn norm_name(value: &Value) -> Result<String, String> {
    let name = match value {
        Value::Null => String::new(),
        v if !truthy(v) => String::new(),
        v => py_str(v).trim().to_lowercase(),
    };
    if !NAME_RE.is_match(&name) {
        return Err(format!(
            "recipe name {} must be lowercase letters, digits and underscores, starting with a \
             letter (max 40), e.g. save_as",
            py_value_repr(value)
        ));
    }
    Ok(name)
}

fn is_number(v: &Value) -> bool {
    matches!(v, Value::Number(_))
}

fn collect_placeholders(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => {
            for m in PLACEHOLDER.captures_iter(s) {
                out.insert(m[1].to_string());
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_placeholders(i, out)),
        Value::Object(map) => map.values().for_each(|i| collect_placeholders(i, out)),
        _ => {}
    }
}

/// Is `when` an object holding some of `text`, `gone_text`, `title`, each non-empty text? `where_`
/// names the step in the error.
pub fn check_when(when: &Value, where_: &str) -> Result<(), String> {
    let ok = match when {
        Value::Object(m) => {
            !m.is_empty()
                && m.keys().all(|k| WHEN_KEYS.contains(&k.as_str()))
                && m.values()
                    .all(|v| matches!(v, Value::String(s) if !s.is_empty()))
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{where_}: when must be an object with text, gone_text or title (non-empty text)"
        ))
    }
}

fn norm_params(raw: Option<&Value>) -> Result<Vec<(String, Param)>, String> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(m)) if m.is_empty() => return Ok(Vec::new()),
        Some(Value::Array(a)) if a.is_empty() => return Ok(Vec::new()),
        Some(Value::Object(m)) => m,
        Some(_) => {
            return Err(
                "params must be an object like {\"path\": {\"required\": true, \"description\": \
                 \"...\"}}"
                    .into(),
            );
        }
    };
    if raw.len() > MAX_PARAMS {
        return Err(format!("a recipe takes at most {MAX_PARAMS} params"));
    }
    let mut out = Vec::new();
    for (pname, spec) in raw {
        if !PARAM_RE.is_match(pname) {
            return Err(format!(
                "param name {} must be letters, digits and underscores",
                py_repr(pname)
            ));
        }
        let spec: Map<String, Value> = match spec {
            Value::Null => Map::new(),
            Value::String(s) => {
                let mut m = Map::new();
                m.insert("description".into(), Value::String(s.clone()));
                m
            }
            Value::Object(m)
                if m.keys()
                    .all(|k| ["required", "default", "description"].contains(&k.as_str())) =>
            {
                m.clone()
            }
            _ => {
                return Err(format!(
                    "param {pname}: use {{required, default, description}}"
                ));
            }
        };
        let default = spec.get("default").filter(|v| !v.is_null());
        if let Some(d) = default
            && !(matches!(d, Value::String(_)) || is_number(d))
        {
            return Err(format!("param {pname}: default must be text or a number"));
        }
        let required = match spec.get("required") {
            None => default.is_none(),
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s))
                if matches!(
                    s.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on" | "0" | "false" | "no" | "off"
                ) =>
            {
                matches!(
                    s.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            }
            Some(_) => {
                return Err(format!("param {pname}: required must be true or false"));
            }
        };
        if required && default.is_some() {
            return Err(format!(
                "param {pname}: required and default together; a param with a default is optional"
            ));
        }
        let description = match spec.get("description") {
            Some(v) if truthy(v) => py_str(v),
            _ => String::new(),
        };
        if description.chars().count() > 200 {
            return Err(format!("param {pname}: description over 200 characters"));
        }
        out.push((
            pname.clone(),
            Param {
                required,
                default: if required {
                    None
                } else {
                    Some(default.map(py_str).unwrap_or_default())
                },
                description,
            },
        ));
    }
    Ok(out)
}

fn recipe_action_names() -> Vec<&'static str> {
    ACTIONS_SORTED
        .iter()
        .copied()
        .filter(|a| !ACTIONS.contains(a))
        .collect()
}

fn check_step(i: usize, step: &Value, params: &[(String, Param)]) -> Result<(), String> {
    let where_ = format!("step {i}");
    let Value::Object(map) = step else {
        return Err(format!("{where_} must be an object with an action"));
    };
    let action = match map.get("action") {
        Some(v) if truthy(v) => py_str(v).trim().to_string(),
        _ => String::new(),
    };
    let allowed = recipe_action_names();
    if !allowed.contains(&action.as_str()) {
        let hint = if ACTIONS.contains(&action.as_str()) {
            " (a recipe cannot run or save recipes: copy the steps instead)"
        } else {
            ""
        };
        return Err(format!(
            "{where_}: unknown action {}{hint}; one of: {}",
            py_repr(&action),
            allowed.join(", ")
        ));
    }
    let mut bad: Vec<&str> = RESERVED_STEP_KEYS
        .iter()
        .copied()
        .filter(|k| map.contains_key(*k))
        .collect();
    bad.sort_unstable();
    if !bad.is_empty() {
        return Err(format!(
            "{where_}: {} cannot be stored in a recipe; pass it on the call that runs the recipe",
            bad.join(", ")
        ));
    }
    if let Some(on_fail) = map.get("on_fail").filter(|v| !v.is_null())
        && !matches!(py_str(on_fail).to_lowercase().as_str(), "stop" | "skip")
    {
        return Err(format!("{where_}: on_fail must be \"stop\" or \"skip\""));
    }
    if let Some(when) = map.get("when") {
        check_when(when, &where_)?;
    }
    let mut seen = BTreeSet::new();
    collect_placeholders(step, &mut seen);
    if let Some(unknown) = seen.iter().find(|p| !params.iter().any(|(n, _)| n == *p)) {
        let declared = if params.is_empty() {
            "none".to_string()
        } else {
            params
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(format!(
            "{where_}: placeholder {{{unknown}}} has no matching param (declared: {declared})"
        ));
    }
    Ok(())
}

/// A recipe object in normal form, or what is wrong with it. The `source` of the result is
/// [`Source::User`]; the loaders set the right one.
pub fn validate(data: &Value) -> Result<Recipe, String> {
    let Value::Object(map) = data else {
        return Err("a recipe is a JSON object {name, description, params, steps}".into());
    };
    let mut extra: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !FIELDS.contains(k))
        .collect();
    extra.sort_unstable();
    if !extra.is_empty() {
        let mut fields = FIELDS.to_vec();
        fields.sort_unstable();
        return Err(format!(
            "unknown recipe field(s): {}; use {}",
            extra.join(", "),
            fields.join(", ")
        ));
    }
    let name = norm_name(map.get("name").unwrap_or(&Value::Null))?;
    let description = match map.get("description") {
        Some(v) if truthy(v) => py_str(v).trim().to_string(),
        _ => String::new(),
    };
    if description.chars().count() > 300 {
        return Err("description is over 300 characters; keep it to one line".into());
    }
    let params = norm_params(map.get("params"))?;
    let only_apps: Vec<String> = match map.get("only_apps") {
        None => Vec::new(),
        Some(v) if !truthy(v) => Vec::new(),
        Some(Value::Array(list))
            if list.len() <= 10
                && list
                    .iter()
                    .all(|a| matches!(a, Value::String(s) if !s.trim().is_empty())) =>
        {
            list.iter().map(|a| py_str(a).trim().to_string()).collect()
        }
        Some(_) => {
            return Err("only_apps must be a list of process names like [\"notepad.exe\"]".into());
        }
    };
    let steps = match map.get("steps") {
        Some(Value::Array(list)) if !list.is_empty() => list.clone(),
        _ => return Err("steps must be a non-empty list of computer steps".into()),
    };
    if steps.len() > MAX_STEPS {
        return Err(format!("a recipe has at most {MAX_STEPS} steps"));
    }
    for (i, step) in steps.iter().enumerate() {
        check_step(i + 1, step, &params)?;
    }
    Ok(Recipe {
        name,
        description,
        params,
        only_apps,
        steps,
        source: Source::User,
    })
}

// ------------------------------------------------------------------------------------- loading

/// Where recipes are read from and saved to: the binary's own and the owner's folder.
#[derive(Debug, Clone)]
pub struct RecipeStore {
    user: PathBuf,
    bundled: Vec<(String, String)>,
}

impl RecipeStore {
    /// The shipped recipes plus the owner's folder `user_dir`.
    pub fn standard(user_dir: impl Into<PathBuf>) -> Self {
        Self {
            user: user_dir.into(),
            bundled: BUNDLED
                .iter()
                .map(|(n, t)| ((*n).to_string(), (*t).to_string()))
                .collect(),
        }
    }

    /// A store with the given bundled recipes (file stem, JSON text) instead of the shipped ones.
    pub fn with_bundled(user_dir: impl Into<PathBuf>, bundled: Vec<(String, String)>) -> Self {
        Self {
            user: user_dir.into(),
            bundled,
        }
    }

    /// The owner's folder.
    pub fn user_dir(&self) -> &Path {
        &self.user
    }

    /// Add or replace a bundled recipe (file stem, JSON text). For tests and embedders.
    pub fn put_bundled(&mut self, stem: &str, text: &str) {
        self.bundled.retain(|(n, _)| n != stem);
        self.bundled.push((stem.to_string(), text.to_string()));
    }

    fn bundled_text(&self, stem: &str) -> Option<&str> {
        self.bundled
            .iter()
            .find(|(n, _)| n == stem)
            .map(|(_, t)| t.as_str())
    }

    fn parse(text: &str, stem: &str, file: &str, source: Source) -> Result<Recipe, String> {
        if text.len() > MAX_BYTES {
            return Err(format!("file is over {} KB", MAX_BYTES / 1024));
        }
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut rec = validate(&data)?;
        if rec.name != stem.to_lowercase() {
            return Err(format!(
                "name {} does not match the file name {file}",
                py_repr(&rec.name)
            ));
        }
        rec.source = source;
        Ok(rec)
    }

    fn read_user(&self, stem: &str) -> Option<Result<Recipe, String>> {
        let file = self.user.join(format!("{stem}.json"));
        if !file.is_file() {
            return None;
        }
        Some(
            std::fs::metadata(&file)
                .map_err(|e| e.to_string())
                .and_then(|m| {
                    if m.len() > MAX_BYTES as u64 {
                        Err(format!("file is over {} KB", MAX_BYTES / 1024))
                    } else {
                        std::fs::read(&file).map_err(|e| e.to_string())
                    }
                })
                .and_then(|bytes| {
                    Self::parse(
                        &String::from_utf8_lossy(&bytes),
                        stem,
                        &format!("{stem}.json"),
                        Source::User,
                    )
                })
                .map_err(|why| {
                    format!("recipe {stem} (user, {}) is invalid: {why}", file.display())
                }),
        )
    }

    /// The recipe called `name`: the owner's if they have one, else the bundled one.
    pub fn get(&self, name: &Value) -> Result<Recipe, String> {
        let key = norm_name(name)?;
        if let Some(found) = self.read_user(&key) {
            return found;
        }
        if let Some(text) = self.bundled_text(&key) {
            return Self::parse(text, &key, &format!("{key}.json"), Source::Bundled)
                .map_err(|why| format!("recipe {key} (bundled, {key}.json) is invalid: {why}"));
        }
        Err(format!(
            "no recipe named {}; recipe_list shows what exists",
            py_repr(&key)
        ))
    }

    /// Every valid recipe by name, and every broken file by stem with the reason. Bundled first, then
    /// the owner's, so the owner's win.
    pub fn all(&self) -> (BTreeMap<String, Recipe>, BTreeMap<String, String>) {
        let mut found = BTreeMap::new();
        let mut bad = BTreeMap::new();
        let mut bundled: Vec<&(String, String)> = self.bundled.iter().collect();
        bundled.sort_by(|a, b| a.0.cmp(&b.0));
        for (stem, text) in bundled {
            match Self::parse(text, stem, &format!("{stem}.json"), Source::Bundled) {
                Ok(rec) => {
                    bad.remove(&rec.name);
                    found.insert(rec.name.clone(), rec);
                }
                Err(why) => {
                    bad.insert(stem.clone(), format!("bundled: {why}"));
                }
            }
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.user)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .is_some_and(|x| x.eq_ignore_ascii_case("json"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        for file in files {
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let name = file
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let read = std::fs::read(&file)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    Self::parse(&String::from_utf8_lossy(&bytes), &stem, &name, Source::User)
                });
            match read {
                Ok(rec) => {
                    bad.remove(&rec.name);
                    found.insert(rec.name.clone(), rec);
                }
                Err(why) => {
                    bad.insert(stem, format!("user: {why}"));
                }
            }
        }
        (found, bad)
    }

    /// One cheap line per recipe, broken files named instead of hidden (`recipe_list`).
    pub fn list_text(&self) -> String {
        let (found, bad) = self.all();
        let mut lines = Vec::new();
        for rec in found.values() {
            let mut desc = rec.description.clone();
            if desc.chars().count() > 110 {
                desc = format!(
                    "{}...",
                    desc.chars().take(107).collect::<String>().trim_end()
                );
            }
            lines.push(format!(
                "{}: {desc}{}",
                rec.signature(),
                if rec.source == Source::User {
                    " [yours]"
                } else {
                    ""
                }
            ));
        }
        for (name, why) in &bad {
            lines.push(format!("{name}: INVALID ({why})"));
        }
        if lines.is_empty() {
            "no recipes bundled or saved".to_string()
        } else {
            lines.join("\n")
        }
    }
}

// ---------------------------------------------------------------------------------- running

/// Param values for a run: what the caller gave, checked, else the defaults. All text, in the
/// order the recipe lists its params.
pub fn bind(rec: &Recipe, given: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    let empty = Map::new();
    let given = match given {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => {
            return Err("params must be an object like {\"path\": \"C:\\\\x.txt\"}".into());
        }
    };
    let mut unknown: Vec<&str> = given
        .keys()
        .map(String::as_str)
        .filter(|k| rec.param(k).is_none())
        .collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        let takes = if rec.params.is_empty() {
            "no params".to_string()
        } else {
            rec.params
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(format!(
            "recipe {} has no param {}; it takes: {takes}",
            rec.name,
            unknown.join(", ")
        ));
    }
    let mut values = Vec::new();
    let mut missing = Vec::new();
    for (pname, spec) in &rec.params {
        let v = given.get(pname).filter(|v| !v.is_null());
        let Some(v) = v else {
            if spec.required {
                missing.push(pname.as_str());
            } else {
                values.push((pname.clone(), spec.default.clone().unwrap_or_default()));
            }
            continue;
        };
        if !(matches!(v, Value::String(_)) || is_number(v)) {
            return Err(format!("param {pname} must be text or a number"));
        }
        let text = py_str(v);
        if text.chars().count() > MAX_PARAM_CHARS {
            return Err(format!(
                "param {pname} is over {MAX_PARAM_CHARS} characters"
            ));
        }
        values.push((pname.clone(), text));
    }
    if !missing.is_empty() {
        let hints: Vec<String> = missing
            .iter()
            .map(|p| match rec.param(p) {
                Some(s) if !s.description.is_empty() => format!("{p}: {}", s.description),
                _ => (*p).to_string(),
            })
            .collect();
        return Err(format!(
            "recipe {} needs param(s) {}",
            rec.name,
            hints.join("; ")
        ));
    }
    Ok(values)
}

fn substitute_text(s: &str, values: &[(String, String)]) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in PLACEHOLDER.captures_iter(s) {
        let whole = m.get(0).expect("whole match");
        out.push_str(&s[last..whole.start()]);
        match values.iter().find(|(n, _)| n == &m[1]) {
            Some((_, v)) => out.push_str(v),
            None => {
                return Err(format!("placeholder {{{}}} has no matching param", &m[1]));
            }
        }
        last = whole.end();
    }
    out.push_str(&s[last..]);
    Ok(out)
}

/// `v` with every `{param}` replaced by its value, in one pass over each string. Dict keys are left
/// alone, and the original is untouched.
pub fn substitute(v: &Value, values: &[(String, String)]) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => Value::String(substitute_text(s, values)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| substitute(i, values))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, i)| Ok((k.clone(), substitute(i, values)?)))
                .collect::<Result<Map<_, _>, String>>()?,
        ),
        other => other.clone(),
    })
}

/// One step ready to run, with the extras a recipe gives it. The extras live beside the step, not in
/// it, so the action itself never sees them.
#[derive(Debug, Clone, PartialEq)]
pub struct StepItem {
    /// The step as the batch loop reads it (not necessarily an object: the loop reports that).
    pub step: Value,
    /// The allowlist this step runs under, when its recipe has one of its own.
    pub scope: Option<BTreeSet<String>>,
    /// A line to print before this step's own note (the first step of a recipe says which recipe).
    pub header: String,
}

fn split_scope(entry: &str) -> (&str, &str) {
    entry.split_once(':').unwrap_or((entry, ""))
}

/// The allowlist a recipe's steps run under. A recipe can only narrow the call's allowlist: with none
/// set the recipe's own applies; with one set, only entries both allow (the stricter of a plain
/// process name and a title-scoped one). Nothing in common is an error.
fn effective_only_apps(
    rec: &Recipe,
    caller: &BTreeSet<String>,
) -> Result<Option<BTreeSet<String>>, String> {
    if rec.only_apps.is_empty() {
        return Ok(None);
    }
    let mine: BTreeSet<String> = rec
        .only_apps
        .iter()
        .map(|a| normalize_only_app(a))
        .collect();
    if caller.is_empty() {
        return Ok(Some(mine));
    }
    let mut out = BTreeSet::new();
    for c in caller {
        let (c_proc, c_scope) = split_scope(c);
        for r in &mine {
            let (r_proc, r_scope) = split_scope(r);
            if c_proc != r_proc {
                continue;
            }
            if c == r || c_scope.is_empty() {
                out.insert(r.clone());
            } else if r_scope.is_empty() {
                out.insert(c.clone());
            }
        }
    }
    if out.is_empty() {
        let list = |s: &BTreeSet<String>| {
            format!(
                "[{}]",
                s.iter().map(|e| py_repr(e)).collect::<Vec<_>>().join(", ")
            )
        };
        return Err(format!(
            "recipe {} is limited to {}, which this call's allowlist {} does not allow",
            rec.name,
            list(&mine),
            list(caller)
        ));
    }
    Ok(Some(out))
}

fn expand_one(
    store: &RecipeStore,
    call: &Map<String, Value>,
    caller: &BTreeSet<String>,
) -> Result<Vec<StepItem>, String> {
    let rec = store.get(call.get("name").unwrap_or(&Value::Null))?;
    let values = bind(&rec, call.get("params"))?;
    let scope = effective_only_apps(&rec, caller)?;
    let inherit = flag(call.get("allow_irreversible"), false);
    let mut out = Vec::new();
    for raw in &rec.steps {
        let mut step = substitute(raw, &values)?;
        if inherit && let Value::Object(m) = &mut step {
            // the caller's per-call opt-in, never the file's
            m.insert("allow_irreversible".into(), Value::Bool(true));
        }
        out.push(StepItem {
            step,
            scope: scope.clone(),
            header: String::new(),
        });
    }
    let n = out.len();
    if let Some(first) = out.first_mut() {
        first.header = format!("recipe {}: {n} steps", rec.signature());
    }
    Ok(out)
}

pub(crate) fn is_recipe_step(s: &Value) -> bool {
    s.as_object().is_some_and(|m| {
        m.get("action")
            .is_some_and(|a| truthy(a) && py_str(a).trim() == "recipe")
    })
}

/// `steps` with each `{"action": "recipe", "name": ..., "params": {...}}` replaced by that recipe's
/// steps. Fails for an unknown recipe or bad params, before anything has run. `caller` is the call's
/// own `only_apps`.
pub fn expand(
    store: &RecipeStore,
    steps: &[Value],
    caller: &BTreeSet<String>,
) -> Result<Vec<StepItem>, String> {
    let mut out = Vec::new();
    for s in steps {
        match s {
            Value::Object(m) if is_recipe_step(s) => out.extend(expand_one(store, m, caller)?),
            other => out.push(StepItem {
                step: other.clone(),
                scope: None,
                header: String::new(),
            }),
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------------- saving

/// Write the recipe a `recipe_save` call describes to the owner's folder. Returns the result line.
pub fn save(store: &RecipeStore, args: &Map<String, Value>) -> Result<String, String> {
    let description = match args.get("description") {
        Some(v) if truthy(v) => v.clone(),
        _ => args.get("text").cloned().unwrap_or(Value::Null),
    };
    let candidate = json!({
        "name": args.get("name").cloned().unwrap_or(Value::Null),
        "description": description,
        "params": args.get("params").cloned().unwrap_or(Value::Null),
        "steps": args.get("steps").cloned().unwrap_or(Value::Null),
    });
    let rec = validate(&candidate)?;
    let name = rec.name.clone();
    let target = store.user.join(format!("{name}.json"));
    let overwrite = flag(args.get("overwrite"), false);
    let clash = if store.bundled_text(&name).is_some() {
        "a bundled recipe"
    } else if target.is_file() {
        "one of your recipes"
    } else {
        ""
    };
    if !clash.is_empty() && !overwrite {
        return Err(format!(
            "refused: {clash} named {} already exists. Pick another name, or send overwrite=true to \
             replace it (a bundled recipe stays as shipped; yours then wins)",
            py_repr(&name)
        ));
    }
    let mut stored = Map::new();
    for (pname, spec) in &rec.params {
        let mut item = Map::new();
        if spec.required {
            item.insert("required".into(), Value::Bool(true));
        } else {
            item.insert(
                "default".into(),
                Value::String(spec.default.clone().unwrap_or_default()),
            );
        }
        if !spec.description.is_empty() {
            item.insert(
                "description".into(),
                Value::String(spec.description.clone()),
            );
        }
        stored.insert(pname.clone(), Value::Object(item));
    }
    let payload = json!({
        "name": name,
        "description": rec.description,
        "params": Value::Object(stored),
        "steps": rec.steps,
    });
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
    );
    if text.len() > MAX_BYTES {
        return Err(format!("recipe is over {} KB", MAX_BYTES / 1024));
    }
    std::fs::create_dir_all(&store.user).map_err(|e| e.to_string())?;
    let part = store.user.join(format!("{name}.json.part"));
    std::fs::write(&part, &text).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&part, &target) {
        // an existing target blocks a rename on some systems: replace it, as os.replace does
        let _ = std::fs::remove_file(&target);
        if let Err(e2) = std::fs::rename(&part, &target) {
            let _ = std::fs::remove_file(&part);
            return Err(format!("{e}; {e2}"));
        }
    }
    let mut note = format!(
        "saved recipe {name} ({} steps) to {}",
        rec.steps.len(),
        target.display()
    );
    if !clash.is_empty() && overwrite {
        note.push_str(if clash.starts_with("one") {
            " (replaced the older one)"
        } else {
            " (yours now overrides the bundled one)"
        });
    }
    let unused = rec.unused_params();
    if !unused.is_empty() {
        note.push_str(&format!(
            "; note: param(s) never used in a step: {}",
            unused.join(", ")
        ));
    }
    Ok(note)
}

// --------------------------------------------------------------------------------------- audit

/// What the audit log records about recipe use in a call: which recipe or save, and the NAMES of the
/// params, never their values (they can hold anything the caller typed). `None` when the call has
/// neither.
pub fn audit_fields(args: &Value) -> Option<Value> {
    let single = [args.clone()];
    let steps: &[Value] = match args.get("actions") {
        Some(Value::Array(list)) if !list.is_empty() => list,
        _ => &single,
    };
    let mut rows = Vec::new();
    for s in steps {
        let Value::Object(map) = s else {
            continue;
        };
        let action = match map.get("action") {
            Some(v) if truthy(v) => py_str(v).trim().to_string(),
            _ => String::new(),
        };
        let name: String = py_str(map.get("name").unwrap_or(&Value::Null))
            .chars()
            .take(40)
            .collect();
        if action == "recipe" {
            let params: Vec<Value> = match map.get("params") {
                Some(Value::Object(p)) => {
                    let mut keys: Vec<&String> = p.keys().collect();
                    keys.sort();
                    keys.into_iter()
                        .take(MAX_PARAMS)
                        .map(|k| Value::String(k.clone()))
                        .collect()
                }
                _ => Vec::new(),
            };
            rows.push(json!({"action": "recipe", "name": name, "params": params}));
        } else if action == "recipe_save" {
            let steps_n = match map.get("steps") {
                Some(Value::Array(a)) => a.len(),
                _ => 0,
            };
            rows.push(json!({
                "action": "recipe_save",
                "name": name,
                "steps": steps_n,
                "overwrite": flag(map.get("overwrite"), false),
            }));
        }
    }
    if rows.is_empty() {
        None
    } else {
        Some(json!({"recipes": rows}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    // ---- helpers (the Python tests' `good()` and `store` fixture)

    fn good(over: Value) -> Value {
        let mut rec = json!({
            "name": "demo",
            "description": "Open a file",
            "params": {"path": {"required": true, "description": "full path"}, "n": {"default": "3"}},
            "steps": [
                {"action": "open_app", "text": "notepad {path}"},
                {"action": "type", "text": "x{n}", "expect": {"text": "{path}"}}
            ]
        });
        for (k, v) in over.as_object().expect("overrides") {
            rec[k] = v.clone();
        }
        rec
    }

    fn store() -> (TempDir, RecipeStore) {
        let tmp = TempDir::new("recipes");
        let store = RecipeStore::with_bundled(tmp.path().join("user"), vec![]);
        (tmp, store)
    }

    fn put_user(store: &RecipeStore, rec: &Value) {
        std::fs::create_dir_all(store.user_dir()).unwrap();
        let name = rec["name"].as_str().unwrap();
        std::fs::write(
            store.user_dir().join(format!("{name}.json")),
            rec.to_string(),
        )
        .unwrap();
    }

    fn put_bundled(store: &mut RecipeStore, rec: &Value) {
        store.put_bundled(rec["name"].as_str().unwrap(), &rec.to_string());
    }

    fn get(store: &RecipeStore, name: &str) -> Result<Recipe, String> {
        store.get(&json!(name))
    }

    fn expand_one(store: &RecipeStore, call: Value) -> Vec<Value> {
        expand(store, &[call], &BTreeSet::new())
            .unwrap()
            .into_iter()
            .map(|i| i.step)
            .collect()
    }

    fn vals(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn actions_of(steps: &[Value]) -> Vec<&str> {
        steps
            .iter()
            .map(|s| s["action"].as_str().unwrap())
            .collect()
    }

    // ---------------------------------------------------------------- the file format

    // Ported from test_a_good_recipe_validates_and_normalizes.
    #[test]
    fn a_good_recipe_validates_and_normalizes() {
        let rec = validate(&good(json!({}))).unwrap();
        assert_eq!(rec.name, "demo");
        assert!(rec.only_apps.is_empty());
        assert_eq!(
            rec.params[0].1,
            Param {
                required: true,
                default: None,
                description: "full path".into()
            }
        );
        assert_eq!(
            rec.params[1].1,
            Param {
                required: false,
                default: Some("3".into()),
                description: String::new()
            }
        );
        assert_eq!(rec.steps.len(), 2);
    }

    // Ported from test_param_shorthands_and_optional_without_default.
    #[test]
    fn param_shorthands_and_optional_without_default() {
        let rec = validate(&good(json!({
            "params": {"a": "the a", "b": null, "c": {"required": false}, "d": {"default": 5}},
            "steps": [{"action": "wait"}]
        })))
        .unwrap();
        let p = |n: &str| rec.param(n).unwrap().clone();
        assert!(p("a").required && p("a").description == "the a");
        assert!(p("b").required);
        assert_eq!(
            p("c"),
            Param {
                required: false,
                default: Some(String::new()),
                description: String::new()
            }
        );
        assert_eq!(p("d").default.as_deref(), Some("5"));
        // a required flag spelled the way a model might
        let rec = validate(&good(json!({
            "params": {"e": {"required": "no"}, "f": {"required": "YES"}},
            "steps": [{"action": "wait"}]
        })))
        .unwrap();
        assert!(!rec.param("e").unwrap().required && rec.param("f").unwrap().required);
    }

    // Ported from test_bad_recipes_are_refused_with_a_reason (all but the `object()` step: a Rust JSON
    // value cannot hold a non-JSON object, so that case cannot be written).
    #[test]
    fn bad_recipes_are_refused_with_a_reason() {
        let key = |extra: Value| {
            let mut s = json!({"action": "key"});
            for (k, v) in extra.as_object().unwrap() {
                s[k] = v.clone();
            }
            s
        };
        let many_params: serde_json::Map<String, Value> =
            (0..13).map(|i| (format!("p{i}"), json!({}))).collect();
        let cases: Vec<(Value, &str)> = vec![
            (json!({"name": "Has Space"}), "recipe name"),
            (json!({"name": "../evil"}), "recipe name"),
            (json!({"name": ""}), "recipe name"),
            (json!({"name": "x".repeat(41)}), "recipe name"),
            (json!({"name": "1abc"}), "recipe name"),
            (json!({"description": "x".repeat(301)}), "over 300"),
            (json!({"steps": []}), "non-empty list"),
            (json!({"steps": "key"}), "non-empty list"),
            (
                json!({"steps": vec![json!({"action": "key"}); 41]}),
                "at most 40",
            ),
            (json!({"steps": ["key"]}), "must be an object"),
            (
                json!({"steps": [{"action": "nope"}]}),
                "unknown action 'nope'",
            ),
            (json!({"steps": [{"text": "x"}]}), "unknown action ''"),
            (
                json!({"steps": [{"action": "recipe", "name": "other"}]}),
                "cannot run or save recipes",
            ),
            (
                json!({"steps": [{"action": "recipe_save"}]}),
                "cannot run or save recipes",
            ),
            (
                json!({"steps": [key(json!({"text": "{missing}"}))]}),
                "{missing} has no matching param",
            ),
            (
                json!({"steps": [key(json!({"expect": {"text": "{ghost}"}}))]}),
                "{ghost} has no matching param",
            ),
            (
                json!({"steps": [key(json!({"text": "a", "on_fail": "retry"}))]}),
                "on_fail must be \"stop\" or \"skip\"",
            ),
            (
                json!({"steps": [key(json!({"text": "a", "when": "yes"}))]}),
                "when must be an object",
            ),
            (
                json!({"steps": [key(json!({"text": "a", "when": {"colour": "red"}}))]}),
                "when must be an object",
            ),
            (
                json!({"steps": [key(json!({"text": "a", "when": {}}))]}),
                "when must be an object",
            ),
            (
                json!({"steps": [key(json!({"text": "a", "allow_irreversible": true}))]}),
                "cannot be stored in a recipe",
            ),
            (
                json!({"steps": [key(json!({"text": "a", "allow_destructive": true}))]}),
                "cannot be stored in a recipe",
            ),
            (
                json!({"steps": [key(json!({"text": "a", "only_apps": ["x.exe"]}))]}),
                "cannot be stored in a recipe",
            ),
            (json!({"params": {"bad name": {}}}), "param name"),
            (
                json!({"params": {"p": {"default": [1]}}}),
                "default must be text or a number",
            ),
            (
                json!({"params": {"p": {"required": true, "default": "x"}}}),
                "required and default together",
            ),
            (
                json!({"params": {"p": {"required": "maybe"}}, "steps": [{"action": "wait"}]}),
                "required must be true or false",
            ),
            (json!({"params": {"p": {"colour": 1}}}), "param p: use"),
            (
                json!({"params": {"p": {"description": "x".repeat(201)}}}),
                "description over 200",
            ),
            (json!({"params": many_params}), "at most 12 params"),
            (json!({"params": ["a"]}), "params must be an object"),
            (
                json!({"only_apps": "notepad.exe"}),
                "only_apps must be a list",
            ),
            (json!({"mystery": 1}), "unknown recipe field"),
        ];
        for (over, why) in cases {
            let err = validate(&good(over.clone())).expect_err(&over.to_string());
            assert!(err.contains(why), "{over} -> {err}");
        }
    }

    // Ported from test_placeholders_with_braces_that_are_not_params_are_plain_text.
    #[test]
    fn placeholders_with_braces_that_are_not_params_are_plain_text() {
        let rec = validate(&good(json!({
            "steps": [{"action": "type", "text": "json {\"a\": 1} and {} and { x }"}],
            "params": {}
        })))
        .unwrap();
        assert_eq!(rec.steps[0]["text"], "json {\"a\": 1} and {} and { x }");
    }

    // Ported from test_a_recipe_must_be_an_object.
    #[test]
    fn a_recipe_must_be_an_object() {
        assert!(
            validate(&json!([1, 2]))
                .unwrap_err()
                .contains("JSON object")
        );
    }

    // ---------------------------------------------------------------- params and placeholders

    // Ported from test_bind_checks_required_unknown_and_types_and_fills_defaults (all but the
    // infinite float, which JSON cannot carry).
    #[test]
    fn bind_checks_required_unknown_and_types_and_fills_defaults() {
        let rec = validate(&good(json!({}))).unwrap();
        let bound = |given: Value| bind(&rec, Some(&given));
        assert_eq!(
            bound(json!({"path": "C:\\a.txt"})).unwrap(),
            vals(&[("path", "C:\\a.txt"), ("n", "3")])
        );
        assert_eq!(
            bound(json!({"path": "x", "n": 7})).unwrap(),
            vals(&[("path", "x"), ("n", "7")])
        );
        assert_eq!(bound(json!({"path": 1.5})).unwrap()[0].1, "1.5");
        let err = bound(json!({})).unwrap_err();
        assert!(err.contains("needs param(s) path: full path"), "{err}");
        let err = bound(json!({"path": "x", "zed": 1})).unwrap_err();
        assert!(err.contains("has no param zed; it takes: path, n"), "{err}");
        for bad in [json!(true), json!(["a"]), json!({"a": 1})] {
            let err = bound(json!({ "path": bad })).unwrap_err();
            assert!(err.contains("must be text or a number"), "{err}");
        }
        let err = bound(json!({"path": "x".repeat(20001)})).unwrap_err();
        assert!(err.contains("over 20000"), "{err}");
        let err = bound(json!(["x"])).unwrap_err();
        assert!(err.contains("params must be an object"), "{err}");
        // null means "not given"
        assert_eq!(bound(json!({"path": "x", "n": null})).unwrap()[1].1, "3");
        assert!(bind(&rec, None).unwrap_err().contains("needs param"));
    }

    // Ported from test_substitution_is_plain_replacement_everywhere_inside_a_step.
    #[test]
    fn substitution_is_plain_replacement_everywhere_inside_a_step() {
        let step = json!({
            "action": "type", "text": "a {x} b {x} c {y}",
            "expect": {"text": "{y}", "gone_text": "none"},
            "list": ["{x}", 3, true, null, {"deep": "{y}"}], "repeat": 2
        });
        let out = substitute(&step, &vals(&[("x", "1"), ("y", "two")])).unwrap();
        assert_eq!(
            out,
            json!({
                "action": "type", "text": "a 1 b 1 c two",
                "expect": {"text": "two", "gone_text": "none"},
                "list": ["1", 3, true, null, {"deep": "two"}], "repeat": 2
            })
        );
        assert_eq!(
            step["text"], "a {x} b {x} c {y}",
            "the original is untouched"
        );
    }

    // Ported from test_a_value_is_never_scanned_again_so_braces_in_it_cannot_reach_another_param.
    #[test]
    fn a_value_is_never_scanned_again() {
        let out = substitute(
            &json!({"text": "{a}|{b}"}),
            &vals(&[("a", "{b}"), ("b", "SECRET")]),
        )
        .unwrap();
        assert_eq!(out, json!({"text": "{b}|SECRET"}));
    }

    // Ported from test_values_with_json_syntax_or_backslashes_cannot_break_a_step.
    #[test]
    fn values_with_json_syntax_or_backslashes_cannot_break_a_step() {
        let out = substitute(
            &json!({"text": "{p}"}),
            &vals(&[("p", "x\", \"action\": \"key")]),
        )
        .unwrap();
        assert_eq!(out, json!({"text": "x\", \"action\": \"key"}));
        assert_eq!(out.as_object().unwrap().len(), 1);
        assert_eq!(
            substitute(&json!({"text": "{p}"}), &vals(&[("p", "C:\\Users\\a\\1")])).unwrap(),
            json!({"text": "C:\\Users\\a\\1"})
        );
    }

    // Ported from test_dict_keys_are_not_substituted.
    #[test]
    fn dict_keys_are_not_substituted() {
        assert_eq!(
            substitute(&json!({"{p}": "{p}"}), &vals(&[("p", "v")])).unwrap(),
            json!({"{p}": "v"})
        );
    }

    // Ported from test_an_unknown_placeholder_is_also_an_error_at_run_time.
    #[test]
    fn an_unknown_placeholder_is_also_an_error_at_run_time() {
        let err = substitute(&json!({"text": "{typo}"}), &vals(&[("path", "x")])).unwrap_err();
        assert!(err.contains("{typo} has no matching param"), "{err}");
    }

    // ---------------------------------------------------------------- where recipes live

    // Ported from test_a_user_recipe_overrides_a_bundled_one_of_the_same_name.
    #[test]
    fn a_user_recipe_overrides_a_bundled_one_of_the_same_name() {
        let (_tmp, mut store) = store();
        put_bundled(
            &mut store,
            &good(json!({"name": "same", "description": "bundled one"})),
        );
        put_user(
            &store,
            &good(json!({"name": "same", "description": "mine"})),
        );
        let same = get(&store, "same").unwrap();
        assert_eq!(
            (same.description.as_str(), same.source),
            ("mine", Source::User)
        );
        put_bundled(
            &mut store,
            &good(json!({"name": "only_bundled", "description": "b"})),
        );
        assert_eq!(get(&store, "only_bundled").unwrap().source, Source::Bundled);
        let (found, bad) = store.all();
        assert_eq!(found["same"].description, "mine");
        assert_eq!(found.keys().collect::<Vec<_>>(), ["only_bundled", "same"]);
        assert!(bad.is_empty());
    }

    // Ported from test_recipe_list_is_one_cheap_line_per_recipe.
    #[test]
    fn recipe_list_is_one_cheap_line_per_recipe() {
        let (_tmp, mut store) = store();
        put_bundled(
            &mut store,
            &good(json!({"name": "b_one", "description": "Bundled"})),
        );
        put_user(
            &store,
            &good(json!({
                "name": "a_two",
                "description": format!("Mine {}", "x".repeat(200)),
                "steps": [{"action": "wait"}],
                "params": {"p": {"required": true}, "q": {"default": "9"}, "r": {"required": false}}
            })),
        );
        let text = store.list_text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].starts_with("a_two(p, q=9, r?): Mine xxx"),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with("... [yours]") && lines[0].len() < 160);
        assert_eq!(lines[1], "b_one(path, n=3): Bundled");
        assert_eq!(lines.len(), 2);
    }

    // Ported from test_recipe_list_names_a_broken_file_instead_of_hiding_it.
    #[test]
    fn recipe_list_names_a_broken_file_instead_of_hiding_it() {
        let (_tmp, mut store) = store();
        put_bundled(&mut store, &good(json!({"name": "fine"})));
        std::fs::create_dir_all(store.user_dir()).unwrap();
        std::fs::write(store.user_dir().join("broken.json"), "{not json").unwrap();
        std::fs::write(
            store.user_dir().join("wrongname.json"),
            good(json!({"name": "other"})).to_string(),
        )
        .unwrap();
        std::fs::write(store.user_dir().join("huge.json"), " ".repeat(70000)).unwrap();
        let out = store.list_text();
        assert!(out.lines().next().unwrap().starts_with("fine("), "{out}");
        assert!(out.contains("broken: INVALID (user:"), "{out}");
        assert!(
            out.contains("wrongname: INVALID") && out.contains("does not match the file name"),
            "{out}"
        );
        assert!(out.contains("huge: INVALID"), "{out}");
    }

    // Ported from test_nothing_to_list_says_so.
    #[test]
    fn nothing_to_list_says_so() {
        let (_tmp, store) = store();
        assert_eq!(store.list_text(), "no recipes bundled or saved");
    }

    // Ported from test_running_a_broken_user_recipe_says_where_the_file_is.
    #[test]
    fn running_a_broken_user_recipe_says_where_the_file_is() {
        let (_tmp, store) = store();
        std::fs::create_dir_all(store.user_dir()).unwrap();
        std::fs::write(store.user_dir().join("oops.json"), "[1]").unwrap();
        let err = get(&store, "oops").unwrap_err();
        assert!(
            err.starts_with("recipe oops (user, ") && err.contains("oops.json) is invalid"),
            "{err}"
        );
    }

    // Ported from test_names_are_normalized_and_cannot_walk_out_of_the_folder.
    #[test]
    fn names_are_normalized_and_cannot_walk_out_of_the_folder() {
        let (_tmp, mut store) = store();
        put_bundled(&mut store, &good(json!({"name": "demo"})));
        assert_eq!(get(&store, "  DEMO ").unwrap().name, "demo");
        for evil in ["../demo", "..\\demo", "demo.json", "a/b", "C:\\x"] {
            let err = get(&store, evil).unwrap_err();
            assert!(err.contains("recipe name"), "{evil}: {err}");
        }
        let err = get(&store, "ghost").unwrap_err();
        assert!(
            err.contains("no recipe named 'ghost'; recipe_list shows"),
            "{err}"
        );
    }

    // ---------------------------------------------------------------- the shipped starters

    fn shipped() -> (TempDir, RecipeStore) {
        let tmp = TempDir::new("shipped");
        let store = RecipeStore::standard(tmp.path().join("user"));
        (tmp, store)
    }

    // Ported from test_the_shipped_starter_recipes_are_all_valid_and_use_real_actions. The Python test
    // read the files from the package folder; here they are compiled in, so this checks the same
    // facts of the embedded text.
    #[test]
    fn the_shipped_starter_recipes_are_all_valid_and_use_real_actions() {
        let names: Vec<&str> = BUNDLED.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "capture_page",
                "close_app",
                "explorer_newest",
                "note",
                "open_url",
                "save_as"
            ]
        );
        let (_tmp, store) = shipped();
        for (name, text) in BUNDLED {
            let rec = get(&store, name).unwrap();
            assert_eq!(rec.source, Source::Bundled);
            assert!(
                !rec.description.is_empty() && rec.description.chars().count() <= 100,
                "{name}"
            );
            assert!(rec.unused_params().is_empty(), "{name}");
            assert!(!text.contains('\u{2014}'), "{name}");
            assert!(!text.contains('\r'), "{name}: line endings are LF");
            for step in &rec.steps {
                let action = step["action"].as_str().unwrap();
                assert!(
                    crate::actions::is_action(action) && !ACTIONS.contains(&action),
                    "{action}"
                );
            }
        }
        let (found, bad) = store.all();
        assert_eq!(found.len(), 6);
        assert!(bad.is_empty(), "{bad:?}");
    }

    // Ported from test_the_starters_target_by_name_and_text_not_by_pixel.
    #[test]
    fn the_starters_target_by_name_and_text_not_by_pixel() {
        let (_tmp, store) = shipped();
        for name in [
            "explorer_newest",
            "save_as",
            "open_url",
            "capture_page",
            "note",
            "close_app",
        ] {
            for step in get(&store, name).unwrap().steps {
                for key in ["coordinate", "start_coordinate", "region"] {
                    assert!(step.get(key).is_none(), "{name} {key}");
                }
            }
        }
    }

    // Ported from test_explorer_newest_expands_to_one_folder_step.
    #[test]
    fn explorer_newest_expands_to_one_folder_step() {
        let (_tmp, store) = shipped();
        let call = |params: Value| json!({"action": "recipe", "name": "explorer_newest", "params": params});
        assert_eq!(
            expand_one(&store, call(json!({"path": "C:\\Users\\a\\Downloads"}))),
            [
                json!({"action": "folder", "path": "C:\\Users\\a\\Downloads", "view": "details",
                    "sort": "date_modified desc", "list": "3"})
            ]
        );
        assert_eq!(
            expand_one(&store, call(json!({"path": "C:\\x", "n": 5})))[0]["list"],
            "5"
        );
    }

    // Ported from test_the_other_starters_expand_as_documented. The one difference from the Python
    // build is `note`: it opens Notepad with `blank: true` (the document is checked, not guessed from
    // a title after a blind Ctrl+N) and still carries the title guard.
    #[test]
    fn the_other_starters_expand_as_documented() {
        let (_tmp, store) = shipped();
        let run = |name: &str, params: Value| {
            expand_one(
                &store,
                json!({"action": "recipe", "name": name, "params": params}),
            )
        };
        let save = run("save_as", json!({"path": "C:\\a\\b.txt"}));
        assert_eq!(actions_of(&save), ["key", "file_dialog"]);
        assert_eq!(save[0]["text"], "ctrl+shift+s");
        // Save As, not a plain save: the File name box has to be there before the path is typed
        assert_eq!(save[0]["expect"], json!({"text": "File name"}));
        assert_eq!(save[1]["text"], "C:\\a\\b.txt");
        assert_eq!(save[1]["expect"], json!({"gone_text": "File name"}));

        let cap = run(
            "capture_page",
            json!({"url": "https://example.com", "file": "C:\\t\\p.png"}),
        );
        assert_eq!(actions_of(&cap), ["open_app", "wait", "screenshot"]);
        assert_eq!(cap[0]["text"], "https://example.com");
        assert_eq!(cap[1]["duration"], "4");
        assert_eq!(
            cap[2],
            json!({"action": "screenshot", "window": "front", "save_to": "C:\\t\\p.png", "shot": false})
        );
        assert_eq!(
            run(
                "capture_page",
                json!({"url": "https://x.org", "file": "f.png", "wait_s": 9})
            )[1]["duration"],
            "9"
        );

        let note = run("note", json!({"text": "hi {there}"}));
        assert_eq!(actions_of(&note), ["open_app", "type"]);
        assert_eq!(note[1]["text"], "hi {there}");
        assert_eq!(note[0]["blank"], true);
        // never types into a document that was already open
        assert_eq!(note[0]["expect"], json!({"title": "Untitled"}));

        assert_eq!(
            run("open_url", json!({"url": "https://example.com"})),
            [json!({"action": "open_app", "text": "https://example.com"})]
        );
        assert_eq!(
            run("close_app", json!({"app": "notepad.exe"})),
            [json!({"action": "window", "op": "close", "window": "notepad.exe", "all": true})]
        );
    }

    // The Python build's own `note` recipe (three steps, Ctrl+N guard) still validates and expands
    // here: the format is shared, so a user's copy of either build's file loads in both.
    #[test]
    fn the_python_builds_note_recipe_loads_unchanged() {
        let python_note = json!({
            "name": "note",
            "description": "Open Notepad and type text into a new blank document (stops if no blank one appears)",
            "params": {"text": {"required": true, "description": "the text to type"}},
            "steps": [
                {"action": "open_app", "text": "notepad"},
                {"action": "key", "text": "ctrl+n", "expect": {"title": "Untitled"}},
                {"action": "type", "text": "{text}"}
            ]
        });
        let (_tmp, store) = store();
        put_user(&store, &python_note);
        let steps = expand_one(
            &store,
            json!({"action": "recipe", "name": "note", "params": {"text": "hi"}}),
        );
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1]["expect"], json!({"title": "Untitled"}));
    }

    // The PLAN.md gate for R1c: recipes from the Python build load unchanged. When the Python
    // repository is checked out next to this one (or `WINHANDS_PY_RECIPES` names its recipes folder),
    // read its real bundled files through the user-folder loader; otherwise there is nothing to compare.
    #[test]
    fn the_python_builds_bundled_recipe_files_load_unchanged() {
        let dir = std::env::var("WINHANDS_PY_RECIPES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(r"C:\Users\ikill\tools\winhands\winhands\recipes"));
        if !dir.is_dir() {
            eprintln!("skipped: no Python recipes folder at {}", dir.display());
            return;
        }
        let store = RecipeStore::with_bundled(&dir, vec![]);
        let (found, bad) = store.all();
        assert!(bad.is_empty(), "{bad:?}");
        assert!(found.len() >= 6, "{:?}", found.keys().collect::<Vec<_>>());
        for name in [
            "capture_page",
            "close_app",
            "explorer_newest",
            "open_url",
            "save_as",
        ] {
            let py = &found[name];
            // the same steps the compiled-in copy has: the two builds share these five files
            let ours = get(&shipped().1, name).unwrap();
            assert_eq!(py.steps, ours.steps, "{name}");
            assert_eq!(py.params, ours.params, "{name}");
        }
    }

    // ---------------------------------------------------------------- expansion

    #[test]
    fn expansion_labels_the_first_step_and_leaves_other_steps_alone() {
        let (_tmp, mut store) = store();
        put_bundled(&mut store, &good(json!({})));
        let items = expand(
            &store,
            &[
                json!({"action": "key", "text": "first"}),
                json!({"action": "recipe", "name": "demo", "params": {"path": "C:\\a.txt"}}),
                json!("not an object"),
            ],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].header, "");
        assert_eq!(items[1].header, "recipe demo(path, n=3): 2 steps");
        assert_eq!(items[2].header, "");
        assert_eq!(items[1].step["text"], "notepad C:\\a.txt");
        assert_eq!(items[3].step, json!("not an object"));
        // nothing to expand: the steps come back as they were
        let same = expand(&store, &[json!({"action": "key"})], &BTreeSet::new()).unwrap();
        assert_eq!(same[0].step, json!({"action": "key"}));
    }

    #[test]
    fn the_callers_allow_irreversible_is_inherited_by_each_step_and_never_stored() {
        let (_tmp, mut store) = store();
        put_bundled(
            &mut store,
            &good(json!({"name": "press", "params": {}, "steps": [{"action": "left_click"}]})),
        );
        let plain = expand_one(&store, json!({"action": "recipe", "name": "press"}));
        assert!(plain[0].get("allow_irreversible").is_none());
        let granted = expand_one(
            &store,
            json!({"action": "recipe", "name": "press", "allow_irreversible": "yes"}),
        );
        assert_eq!(granted[0]["allow_irreversible"], true);
        let sneaky = good(json!({
            "name": "sneaky", "params": {},
            "steps": [{"action": "left_click", "allow_irreversible": true}]
        }));
        put_bundled(&mut store, &sneaky);
        let err = expand(
            &store,
            &[json!({"action": "recipe", "name": "sneaky"})],
            &BTreeSet::new(),
        )
        .unwrap_err();
        assert!(err.contains("cannot be stored in a recipe"), "{err}");
    }

    // ---------------------------------------------------------------- recipe_save

    fn do_save(store: &RecipeStore, over: Value) -> Result<String, String> {
        save(store, over.as_object().unwrap())
    }

    // Ported from test_a_saved_recipe_round_trips_through_the_user_folder.
    #[test]
    fn a_saved_recipe_round_trips_through_the_user_folder() {
        let (_tmp, store) = store();
        let note = do_save(
            &store,
            json!({
                "name": "Greeter", "text": "Say hello",
                "params": {"who": {"required": true, "description": "name"}, "how": {"default": "kindly"}, "x": {"required": false}},
                "steps": [{"action": "type", "text": "hello {who} {how}"}, {"action": "key", "text": "Return"}]
            }),
        )
        .unwrap();
        assert!(
            note.starts_with("saved recipe greeter (2 steps) to "),
            "{note}"
        );
        assert!(
            note.contains(&store.user_dir().display().to_string()),
            "{note}"
        );
        let on_disk: Value = serde_json::from_str(
            &std::fs::read_to_string(store.user_dir().join("greeter.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            on_disk["params"],
            json!({"who": {"required": true, "description": "name"}, "how": {"default": "kindly"}, "x": {"default": ""}})
        );
        assert_eq!(on_disk["description"], "Say hello");
        let rec = get(&store, "greeter").unwrap();
        assert_eq!(rec.source, Source::User);
        assert!(!rec.param("x").unwrap().required);
        assert!(
            note.contains("note: param(s) never used in a step: x"),
            "{note}"
        );
        let steps = expand_one(
            &store,
            json!({"action": "recipe", "name": "greeter", "params": {"who": "Jon"}}),
        );
        assert_eq!(steps[0]["text"], "hello Jon kindly");
    }

    // Ported from test_description_can_come_from_either_key.
    #[test]
    fn description_can_come_from_either_key() {
        let (_tmp, store) = store();
        do_save(
            &store,
            json!({"name": "d1", "description": "from description", "steps": [{"action": "wait"}]}),
        )
        .unwrap();
        do_save(
            &store,
            json!({"name": "d2", "text": "from text", "steps": [{"action": "wait"}]}),
        )
        .unwrap();
        assert_eq!(get(&store, "d1").unwrap().description, "from description");
        assert_eq!(get(&store, "d2").unwrap().description, "from text");
    }

    // Ported from test_save_refuses_to_replace_a_bundled_recipe_without_overwrite.
    #[test]
    fn save_refuses_to_replace_a_bundled_recipe_without_overwrite() {
        let (_tmp, mut store) = store();
        put_bundled(&mut store, &good(json!({"name": "shipped"})));
        let err = do_save(
            &store,
            json!({"name": "shipped", "params": {"path": "p"}, "steps": [{"action": "wait"}]}),
        )
        .unwrap_err();
        assert!(
            err.contains("refused: a bundled recipe named 'shipped' already exists"),
            "{err}"
        );
        assert!(!store.user_dir().exists());
        let err = do_save(
            &store,
            json!({"name": "shipped", "overwrite": false, "steps": [{"action": "wait"}]}),
        )
        .unwrap_err();
        assert!(err.contains("overwrite=true"), "{err}");
    }

    // Ported from test_save_over_a_bundled_recipe_with_overwrite_writes_a_user_copy_and_leaves_the_bundled_file.
    #[test]
    fn save_over_a_bundled_recipe_with_overwrite_writes_a_user_copy() {
        let (_tmp, mut store) = store();
        put_bundled(&mut store, &good(json!({"name": "shipped"})));
        let before = store.bundled_text("shipped").unwrap().to_string();
        let note = do_save(
            &store,
            json!({"name": "shipped", "overwrite": true, "text": "mine", "steps": [{"action": "wait"}]}),
        )
        .unwrap();
        assert!(
            note.contains("yours now overrides the bundled one"),
            "{note}"
        );
        assert_eq!(store.bundled_text("shipped").unwrap(), before);
        let rec = get(&store, "shipped").unwrap();
        assert_eq!(
            (rec.description.as_str(), rec.source),
            ("mine", Source::User)
        );
    }

    // Ported from test_save_refuses_to_replace_your_own_recipe_unless_told_to.
    #[test]
    fn save_refuses_to_replace_your_own_recipe_unless_told_to() {
        let (_tmp, store) = store();
        do_save(
            &store,
            json!({"name": "mine", "text": "v1", "steps": [{"action": "wait"}]}),
        )
        .unwrap();
        let err = do_save(
            &store,
            json!({"name": "mine", "text": "v2", "steps": [{"action": "wait"}]}),
        )
        .unwrap_err();
        assert!(
            err.contains("refused: one of your recipes named 'mine' already exists"),
            "{err}"
        );
        assert_eq!(get(&store, "mine").unwrap().description, "v1");
        let note = do_save(
            &store,
            json!({"name": "mine", "text": "v2", "overwrite": "true", "steps": [{"action": "wait"}]}),
        )
        .unwrap();
        assert!(note.contains("replaced the older one"), "{note}");
        assert_eq!(get(&store, "mine").unwrap().description, "v2");
    }

    // Ported from test_save_refuses_a_recipe_that_would_not_load_and_writes_nothing.
    #[test]
    fn save_refuses_a_recipe_that_would_not_load_and_writes_nothing() {
        let (_tmp, store) = store();
        for (over, why) in [
            (
                json!({"name": "../x", "steps": [{"action": "wait"}]}),
                "recipe name",
            ),
            (json!({"name": "x"}), "non-empty list"),
            (
                json!({"name": "x", "steps": [{"action": "launch_missiles"}]}),
                "unknown action",
            ),
            (
                json!({"name": "x", "steps": [{"action": "type", "text": "{nope}"}]}),
                "has no matching param",
            ),
            (
                json!({"name": "x", "steps": [{"action": "recipe", "name": "y"}]}),
                "cannot run or save recipes",
            ),
            (
                json!({"name": "x", "steps": [{"action": "key", "allow_irreversible": true}]}),
                "cannot be stored",
            ),
            (
                json!({"name": "x", "params": {"p": {}}, "steps": [{"action": "key", "when": {"zzz": "a"}}]}),
                "when must be",
            ),
        ] {
            let err = do_save(&store, over.clone()).unwrap_err();
            assert!(err.contains(why), "{over} -> {err}");
        }
        assert!(
            !store.user_dir().exists()
                || std::fs::read_dir(store.user_dir())
                    .unwrap()
                    .next()
                    .is_none(),
            "nothing may be written"
        );
    }

    // Ported from test_a_save_is_atomic_and_leaves_no_part_file.
    #[test]
    fn a_save_is_atomic_and_leaves_no_part_file() {
        let (_tmp, store) = store();
        do_save(
            &store,
            json!({"name": "clean", "steps": [{"action": "wait"}]}),
        )
        .unwrap();
        let names: Vec<String> = std::fs::read_dir(store.user_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["clean.json"]);
    }

    // Ported from test_saving_does_not_store_a_call_level_allowlist.
    #[test]
    fn saving_does_not_store_a_call_level_allowlist() {
        let (_tmp, store) = store();
        do_save(
            &store,
            json!({"name": "plain", "steps": [{"action": "wait"}], "only_apps": ["notepad.exe"]}),
        )
        .unwrap();
        let text = std::fs::read_to_string(store.user_dir().join("plain.json")).unwrap();
        assert!(!text.contains("only_apps"), "{text}");
        // written the way Python's json.dumps(indent=2) writes it
        assert!(
            text.ends_with("}\n") && text.contains("\n  \"name\": \"plain\""),
            "{text}"
        );
    }

    #[test]
    fn a_recipe_over_the_size_cap_is_not_saved() {
        let (_tmp, store) = store();
        let steps: Vec<Value> = (0..40)
            .map(|_| json!({"action": "type", "text": "y".repeat(2000)}))
            .collect();
        let err = do_save(&store, json!({"name": "big", "steps": steps})).unwrap_err();
        assert!(err.contains("recipe is over 64 KB"), "{err}");
    }

    // ---------------------------------------------------------------- audit

    // Ported from test_the_audit_log_names_the_recipe_and_the_param_names_never_the_values.
    #[test]
    fn the_audit_log_names_the_recipe_and_the_param_names_never_the_values() {
        let fields = audit_fields(&json!({
            "action": "recipe", "name": "note", "params": {"text": "my password is hunter2"}
        }))
        .unwrap();
        assert_eq!(
            fields,
            json!({"recipes": [{"action": "recipe", "name": "note", "params": ["text"]}]})
        );
        assert!(!fields.to_string().contains("hunter2"));
        let fields = audit_fields(&json!({"actions": [
            {"action": "key"},
            {"action": "recipe_save", "name": "x", "overwrite": "true",
             "steps": [{"action": "key", "text": "secret"}, {"action": "key", "text": "secret"},
                       {"action": "key", "text": "secret"}]},
            {"action": "recipe", "name": "y"},
            "junk"
        ]}))
        .unwrap();
        assert_eq!(
            fields,
            json!({"recipes": [
                {"action": "recipe_save", "name": "x", "steps": 3, "overwrite": true},
                {"action": "recipe", "name": "y", "params": []}
            ]})
        );
        assert!(!fields.to_string().contains("secret"));
        assert_eq!(audit_fields(&json!({"action": "key"})), None);
    }

    // ---------------------------------------------------------------- housekeeping

    // Ported from test_no_em_dashes_in_the_new_modules (every new source file).
    #[test]
    fn no_em_dashes_in_the_new_modules() {
        for (name, text) in [
            ("recipes.rs", include_str!("recipes.rs")),
            ("folders.rs", include_str!("folders.rs")),
            ("tool_schema.rs", include_str!("tool_schema.rs")),
            (
                "orchestrate/recipe.rs",
                include_str!("orchestrate/recipe.rs"),
            ),
            (
                "orchestrate/shortcuts.rs",
                include_str!("orchestrate/shortcuts.rs"),
            ),
            (
                "orchestrate/folder.rs",
                include_str!("orchestrate/folder.rs"),
            ),
        ] {
            assert!(!text.contains('\u{2014}'), "{name}");
        }
    }
}
