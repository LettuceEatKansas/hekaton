//! Small text helpers that keep messages byte-identical to the Python build.

use serde_json::Value;

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

/// Python's `repr()` of a JSON value as `json.loads` would have produced it: `None`, `True`,
/// `[10, 10]`, `{'a': 1}`, `'text'`. Used where the Python build interpolated the raw argument
/// into a result line (`dragged [10, 10] -> [600, 10]`).
pub fn py_value_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                py_float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => py_repr(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_value_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr(k), py_value_repr(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// Python `str(x)` of a JSON value: strings as they are, everything else as its repr.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_value_repr(other),
    }
}

/// Python's `repr()` of a float: the shortest text that reads back, always with a `.0` or an
/// exponent so it cannot be mistaken for an int.
pub fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    let s = format!("{f:?}");
    if let Some((mant, exp)) = s.split_once('e') {
        let n: i32 = exp.parse().unwrap_or(0);
        return format!("{mant}e{}{:02}", if n < 0 { '-' } else { '+' }, n.abs());
    }
    s
}

/// Python's `format(x, "g")`: six significant digits, trailing zeros dropped, an exponent for very
/// large or small values. `1` for `1.0`, `0.5`, `2.5`, `30`.
pub fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    if !x.is_finite() {
        return py_float(x);
    }
    let exp = x.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let s = format!("{x:.5e}");
        let (mant, e) = s.split_once('e').unwrap_or((&s, "0"));
        let mant = trim_zeros(mant);
        let n: i32 = e.parse().unwrap_or(0);
        return format!("{mant}e{}{:02}", if n < 0 { '-' } else { '+' }, n.abs());
    }
    let decimals = (5 - exp).max(0) as usize;
    trim_zeros(&format!("{x:.decimals$}"))
}

fn trim_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_values_print_like_python_objects() {
        assert_eq!(py_value_repr(&json!([10, 10])), "[10, 10]");
        assert_eq!(py_value_repr(&json!([10.5, 2.0])), "[10.5, 2.0]");
        assert_eq!(py_value_repr(&json!(null)), "None");
        assert_eq!(py_value_repr(&json!(true)), "True");
        assert_eq!(
            py_value_repr(&json!({"a": "x", "b": [1]})),
            "{'a': 'x', 'b': [1]}"
        );
        assert_eq!(py_str(&json!("plain")), "plain");
        assert_eq!(py_str(&json!(7)), "7");
    }

    #[test]
    fn g_format_matches_python() {
        assert_eq!(fmt_g(1.0), "1");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(2.5), "2.5");
        assert_eq!(fmt_g(30.0), "30");
        assert_eq!(fmt_g(0.01), "0.01");
        assert_eq!(fmt_g(120.0), "120");
        assert_eq!(fmt_g(0.1234567), "0.123457");
        assert_eq!(fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(fmt_g(0.00001), "1e-05");
        assert_eq!(fmt_g(0.0), "0");
    }

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
