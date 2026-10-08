//! The routes older than the `computer` tool: `click`, `type`, `key`, `scroll`, `drag`, `hover` and
//! `open` (Python `daemon.ACTION_HANDLERS` over `core.do_*`). The `wh` command line still sends them,
//! so the Rust daemon answers them by turning each into a `computer` call.
//!
//! What maps cleanly is mapped; what does not is refused with a message that says so, never guessed:
//!
//! | Python route | Rust adapter |
//! |---|---|
//! | `click` target `x,y` (screen pixels) | `left_click` / `right_click` / `middle_click` / `double_click`, aimed on the monitor holding the point at 1:1 scale |
//! | `click` target `text:Name` | `click_text` (OCR/UIA text search; Python matched UIA element names from a snapshot) |
//! | `type` text, `enter`, `clear` | `key` (select all, delete), `type`, `key Return` in one batch |
//! | `key` combo, `repeat` | `key` |
//! | `scroll` target `x,y` or none, direction, amount | `scroll` |
//! | `hover` target `x,y` | `mouse_move` |
//! | `drag` `x,y` to `x,y` on one monitor | `left_click_drag` |
//! | `open` app | `open_app` (its `wait`, `new` and `reuse` arguments are fixed inside the action) |
//!
//! Not reproduced: element-number and `t`-ref targets (they lived in the old snapshot caches),
//! `window` on any input route (the old routes focused that window first), `exact`, `capture_after`'s
//! region and scale options, `report` deltas, and `screenshot`'s `window`, `region`, `scale` and
//! `no_focus` (the route returns the whole display as a PNG). Each refusal names the `computer` tool as
//! the way to do it.

use serde_json::{Map, Value, json};

use crate::backend::{Display, Rect};
use crate::config::{to_i64, truthy};
use crate::keys::Platform;
use crate::scale::{SHOT_LONG_MAX, sort_displays};

/// Routes that only read, so they work while paused and before acknowledgement (`READ_ONLY_ACTIONS`).
pub(crate) const READ_ONLY_ROUTES: [&str; 3] = ["windows", "snapshot", "screenshot"];

/// The routes this module turns into `computer` calls.
pub(crate) const INPUT_ROUTES: [&str; 7] =
    ["click", "type", "key", "scroll", "drag", "hover", "open"];

/// Is `route` one of the input routes handled here?
pub(crate) fn is_input_route(route: &str) -> bool {
    INPUT_ROUTES.contains(&route)
}

fn refuse_window(route: &str) -> String {
    format!(
        "the {route} route no longer takes window=: it cannot focus a window for you. \
         Use the computer tool (open_app or a click to focus it, then act)"
    )
}

fn missing(key: &str) -> String {
    // Python: a KeyError became "missing required argument 'key'"
    format!("missing required argument '{key}'")
}

fn text_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn flag(args: &Map<String, Value>, key: &str) -> bool {
    args.get(key).is_some_and(truthy)
}

fn int_arg(args: &Map<String, Value>, key: &str, default: i64) -> Result<i64, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => to_i64(v).ok_or_else(|| format!("bad argument {key}")),
    }
}

/// `x,y` as `core._parse_coords` reads it: two comma separated numbers, floats truncated.
fn parse_coords(s: &str) -> Option<(i32, i32)> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 2 {
        return None;
    }
    let x: f64 = parts[0].trim().parse().ok()?;
    let y: f64 = parts[1].trim().parse().ok()?;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    Some((x as i32, y as i32))
}

/// What a `target` names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Point(i32, i32),
    Text(String),
}

fn parse_target(raw: &str) -> Result<Target, String> {
    let t = raw.trim();
    if let Some((x, y)) = parse_coords(t) {
        return Ok(Target::Point(x, y));
    }
    if t.len() >= 5 && t[..5].eq_ignore_ascii_case("text:") {
        let name = t[5..].trim();
        if name.is_empty() {
            return Err("text target must not be empty".into());
        }
        return Ok(Target::Text(name.to_string()));
    }
    let digits = t.strip_prefix('-').unwrap_or(t);
    let numeric = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
    let tref =
        t.len() > 1 && t.starts_with(['t', 'T']) && t[1..].chars().all(|c| c.is_ascii_digit());
    if numeric || tref {
        return Err(format!(
            "target {raw:?} is an element or text number from an old snapshot, which this daemon \
             does not keep. Use 'x,y' or 'text:Name', or the computer tool (read, click_text)"
        ));
    }
    Err(format!(
        "unrecognized target: {raw:?}. Use 'x,y' or 'text:Name', or the computer tool"
    ))
}

/// The monitor holding a screen point: its 1-based number in the order the `display` argument counts
/// (primary first, then by left edge) and its rect.
fn monitor_of(displays: &[Display], x: i32, y: i32) -> Result<(usize, Rect), String> {
    let mut list = displays.to_vec();
    sort_displays(&mut list);
    list.iter()
        .enumerate()
        .find(|(_, d)| d.rect.contains(crate::backend::Point::new(x, y)))
        .map(|(i, d)| (i + 1, d.rect))
        .ok_or_else(|| format!("point {x},{y} is outside every monitor"))
}

/// A screen point as the `computer` tool wants it: relative to its monitor at 1:1 scale.
struct Aim {
    display: usize,
    long: i64,
    rel: [i32; 2],
}

fn aim(displays: &[Display], x: i32, y: i32) -> Result<Aim, String> {
    let (display, rect) = monitor_of(displays, x, y)?;
    let long = i64::from(rect.width().max(rect.height()));
    if long > SHOT_LONG_MAX {
        return Err(format!(
            "the monitor is {long} px on its long side, more than the {SHOT_LONG_MAX} px a 1:1 \
             legacy click can address. Use the computer tool"
        ));
    }
    Ok(Aim {
        display,
        long,
        rel: [x - rect.left, y - rect.top],
    })
}

fn base_payload(args: &Map<String, Value>) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert("coords".into(), Value::from("pixels"));
    // the old routes answered with text; a picture only when they were asked for one
    p.insert("shot".into(), Value::from(flag(args, "capture_after")));
    if flag(args, "allow_destructive") {
        p.insert("allow_destructive".into(), Value::Bool(true));
    }
    p
}

fn put_aim(p: &mut Map<String, Value>, a: &Aim) {
    p.insert("display".into(), Value::from(a.display));
    p.insert("shot_long".into(), Value::from(a.long));
}

fn select_all() -> &'static str {
    match Platform::current() {
        Platform::Windows => "ctrl+a",
        Platform::Mac => "cmd+a",
    }
}

/// The `computer` payload that does what the legacy route `route` did with `args`, or the reason it
/// cannot. `displays` is the monitor list (any order) for turning screen pixels into the tool's
/// per-monitor coordinates.
pub(crate) fn to_computer(
    route: &str,
    args: &Map<String, Value>,
    displays: &[Display],
) -> Result<Value, String> {
    let mut p = base_payload(args);
    match route {
        "click" => {
            if text_arg(args, "window").is_some_and(|w| !w.is_empty()) {
                return Err(refuse_window(route));
            }
            let target = text_arg(args, "target").ok_or("no target given")?;
            let button = text_arg(args, "button").unwrap_or("left");
            if !["left", "right", "middle"].contains(&button) {
                return Err("button must be left, right or middle".into());
            }
            let double = flag(args, "double");
            match parse_target(target)? {
                Target::Point(x, y) => {
                    let action = match (button, double) {
                        ("left", false) => "left_click",
                        ("left", true) => "double_click",
                        ("right", false) => "right_click",
                        ("middle", false) => "middle_click",
                        _ => return Err("double click works with the left button only".into()),
                    };
                    let a = aim(displays, x, y)?;
                    p.insert("action".into(), Value::from(action));
                    p.insert("coordinate".into(), json!(a.rel));
                    put_aim(&mut p, &a);
                }
                Target::Text(name) => {
                    p.insert("action".into(), Value::from("click_text"));
                    p.insert("text".into(), Value::from(name));
                    if button != "left" {
                        p.insert("button".into(), Value::from(button));
                    }
                    if double {
                        p.insert("double".into(), Value::Bool(true));
                    }
                }
            }
        }
        "hover" => {
            if text_arg(args, "window").is_some_and(|w| !w.is_empty()) {
                return Err(refuse_window(route));
            }
            let target = text_arg(args, "target").ok_or("no target given")?;
            let Target::Point(x, y) = parse_target(target)? else {
                return Err("hover takes an 'x,y' target; use the computer tool for text".into());
            };
            let a = aim(displays, x, y)?;
            p.insert("action".into(), Value::from("mouse_move"));
            p.insert("coordinate".into(), json!(a.rel));
            put_aim(&mut p, &a);
        }
        "scroll" => {
            if text_arg(args, "window").is_some_and(|w| !w.is_empty()) {
                return Err(refuse_window(route));
            }
            let direction = text_arg(args, "direction").ok_or_else(|| missing("direction"))?;
            let amount = int_arg(args, "amount", 3)?;
            if !["up", "down", "left", "right"].contains(&direction) || amount < 1 {
                return Err("scroll requires valid direction and positive amount".into());
            }
            p.insert("action".into(), Value::from("scroll"));
            p.insert("scroll_direction".into(), Value::from(direction));
            p.insert("scroll_amount".into(), Value::from(amount));
            if let Some(target) = text_arg(args, "target") {
                let Target::Point(x, y) = parse_target(target)? else {
                    return Err(
                        "scroll takes an 'x,y' target; use the computer tool for text".into(),
                    );
                };
                let a = aim(displays, x, y)?;
                p.insert("coordinate".into(), json!(a.rel));
                put_aim(&mut p, &a);
            }
        }
        "drag" => {
            if text_arg(args, "window").is_some_and(|w| !w.is_empty()) {
                return Err(refuse_window(route));
            }
            let from = text_arg(args, "from").ok_or_else(|| missing("from"))?;
            let to = text_arg(args, "to").ok_or_else(|| missing("to"))?;
            let (Target::Point(x1, y1), Target::Point(x2, y2)) =
                (parse_target(from)?, parse_target(to)?)
            else {
                return Err("drag takes 'x,y' endpoints; use the computer tool for text".into());
            };
            let (a, b) = (aim(displays, x1, y1)?, aim(displays, x2, y2)?);
            if a.display != b.display {
                return Err("drag endpoints must be on the same monitor".into());
            }
            p.insert("action".into(), Value::from("left_click_drag"));
            p.insert("start_coordinate".into(), json!(a.rel));
            p.insert("coordinate".into(), json!(b.rel));
            put_aim(&mut p, &a);
        }
        "key" => {
            if text_arg(args, "window").is_some_and(|w| !w.is_empty()) {
                return Err(refuse_window(route));
            }
            let combo = text_arg(args, "combo").ok_or_else(|| missing("combo"))?;
            p.insert("action".into(), Value::from("key"));
            p.insert("text".into(), Value::from(combo));
            p.insert("repeat".into(), Value::from(int_arg(args, "repeat", 1)?));
        }
        "type" => {
            if text_arg(args, "window").is_some_and(|w| !w.is_empty()) {
                return Err(refuse_window(route));
            }
            if args.get("target").is_some_and(|t| !t.is_null()) {
                return Err(
                    "type no longer takes target=: click the field first (click route or the \
                     computer tool), then type"
                        .into(),
                );
            }
            let text = text_arg(args, "text").ok_or_else(|| missing("text"))?;
            let mut steps: Vec<Value> = Vec::new();
            if flag(args, "clear") {
                steps.push(json!({"action": "key", "text": select_all()}));
                steps.push(json!({"action": "key", "text": "delete"}));
            }
            steps.push(json!({"action": "type", "text": text}));
            if flag(args, "enter") {
                steps.push(json!({"action": "key", "text": "Return"}));
            }
            p.insert("actions".into(), Value::Array(steps));
        }
        "open" => {
            let app = text_arg(args, "app").ok_or_else(|| missing("app"))?;
            p.insert("action".into(), Value::from("open_app"));
            p.insert("text".into(), Value::from(app));
        }
        other => return Err(format!("unknown action: {other}")),
    }
    Ok(Value::Object(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disp(id: u32, x: i32, y: i32, w: i32, h: i32, primary: bool) -> Display {
        Display {
            id,
            name: format!("d{id}"),
            rect: Rect::from_xywh(x, y, w, h),
            scale: 1.0,
            primary,
        }
    }

    fn two() -> Vec<Display> {
        // listed secondary first on purpose: the numbering follows primary first, then left edge
        vec![
            disp(2, 1920, 0, 1280, 1024, false),
            disp(1, 0, 0, 1920, 1080, true),
        ]
    }

    fn map(route: &str, v: Value) -> Result<Value, String> {
        to_computer(route, v.as_object().unwrap(), &two())
    }

    #[test]
    fn a_point_click_is_aimed_at_one_to_one_on_its_own_monitor() {
        let p = map("click", json!({"target": "100,200"})).unwrap();
        assert_eq!(p["action"], "left_click");
        assert_eq!(p["coordinate"], json!([100, 200]));
        assert_eq!(p["display"], 1);
        assert_eq!(p["shot_long"], 1920);
        assert_eq!(p["shot"], false);
        assert_eq!(p["coords"], "pixels");
        let p = map("click", json!({"target": "2000,10", "button": "right"})).unwrap();
        assert_eq!(p["action"], "right_click");
        assert_eq!(p["coordinate"], json!([80, 10]));
        assert_eq!(p["display"], 2);
        assert_eq!(p["shot_long"], 1280);
    }

    #[test]
    fn click_variants_and_text_targets() {
        let p = map("click", json!({"target": "5.9, 6.2", "double": true})).unwrap();
        assert_eq!(p["action"], "double_click");
        assert_eq!(p["coordinate"], json!([5, 6]));
        let p = map(
            "click",
            json!({"target": "text:Save", "button": "right", "double": true}),
        )
        .unwrap();
        assert_eq!(p["action"], "click_text");
        assert_eq!(p["text"], "Save");
        assert_eq!(p["button"], "right");
        assert_eq!(p["double"], true);
        let p = map(
            "click",
            json!({"target": "TEXT: Save as ", "capture_after": true}),
        )
        .unwrap();
        assert_eq!(p["text"], "Save as");
        assert_eq!(p["shot"], true);
    }

    #[test]
    fn what_cannot_be_reproduced_is_refused_with_the_way_out() {
        for target in ["12", "-3", "t4", "T4"] {
            let err = map("click", json!({"target": target})).unwrap_err();
            assert!(
                err.contains("old snapshot") && err.contains("computer tool"),
                "{err}"
            );
        }
        assert!(
            map("click", json!({"target": "w3"}))
                .unwrap_err()
                .contains("unrecognized")
        );
        assert!(
            map("click", json!({"target": "text:"}))
                .unwrap_err()
                .contains("empty")
        );
        assert!(map("click", json!({})).unwrap_err().contains("no target"));
        let err = map("click", json!({"target": "1,2", "window": "w3"})).unwrap_err();
        assert!(
            err.contains("window=") && err.contains("computer tool"),
            "{err}"
        );
        assert!(
            map("click", json!({"target": "9999,9999"}))
                .unwrap_err()
                .contains("outside")
        );
        assert!(map("click", json!({"target": "1,2", "button": "left-ish"})).is_err());
        assert!(
            map(
                "click",
                json!({"target": "1,2", "button": "right", "double": true})
            )
            .is_err()
        );
        assert!(map("hover", json!({"target": "text:x"})).is_err());
    }

    #[test]
    fn hover_scroll_and_drag() {
        let p = map("hover", json!({"target": "30,40"})).unwrap();
        assert_eq!(
            (p["action"].as_str(), &p["coordinate"]),
            (Some("mouse_move"), &json!([30, 40]))
        );
        let p = map("scroll", json!({"direction": "up", "amount": 5})).unwrap();
        assert_eq!(p["action"], "scroll");
        assert_eq!(p["scroll_direction"], "up");
        assert_eq!(p["scroll_amount"], 5);
        assert!(
            p.get("coordinate").is_none(),
            "no target scrolls at the pointer"
        );
        let p = map("scroll", json!({"direction": "down", "target": "10,10"})).unwrap();
        assert_eq!(p["scroll_amount"], 3);
        assert_eq!(p["coordinate"], json!([10, 10]));
        assert!(map("scroll", json!({"direction": "sideways"})).is_err());
        assert!(map("scroll", json!({"direction": "up", "amount": 0})).is_err());
        assert_eq!(
            map("scroll", json!({})).unwrap_err(),
            "missing required argument 'direction'"
        );
        let p = map("drag", json!({"from": "10,10", "to": "300,200"})).unwrap();
        assert_eq!(p["action"], "left_click_drag");
        assert_eq!(p["start_coordinate"], json!([10, 10]));
        assert_eq!(p["coordinate"], json!([300, 200]));
        let err = map("drag", json!({"from": "10,10", "to": "2000,10"})).unwrap_err();
        assert!(err.contains("same monitor"), "{err}");
        assert_eq!(
            map("drag", json!({"from": "1,1"})).unwrap_err(),
            "missing required argument 'to'"
        );
    }

    #[test]
    fn key_type_and_open() {
        let p = map("key", json!({"combo": "ctrl+s", "repeat": 2})).unwrap();
        assert_eq!(p["action"], "key");
        assert_eq!(p["text"], "ctrl+s");
        assert_eq!(p["repeat"], 2);
        assert_eq!(
            map("key", json!({})).unwrap_err(),
            "missing required argument 'combo'"
        );
        let p = map("type", json!({"text": "hi"})).unwrap();
        assert_eq!(p["actions"], json!([{"action": "type", "text": "hi"}]));
        let p = map("type", json!({"text": "hi", "clear": true, "enter": true})).unwrap();
        let steps = p["actions"].as_array().unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[1], json!({"action": "key", "text": "delete"}));
        assert_eq!(steps[3], json!({"action": "key", "text": "Return"}));
        assert_eq!(
            map("type", json!({})).unwrap_err(),
            "missing required argument 'text'"
        );
        assert!(
            map("type", json!({"text": "x", "target": "1,2"}))
                .unwrap_err()
                .contains("target=")
        );
        let p = map("open", json!({"app": "notepad", "allow_destructive": true})).unwrap();
        assert_eq!(
            (p["action"].as_str(), p["text"].as_str()),
            (Some("open_app"), Some("notepad"))
        );
        assert_eq!(p["allow_destructive"], true);
        assert!(
            map("nope", json!({}))
                .unwrap_err()
                .contains("unknown action")
        );
    }

    #[test]
    fn typed_text_stays_exactly_what_was_typed() {
        let p = map("type", json!({"text": "  007 yes  "})).unwrap();
        assert_eq!(p["actions"][0]["text"], "  007 yes  ");
    }
}
