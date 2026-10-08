//! Folder views and folder listings, the pure half of the `folder` action. Port of the parsing,
//! sorting and listing parts of `winhands/shortcuts.py` (`parse_view`, `parse_sort`, `_entries`,
//! `sorted_entries`, `list_text`, `_natural`).
//!
//! `folder` opens a file manager at a path in a requested view and sort, and answers "what are the
//! newest entries" straight from the filesystem in that sort order, so a model never needs a picture to
//! read it. Opening and arranging the window is the backend's and the orchestration's business; this
//! module only turns the words a model sends into [`FolderView`] and [`FolderSort`], and reads and
//! orders the entries the way Explorer would show them.

use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::backend::{FolderSort, FolderView, SortColumn};
use crate::textutil::{py_str, py_value_repr};

/// The most entries one `list` returns.
pub const LIST_MAX: usize = 200;

const DESC_WORDS: [&str; 6] = [
    "desc",
    "descending",
    "newest",
    "latest",
    "largest",
    "biggest",
];
const ASC_WORDS: [&str; 5] = ["asc", "ascending", "oldest", "earliest", "smallest"];

/// A bare direction word that also says which column ("newest" means by date modified).
fn implied_column(word: &str) -> Option<&'static str> {
    Some(match word {
        "newest" | "latest" | "oldest" | "earliest" => "date_modified",
        "largest" | "biggest" | "smallest" => "size",
        _ => return None,
    })
}

fn view_alias(v: &str) -> &str {
    match v {
        "detail" => "details",
        "tile" => "tiles",
        "icon" | "medium" | "medium_icons" => "icons",
        "large_icons" | "thumbnails" | "thumbnail" => "large",
        "extra_large" | "extra_large_icons" => "xlarge",
        "small_icons" => "small",
        other => other,
    }
}

fn column_alias(v: &str) -> &str {
    match v {
        "modified" | "date" | "mtime" | "time" => "date_modified",
        "created" | "ctime" => "date_created",
        "kind" => "type",
        "filename" | "file_name" => "name",
        other => other,
    }
}

/// `"Large icons"` -> [`FolderView::Large`]. `None` for nothing asked.
pub fn parse_view(value: &Value) -> Result<Option<FolderView>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let text = py_str(value);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut v = String::new();
    let mut in_gap = false;
    for ch in trimmed.to_lowercase().chars() {
        if ch.is_whitespace() || ch == '-' {
            if !in_gap {
                v.push('_');
            }
            in_gap = true;
        } else {
            v.push(ch);
            in_gap = false;
        }
    }
    let v = view_alias(&v);
    FolderView::ALL
        .into_iter()
        .find(|f| f.name() == v)
        .map(Some)
        .ok_or_else(|| {
            let names: Vec<&str> = FolderView::ALL.iter().map(|f| f.name()).collect();
            format!(
                "view must be one of {} (got {})",
                names.join(", "),
                py_value_repr(value)
            )
        })
}

/// `"date_modified desc"`, `"name"`, `"-size"`, `"size:asc"`, `"newest"` -> a column and direction.
/// With no direction given a column sorts the way a person wants it: name and type A to Z, dates
/// newest first, size largest first. `None` for nothing asked.
pub fn parse_sort(value: &Value) -> Result<Option<FolderSort>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let raw = py_str(value);
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let lowered = raw.trim().to_lowercase().replace([':', ','], " ");
    let mut text = lowered.as_str();
    let mut desc: Option<bool> = None;
    if let Some(first) = text.chars().next()
        && (first == '+' || first == '-')
    {
        desc = Some(first == '-');
        text = &text[1..];
    }
    let mut column_words: Vec<&str> = Vec::new();
    let mut implied: Option<&'static str> = None;
    for word in text.split_whitespace() {
        if DESC_WORDS.contains(&word) {
            desc = Some(true);
        } else if ASC_WORDS.contains(&word) {
            desc = Some(false);
        } else {
            column_words.push(word);
        }
        implied = implied.or_else(|| implied_column(word));
    }
    let joined = column_words.join("_");
    let col = if joined.is_empty() {
        implied.unwrap_or("").to_string()
    } else {
        joined
    };
    let col = column_alias(&col);
    let column = SortColumn::ALL
        .into_iter()
        .find(|c| c.name() == col)
        .ok_or_else(|| {
            let names: Vec<&str> = SortColumn::ALL.iter().map(|c| c.name()).collect();
            format!(
                "sort must name a column ({}) and optionally asc or desc, e.g. 'date_modified \
                 desc' (got {})",
                names.join(", "),
                py_value_repr(value)
            )
        })?;
    Ok(Some(FolderSort {
        column,
        descending: desc.unwrap_or_else(|| column.natural_descending()),
    }))
}

/// `list=n`: 0 for nothing asked, else the number of entries wanted.
pub fn parse_count(value: &Value) -> Result<usize, String> {
    let n: i64 = match value {
        Value::Null | Value::Bool(false) => return Ok(0),
        Value::String(s) if s.is_empty() => return Ok(0),
        Value::Bool(true) => 1,
        Value::Number(num) => num
            .as_i64()
            .or_else(|| num.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| {
            format!(
                "list must be a number of entries, 1 or more (got {})",
                py_value_repr(value)
            )
        })?,
        other => {
            return Err(format!(
                "list must be a number of entries, 1 or more (got {})",
                py_value_repr(other)
            ));
        }
    };
    if n < 0 {
        return Err("list must be a number of entries, 1 or more".into());
    }
    Ok(n as usize)
}

// ------------------------------------------------------------------------------------ entries

/// One item Explorer would show in a folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The file or folder name.
    pub name: String,
    /// It is a folder.
    pub dir: bool,
    /// Seconds since the Unix epoch.
    pub mtime: f64,
    /// Creation time, seconds since the Unix epoch.
    pub ctime: f64,
    /// Bytes.
    pub size: u64,
}

fn epoch_secs(t: std::time::SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

#[cfg(windows)]
fn is_hidden(meta: &std::fs::Metadata, _name: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM
    meta.file_attributes() & 0x6 != 0
}

#[cfg(not(windows))]
fn is_hidden(_meta: &std::fs::Metadata, name: &str) -> bool {
    name.starts_with('.') // what Finder hides by default
}

/// Every item the file manager would show in `folder`, and how many hidden or system items it
/// would not. Hidden and system items are left out, as in Explorer's default view.
pub fn entries(folder: &Path) -> (Vec<Entry>, usize) {
    let mut rows = Vec::new();
    let mut skipped = 0;
    let Ok(read) = std::fs::read_dir(folder) else {
        return (rows, skipped);
    };
    for e in read.filter_map(Result::ok) {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(meta) = std::fs::metadata(e.path()) else {
            continue;
        };
        if is_hidden(&meta, &name) {
            skipped += 1;
            continue;
        }
        let mtime = meta.modified().map(epoch_secs).unwrap_or(0.0);
        let ctime = meta.created().map(epoch_secs).unwrap_or(mtime);
        rows.push(Entry {
            name,
            dir: meta.is_dir(),
            mtime,
            ctime,
            size: meta.len(),
        });
    }
    (rows, skipped)
}

/// One piece of a name for natural ordering: a run of digits compares as a number, anything else
/// as lowercase text, numbers before text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Piece {
    Num(usize, String),
    Text(String),
}

fn natural(name: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut digits = false;
    let flush = |run: &mut String, digits: bool, out: &mut Vec<Piece>| {
        if run.is_empty() {
            return;
        }
        if digits {
            let trimmed = run.trim_start_matches('0').to_string();
            out.push(Piece::Num(trimmed.len(), trimmed));
        } else {
            out.push(Piece::Text(run.to_lowercase()));
        }
        run.clear();
    };
    for ch in name.chars() {
        let d = ch.is_ascii_digit();
        if d != digits {
            flush(&mut run, digits, &mut out);
            digits = d;
        }
        run.push(ch);
    }
    flush(&mut run, digits, &mut out);
    out
}

/// `os.path.splitext(name)[1]`, lowercased: the extension with its dot, empty for a name with none
/// (leading dots do not start one).
fn extension(name: &str) -> String {
    let stem_start = name.len() - name.trim_start_matches('.').len();
    match name[stem_start..].rfind('.') {
        Some(i) => name[stem_start + i..].to_lowercase(),
        None => String::new(),
    }
}

fn key_cmp(col: SortColumn, a: &Entry, b: &Entry) -> Ordering {
    match col {
        SortColumn::Name => natural(&a.name).cmp(&natural(&b.name)),
        SortColumn::DateModified => a.mtime.partial_cmp(&b.mtime).unwrap_or(Ordering::Equal),
        SortColumn::DateCreated => a.ctime.partial_cmp(&b.ctime).unwrap_or(Ordering::Equal),
        SortColumn::Size => {
            let size = |e: &Entry| if e.dir { 0 } else { e.size };
            size(a).cmp(&size(b))
        }
        SortColumn::Type => {
            let kind = |e: &Entry| {
                if e.dir {
                    String::new()
                } else {
                    extension(&e.name)
                }
            };
            kind(a).cmp(&kind(b))
        }
    }
}

/// `rows` in the order Explorer shows them for this sort. Ties stay in name order either way.
pub fn sorted(rows: &[Entry], sort: FolderSort) -> Vec<Entry> {
    let mut out = rows.to_vec();
    out.sort_by_cached_key(|r| natural(&r.name));
    out.sort_by(|a, b| {
        let o = key_cmp(sort.column, a, b);
        if sort.descending { o.reverse() } else { o }
    });
    out
}

/// The listing a `folder list=n` call returns: a header line, then `kind  date  name  size` per
/// entry. `utc_offset(unix_secs)` is the local zone's offset in seconds east, for the printed times.
pub fn list_text(
    folder: &str,
    rows: &[Entry],
    skipped: usize,
    n: usize,
    sort: FolderSort,
    utc_offset: &dyn Fn(i64) -> i32,
) -> String {
    let ordered = sorted(rows, sort);
    let n = n.clamp(1, LIST_MAX);
    let mut head = format!(
        "{folder}: first {} of {} entries by {}",
        n.min(ordered.len()),
        ordered.len(),
        sort.describe()
    );
    if skipped > 0 {
        head.push_str(&format!(
            " ({skipped} hidden or system items not shown, as in Explorer)"
        ));
    }
    let mut lines = vec![head];
    for r in ordered.iter().take(n) {
        let secs = r.mtime.floor() as i64;
        let local = secs + i64::from(utc_offset(secs));
        let stamp = crate::ack::format_utc(local);
        let when = format!("{} {}", &stamp[..10], &stamp[11..16]);
        let size = if r.dir {
            String::new()
        } else {
            format!("  {}", crate::orchestrate::fmt_size(r.size as f64))
        };
        lines.push(format!(
            "{}  {when}  {}{size}",
            if r.dir { "dir " } else { "file" },
            r.name
        ));
    }
    lines.join("\n")
}

/// `os.path.normpath`, lexically: `/` and `\` both separate, `.` and empty pieces vanish, `..`
/// removes the piece before it (and is dropped at a root).
pub fn normpath(path: &str) -> String {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => parts.push(comp),
            },
            other => parts.push(other),
        }
    }
    let mut out = PathBuf::new();
    for c in parts {
        out.push(c.as_os_str());
    }
    let s = out.to_string_lossy().into_owned();
    if s.is_empty() { ".".to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view(v: Value) -> Option<FolderView> {
        parse_view(&v).unwrap()
    }

    fn sort(v: Value) -> Option<(SortColumn, bool)> {
        parse_sort(&v).unwrap().map(|s| (s.column, s.descending))
    }

    // ---- ported from tests/test_20261008b_shortcuts.py::test_view_names
    #[test]
    fn view_names() {
        for (raw, want) in [
            ("details", Some(FolderView::Details)),
            ("Details", Some(FolderView::Details)),
            ("detail", Some(FolderView::Details)),
            ("Large icons", Some(FolderView::Large)),
            ("medium-icons", Some(FolderView::Icons)),
            ("tile", Some(FolderView::Tiles)),
            ("list", Some(FolderView::List)),
            ("", None),
        ] {
            assert_eq!(view(json!(raw)), want, "{raw}");
        }
        assert_eq!(view(Value::Null), None);
    }

    #[test]
    fn an_unknown_view_lists_the_choices() {
        let err = parse_view(&json!("gallery")).unwrap_err();
        assert!(err.contains("view must be one of details, list"), "{err}");
        assert!(err.contains("(got 'gallery')"), "{err}");
    }

    // ---- ported from test_sort_specs
    #[test]
    fn sort_specs() {
        use SortColumn::*;
        for (raw, want) in [
            ("date_modified desc", (DateModified, true)),
            ("date_modified asc", (DateModified, false)),
            ("date_modified", (DateModified, true)), // a date column defaults to newest first
            ("name", (Name, false)),
            ("name asc", (Name, false)),
            ("name desc", (Name, true)),
            ("size desc", (Size, true)),
            ("size", (Size, true)),
            ("type", (Type, false)),
            ("modified", (DateModified, true)),
            ("date modified descending", (DateModified, true)),
            ("-size", (Size, true)),
            ("+name", (Name, false)),
            ("size:asc", (Size, false)),
            ("newest", (DateModified, true)),
            ("oldest", (DateModified, false)),
            ("largest", (Size, true)),
            ("smallest", (Size, false)),
            ("created", (DateCreated, true)),
            ("  Date_Modified   DESC ", (DateModified, true)),
        ] {
            assert_eq!(sort(json!(raw)), Some(want), "{raw}");
        }
        assert_eq!(sort(Value::Null), None);
        assert_eq!(sort(json!("")), None);
    }

    #[test]
    fn a_sort_that_names_no_column_is_refused() {
        for bad in ["desc", "colour"] {
            let err = parse_sort(&json!(bad)).unwrap_err();
            assert!(err.contains("sort must name a column"), "{err}");
        }
    }

    #[test]
    fn counts_accept_numbers_and_numeric_text() {
        assert_eq!(parse_count(&json!(3)).unwrap(), 3);
        assert_eq!(parse_count(&json!("3")).unwrap(), 3);
        assert_eq!(parse_count(&Value::Null).unwrap(), 0);
        assert_eq!(parse_count(&json!("")).unwrap(), 0);
        assert_eq!(parse_count(&json!(false)).unwrap(), 0);
        assert!(parse_count(&json!("many")).is_err());
        assert!(parse_count(&json!(-1)).is_err());
    }

    // ---- ported from test_the_sort_string_the_shell_wants_and_its_reading_back
    #[test]
    fn the_shell_sort_string_round_trips() {
        let s = |c, d| FolderSort {
            column: c,
            descending: d,
        };
        assert_eq!(
            s(SortColumn::DateModified, true).shell_spec(),
            "prop:-System.DateModified;"
        );
        assert_eq!(
            s(SortColumn::Name, false).shell_spec(),
            "prop:System.ItemNameDisplay;"
        );
        let read = |t: &str| FolderSort::from_shell_spec(t).map(|s| s.describe());
        assert_eq!(
            read("prop:-System.DateModified;").as_deref(),
            Some("date_modified desc")
        );
        assert_eq!(
            read("prop:System.Size;-System.DateModified;").as_deref(),
            Some("size asc")
        );
        assert_eq!(read(""), None);
        assert_eq!(read("garbage"), None);
    }

    // ---- ported from test_explorers_own_keyboard_shortcuts_for_each_view
    #[test]
    fn explorers_own_keyboard_shortcuts_for_each_view() {
        let got: Vec<(&str, char)> = FolderView::ALL
            .iter()
            .map(|v| (v.name(), v.shortcut_digit()))
            .collect();
        assert_eq!(
            got,
            [
                ("details", '6'),
                ("list", '5'),
                ("tiles", '7'),
                ("content", '8'),
                ("icons", '3'),
                ("large", '2'),
                ("xlarge", '1'),
                ("small", '4'),
            ]
        );
    }

    fn entry(name: &str, dir: bool, size: u64, t: f64) -> Entry {
        Entry {
            name: name.into(),
            dir,
            mtime: t,
            ctime: t,
            size,
        }
    }

    /// Five entries with known times and sizes. Newest first: new.txt, b10.txt, sub, b2.txt, old.txt.
    fn five() -> Vec<Entry> {
        let t0 = 1_700_000_000.0;
        vec![
            entry("old.txt", false, 5, t0),
            entry("b2.txt", false, 300, t0 + 100.0),
            entry("b10.txt", false, 20, t0 + 200.0),
            entry("new.txt", false, 1000, t0 + 300.0),
            entry("sub", true, 0, t0 + 150.0),
        ]
    }

    fn names(rows: &[Entry], col: SortColumn, desc: bool) -> Vec<String> {
        sorted(
            rows,
            FolderSort {
                column: col,
                descending: desc,
            },
        )
        .into_iter()
        .map(|e| e.name)
        .collect()
    }

    // ---- ported from test_sorting_by_each_column
    #[test]
    fn sorting_by_each_column() {
        let rows = five();
        assert_eq!(
            names(&rows, SortColumn::DateModified, true),
            ["new.txt", "b10.txt", "sub", "b2.txt", "old.txt"]
        );
        assert_eq!(
            names(&rows, SortColumn::DateModified, false),
            ["old.txt", "b2.txt", "sub", "b10.txt", "new.txt"]
        );
        assert_eq!(
            &names(&rows, SortColumn::Size, true)[..2],
            ["new.txt", "b2.txt"]
        );
        assert_eq!(
            names(&rows, SortColumn::Size, false).last().unwrap(),
            "new.txt"
        );
        // b2 before b10, as Explorer does
        assert_eq!(
            names(&rows, SortColumn::Name, false),
            ["b2.txt", "b10.txt", "new.txt", "old.txt", "sub"]
        );
        assert_eq!(names(&rows, SortColumn::Name, true)[0], "sub");
        // a folder has no extension, so it sorts first
        assert_eq!(names(&rows, SortColumn::Type, false)[0], "sub");
    }

    // ---- ported from test_list_text_newest_first_with_kind_time_and_size
    #[test]
    fn list_text_newest_first_with_kind_time_and_size() {
        let sort = FolderSort {
            column: SortColumn::DateModified,
            descending: true,
        };
        let out = list_text("C:\\x", &five(), 0, 3, sort, &|_| 0);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].ends_with("first 3 of 5 entries by date_modified desc"),
            "{}",
            lines[0]
        );
        let name_of = |l: &str| l.split("  ").nth(2).unwrap().to_string();
        assert_eq!(
            lines[1..].iter().map(|l| name_of(l)).collect::<Vec<_>>(),
            ["new.txt", "b10.txt", "sub"]
        );
        assert!(lines[1].starts_with("file  ") && lines[1].contains("new.txt  1000B"));
        assert_eq!(
            lines[1..]
                .iter()
                .map(|l| l.split_whitespace().next().unwrap())
                .collect::<Vec<_>>(),
            ["file", "file", "dir"]
        );
        // 1_700_000_300 is 2023-11-14 22:18:20 UTC; a zone two hours east prints 00:18 the next day
        let shifted = list_text("C:\\x", &five(), 0, 1, sort, &|_| 7200);
        assert!(shifted.contains("2023-11-15 00:18  new.txt"), "{shifted}");
    }

    // ---- ported from test_list_is_capped_and_an_empty_folder_is_fine
    #[test]
    fn list_is_capped_and_an_empty_folder_is_fine() {
        let sort = FolderSort {
            column: SortColumn::Name,
            descending: false,
        };
        assert_eq!(
            list_text("C:\\x", &[], 0, 5, sort, &|_| 0),
            "C:\\x: first 0 of 0 entries by name asc"
        );
        let rows: Vec<Entry> = (0..3)
            .map(|i| entry(&format!("f{i}.txt"), false, 1, 0.0))
            .collect();
        assert_eq!(
            list_text("C:\\x", &rows, 0, 9999, sort, &|_| 0)
                .lines()
                .count(),
            4
        );
        let many: Vec<Entry> = (0..300)
            .map(|i| entry(&format!("f{i}.txt"), false, 1, 0.0))
            .collect();
        assert_eq!(
            list_text("C:\\x", &many, 0, 9999, sort, &|_| 0)
                .lines()
                .count(),
            LIST_MAX + 1
        );
        assert!(
            list_text("C:\\x", &rows, 2, 5, sort, &|_| 0)
                .contains("(2 hidden or system items not shown, as in Explorer)")
        );
    }

    #[test]
    fn extensions_follow_splitext() {
        assert_eq!(extension("a.TXT"), ".txt");
        assert_eq!(extension(".bashrc"), "");
        assert_eq!(extension("a.b.c"), ".c");
        assert_eq!(extension("noext"), "");
    }

    #[test]
    fn normpath_tidies_a_path_lexically() {
        let sep = std::path::MAIN_SEPARATOR;
        let join = |parts: &[&str]| parts.join(&sep.to_string());
        assert_eq!(normpath(&join(&["a", "b", "..", "c"])), join(&["a", "c"]));
        assert_eq!(normpath(&join(&["a", ".", "b", ""])), join(&["a", "b"]));
        assert_eq!(normpath("."), ".");
        #[cfg(windows)]
        {
            assert_eq!(normpath("C:/Users/x/../y/"), "C:\\Users\\y");
            assert_eq!(
                normpath("C:\\Users\\ikill\\My Docs"),
                "C:\\Users\\ikill\\My Docs"
            );
        }
    }

    #[test]
    fn a_real_folder_is_read_and_hidden_items_are_not_listed() {
        let tmp = crate::testutil::TempDir::new("folders");
        std::fs::write(tmp.path().join("a.txt"), b"abc").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join(".hidden"), b"x").unwrap();
        #[cfg(windows)]
        {
            let hidden = tmp.path().join("desktop.ini");
            std::fs::write(&hidden, b"x").unwrap();
            let status = std::process::Command::new("attrib")
                .arg("+h")
                .arg(&hidden)
                .status();
            if !status.is_ok_and(|s| s.success()) {
                return;
            }
        }
        let (rows, skipped) = entries(tmp.path());
        let mut got: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        got.sort_unstable();
        #[cfg(windows)]
        {
            assert_eq!(got, [".hidden", "a.txt", "sub"]);
            assert_eq!(skipped, 1);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(got, ["a.txt", "sub"]);
            assert_eq!(skipped, 1);
        }
        let a = rows.iter().find(|r| r.name == "a.txt").unwrap();
        assert!(!a.dir && a.size == 3);
        assert!(rows.iter().find(|r| r.name == "sub").unwrap().dir);
    }
}
