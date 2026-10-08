//! Small text helpers that keep messages byte-identical to the Python build.

/// Python's `repr()` of a `str`: single quotes unless the text contains a single quote and no
/// double quote; backslash, the quote, `\n`, `\r`, `\t` escaped; other control characters as
/// `\xNN`. Printable non-ASCII is kept, as Python 3 does.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("ctrl+a"), "'ctrl+a'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("it's \"x\""), "'it\\'s \"x\"'");
        assert_eq!(py_repr("a\\b\n\t"), "'a\\\\b\\n\\t'");
        assert_eq!(py_repr("\u{1}"), "'\\x01'");
        assert_eq!(py_repr("caf\u{e9}"), "'caf\u{e9}'");
        assert_eq!(py_repr(""), "''");
    }
}
