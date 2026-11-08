//! `A1` addresses for the Excel channel: the one place that knows how a column letter maps to a
//! number and how big a sheet is, shared by the core's checks, the fake workbook and the Windows
//! backend, so all three agree on what a range means. Pure text and arithmetic, no OS calls.

use std::borrow::Cow;

use super::ExcelCell;

/// Rows on a sheet (Excel 2007 and later).
pub const MAX_ROWS: u32 = 1_048_576;
/// Columns on a sheet.
pub const MAX_COLS: u32 = 16_384;
/// Most cells one `write` may carry. One call this big is already minutes of Excel time.
pub const WRITE_CELL_CAP: usize = 2_000_000;

/// A block of cells: 1-based top-left corner and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    /// Left column, `A` is 1.
    pub col: u32,
    /// Top row, 1 is the first.
    pub row: u32,
    /// Width in columns.
    pub cols: u32,
    /// Height in rows.
    pub rows: u32,
}

impl Area {
    /// Cells in the block.
    pub fn cells(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    /// `A1` for one cell, `A1:C5` for a block.
    pub fn address(&self) -> String {
        let first = format!("{}{}", col_name(self.col), self.row);
        if self.cols == 1 && self.rows == 1 {
            return first;
        }
        let last = format!(
            "{}{}",
            col_name(self.col + self.cols - 1),
            self.row + self.rows - 1
        );
        format!("{first}:{last}")
    }

    /// The same block cut down to its first `rows` rows.
    pub fn first_rows(&self, rows: u32) -> Self {
        Self {
            rows: rows.min(self.rows),
            ..*self
        }
    }
}

/// `1` is `A`, `27` is `AA`.
pub fn col_name(mut n: u32) -> String {
    let mut out = Vec::new();
    while n > 0 {
        let rem = (n - 1) % 26;
        out.push(b'A' + rem as u8);
        n = (n - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `(column, row)` of a cell address like `B7` or `$B$7`, `None` when it is not one or lies off the
/// sheet.
pub fn parse_cell(text: &str) -> Option<(u32, u32)> {
    let t = text.trim().replace('$', "");
    let letters = t.chars().take_while(char::is_ascii_alphabetic).count();
    let (letters, digits) = t.split_at(letters);
    if letters.is_empty() || letters.len() > 3 || digits.is_empty() {
        return None;
    }
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let col = letters
        .to_ascii_uppercase()
        .bytes()
        .try_fold(0u32, |acc, b| {
            acc.checked_mul(26)?.checked_add(u32::from(b - b'A') + 1)
        })?;
    let row: u32 = digits.parse().ok()?;
    ((1..=MAX_COLS).contains(&col) && (1..=MAX_ROWS).contains(&row)).then_some((col, row))
}

/// An address like `B7` or `A1:C5` as a block. Corners may come in either order.
pub fn parse_area(text: &str) -> Option<Area> {
    let t = text.trim();
    let (a, b) = match t.split_once(':') {
        Some((a, b)) => (parse_cell(a)?, parse_cell(b)?),
        None => {
            let one = parse_cell(t)?;
            (one, one)
        }
    };
    let (col, row) = (a.0.min(b.0), a.1.min(b.1));
    Some(Area {
        col,
        row,
        cols: a.0.max(b.0) - col + 1,
        rows: a.1.max(b.1) - row + 1,
    })
}

/// A `#RRGGBB` color as the number Excel uses for `Interior.Color` (`R + G * 256 + B * 65536`).
pub fn parse_color(text: &str) -> Option<u32> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    let (r, g, b) = (rgb >> 16 & 0xFF, rgb >> 8 & 0xFF, rgb & 0xFF);
    Some(r | g << 8 | b << 16)
}

/// The text to hand Excel so a written string is stored as the literal text it is. Excel would
/// parse `"007"` as 7 and `"1/2"` as a date, and read `-x` or `+x` as a formula; a leading
/// apostrophe is Excel's own "this is text" marker, never part of the stored string, so the saved
/// cell stays a plain string and reads back without it. A string starting with `=` is a formula and
/// goes through untouched; an empty string clears the cell as before.
pub fn entered_text(text: &str) -> Cow<'_, str> {
    if text.is_empty() || text.starts_with('=') {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("'{text}"))
    }
}

/// The header cell named `want` in a header row: trimmed, case-insensitive. Returns the header as
/// the sheet spells it. Numeric headers match their plain text (`2025`).
pub fn find_header(headers: &[ExcelCell], want: &str) -> Option<String> {
    let want = want.trim();
    headers.iter().find_map(|h| {
        let shown = match h {
            ExcelCell::Text(t) => t.clone(),
            ExcelCell::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
            ExcelCell::Number(n) => n.to_string(),
            ExcelCell::Bool(b) => b.to_string().to_uppercase(),
            ExcelCell::Empty => return None,
        };
        shown.trim().eq_ignore_ascii_case(want).then_some(shown)
    })
}

/// Where a chart of `area` goes when the caller names no cell: two columns right of the data, in
/// the data's first row; below the data when that would run off the sheet.
pub fn default_chart_anchor(area: &Area) -> String {
    let col = area.col + area.cols + 2;
    if col <= MAX_COLS {
        format!("{}{}", col_name(col), area.row)
    } else {
        let row = (area.row + area.rows + 1).min(MAX_ROWS);
        format!("{}{row}", col_name(area.col))
    }
}

/// A sheet name Excel accepts: 1 to 31 characters, none of `\ / ? * [ ] :`, not starting or ending
/// with an apostrophe.
pub fn valid_sheet_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=31).contains(&n)
        && !name.contains(['\\', '/', '?', '*', '[', ']', ':'])
        && !name.starts_with('\'')
        && !name.ends_with('\'')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_become_excel_numbers() {
        assert_eq!(parse_color("#FF0000"), Some(0x0000FF));
        assert_eq!(parse_color("#00ff00"), Some(0x00FF00));
        assert_eq!(parse_color(" #0000FF "), Some(0xFF0000));
        assert_eq!(parse_color("#123456"), Some(0x56_34_12));
        for bad in ["FF0000", "#FFF", "#GG0000", "#FF00000", "", "red"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn text_is_literal_unless_it_is_a_formula() {
        assert_eq!(entered_text("007"), "'007");
        assert_eq!(entered_text("1/2"), "'1/2");
        assert_eq!(entered_text("TRUE"), "'TRUE");
        assert_eq!(entered_text("-x"), "'-x");
        assert_eq!(entered_text("hello"), "'hello");
        assert_eq!(entered_text("=A1+1"), "=A1+1");
        assert_eq!(entered_text(""), "");
    }

    #[test]
    fn headers_match_by_trimmed_case_blind_text() {
        let row = vec![
            ExcelCell::Text("Region".into()),
            ExcelCell::Text(" Qty ".into()),
            ExcelCell::Number(2025.0),
            ExcelCell::Empty,
        ];
        assert_eq!(find_header(&row, "region").as_deref(), Some("Region"));
        assert_eq!(find_header(&row, "QTY").as_deref(), Some(" Qty "));
        assert_eq!(find_header(&row, "2025").as_deref(), Some("2025"));
        assert_eq!(find_header(&row, "Price"), None);
        assert_eq!(find_header(&row, ""), None);
    }

    #[test]
    fn a_chart_defaults_to_the_right_of_its_data() {
        let area = parse_area("D1:G201").unwrap();
        assert_eq!(default_chart_anchor(&area), "J1");
        let edge = parse_area("XFA1:XFC9").unwrap();
        assert_eq!(default_chart_anchor(&edge), "XFA11");
    }

    #[test]
    fn sheet_names_follow_excels_rules() {
        assert!(valid_sheet_name("Pivot"));
        assert!(valid_sheet_name("My Sheet 2"));
        assert!(!valid_sheet_name(""));
        assert!(!valid_sheet_name(&"x".repeat(32)));
        for bad in ["a/b", "a:b", "a[b", "a?b", "a*b", "a\\b", "'a", "a'"] {
            assert!(!valid_sheet_name(bad), "{bad}");
        }
    }

    #[test]
    fn column_names_round_trip() {
        assert_eq!(col_name(1), "A");
        assert_eq!(col_name(26), "Z");
        assert_eq!(col_name(27), "AA");
        assert_eq!(col_name(MAX_COLS), "XFD");
        for n in [1, 26, 27, 52, 702, 703, MAX_COLS] {
            assert_eq!(parse_cell(&format!("{}1", col_name(n))), Some((n, 1)));
        }
    }

    #[test]
    fn cells_and_blocks_parse() {
        assert_eq!(parse_cell("b7"), Some((2, 7)));
        assert_eq!(parse_cell("$B$7"), Some((2, 7)));
        assert_eq!(parse_cell("XFE1"), None);
        assert_eq!(parse_cell("A1048577"), None);
        assert_eq!(parse_cell("A0"), None);
        assert_eq!(parse_cell("7"), None);
        assert_eq!(parse_cell("A"), None);
        assert_eq!(parse_cell("A1B"), None);
        let a = parse_area("C5:A1").unwrap();
        assert_eq!((a.col, a.row, a.cols, a.rows), (1, 1, 3, 5));
        assert_eq!(a.cells(), 15);
        assert_eq!(a.address(), "A1:C5");
        assert_eq!(parse_area("D4").unwrap().address(), "D4");
        assert_eq!(
            parse_area("A1:C5").unwrap().first_rows(2).address(),
            "A1:C2"
        );
        assert_eq!(parse_area("Sheet1!A1"), None);
        assert_eq!(parse_area("A:A"), None);
    }
}
