//! A scriptable in-memory Excel for [`FakeBackend`](super::fake::FakeBackend), so the orchestration
//! tests of the `excel` action run without Excel. Workbooks keep cells as written; reading a formula
//! evaluates `=SUM(range)` and `=cell`, which is all the tests need. Anything else reads `#NAME?`.

use std::collections::BTreeMap;

use super::excel::{Area, default_chart_anchor, find_header, parse_area, parse_cell};
use super::{BackendError, BackendResult, ExcelCell, ExcelReply, ExcelRequest, ExcelRule};

struct Sheet {
    name: String,
    cells: BTreeMap<(u32, u32), ExcelCell>,
    /// Conditional formats added: the range and the rule.
    formats: Vec<(String, ExcelRule)>,
    /// Charts added.
    charts: u32,
}

impl Sheet {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            cells: BTreeMap::new(),
            formats: Vec::new(),
            charts: 0,
        }
    }
}

struct Book {
    sheets: Vec<Sheet>,
    saved_as: Option<String>,
}

/// The fake's whole Excel: the instance flag, the workbooks and a log of what was asked.
pub(super) struct FakeExcel {
    pub(super) available: bool,
    pub(super) running: bool,
    pub(super) quits: u32,
    pub(super) requests: Vec<ExcelRequest>,
    books: BTreeMap<String, Book>,
    next: u32,
}

impl FakeExcel {
    pub(super) fn new() -> Self {
        Self {
            available: true,
            running: false,
            quits: 0,
            requests: Vec::new(),
            books: BTreeMap::new(),
            next: 1,
        }
    }

    pub(super) fn saved_as(&self, handle: &str) -> Option<String> {
        self.books.get(handle)?.saved_as.clone()
    }

    /// Quit the instance with whatever is still open, as the real backend does on shutdown.
    pub(super) fn shutdown(&mut self) {
        if self.running {
            self.books.clear();
            self.running = false;
            self.quits += 1;
        }
    }

    fn start(&mut self) -> BackendResult<()> {
        if !self.available {
            return Err(BackendError::Unsupported("Excel is not installed".into()));
        }
        self.running = true;
        Ok(())
    }

    fn book(&mut self, handle: &str) -> BackendResult<&mut Book> {
        self.books.get_mut(handle).ok_or_else(|| {
            BackendError::Os(format!(
                "no workbook {handle:?}; use a handle that excel new or excel open returned"
            ))
        })
    }

    fn sheet<'a>(book: &'a mut Book, name: Option<&str>) -> BackendResult<&'a mut Sheet> {
        match name {
            None => book.sheets.first_mut(),
            Some(n) => book
                .sheets
                .iter_mut()
                .find(|s| s.name.eq_ignore_ascii_case(n)),
        }
        .ok_or_else(|| BackendError::Os(format!("no sheet {:?}", name.unwrap_or("(first)"))))
    }

    pub(super) fn run(&mut self, req: &ExcelRequest) -> BackendResult<ExcelReply> {
        self.requests.push(req.clone());
        match req {
            ExcelRequest::New | ExcelRequest::Open { .. } => {
                self.start()?;
                let handle = format!("x{}", self.next);
                self.next += 1;
                let book = Book {
                    sheets: vec![Sheet::new("Sheet1")],
                    saved_as: None,
                };
                self.books.insert(handle.clone(), book);
                Ok(ExcelReply {
                    workbook: Some(handle),
                    sheets: vec!["Sheet1".into()],
                    ..Default::default()
                })
            }
            ExcelRequest::Write {
                workbook,
                sheet,
                range,
                values,
            } => {
                let (col, row) = parse_cell(range)
                    .ok_or_else(|| BackendError::Os(format!("bad range {range:?}")))?;
                let sheet = Self::sheet(self.book(workbook)?, sheet.as_deref())?;
                for (r, line) in values.iter().enumerate() {
                    for (c, cell) in line.iter().enumerate() {
                        let at = (row + r as u32, col + c as u32);
                        if *cell == ExcelCell::Empty {
                            sheet.cells.remove(&at);
                        } else {
                            sheet.cells.insert(at, cell.clone());
                        }
                    }
                }
                Ok(ExcelReply::default())
            }
            ExcelRequest::Read {
                workbook,
                sheet,
                range,
                formulas,
            } => {
                let area = parse_area(range)
                    .ok_or_else(|| BackendError::Os(format!("bad range {range:?}")))?;
                let sheet = Self::sheet(self.book(workbook)?, sheet.as_deref())?;
                let grid = |f: &dyn Fn(&Sheet, u32, u32) -> ExcelCell| {
                    (0..area.rows)
                        .map(|r| {
                            (0..area.cols)
                                .map(|c| f(sheet, area.row + r, area.col + c))
                                .collect()
                        })
                        .collect::<Vec<Vec<ExcelCell>>>()
                };
                Ok(ExcelReply {
                    values: grid(&|s, r, c| value_of(s, r, c, 0)),
                    formulas: if *formulas {
                        grid(&|s, r, c| s.cells.get(&(r, c)).cloned().unwrap_or(ExcelCell::Empty))
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                })
            }
            ExcelRequest::AddSheet { workbook, name } => {
                let book = self.book(workbook)?;
                if book
                    .sheets
                    .iter()
                    .any(|s| s.name.eq_ignore_ascii_case(name))
                {
                    return Err(BackendError::Os(format!("a sheet named {name:?} exists")));
                }
                book.sheets.push(Sheet::new(name));
                Ok(ExcelReply {
                    sheets: book.sheets.iter().map(|s| s.name.clone()).collect(),
                    ..Default::default()
                })
            }
            ExcelRequest::Calc => {
                if !self.running {
                    return Err(BackendError::Os(
                        "no Excel instance; excel new or excel open starts one".into(),
                    ));
                }
                Ok(ExcelReply::default())
            }
            ExcelRequest::SaveAs { workbook, path } => {
                self.book(workbook)?.saved_as = Some(path.clone());
                // Like Excel, a save leaves a file behind, so the steps after it see one.
                std::fs::write(path, b"fake xlsx")
                    .map_err(|e| BackendError::Os(format!("save {path}: {e}")))?;
                Ok(ExcelReply::default())
            }
            ExcelRequest::FormatConditional {
                workbook,
                sheet,
                range,
                rule,
            } => {
                let area = parse_area(range)
                    .ok_or_else(|| BackendError::Os(format!("bad range {range:?}")))?;
                let sheet = Self::sheet(self.book(workbook)?, sheet.as_deref())?;
                sheet.formats.push((area.address(), rule.clone()));
                Ok(ExcelReply {
                    address: Some(area.address()),
                    ..Default::default()
                })
            }
            ExcelRequest::Chart {
                workbook,
                sheet,
                range,
                at,
                ..
            } => {
                let area = parse_area(range)
                    .ok_or_else(|| BackendError::Os(format!("bad range {range:?}")))?;
                let sheet = Self::sheet(self.book(workbook)?, sheet.as_deref())?;
                sheet.charts += 1;
                Ok(ExcelReply {
                    name: Some(format!("Chart {}", sheet.charts)),
                    address: Some(at.clone().unwrap_or_else(|| default_chart_anchor(&area))),
                    ..Default::default()
                })
            }
            ExcelRequest::Pivot {
                workbook,
                sheet,
                range,
                rows,
                sum_field,
                columns,
                name,
            } => {
                let area = parse_area(range)
                    .ok_or_else(|| BackendError::Os(format!("bad range {range:?}")))?;
                let book = self.book(workbook)?;
                let source = Self::sheet(book, sheet.as_deref())?;
                // header names are checked first; an error names only what was asked for
                let headers: Vec<ExcelCell> = (0..area.cols)
                    .map(|c| value_of(source, area.row, area.col + c, 0))
                    .collect();
                let column_of = |want: &str, role: &str| -> BackendResult<u32> {
                    let found = find_header(&headers, want).ok_or_else(|| {
                        BackendError::Os(format!(
                            "no column headed {want:?} in the first row of {} ({role})",
                            area.address()
                        ))
                    })?;
                    let at = headers
                        .iter()
                        .position(|h| find_header(std::slice::from_ref(h), &found).is_some())
                        .unwrap_or(0);
                    Ok(area.col + at as u32)
                };
                let row_col = column_of(rows, "rows")?;
                let sum_col = column_of(sum_field, "sum_field")?;
                let col_col = columns
                    .as_deref()
                    .map(|c| column_of(c, "columns"))
                    .transpose()?;
                let table = pivot_table(source, &area, row_col, sum_col, col_col, sum_field);
                if book
                    .sheets
                    .iter()
                    .any(|s| s.name.eq_ignore_ascii_case(name))
                {
                    return Err(BackendError::Os(format!("a sheet named {name:?} exists")));
                }
                let mut out = Sheet::new(name);
                let mut width = 1u32;
                for (r, line) in table.iter().enumerate() {
                    for (c, cell) in line.iter().enumerate() {
                        if *cell != ExcelCell::Empty {
                            out.cells.insert((3 + r as u32, 1 + c as u32), cell.clone());
                        }
                    }
                    width = width.max(line.len() as u32);
                }
                book.sheets.push(out);
                let address = Area {
                    col: 1,
                    row: 3,
                    cols: width,
                    rows: table.len() as u32,
                }
                .address();
                Ok(ExcelReply {
                    name: Some(name.clone()),
                    address: Some(address),
                    ..Default::default()
                })
            }
            ExcelRequest::Close { workbook } => {
                self.book(workbook)?;
                self.books.remove(workbook);
                let last = self.books.is_empty();
                if last {
                    self.shutdown();
                }
                Ok(ExcelReply {
                    instance_quit: last,
                    ..Default::default()
                })
            }
        }
    }
}

/// The value a cell shows: constants as written, `=SUM(A1:A3)` and `=B2` worked out.
fn value_of(sheet: &Sheet, row: u32, col: u32, depth: u32) -> ExcelCell {
    let Some(cell) = sheet.cells.get(&(row, col)) else {
        return ExcelCell::Empty;
    };
    let ExcelCell::Text(text) = cell else {
        return cell.clone();
    };
    let Some(formula) = text.strip_prefix('=') else {
        return cell.clone();
    };
    if depth > 8 {
        return ExcelCell::Text("#REF!".into());
    }
    let upper = formula.trim().to_ascii_uppercase();
    if let Some(inner) = upper.strip_prefix("SUM(").and_then(|r| r.strip_suffix(')'))
        && let Some(area) = parse_area(inner)
    {
        let mut total = 0.0;
        for r in 0..area.rows {
            for c in 0..area.cols {
                if let ExcelCell::Number(n) = value_of(sheet, area.row + r, area.col + c, depth + 1)
                {
                    total += n;
                }
            }
        }
        return ExcelCell::Number(total);
    }
    if let Some((c, r)) = parse_cell(&upper) {
        return value_of(sheet, r, c, depth + 1);
    }
    ExcelCell::Text("#NAME?".into())
}

/// The pivot a sum over `sum_col` grouped by `row_col` (and `col_col`) would show, laid out the way
/// Excel's compact form does, with a Grand Total row and, with a column field, a Grand Total column.
fn pivot_table(
    sheet: &Sheet,
    area: &Area,
    row_col: u32,
    sum_col: u32,
    col_col: Option<u32>,
    sum_field: &str,
) -> Vec<Vec<ExcelCell>> {
    let label = |r: u32, c: u32| match value_of(sheet, r, c, 0) {
        ExcelCell::Text(t) => t,
        ExcelCell::Number(n) if n.fract() == 0.0 => format!("{}", n as i64),
        ExcelCell::Number(n) => n.to_string(),
        ExcelCell::Bool(b) => b.to_string().to_uppercase(),
        ExcelCell::Empty => "(blank)".to_string(),
    };
    let mut sums: BTreeMap<(String, String), f64> = BTreeMap::new();
    for r in area.row + 1..area.row + area.rows {
        let n = match value_of(sheet, r, sum_col, 0) {
            ExcelCell::Number(n) => n,
            _ => 0.0,
        };
        let key = (
            label(r, row_col),
            col_col.map_or(String::new(), |c| label(r, c)),
        );
        *sums.entry(key).or_insert(0.0) += n;
    }
    let row_keys: Vec<String> = sums
        .keys()
        .map(|k| k.0.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let col_keys: Vec<String> = sums
        .keys()
        .map(|k| k.1.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let num = ExcelCell::Number;
    let text = |t: &str| ExcelCell::Text(t.to_string());
    let caption = format!("Sum of {sum_field}");
    let mut out = Vec::new();
    if col_col.is_none() {
        out.push(vec![text("Row Labels"), text(&caption)]);
        for r in &row_keys {
            out.push(vec![text(r), num(sums[&(r.clone(), String::new())])]);
        }
        out.push(vec![text("Grand Total"), num(sums.values().sum())]);
        return out;
    }
    let mut head = vec![text(&caption), text("Column Labels")];
    head.extend((0..col_keys.len()).map(|_| ExcelCell::Empty));
    out.push(head);
    let mut labels = vec![text("Row Labels")];
    labels.extend(col_keys.iter().map(|c| text(c)));
    labels.push(text("Grand Total"));
    out.push(labels);
    for r in &row_keys {
        let mut line = vec![text(r)];
        let mut total = 0.0;
        for c in &col_keys {
            match sums.get(&(r.clone(), c.clone())) {
                Some(n) => {
                    total += n;
                    line.push(num(*n));
                }
                None => line.push(ExcelCell::Empty),
            }
        }
        line.push(num(total));
        out.push(line);
    }
    let mut last = vec![text("Grand Total")];
    for c in &col_keys {
        last.push(num(sums
            .iter()
            .filter(|(k, _)| &k.1 == c)
            .map(|(_, n)| n)
            .sum()));
    }
    last.push(num(sums.values().sum()));
    out.push(last);
    out
}
