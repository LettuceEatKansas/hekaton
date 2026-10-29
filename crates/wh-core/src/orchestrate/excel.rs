//! The `excel` action: bulk spreadsheet work through Excel's own automation interface instead of
//! clicking through its window. Writing 100,000 rows by clicking and typing takes hours; handing
//! Excel the whole block in one call takes seconds.
//!
//! It is a power channel, so it is off until the owner lists `excel` in `power_channels`
//! (`WINHANDS_POWER_CHANNELS=excel`), and it only ever drives a hidden Excel this tool started
//! itself (`new`, `open`), never the one the person has open. `read` is a look: it runs under
//! `read_only` and the gaming lock. Everything else is an acting op, `format_conditional`, `chart`
//! and `pivot` included. A string written is stored as literal text unless it starts with `=`.
//!
//! The audit log gets the op, handle, sheet, range, row and column counts and the path. Cell values
//! never reach it, neither in the arguments (`values` is masked) nor in the result (a call that read
//! cells logs a fixed line instead of its output).

use std::path::Path;

use serde_json::{Map, Value, json};

use super::Ctx;
use super::args::{Args, flag, get, text as text_arg, to_float};
use super::error::{StepError, StepResult};
use super::files::expand_vars;
use super::guard::norm_abs;
use crate::backend::excel::{
    MAX_COLS, MAX_ROWS, WRITE_CELL_CAP, col_name, parse_area, parse_cell, parse_color,
    valid_sheet_name,
};
use crate::backend::{ExcelCell, ExcelChartKind, ExcelRequest, ExcelRule};
use crate::safety::Profile;

const OPS: &str =
    "new, open, write, read, add_sheet, calc, save_as, close, format_conditional, chart, pivot";
const RULES: &str = "greater_than, less_than, between, color_scale";
const KINDS: &str = "column, line, bar, pie";

/// A cell as JSON: whole numbers without the `.0`.
fn cell_json(cell: &ExcelCell) -> Value {
    match cell {
        ExcelCell::Empty => Value::Null,
        ExcelCell::Text(t) => Value::from(t.clone()),
        ExcelCell::Bool(b) => Value::from(*b),
        ExcelCell::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => Value::from(*n as i64),
        ExcelCell::Number(n) => Value::from(*n),
    }
}

fn grid_json(rows: &[Vec<ExcelCell>]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| Value::Array(r.iter().map(cell_json).collect()))
            .collect(),
    )
}

/// `values` as rows of cells: a list of equal-length lists of text, numbers, bools and nulls. The
/// errors name positions only, never a cell's content.
fn parse_values(v: Option<&Value>) -> StepResult<Vec<Vec<ExcelCell>>> {
    let Some(Value::Array(rows)) = v else {
        return Err(StepError::value(
            "excel write needs values: a list of rows, each a list of cells, like [[1, 2], [\"a\", \"=A1+B1\"]]",
        ));
    };
    let width = match rows.first() {
        Some(Value::Array(first)) => first.len(),
        _ => 0,
    };
    if rows.is_empty() || width == 0 {
        return Err(StepError::value(
            "excel write needs at least one cell in values",
        ));
    }
    let mut out = Vec::with_capacity(rows.len());
    for (r, row) in rows.iter().enumerate() {
        let Value::Array(cells) = row else {
            return Err(StepError::value(format!(
                "values row {} is not a list",
                r + 1
            )));
        };
        if cells.len() != width {
            return Err(StepError::value(format!(
                "values row {} has {} cells but row 1 has {width}; every row needs the same length",
                r + 1,
                cells.len()
            )));
        }
        let mut line = Vec::with_capacity(width);
        for (c, cell) in cells.iter().enumerate() {
            line.push(match cell {
                Value::Null => ExcelCell::Empty,
                Value::Bool(b) => ExcelCell::Bool(*b),
                Value::Number(n) => ExcelCell::Number(n.as_f64().unwrap_or(0.0)),
                Value::String(s) => ExcelCell::Text(s.clone()),
                _ => {
                    return Err(StepError::value(format!(
                        "values row {} cell {} must be text, a number, true/false or null",
                        r + 1,
                        c + 1
                    )));
                }
            });
        }
        out.push(line);
    }
    Ok(out)
}

impl Ctx<'_> {
    /// One `excel` step. See the [module docs](self).
    pub(crate) fn run_excel(&self, step: &Args) -> StepResult<String> {
        if !self.cfg().power_channels().contains("excel") {
            return Err(StepError::permission(
                "excel is a power channel and it is off. The owner turns it on with power_channels: \
                 \"excel\" in config.json or WINHANDS_POWER_CHANNELS=excel. Nothing was sent to Excel",
            ));
        }
        let op = text_arg(step, "op").trim().to_ascii_lowercase();
        if !self.o.only_apps.is_empty()
            && !self
                .o
                .only_apps
                .iter()
                .any(|a| a.split(':').next() == Some("excel.exe"))
        {
            return Err(StepError::target(
                "refused: an allowlist is active and it does not include excel.exe",
            ));
        }
        if self.o.read_only && op != "read" {
            return Err(StepError::permission(
                "read_only lets excel read and nothing else; nothing was sent to Excel",
            ));
        }
        let workbook = || -> StepResult<String> {
            let wb = text_arg(step, "workbook");
            if wb.is_empty() {
                return Err(StepError::value(format!(
                    "excel {op} needs workbook: the handle excel new or excel open returned, like x1"
                )));
            }
            Ok(wb)
        };
        let sheet = Some(text_arg(step, "sheet")).filter(|s| !s.is_empty());
        match op.as_str() {
            "new" => {
                let reply = self.be().excel(&ExcelRequest::New)?;
                Ok(format!(
                    "excel: new workbook {} (sheets: {}); a hidden Excel this tool owns",
                    reply.workbook.unwrap_or_default(),
                    reply.sheets.join(", ")
                ))
            }
            "open" => {
                let path = self.excel_path(step, "the excel open path")?;
                if !Path::new(&path).is_file() {
                    return Err(StepError::target(format!("no file at {path}")));
                }
                self.excel_open_allowed(&path)?;
                let reply = self
                    .be()
                    .excel(&ExcelRequest::Open { path: path.clone() })?;
                Ok(format!(
                    "excel: opened {path} as {} (sheets: {})",
                    reply.workbook.unwrap_or_default(),
                    reply.sheets.join(", ")
                ))
            }
            "write" => {
                let wb = workbook()?;
                let range = Some(text_arg(step, "range"))
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "A1".into());
                let Some((col, row)) = parse_cell(&range) else {
                    return Err(StepError::value(
                        "excel write range is the top-left cell, like A1; the block's size comes from values",
                    ));
                };
                let values = parse_values(get(step, "values"))?;
                let (rows, cols) = (values.len(), values[0].len());
                if rows * cols > WRITE_CELL_CAP {
                    return Err(StepError::value(format!(
                        "refusing to write {} cells in one call (cap {WRITE_CELL_CAP}); split it into smaller blocks",
                        rows * cols
                    )));
                }
                if col as usize + cols - 1 > MAX_COLS as usize
                    || row as usize + rows - 1 > MAX_ROWS as usize
                {
                    return Err(StepError::value(format!(
                        "{rows} rows x {cols} cols from {range} runs off the sheet"
                    )));
                }
                self.be().excel(&ExcelRequest::Write {
                    workbook: wb.clone(),
                    sheet: sheet.clone(),
                    range: range.clone(),
                    values,
                })?;
                Ok(format!(
                    "excel: wrote {rows} rows x {cols} cols ({} cells) at {}{range} in {wb}",
                    rows * cols,
                    sheet.map(|s| format!("{s}!")).unwrap_or_default()
                ))
            }
            "read" => {
                let wb = workbook()?;
                let range = text_arg(step, "range");
                let Some(area) = parse_area(&range) else {
                    return Err(StepError::value(
                        "excel read needs range like A1:C5 (cells on one sheet; name the sheet in sheet)",
                    ));
                };
                // Cut the read down to the cap before Excel is asked, so a huge range costs nothing.
                let cap = self.cfg().excel_read_max_cells() as usize;
                if area.cols as usize > cap {
                    return Err(StepError::value(format!(
                        "the range is {} columns wide, over the read cap of {cap} cells",
                        area.cols
                    )));
                }
                let used = area.first_rows((cap / area.cols as usize).max(1) as u32);
                let formulas = flag(step.get("formulas"), false);
                let reply = self.be().excel(&ExcelRequest::Read {
                    workbook: wb.clone(),
                    sheet: sheet.clone(),
                    range: used.address(),
                    formulas,
                })?;
                self.o
                    .private_output
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let mut body = Map::new();
                body.insert("values".into(), grid_json(&reply.values));
                if formulas {
                    body.insert("formulas".into(), grid_json(&reply.formulas));
                }
                let cut = if used.rows < area.rows {
                    format!(
                        "; TRUNCATED to the first {} of {} rows by the {cap}-cell read cap",
                        used.rows, area.rows
                    )
                } else {
                    String::new()
                };
                Ok(format!(
                    "excel: {}{} of {wb}, {} rows x {} cols{cut}\n{}",
                    sheet.map(|s| format!("{s}!")).unwrap_or_default(),
                    used.address(),
                    used.rows,
                    used.cols,
                    Value::Object(body)
                ))
            }
            "add_sheet" => {
                let wb = workbook()?;
                let name = text_arg(step, "name");
                if name.is_empty() {
                    return Err(StepError::value(
                        "excel add_sheet needs name: the new sheet's name",
                    ));
                }
                let reply = self.be().excel(&ExcelRequest::AddSheet {
                    workbook: wb.clone(),
                    name: name.clone(),
                })?;
                Ok(format!(
                    "excel: added sheet {name:?} to {wb} (sheets: {})",
                    reply.sheets.join(", ")
                ))
            }
            "calc" => {
                self.be().excel(&ExcelRequest::Calc)?;
                Ok("excel: recalculated".into())
            }
            "save_as" => {
                let wb = workbook()?;
                let path = self.excel_path(step, "the excel save path")?;
                if !path.to_ascii_lowercase().ends_with(".xlsx") {
                    return Err(StepError::value("excel save_as writes .xlsx files only"));
                }
                if Path::new(&path).exists() {
                    // The model wrote overwrite, so under strict it is not the owner's approval.
                    if self.cfg().profile() == Profile::Strict {
                        return Err(StepError::permission(format!(
                            "the strict profile never lets excel replace a file, and {path} exists"
                        )));
                    }
                    if !flag(step.get("overwrite"), false) {
                        return Err(StepError::target(format!(
                            "refusing to overwrite {path}; pass overwrite: true to replace it"
                        )));
                    }
                }
                self.be().excel(&ExcelRequest::SaveAs {
                    workbook: wb.clone(),
                    path: path.clone(),
                })?;
                if let Ok(mut saved) = self.o.saved_paths.lock() {
                    saved.push(path.clone());
                }
                Ok(format!("excel: saved {wb} as {path}"))
            }
            "close" => {
                let wb = workbook()?;
                let reply = self.be().excel(&ExcelRequest::Close {
                    workbook: wb.clone(),
                })?;
                let quit = if reply.instance_quit {
                    "; no workbooks left, so the hidden Excel quit"
                } else {
                    ""
                };
                Ok(format!("excel: closed {wb} without saving{quit}"))
            }
            "format_conditional" => {
                let wb = workbook()?;
                let range = text_arg(step, "range");
                let Some(area) = parse_area(&range) else {
                    return Err(StepError::value(
                        "excel format_conditional needs range like D2:D100001 (cells on one sheet; name the sheet in sheet)",
                    ));
                };
                let rule_name = text_arg(step, "rule").trim().to_ascii_lowercase();
                if rule_name.is_empty() {
                    return Err(StepError::value(format!(
                        "excel format_conditional needs rule: one of {RULES}"
                    )));
                }
                let number = |key: &str| -> StepResult<f64> {
                    match get(step, key) {
                        Some(v) => to_float(v),
                        None => Err(StepError::value(format!(
                            "excel format_conditional {rule_name} needs {key}: a number"
                        ))),
                    }
                };
                let color = || -> StepResult<u32> {
                    let raw = text_arg(step, "color");
                    if raw.is_empty() {
                        return Err(StepError::value(format!(
                            "excel format_conditional {rule_name} needs color: the fill, like #FFC7CE"
                        )));
                    }
                    parse_color(&raw).ok_or_else(|| {
                        StepError::value(format!("color must look like #RRGGBB, got {raw:?}"))
                    })
                };
                let rule = match rule_name.as_str() {
                    "greater_than" => ExcelRule::GreaterThan {
                        value: number("value")?,
                        color: color()?,
                    },
                    "less_than" => ExcelRule::LessThan {
                        value: number("value")?,
                        color: color()?,
                    },
                    "between" => ExcelRule::Between {
                        low: number("value")?,
                        high: number("value2")?,
                        color: color()?,
                    },
                    "color_scale" => ExcelRule::ColorScale,
                    other => {
                        return Err(StepError::value(format!(
                            "unknown excel rule {other:?}; one of: {RULES}"
                        )));
                    }
                };
                self.be().excel(&ExcelRequest::FormatConditional {
                    workbook: wb.clone(),
                    sheet: sheet.clone(),
                    range: area.address(),
                    rule,
                })?;
                Ok(format!(
                    "excel: conditional format {rule_name} added to {}{} in {wb}",
                    sheet.map(|s| format!("{s}!")).unwrap_or_default(),
                    area.address()
                ))
            }
            "chart" => {
                let wb = workbook()?;
                let range = text_arg(step, "range");
                let Some(area) = parse_area(&range) else {
                    return Err(StepError::value(
                        "excel chart needs range like A1:C20, the data with its header row (name the sheet in sheet)",
                    ));
                };
                if area.rows < 2 {
                    return Err(StepError::value(
                        "excel chart needs a header row and at least one row of data in range",
                    ));
                }
                let kind_name = text_arg(step, "kind").trim().to_ascii_lowercase();
                let kind = match kind_name.as_str() {
                    "column" => ExcelChartKind::Column,
                    "line" => ExcelChartKind::Line,
                    "bar" => ExcelChartKind::Bar,
                    "pie" => ExcelChartKind::Pie,
                    "" => {
                        return Err(StepError::value(format!(
                            "excel chart needs kind: one of {KINDS}"
                        )));
                    }
                    other => {
                        return Err(StepError::value(format!(
                            "unknown chart kind {other:?}; one of: {KINDS}"
                        )));
                    }
                };
                // `at` already means fractional window coordinates on the MCP wire, so a cell goes
                // in `anchor`; a text `at` is taken too for callers that skip the shim.
                let placed = Some(text_arg(step, "anchor"))
                    .filter(|a| !a.is_empty())
                    .or_else(|| match get(step, "at") {
                        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
                        _ => None,
                    });
                let at = match placed {
                    Some(cell) => {
                        let Some((col, row)) = parse_cell(&cell) else {
                            return Err(StepError::value(
                                "excel chart anchor is the top-left cell for the chart, like H2",
                            ));
                        };
                        Some(format!("{}{row}", col_name(col)))
                    }
                    None => None,
                };
                let title = Some(text_arg(step, "title")).filter(|t| !t.is_empty());
                if title.as_ref().is_some_and(|t| t.chars().count() > 255) {
                    return Err(StepError::value(
                        "excel chart title is over Excel's 255 character limit",
                    ));
                }
                let reply = self.be().excel(&ExcelRequest::Chart {
                    workbook: wb.clone(),
                    sheet: sheet.clone(),
                    range: area.address(),
                    kind,
                    title,
                    at,
                })?;
                Ok(format!(
                    "excel: added {kind_name} chart {:?} of {}{} to {wb}, top-left at {}",
                    reply.name.unwrap_or_default(),
                    sheet.map(|s| format!("{s}!")).unwrap_or_default(),
                    area.address(),
                    reply.address.unwrap_or_default()
                ))
            }
            "pivot" => {
                let wb = workbook()?;
                let range = text_arg(step, "range");
                let Some(area) = parse_area(&range) else {
                    return Err(StepError::value(
                        "excel pivot needs range like A1:D200, the source data with its header row (name the sheet in sheet)",
                    ));
                };
                if area.rows < 2 || area.cols < 2 {
                    return Err(StepError::value(
                        "excel pivot needs a header row, at least one row of data and two columns in range",
                    ));
                }
                let rows = text_arg(step, "rows");
                if rows.trim().is_empty() {
                    return Err(StepError::value(
                        "excel pivot needs rows: the header of the column to group by",
                    ));
                }
                let sum_field = text_arg(step, "sum_field");
                if sum_field.trim().is_empty() {
                    return Err(StepError::value(
                        "excel pivot needs sum_field: the header of the column to add up (values is the write argument)",
                    ));
                }
                let columns = Some(text_arg(step, "columns")).filter(|c| !c.trim().is_empty());
                if columns
                    .as_ref()
                    .is_some_and(|c| c.trim().eq_ignore_ascii_case(rows.trim()))
                {
                    return Err(StepError::value(
                        "excel pivot columns and rows must be different headers",
                    ));
                }
                let name = Some(text_arg(step, "name"))
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "Pivot".into());
                if !valid_sheet_name(&name) {
                    return Err(StepError::value(
                        "excel pivot name is the new sheet's name: 1 to 31 characters, none of \\ / ? * [ ] :",
                    ));
                }
                let reply = self.be().excel(&ExcelRequest::Pivot {
                    workbook: wb.clone(),
                    sheet: sheet.clone(),
                    range: area.address(),
                    rows,
                    sum_field,
                    columns,
                    name,
                })?;
                Ok(format!(
                    "excel: pivot of {}{} in {wb} is on sheet {:?}, table at {}",
                    sheet.map(|s| format!("{s}!")).unwrap_or_default(),
                    area.address(),
                    reply.name.unwrap_or_default(),
                    reply.address.unwrap_or_default()
                ))
            }
            other => Err(StepError::value(format!(
                "unknown excel op {other:?}; one of: {OPS}"
            ))),
        }
    }

    /// The `path` argument (or `text`), variables expanded, absolute, and past the owner's hard
    /// line on system and credential paths: the same check the file dialog applies to a save path.
    fn excel_path(&self, step: &Args, what: &str) -> StepResult<String> {
        let raw = Some(text_arg(step, "path"))
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| text_arg(step, "text"));
        if raw.is_empty() {
            return Err(StepError::value("excel needs path: the full file path"));
        }
        let path = expand_vars(self.cfg().env(), &raw);
        self.o.policy.check_path(&path, what)?;
        if !Path::new(&path).is_absolute() {
            return Err(StepError::value(format!(
                "excel needs a full path starting at a drive, got {path}"
            )));
        }
        Ok(path)
    }

    /// Under the strict profile Excel may open only files under %TEMP% and files this call saved.
    /// A workbook can carry links and macros; opening one from anywhere else is the owner's call.
    fn excel_open_allowed(&self, path: &str) -> StepResult<()> {
        if self.cfg().profile() != Profile::Strict {
            return Ok(());
        }
        let want = norm_abs(path);
        let saved = self
            .o
            .saved_paths
            .lock()
            .is_ok_and(|saved| saved.iter().any(|s| norm_abs(s) == want));
        let env = self.cfg().env();
        let temp = env
            .var("TEMP")
            .filter(|t| !t.is_empty())
            .or_else(|| env.var("TMP").filter(|t| !t.is_empty()))
            .map(|t| norm_abs(&t));
        let in_temp = temp.is_some_and(|t| want.starts_with(&format!("{t}/")));
        if saved || in_temp {
            return Ok(());
        }
        Err(StepError::permission(format!(
            "the strict profile lets excel open only files under %TEMP% or ones this call saved, not {path}"
        )))
    }
}

/// What the audit entry records about the `excel` steps of a call: op, handle, sheet, range, the
/// size and the path. Never cell values. `None` when the call has no excel step.
pub(crate) fn audit_fields(args: &Value) -> Option<Map<String, Value>> {
    let steps: Vec<&Value> = match args.get("actions").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().collect(),
        _ => vec![args],
    };
    let mut entries = Vec::new();
    for step in steps {
        if step.get("action").and_then(Value::as_str) != Some("excel") {
            continue;
        }
        let text = |key: &str| {
            step.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let (rows, cols) = match step.get("values").and_then(Value::as_array) {
            Some(r) => (
                Some(r.len()),
                r.first().and_then(Value::as_array).map(Vec::len),
            ),
            None => match text("range").and_then(parse_area) {
                Some(a) => (Some(a.rows as usize), Some(a.cols as usize)),
                None => (None, None),
            },
        };
        let mut e = Map::new();
        for (key, value) in [
            ("op", text("op")),
            ("workbook", text("workbook")),
            ("sheet", text("sheet")),
            ("range", text("range")),
            ("path", text("path")),
            ("rule", text("rule")),
            ("kind", text("kind")),
        ] {
            if let Some(v) = value {
                e.insert(key.into(), json!(v));
            }
        }
        if let (Some(r), Some(c)) = (rows, cols) {
            e.insert("rows".into(), json!(r));
            e.insert("cols".into(), json!(c));
        }
        entries.push(Value::Object(e));
    }
    (!entries.is_empty()).then(|| Map::from_iter([("excel".to_string(), Value::Array(entries))]))
}
