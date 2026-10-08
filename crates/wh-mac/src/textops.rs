//! Text and input arithmetic with no OS calls: how to cut text into key events, line and match
//! finding in UTF-16 coordinates (what `AXValue` ranges count in), click chaining, scroll steps and
//! drag paths. Tested on every OS.

use std::time::Duration;

use wh_core::backend::{MouseButton, Point, ScrollDirection};

use crate::keymap::ascii_key;
use crate::ocrgeom::collapse_ws;

/// Most UTF-16 units sent in one synthetic key event. Some apps drop longer unicode strings.
pub(crate) const TYPE_CHUNK_UNITS: usize = 20;
/// Text longer than this many UTF-16 units goes in through the clipboard instead.
pub(crate) const PASTE_ABOVE_UNITS: usize = 400;
/// Pixels one wheel notch scrolls.
pub(crate) const PIXELS_PER_NOTCH: i32 = 60;
/// Longest line text a document hit carries.
pub(crate) const MAX_HIT_LINE_CHARS: usize = 400;

/// Drop the invisible direction marks some apps put in front of numbers and labels (Calculator
/// prefixes its display with U+200E), which no caller wants in a name.
pub(crate) fn strip_bidi(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(
                c,
                '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
            )
        })
        .collect()
}

// ------------------------------------------------------------------------------------ typing

/// One step of typing a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypeStep {
    /// A run of UTF-16 units sent as one unicode key event.
    Units(Vec<u16>),
    /// The Return key (a newline in the text).
    Return,
    /// The Tab key.
    Tab,
    /// An ASCII character as the real US-layout key (`code`, with Shift if `shift`) carrying the
    /// character as its unicode string too. Apps that read the key code (Calculator) and apps
    /// that read the string (text fields) both get what they look for.
    Key {
        /// Virtual keycode.
        code: u16,
        /// Shift is part of the character.
        shift: bool,
        /// The character itself.
        unit: u16,
    },
}

/// Cut `text` into key events: ASCII as real keys, everything else in unicode runs of at most
/// `max_units` (never splitting a surrogate pair), with newlines and tabs as real keys so they act
/// the way the keys do.
pub(crate) fn plan_typing(text: &str, max_units: usize) -> Vec<TypeStep> {
    let max_units = max_units.max(2);
    let mut steps = Vec::new();
    let mut run: Vec<u16> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' | '\n' => {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                flush(&mut steps, &mut run);
                steps.push(TypeStep::Return);
            }
            '\t' => {
                flush(&mut steps, &mut run);
                steps.push(TypeStep::Tab);
            }
            c if ascii_key(c).is_some() => {
                flush(&mut steps, &mut run);
                let (code, shift) = ascii_key(c).unwrap_or((0, false));
                steps.push(TypeStep::Key {
                    code,
                    shift,
                    unit: c as u16,
                });
            }
            _ => {
                if run.len() + ch.len_utf16() > max_units {
                    flush(&mut steps, &mut run);
                }
                let mut buf = [0u16; 2];
                run.extend_from_slice(ch.encode_utf16(&mut buf));
            }
        }
    }
    flush(&mut steps, &mut run);
    steps
}

fn flush(steps: &mut Vec<TypeStep>, run: &mut Vec<u16>) {
    if !run.is_empty() {
        steps.push(TypeStep::Units(std::mem::take(run)));
    }
}

/// Length in UTF-16 units.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

// ------------------------------------------------------------------------------------ lines and search

/// The line holding UTF-16 offset `idx`, as `(start, end)` with `end` before the newline.
pub(crate) fn line_range(units: &[u16], idx: usize) -> (usize, usize) {
    let idx = idx.min(units.len());
    let is_break = |u: &u16| *u == 0x0A || *u == 0x0D;
    let start = units[..idx].iter().rposition(is_break).map_or(0, |p| p + 1);
    let end = units[idx..]
        .iter()
        .position(is_break)
        .map_or(units.len(), |p| idx + p);
    (start, end)
}

/// Text of `units[start..end]`, clamped.
pub(crate) fn slice16(units: &[u16], start: usize, end: usize) -> String {
    let end = end.min(units.len());
    let start = start.min(end);
    String::from_utf16_lossy(&units[start..end])
}

/// A found occurrence, all offsets in UTF-16 units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextMatch {
    /// Where the match starts.
    pub(crate) start: usize,
    /// How long the match is.
    pub(crate) len: usize,
    /// Start of the line holding the match.
    pub(crate) line_start: usize,
    /// End of that line.
    pub(crate) line_end: usize,
    /// The line, whitespace collapsed and capped.
    pub(crate) line: String,
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Case-insensitive, non-overlapping matches of `query` in `text`, up to `max_hits`.
pub(crate) fn find_matches(text: &str, query: &str, max_hits: usize) -> Vec<TextMatch> {
    let q: Vec<char> = query.chars().map(fold).collect();
    if q.is_empty() || max_hits == 0 {
        return Vec::new();
    }
    let mut chars: Vec<(char, usize)> = Vec::new();
    let mut at = 0usize;
    for c in text.chars() {
        chars.push((fold(c), at));
        at += c.len_utf16();
    }
    let total16 = at;
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + q.len() <= chars.len() && out.len() < max_hits {
        if chars[i..i + q.len()]
            .iter()
            .map(|(c, _)| *c)
            .eq(q.iter().copied())
        {
            let start = chars[i].1;
            let end = chars.get(i + q.len()).map_or(total16, |(_, o)| *o);
            let (line_start, line_end) = line_range(&units, start);
            let line: String = collapse_ws(&slice16(&units, line_start, line_end))
                .chars()
                .take(MAX_HIT_LINE_CHARS)
                .collect();
            out.push(TextMatch {
                start,
                len: end - start,
                line_start,
                line_end,
                line,
            });
            i += q.len();
        } else {
            i += 1;
        }
    }
    out
}

// ------------------------------------------------------------------------------------ clicks

/// Counts clicks the way the OS does: a click on the same spot with the same button soon after
/// the last one is the next click of a double or triple click. The core makes a triple click out
/// of three single-click calls, so the chain has to live in the backend.
#[derive(Debug, Default)]
pub(crate) struct ClickChain {
    last: Option<LastClick>,
}

#[derive(Debug, Clone, Copy)]
struct LastClick {
    at: Duration,
    pos: Point,
    button: MouseButton,
    count: i64,
}

/// Longest gap between clicks of one chain.
pub(crate) const CHAIN_WINDOW: Duration = Duration::from_millis(450);
/// Farthest the pointer may drift, in points, between clicks of one chain.
pub(crate) const CHAIN_SLOP: i32 = 4;

impl ClickChain {
    /// The `kCGMouseEventClickState` for a click at `pos` at monotonic time `now`: 1, 2, or 3.
    pub(crate) fn next_state(&mut self, now: Duration, pos: Point, button: MouseButton) -> i64 {
        let count = match self.last {
            Some(l)
                if l.button == button
                    && now.saturating_sub(l.at) <= CHAIN_WINDOW
                    && (l.pos.x - pos.x).abs() <= CHAIN_SLOP
                    && (l.pos.y - pos.y).abs() <= CHAIN_SLOP =>
            {
                (l.count + 1).min(3)
            }
            _ => 1,
        };
        self.last = Some(LastClick {
            at: now,
            pos,
            button,
            count,
        });
        count
    }

    /// Forget the chain (the pointer was moved elsewhere on purpose).
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }
}

// ------------------------------------------------------------------------------------ scroll and drag

/// The wheel deltas `(vertical, horizontal)` for one notch. Positive moves content down or right
/// (scroll up, scroll left), matching `ScrollDirection`'s documentation.
pub(crate) fn scroll_step(direction: ScrollDirection) -> (i32, i32) {
    match direction {
        ScrollDirection::Up => (PIXELS_PER_NOTCH, 0),
        ScrollDirection::Down => (-PIXELS_PER_NOTCH, 0),
        ScrollDirection::Left => (0, PIXELS_PER_NOTCH),
        ScrollDirection::Right => (0, -PIXELS_PER_NOTCH),
    }
}

/// How many moves a drag makes on the way from press to release (Python `core.drag`, 2026-10-08).
pub(crate) const DRAG_STEPS: usize = 12;
/// After the press, before the first move: lets the app register the button going down.
pub(crate) const DRAG_PRESS_SETTLE: Duration = Duration::from_millis(50);
/// Between two moves, about one frame of a game capped at 60 FPS.
pub(crate) const DRAG_STEP_GAP: Duration = Duration::from_millis(15);
/// On the target before the release, a few frames, so a drop target sees the item over it.
///
/// 2026-10-08 live run (a game capped at 24 FPS): an 80 ms drag released on the same frame it
/// arrived, so the game often never saw the card over its target and the drop did nothing. About
/// 0.35 s per drag now.
pub(crate) const DRAG_DWELL: Duration = Duration::from_millis(120);

/// The pointer positions of a drag from `from` to `to`: [`DRAG_STEPS`] evenly spaced moves, the last
/// exactly on `to`.
pub(crate) fn drag_path(from: Point, to: Point) -> Vec<Point> {
    (1..=DRAG_STEPS)
        .map(|i| {
            let t = i as f64 / DRAG_STEPS as f64;
            Point {
                x: from.x + (f64::from(to.x - from.x) * t).round() as i32,
                y: from.y + (f64::from(to.y - from.y) * t).round() as i32,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn key(c: char) -> TypeStep {
        let (code, shift) = ascii_key(c).unwrap();
        TypeStep::Key {
            code,
            shift,
            unit: c as u16,
        }
    }

    #[test]
    fn ascii_text_is_typed_as_real_keys() {
        assert_eq!(
            plan_typing("Hi+2", 20),
            [key('H'), key('i'), key('+'), key('2')]
        );
        assert!(plan_typing("", 20).is_empty());
    }

    #[test]
    fn other_characters_go_in_unicode_runs_between_keys() {
        assert_eq!(
            plan_typing("a é😀b", 20),
            [key('a'), key(' '), TypeStep::Units(units("é😀")), key('b')]
        );
    }

    #[test]
    fn long_text_splits_at_the_unit_limit() {
        let text = "éèêëīïîí".repeat(7);
        let steps = plan_typing(&text, 20);
        assert_eq!(steps.len(), 3);
        for s in &steps {
            let TypeStep::Units(u) = s else {
                panic!("{s:?}")
            };
            assert!(u.len() <= 20);
        }
        let rejoined: Vec<u16> = steps
            .iter()
            .flat_map(|s| match s {
                TypeStep::Units(u) => u.clone(),
                _ => vec![],
            })
            .collect();
        assert_eq!(rejoined, units(&text));
    }

    #[test]
    fn surrogate_pairs_are_never_split() {
        // 19 accented units then an emoji (2 units) would straddle the 20 unit limit.
        let text = format!("{}😀é", "é".repeat(19));
        let steps = plan_typing(&text, 20);
        assert_eq!(steps.len(), 2);
        let TypeStep::Units(first) = &steps[0] else {
            panic!()
        };
        assert_eq!(first.len(), 19);
        let TypeStep::Units(second) = &steps[1] else {
            panic!()
        };
        assert_eq!(String::from_utf16(second).unwrap(), "😀é");
    }

    #[test]
    fn newlines_and_tabs_become_keys() {
        assert_eq!(
            plan_typing("a\r\nb\tc\nd", 20),
            [
                key('a'),
                TypeStep::Return,
                key('b'),
                TypeStep::Tab,
                key('c'),
                TypeStep::Return,
                key('d'),
            ]
        );
        assert_eq!(
            plan_typing("\n\n", 20),
            [TypeStep::Return, TypeStep::Return]
        );
    }

    #[test]
    fn utf16_length_counts_astral_as_two() {
        assert_eq!(utf16_len("ab😀"), 4);
        assert_eq!(utf16_len(""), 0);
    }

    #[test]
    fn line_ranges() {
        let u = units("first\nsecond line\r\nthird");
        assert_eq!(line_range(&u, 0), (0, 5));
        assert_eq!(line_range(&u, 3), (0, 5));
        assert_eq!(line_range(&u, 8), (6, 17));
        assert_eq!(slice16(&u, 6, 17), "second line");
        assert_eq!(line_range(&u, u.len()), (19, u.len()));
        assert_eq!(line_range(&[], 5), (0, 0));
    }

    #[test]
    fn finds_matches_case_insensitively_with_utf16_offsets() {
        let text = "Hello 😀 World\nsecond: hello again";
        let m = find_matches(text, "HELLO", 10);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].start, m[0].len), (0, 5));
        assert_eq!(m[0].line, "Hello 😀 World");
        // "Hello " is 6 units, the emoji 2, " World\n" 7: the second line starts at 15.
        assert_eq!(m[1].line_start, 15);
        assert_eq!(m[1].start, 15 + "second: ".len());
        assert_eq!(m[1].line, "second: hello again");
    }

    #[test]
    fn match_after_an_astral_character_has_the_right_offset() {
        let m = find_matches("😀 target", "target", 5);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].start, m[0].len), (3, 6));
    }

    #[test]
    fn match_limits_and_empty_queries() {
        assert_eq!(find_matches("aaaa", "a", 2).len(), 2);
        assert!(find_matches("abc", "", 5).is_empty());
        assert!(find_matches("abc", "a", 0).is_empty());
        assert!(find_matches("abc", "zzz", 5).is_empty());
        assert_eq!(find_matches("aaaa", "aa", 9).len(), 2, "non-overlapping");
    }

    #[test]
    fn long_hit_lines_are_capped() {
        let text = format!("needle {}", "x".repeat(600));
        let m = find_matches(&text, "needle", 1);
        assert_eq!(m[0].line.chars().count(), MAX_HIT_LINE_CHARS);
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn quick_clicks_on_one_spot_chain_up_to_three() {
        let mut c = ClickChain::default();
        let p = Point::new(100, 100);
        assert_eq!(c.next_state(ms(0), p, MouseButton::Left), 1);
        assert_eq!(c.next_state(ms(100), p, MouseButton::Left), 2);
        assert_eq!(c.next_state(ms(200), p, MouseButton::Left), 3);
        assert_eq!(c.next_state(ms(300), p, MouseButton::Left), 3);
    }

    #[test]
    fn slow_moved_or_different_button_clicks_start_over() {
        let mut c = ClickChain::default();
        let p = Point::new(100, 100);
        assert_eq!(c.next_state(ms(0), p, MouseButton::Left), 1);
        assert_eq!(c.next_state(ms(900), p, MouseButton::Left), 1, "too slow");
        assert_eq!(
            c.next_state(ms(1000), Point::new(150, 100), MouseButton::Left),
            1,
            "moved"
        );
        assert_eq!(
            c.next_state(ms(1100), Point::new(152, 101), MouseButton::Left),
            2,
            "within slop"
        );
        assert_eq!(
            c.next_state(ms(1200), Point::new(152, 101), MouseButton::Right),
            1,
            "other button"
        );
        c.reset();
        assert_eq!(
            c.next_state(ms(1250), Point::new(152, 101), MouseButton::Right),
            1
        );
    }

    #[test]
    fn scroll_steps_follow_the_direction_docs() {
        assert_eq!(scroll_step(ScrollDirection::Up), (60, 0));
        assert_eq!(scroll_step(ScrollDirection::Down), (-60, 0));
        assert_eq!(scroll_step(ScrollDirection::Left), (0, 60));
        assert_eq!(scroll_step(ScrollDirection::Right), (0, -60));
    }

    #[test]
    fn drag_paths_end_on_the_target() {
        let path = drag_path(Point::new(0, 0), Point::new(300, 100));
        assert_eq!(path.len(), 12);
        assert_eq!(*path.last().unwrap(), Point::new(300, 100));
        assert!(path.windows(2).all(|w| w[0].x <= w[1].x));
        // even a one pixel drag makes all twelve moves and ends on the target
        let short = drag_path(Point::new(5, 5), Point::new(5, 6));
        assert_eq!(short.len(), 12);
        assert_eq!(*short.last().unwrap(), Point::new(5, 6));
        let long = drag_path(Point::new(0, 0), Point::new(5000, 0));
        assert_eq!(long.len(), 12);
        assert_eq!(*long.last().unwrap(), Point::new(5000, 0));
    }

    // Ported from core.drag (7e3838c): press, 50 ms, 12 moves 15 ms apart, 120 ms on the target,
    // then release. About a third of a second per drag.
    #[test]
    fn a_drag_takes_about_a_third_of_a_second() {
        assert_eq!(DRAG_STEPS, 12);
        assert_eq!(DRAG_PRESS_SETTLE, Duration::from_millis(50));
        assert_eq!(DRAG_STEP_GAP, Duration::from_millis(15));
        assert_eq!(DRAG_DWELL, Duration::from_millis(120));
        let total = DRAG_PRESS_SETTLE + DRAG_STEP_GAP * DRAG_STEPS as u32 + DRAG_DWELL;
        assert_eq!(total, Duration::from_millis(350));
    }

    #[test]
    fn bidi_marks_are_stripped_from_names() {
        assert_eq!(strip_bidi("\u{200E}753"), "753");
        assert_eq!(strip_bidi("a\u{202B}b\u{2069}c"), "abc");
        assert_eq!(strip_bidi("plain é"), "plain é");
    }
}
