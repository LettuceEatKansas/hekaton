//! The `computer` tool's action vocabulary. Port of the constants at the top of
//! `computer.py` plus the route lists in `daemon.py`.

/// Actions that aim at a screen point (computer.py `POINTER_ACTIONS`).
pub const POINTER_ACTIONS: [&str; 12] = [
    "left_click",
    "right_click",
    "middle_click",
    "double_click",
    "triple_click",
    "left_click_drag",
    "mouse_move",
    "scroll",
    "left_mouse_down",
    "left_mouse_up",
    "click_text",
    "replace_word",
];

/// The rest of computer.py `ACTIONS` (everything that is not a pointer action).
pub const OTHER_ACTIONS: [&str; 27] = [
    "press",
    "screenshot",
    "zoom",
    "cursor_position",
    "type",
    "key",
    "hold_key",
    "wait",
    "open_app",
    "file_dialog",
    "find_text",
    "read_field",
    "clipboard_get",
    "clipboard_set",
    "wait_for_text",
    "wait_for_file",
    "list_windows",
    "read",
    "list_files",
    "read_text",
    "window",
    "folder",
    "recipe",
    "recipe_list",
    "recipe_save",
    "status",
    "excel",
];

/// Actions that only read state or touch the OS clipboard, never a window. They skip the
/// human-input wait and target restore (computer.py `_PASSIVE_ACTIONS`).
pub const PASSIVE_ACTIONS: [&str; 15] = [
    "screenshot",
    "zoom",
    "cursor_position",
    "wait",
    "find_text",
    "read_field",
    "clipboard_get",
    "clipboard_set",
    "wait_for_text",
    "wait_for_file",
    "list_windows",
    "read",
    "list_files",
    "recipe_list",
    "status",
];

/// Actions a client may use BEFORE the owner has acknowledged hekaton: reads only. `clipboard_set`
/// is excluded because it changes state the user owns (computer.py `ACK_EXEMPT_ACTIONS`).
pub const ACK_EXEMPT_ACTIONS: [&str; 14] = [
    "screenshot",
    "zoom",
    "cursor_position",
    "wait",
    "find_text",
    "read_field",
    "clipboard_get",
    "wait_for_text",
    "wait_for_file",
    "list_windows",
    "read",
    "list_files",
    "recipe_list",
    "status",
];

/// Text-only answers: a batch ending in one of these skips the trailing screenshot
/// (computer.py `_NO_TRAILING_SHOT`).
pub const NO_TRAILING_SHOT: [&str; 15] = [
    "screenshot",
    "zoom",
    "cursor_position",
    "find_text",
    "read_field",
    "clipboard_get",
    "list_windows",
    "read",
    "list_files",
    "read_text",
    "folder",
    "recipe_list",
    "recipe_save",
    "status",
    "excel",
];

/// Routes the daemon answers before acknowledgement (daemon.py `READ_ONLY_ACTIONS` plus `files`).
pub const ACK_EXEMPT_ROUTES: [&str; 4] = ["windows", "snapshot", "screenshot", "files"];

/// Routes whose outcome is reported back with a delta (daemon.py `REPORTABLE_ACTIONS`).
pub const REPORTABLE_ACTIONS: [&str; 6] = ["click", "type", "key", "scroll", "drag", "hover"];

/// Every action name in the order Python's `sorted(ACTIONS)` lists them (the "unknown action"
/// message prints this).
pub const ACTIONS_SORTED: [&str; 39] = [
    "click_text",
    "clipboard_get",
    "clipboard_set",
    "cursor_position",
    "double_click",
    "excel",
    "file_dialog",
    "find_text",
    "folder",
    "hold_key",
    "key",
    "left_click",
    "left_click_drag",
    "left_mouse_down",
    "left_mouse_up",
    "list_files",
    "list_windows",
    "middle_click",
    "mouse_move",
    "open_app",
    "press",
    "read",
    "read_field",
    "read_text",
    "recipe",
    "recipe_list",
    "recipe_save",
    "replace_word",
    "right_click",
    "screenshot",
    "scroll",
    "status",
    "triple_click",
    "type",
    "wait",
    "wait_for_file",
    "wait_for_text",
    "window",
    "zoom",
];

/// Is `action` one of the 39 actions the `computer` tool knows?
pub fn is_action(action: &str) -> bool {
    POINTER_ACTIONS.contains(&action) || OTHER_ACTIONS.contains(&action)
}

/// Is `action` a pointer action?
pub fn is_pointer(action: &str) -> bool {
    POINTER_ACTIONS.contains(&action)
}

/// Does `action` skip the human-input wait?
pub fn is_passive(action: &str) -> bool {
    PASSIVE_ACTIONS.contains(&action)
}

/// Is this `excel` with `op: read`, the one Excel call that changes nothing? It is observation like
/// `read`: it runs under `read_only` and while the gaming lock is on. Every other op is acting.
pub fn is_excel_read(action: &str, op: &str) -> bool {
    action == "excel" && op.trim().eq_ignore_ascii_case("read")
}

/// May `action` run before the owner has acknowledged?
pub fn is_ack_exempt(action: &str) -> bool {
    ACK_EXEMPT_ACTIONS.contains(&action)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/test_20260924_decisions.py::test_every_exempt_action_is_a_read.
    #[test]
    fn every_exempt_action_is_a_read() {
        assert!(!ACK_EXEMPT_ACTIONS.contains(&"clipboard_set"));
        for a in ACK_EXEMPT_ACTIONS {
            assert!(is_action(a), "{a} is not an action");
            assert!(!is_pointer(a), "{a} is a pointer action");
        }
        for a in ["screenshot", "list_windows", "cursor_position", "read"] {
            assert!(ACK_EXEMPT_ACTIONS.contains(&a));
        }
    }

    #[test]
    fn the_action_set_has_the_documented_size() {
        assert_eq!(POINTER_ACTIONS.len() + OTHER_ACTIONS.len(), 39);
        assert!(PASSIVE_ACTIONS.iter().all(|a| is_action(a)));
        assert!(NO_TRAILING_SHOT.iter().all(|a| is_action(a)));
        assert!(!is_action("nope"));
    }

    #[test]
    fn exempt_is_passive_minus_clipboard_set() {
        let derived: Vec<&str> = PASSIVE_ACTIONS
            .iter()
            .copied()
            .filter(|a| *a != "clipboard_set")
            .collect();
        assert_eq!(derived, ACK_EXEMPT_ACTIONS);
    }

    #[test]
    fn the_sorted_list_is_sorted_and_complete() {
        let mut expect: Vec<&str> = POINTER_ACTIONS
            .iter()
            .chain(OTHER_ACTIONS.iter())
            .copied()
            .collect();
        expect.sort_unstable();
        assert_eq!(ACTIONS_SORTED.to_vec(), expect);
    }

    #[test]
    fn only_an_excel_read_counts_as_a_look() {
        assert!(is_excel_read("excel", "read"));
        assert!(is_excel_read("excel", " READ "));
        assert!(!is_excel_read("excel", "write"));
        assert!(!is_excel_read("read", "read"));
        assert!(!is_passive("excel") && !is_ack_exempt("excel"));
    }

    #[test]
    fn no_pointer_action_is_passive() {
        assert!(POINTER_ACTIONS.iter().all(|a| !is_passive(a)));
    }
}
