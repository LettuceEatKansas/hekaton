//! A scriptable in-memory Excel for [`FakeBackend`](super::fake::FakeBackend), so the orchestration
//! tests of the `excel` action run without Excel. Workbooks keep cells as written; reading a formula
//! evaluates `=SUM(range)` and `=cell`, which is all the tests need. Anything else reads `#NAME?`.

use std::collections::BTreeMap;

use super::excel::{parse_area, parse_cell};
use super::{BackendError, BackendResult, ExcelCell, ExcelReply, ExcelRequest};

struct Sheet {
    name: String,
    cells: BTreeMap<(u32, u32), ExcelCell>,
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
                    sheets: vec![Sheet {
                        name: "Sheet1".into(),
                        cells: BTreeMap::new(),
                    }],
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
                book.sheets.push(Sheet {
                    name: name.clone(),
                    cells: BTreeMap::new(),
                });
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
