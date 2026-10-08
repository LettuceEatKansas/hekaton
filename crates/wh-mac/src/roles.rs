//! Accessibility vocabulary: AX role and subrole strings to the neutral [`Role`], toggle values,
//! window kinds. Pure string matching, tested on every OS.

use wh_core::backend::{Role, Toggle, WindowKind};

/// Map `(AXRole, AXSubrole)` to the neutral role. `disclosing` is true when the element has an
/// `AXDisclosing` attribute, which tells an outline row (tree item) from a plain table row.
pub(crate) fn role_from_ax(role: &str, subrole: Option<&str>, disclosing: bool) -> Role {
    let sub = subrole.unwrap_or("");
    match role {
        "AXButton" => match sub {
            "AXToggle" | "AXSwitch" => Role::CheckBox,
            _ => Role::Button,
        },
        "AXPopUpButton" | "AXComboBox" => Role::ComboBox,
        "AXMenuButton" => Role::SplitButton,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXSecureTextField" => Role::Edit,
        "AXCheckBox" => Role::CheckBox,
        "AXRadioButton" => match sub {
            "AXTabButton" => Role::TabItem,
            _ => Role::RadioButton,
        },
        "AXRow" => {
            if disclosing {
                Role::TreeItem
            } else {
                Role::ListItem
            }
        }
        "AXCell" => Role::DataItem,
        "AXMenuItem" | "AXMenuBarItem" => Role::MenuItem,
        "AXMenu" | "AXMenuBar" => Role::Menu,
        "AXLink" => Role::Hyperlink,
        "AXSlider" => Role::Slider,
        "AXStaticText" => Role::Text,
        "AXList" | "AXBrowser" => Role::List,
        "AXOutline" => Role::Tree,
        "AXTable" => Role::Table,
        "AXWebArea" => Role::Document,
        "AXWindow" | "AXSheet" | "AXDrawer" => Role::Window,
        "AXGroup" | "AXSplitGroup" | "AXScrollArea" | "AXToolbar" | "AXLayoutArea"
        | "AXTabGroup" | "AXScrollBar" | "AXSplitter" | "AXLayoutItem" | "AXApplication" => {
            Role::Pane
        }
        "" => Role::Other("Unknown".into()),
        other => Role::Other(other.strip_prefix("AX").unwrap_or(other).to_string()),
    }
}

/// A secure (password) text field.
pub(crate) fn is_secure_field(role: &str, subrole: Option<&str>) -> bool {
    role == "AXSecureTextField" || subrole == Some("AXSecureTextField")
}

/// A role that holds editable text.
pub(crate) fn is_text_input(role: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox" | "AXSecureTextField"
    )
}

/// Roles that take the checked/unchecked `AXValue` 0, 1 or 2.
pub(crate) fn has_toggle_value(role: &str, subrole: Option<&str>) -> bool {
    matches!(role, "AXCheckBox" | "AXRadioButton")
        || (role == "AXButton" && matches!(subrole, Some("AXToggle" | "AXSwitch")))
}

/// `AXValue` of a checkbox: 0 off, 1 on, 2 mixed.
pub(crate) fn toggle_from_value(v: i64) -> Toggle {
    match v {
        0 => Toggle::Off,
        2 => Toggle::Mixed,
        _ => Toggle::On,
    }
}

/// The kind of window from its `AXSubrole` and `AXRole`, plus the CG layer and owner bundle id.
pub(crate) fn window_kind(subrole: Option<&str>, layer: i32, bundle_id: &str) -> WindowKind {
    if layer < 0 {
        return WindowKind::Desktop;
    }
    if bundle_id == "com.apple.dock" {
        return WindowKind::Taskbar;
    }
    // kCGPopUpMenuWindowLevel
    if layer == 101 {
        return WindowKind::PopupMenu;
    }
    match subrole {
        Some("AXDialog" | "AXSystemDialog" | "AXSheet") => WindowKind::Dialog,
        _ if bundle_id == "com.apple.finder" => WindowKind::FileManager,
        _ => WindowKind::Normal,
    }
}

/// Bundle ids of system surfaces that ignore synthetic input on purpose: authorization prompts
/// (the admin password sheet), privacy consent prompts and Gatekeeper's first-launch question.
const PROTECTED_PROMPTS: [&str; 3] = [
    "com.apple.SecurityAgent",
    "com.apple.UserNotificationCenter",
    "com.apple.coreservices.uiagent",
];

/// Is this app one of the system surfaces software cannot operate?
pub(crate) fn is_protected_prompt(bundle_id: &str) -> bool {
    PROTECTED_PROMPTS.contains(&bundle_id)
}

/// Actions that mean "do the primary thing", best first.
pub(crate) const PRESS_ACTIONS: [&str; 3] = ["AXPress", "AXPick", "AXConfirm"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_roles_map_to_the_neutral_set() {
        let cases = [
            ("AXButton", None, Role::Button),
            ("AXPopUpButton", None, Role::ComboBox),
            ("AXMenuButton", None, Role::SplitButton),
            ("AXTextField", None, Role::Edit),
            ("AXTextArea", None, Role::Edit),
            ("AXSearchField", Some("AXSearchField"), Role::Edit),
            ("AXCheckBox", None, Role::CheckBox),
            ("AXRadioButton", None, Role::RadioButton),
            ("AXRadioButton", Some("AXTabButton"), Role::TabItem),
            ("AXMenuItem", None, Role::MenuItem),
            ("AXMenuBarItem", None, Role::MenuItem),
            ("AXMenu", None, Role::Menu),
            ("AXLink", None, Role::Hyperlink),
            ("AXSlider", None, Role::Slider),
            ("AXStaticText", None, Role::Text),
            ("AXOutline", None, Role::Tree),
            ("AXTable", None, Role::Table),
            ("AXList", None, Role::List),
            ("AXCell", None, Role::DataItem),
            ("AXWebArea", None, Role::Document),
            ("AXWindow", Some("AXStandardWindow"), Role::Window),
            ("AXGroup", None, Role::Pane),
            ("AXScrollArea", None, Role::Pane),
        ];
        for (role, sub, want) in cases {
            assert_eq!(role_from_ax(role, sub, false), want, "{role} {sub:?}");
        }
    }

    #[test]
    fn switches_and_toggle_buttons_are_check_boxes() {
        assert_eq!(
            role_from_ax("AXButton", Some("AXToggle"), false),
            Role::CheckBox
        );
        assert_eq!(
            role_from_ax("AXButton", Some("AXSwitch"), false),
            Role::CheckBox
        );
        assert_eq!(
            role_from_ax("AXButton", Some("AXCloseButton"), false),
            Role::Button
        );
    }

    #[test]
    fn rows_are_tree_items_only_when_they_disclose() {
        assert_eq!(role_from_ax("AXRow", None, false), Role::ListItem);
        assert_eq!(role_from_ax("AXRow", None, true), Role::TreeItem);
    }

    #[test]
    fn unknown_roles_keep_their_name_without_the_prefix() {
        assert_eq!(
            role_from_ax("AXColorWell", None, false),
            Role::Other("ColorWell".into())
        );
        assert_eq!(
            role_from_ax("AXImage", None, false),
            Role::Other("Image".into())
        );
        assert_eq!(role_from_ax("", None, false), Role::Other("Unknown".into()));
        assert_eq!(
            role_from_ax("Custom", None, false),
            Role::Other("Custom".into())
        );
    }

    #[test]
    fn secure_fields_and_text_inputs() {
        assert!(is_secure_field("AXTextField", Some("AXSecureTextField")));
        assert!(is_secure_field("AXSecureTextField", None));
        assert!(!is_secure_field("AXTextField", None));
        assert!(is_text_input("AXTextArea") && is_text_input("AXComboBox"));
        assert!(!is_text_input("AXButton"));
    }

    #[test]
    fn toggle_values() {
        assert_eq!(toggle_from_value(0), Toggle::Off);
        assert_eq!(toggle_from_value(1), Toggle::On);
        assert_eq!(toggle_from_value(2), Toggle::Mixed);
        assert!(has_toggle_value("AXCheckBox", None));
        assert!(has_toggle_value("AXButton", Some("AXSwitch")));
        assert!(!has_toggle_value("AXButton", None));
    }

    #[test]
    fn protected_system_prompts() {
        assert!(is_protected_prompt("com.apple.SecurityAgent"));
        assert!(is_protected_prompt("com.apple.UserNotificationCenter"));
        assert!(!is_protected_prompt("com.apple.finder"));
        assert!(!is_protected_prompt(""));
    }

    #[test]
    fn window_kinds() {
        assert_eq!(
            window_kind(None, 0, "com.apple.TextEdit"),
            WindowKind::Normal
        );
        assert_eq!(
            window_kind(Some("AXSheet"), 0, "com.apple.TextEdit"),
            WindowKind::Dialog
        );
        assert_eq!(
            window_kind(Some("AXStandardWindow"), 0, "com.apple.finder"),
            WindowKind::FileManager
        );
        assert_eq!(
            window_kind(None, -2147483603, "com.apple.finder"),
            WindowKind::Desktop
        );
        assert_eq!(window_kind(None, 20, "com.apple.dock"), WindowKind::Taskbar);
        assert_eq!(window_kind(None, 101, "com.foo"), WindowKind::PopupMenu);
    }
}
