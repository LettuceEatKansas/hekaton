//! Reading a step's arguments the way the Python build did: JSON off the wire, loose types, no
//! schema. A model may send a number where text belongs or the string "false" for a flag, so every
//! reader follows Python's own coercions (`str(x)`, `int(x or default)`, `_flag`).

use serde_json::{Map, Value};

use super::error::{StepError, StepResult};
use crate::config::truthy;
use crate::textutil::{py_repr, py_str};

/// One step (or the whole call) as a JSON object.
pub type Args = Map<String, Value>;

/// `args.get(key)` when it is not `None`.
pub fn get<'a>(args: &'a Args, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
}

/// `args.get(key) or ""` as text: a falsy value is empty text, anything else is `str(value)`.
pub fn text(args: &Args, key: &str) -> String {
    match args.get(key) {
        Some(v) if truthy(v) => py_str(v),
        _ => String::new(),
    }
}

/// Python truthiness of `args.get(key)`.
pub fn is_truthy(args: &Args, key: &str) -> bool {
    args.get(key).is_some_and(truthy)
}

/// `_flag(value, default)`: real bools, or the strings a model may send instead.
pub fn flag(value: Option<&Value>, default: bool) -> bool {
    match value {
        None | Some(Value::Null) => default,
        Some(Value::String(s)) => matches!(
            s.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Some(v) => truthy(v),
    }
}

/// `int(x)` with Python's error text.
pub fn to_int(v: &Value) -> StepResult<i64> {
    match v {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(|| StepError::value("cannot convert the number to an int")),
        Value::String(s) => s.trim().parse::<i64>().map_err(|_| {
            StepError::value(format!(
                "invalid literal for int() with base 10: {}",
                py_repr(s)
            ))
        }),
        other => Err(StepError::value(format!(
            "int() argument must be a string or a real number, not '{}'",
            type_name(other)
        ))),
    }
}

/// `float(x)` with Python's error text.
pub fn to_float(v: &Value) -> StepResult<f64> {
    match v {
        Value::Bool(b) => Ok(f64::from(u8::from(*b))),
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| StepError::value("cannot convert the number to a float")),
        Value::String(s) => s.trim().parse::<f64>().map_err(|_| {
            StepError::value(format!("could not convert string to float: {}", py_repr(s)))
        }),
        other => Err(StepError::value(format!(
            "float() argument must be a string or a real number, not '{}'",
            type_name(other)
        ))),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `int(args.get(key) or default)`.
pub fn int_or(args: &Args, key: &str, default: i64) -> StepResult<i64> {
    match args.get(key) {
        Some(v) if truthy(v) => to_int(v),
        _ => Ok(default),
    }
}

/// `float(args.get(key) or default)`.
pub fn float_or(args: &Args, key: &str, default: f64) -> StepResult<f64> {
    match args.get(key) {
        Some(v) if truthy(v) => to_float(v),
        _ => Ok(default),
    }
}

/// A coordinate box read as four numbers, or `None` unless it is a list of exactly four.
pub fn four_numbers(v: &Value) -> Option<StepResult<[f64; 4]>> {
    let Value::Array(items) = v else {
        return None;
    };
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (slot, item) in out.iter_mut().zip(items) {
        match to_float(item) {
            Ok(f) => *slot = f,
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn flags_accept_the_spellings_a_model_sends() {
        for v in [
            json!(false),
            json!("false"),
            json!("0"),
            json!("no"),
            json!("off"),
            json!(0),
        ] {
            assert!(!flag(Some(&v), true), "{v}");
        }
        for v in [
            json!(true),
            json!("true"),
            json!("1"),
            json!("YES"),
            json!(" on "),
        ] {
            assert!(flag(Some(&v), false), "{v}");
        }
        assert!(flag(None, true) && !flag(None, false) && flag(Some(&Value::Null), true));
    }

    #[test]
    fn text_is_empty_for_falsy_values_and_str_otherwise() {
        let a = args(json!({"a": "x", "b": 10, "c": null, "d": 0, "e": ""}));
        assert_eq!(text(&a, "a"), "x");
        assert_eq!(text(&a, "b"), "10");
        assert_eq!(text(&a, "c"), "");
        assert_eq!(text(&a, "d"), "");
        assert_eq!(text(&a, "e"), "");
        assert_eq!(text(&a, "missing"), "");
    }

    #[test]
    fn numbers_coerce_like_python() {
        let a = args(json!({"n": "7", "f": 2.9, "z": 0, "bad": "x"}));
        assert_eq!(int_or(&a, "n", 3).unwrap(), 7);
        assert_eq!(int_or(&a, "f", 3).unwrap(), 2);
        assert_eq!(int_or(&a, "z", 3).unwrap(), 3);
        assert_eq!(int_or(&a, "missing", 3).unwrap(), 3);
        let err = int_or(&a, "bad", 3).unwrap_err();
        assert_eq!(err.message(), "invalid literal for int() with base 10: 'x'");
        assert_eq!(float_or(&a, "n", 1.0).unwrap(), 7.0);
        assert_eq!(
            float_or(&a, "bad", 1.0).unwrap_err().message(),
            "could not convert string to float: 'x'"
        );
    }

    #[test]
    fn four_numbers_wants_a_list_of_four() {
        assert_eq!(
            four_numbers(&json!([1, 2, 3, 4])).unwrap().unwrap(),
            [1.0, 2.0, 3.0, 4.0]
        );
        assert!(four_numbers(&json!([1, 2, 3])).is_none());
        assert!(four_numbers(&json!("1,2,3,4")).is_none());
        assert!(four_numbers(&json!([1, 2, "x", 4])).unwrap().is_err());
    }
}
