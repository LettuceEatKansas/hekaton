//! `A1` addresses for the Excel channel: the one place that knows how a column letter maps to a
//! number and how big a sheet is, shared by the core's checks, the fake workbook and the Windows
//! backend, so all three agree on what a range means. Pure text and arithmetic, no OS calls.

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

#[cfg(test)]
mod tests {
    use super::*;

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
