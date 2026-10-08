//! A thin safe layer over `AXUIElement`: attribute reads, one-shot multi-attribute fetches,
//! actions, the window-id bridge. Every call can fail with an `AXError`; callers decide whether a
//! failure means "gone", "denied" or "just not there".

use std::ptr::NonNull;

use objc2_application_services::{
    AXCopyMultipleAttributeOptions, AXError, AXUIElement, AXValue, AXValueType,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFRange, CFRetained, CFString, CFType, CGPoint, CGSize,
};

use crate::cf;

/// An accessibility element reference. `AXUIElement` is a plain CF object whose calls go through
/// a mach port; Apple documents the API as callable from any thread.
pub(crate) struct Ax(pub(crate) CFRetained<AXUIElement>);

// SAFETY: AXUIElement calls are thread-safe message sends; the reference itself is immutable.
unsafe impl Send for Ax {}
// SAFETY: as above, no interior state is touched without the OS serialising it.
unsafe impl Sync for Ax {}

impl Clone for Ax {
    fn clone(&self) -> Self {
        Ax(self.0.clone())
    }
}

#[allow(improper_ctypes)]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Private but long-stable bridge from an AX window to its `CGWindowID` (used by yabai,
    /// Hammerspoon, Rectangle and every other window manager).
    fn _AXUIElementGetWindow(element: &AXUIElement, window_id: *mut u32) -> AXError;
}

/// The attributes one multi-fetch asks for, in order.
const FETCH: [&str; 15] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXValue",
    "AXDescription",
    "AXPosition",
    "AXSize",
    "AXEnabled",
    "AXFocused",
    "AXSelected",
    "AXExpanded",
    "AXDisclosing",
    "AXIdentifier",
    "AXPlaceholderValue",
    "AXChildren",
];

/// Everything one multi-fetch returns about an element.
pub(crate) struct Props {
    pub(crate) role: String,
    pub(crate) subrole: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) value: Option<CFRetained<CFType>>,
    pub(crate) description: Option<String>,
    pub(crate) position: Option<CGPoint>,
    pub(crate) size: Option<CGSize>,
    pub(crate) enabled: Option<bool>,
    pub(crate) focused: bool,
    pub(crate) selected: bool,
    pub(crate) expanded: Option<bool>,
    pub(crate) disclosing: Option<bool>,
    pub(crate) identifier: Option<String>,
    pub(crate) placeholder: Option<String>,
    pub(crate) children: Vec<Ax>,
}

impl Props {
    /// The value rendered as text (strings as is, numbers and booleans spelled out).
    pub(crate) fn value_text(&self) -> Option<String> {
        self.value.as_deref().and_then(value_text)
    }
}

/// Render an `AXValue` attribute as text.
pub(crate) fn value_text(v: &CFType) -> Option<String> {
    if let Some(s) = cf::string_of(v) {
        return Some(s);
    }
    if let Some(b) = cf::bool_of(v) {
        return Some(b.to_string());
    }
    if let Some(i) = cf::i64_of(v) {
        return Some(i.to_string());
    }
    cf::f64_of(v).map(|f| f.to_string())
}

impl Ax {
    /// The application element for a pid, with a bounded messaging timeout so a hung app cannot
    /// stall a call for the default six seconds.
    pub(crate) fn app(pid: i32) -> Ax {
        // SAFETY: AXUIElementCreateApplication takes any pid and never fails.
        let el = unsafe { AXUIElement::new_application(pid) };
        let ax = Ax(el);
        ax.set_timeout(1.0);
        ax
    }

    /// The system-wide element.
    pub(crate) fn system_wide() -> Ax {
        // SAFETY: AXUIElementCreateSystemWide has no preconditions.
        let el = unsafe { AXUIElement::new_system_wide() };
        Ax(el)
    }

    /// Bound how long one call to this element (or, for the system-wide element, to any element
    /// without its own timeout) waits.
    pub(crate) fn set_timeout(&self, seconds: f32) {
        // SAFETY: plain call on a valid element.
        let _ = unsafe { self.0.set_messaging_timeout(seconds) };
    }

    /// The process that owns the element.
    pub(crate) fn pid(&self) -> Option<i32> {
        let mut pid: i32 = 0;
        // SAFETY: `pid` is a valid out pointer.
        let err = unsafe { self.0.pid(NonNull::from(&mut pid)) };
        (err == AXError::Success).then_some(pid)
    }

    /// The `CGWindowID` of a window element.
    pub(crate) fn window_id(&self) -> Option<u32> {
        let mut id: u32 = 0;
        // SAFETY: `id` is a valid out pointer; the element is a live AXUIElement.
        let err = unsafe { _AXUIElementGetWindow(&self.0, &mut id) };
        (err == AXError::Success && id != 0).then_some(id)
    }

    /// Read one attribute.
    pub(crate) fn copy(&self, name: &str) -> Result<CFRetained<CFType>, AXError> {
        let key = CFString::from_str(name);
        let mut out: *const CFType = std::ptr::null();
        // SAFETY: `out` is a valid out pointer for the copied value.
        let err = unsafe { self.0.copy_attribute_value(&key, NonNull::from(&mut out)) };
        if err != AXError::Success {
            return Err(err);
        }
        let ptr = NonNull::new(out.cast_mut()).ok_or(AXError::NoValue)?;
        // SAFETY: the Copy rule hands back a +1 retained object.
        Ok(unsafe { CFRetained::from_raw(ptr) })
    }

    /// A string attribute.
    pub(crate) fn string(&self, name: &str) -> Option<String> {
        self.copy(name).ok().and_then(|v| cf::string_of(&v))
    }

    /// A boolean attribute.
    pub(crate) fn bool(&self, name: &str) -> Option<bool> {
        self.copy(name).ok().and_then(|v| cf::bool_of(&v))
    }

    /// An element-valued attribute (`AXFocusedWindow`, `AXFocusedUIElement`, `AXParent`).
    pub(crate) fn element(&self, name: &str) -> Option<Ax> {
        let v = self.copy(name).ok()?;
        v.downcast::<AXUIElement>().ok().map(Ax)
    }

    /// An attribute holding a list of elements (`AXWindows`, `AXChildren`).
    pub(crate) fn elements(&self, name: &str) -> Vec<Ax> {
        self.copy(name)
            .ok()
            .and_then(|v| v.downcast::<CFArray>().ok())
            .map(|a| elements_of(&a))
            .unwrap_or_default()
    }

    /// Position of a window or element.
    pub(crate) fn position(&self) -> Option<CGPoint> {
        self.copy("AXPosition").ok().and_then(|v| cf::ax_point(&v))
    }

    /// Size of a window or element.
    pub(crate) fn size(&self) -> Option<CGSize> {
        self.copy("AXSize").ok().and_then(|v| cf::ax_size(&v))
    }

    /// Fetch the standard attribute set in one round trip.
    pub(crate) fn fetch(&self) -> Result<Props, AXError> {
        let names: Vec<CFRetained<CFString>> =
            FETCH.iter().map(|n| CFString::from_str(n)).collect();
        let refs: Vec<&CFString> = names.iter().map(|n| &**n).collect();
        let list = CFArray::from_objects(&refs);
        let mut out: *const CFArray = std::ptr::null();
        // SAFETY: `list` is a CFArray of CFString names; `out` is a valid out pointer.
        let err = unsafe {
            self.0.copy_multiple_attribute_values(
                list.as_ref(),
                AXCopyMultipleAttributeOptions(0),
                NonNull::from(&mut out),
            )
        };
        if err != AXError::Success {
            return Err(err);
        }
        let ptr = NonNull::new(out.cast_mut()).ok_or(AXError::NoValue)?;
        // SAFETY: the Copy rule hands back a +1 retained array.
        let values = unsafe { CFRetained::from_raw(ptr) };
        let items = cf::array_items(&values);
        let get = |i: usize| -> Option<&CFType> {
            items.get(i).map(|v| &**v).filter(|v| !is_ax_error_value(v))
        };
        let string = |i: usize| get(i).and_then(cf::string_of);
        let boolean = |i: usize| get(i).and_then(cf::bool_of);
        let children = get(14)
            .and_then(|v| v.downcast_ref::<CFArray>())
            .map(|a| {
                cf::array_items(a)
                    .into_iter()
                    .filter_map(|c| c.downcast::<AXUIElement>().ok().map(Ax))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Props {
            role: string(0).unwrap_or_default(),
            subrole: string(1),
            title: string(2),
            value: items.get(3).filter(|v| !is_ax_error_value(v)).cloned(),
            description: string(4),
            position: get(5).and_then(cf::ax_point),
            size: get(6).and_then(cf::ax_size),
            enabled: boolean(7),
            focused: boolean(8).unwrap_or(false),
            selected: boolean(9).unwrap_or(false),
            expanded: boolean(10),
            disclosing: boolean(11),
            identifier: string(12),
            placeholder: string(13),
            children,
        })
    }

    /// Set a boolean attribute.
    pub(crate) fn set_bool(&self, name: &str, value: bool) -> AXError {
        let key = CFString::from_str(name);
        let v: &CFBoolean = CFBoolean::new(value);
        // SAFETY: plain call with valid CF arguments.
        unsafe { self.0.set_attribute_value(&key, v) }
    }

    /// Set a string attribute.
    pub(crate) fn set_string(&self, name: &str, value: &str) -> AXError {
        let key = CFString::from_str(name);
        let v = CFString::from_str(value);
        // SAFETY: plain call with valid CF arguments.
        unsafe { self.0.set_attribute_value(&key, &v) }
    }

    /// Set an attribute to an `AXValue` (a point or a size).
    pub(crate) fn set_value(&self, name: &str, value: &AXValue) -> AXError {
        let key = CFString::from_str(name);
        // SAFETY: plain call with valid CF arguments.
        unsafe { self.0.set_attribute_value(&key, value) }
    }

    /// Can this attribute be written?
    pub(crate) fn is_settable(&self, name: &str) -> bool {
        let key = CFString::from_str(name);
        let mut settable: u8 = 0;
        // SAFETY: `settable` is a valid out pointer.
        let err = unsafe {
            self.0
                .is_attribute_settable(&key, NonNull::from(&mut settable))
        };
        err == AXError::Success && settable != 0
    }

    /// Names of the actions the element supports.
    pub(crate) fn actions(&self) -> Vec<String> {
        let mut out: *const CFArray = std::ptr::null();
        // SAFETY: `out` is a valid out pointer.
        let err = unsafe { self.0.copy_action_names(NonNull::from(&mut out)) };
        if err != AXError::Success {
            return Vec::new();
        }
        let Some(ptr) = NonNull::new(out.cast_mut()) else {
            return Vec::new();
        };
        // SAFETY: the Copy rule hands back a +1 retained array.
        let arr = unsafe { CFRetained::from_raw(ptr) };
        cf::array_items(&arr)
            .iter()
            .filter_map(|v| cf::string_of(v))
            .collect()
    }

    /// Perform a named action (`AXPress`, `AXRaise`).
    pub(crate) fn perform(&self, action: &str) -> AXError {
        let key = CFString::from_str(action);
        // SAFETY: plain call with valid CF arguments.
        unsafe { self.0.perform_action(&key) }
    }

    /// Query a parameterized attribute with a `CFRange` parameter and read a rect back
    /// (`AXBoundsForRange`).
    pub(crate) fn bounds_for_range(
        &self,
        location: isize,
        length: isize,
    ) -> Option<objc2_core_foundation::CGRect> {
        let param = cf::make_ax_range(CFRange { location, length })?;
        let key = CFString::from_str("AXBoundsForRange");
        let mut out: *const CFType = std::ptr::null();
        // SAFETY: `param` is an AXValue range, `out` a valid out pointer.
        let err = unsafe {
            self.0
                .copy_parameterized_attribute_value(&key, &param, NonNull::from(&mut out))
        };
        if err != AXError::Success {
            return None;
        }
        let ptr = NonNull::new(out.cast_mut())?;
        // SAFETY: the Copy rule hands back a +1 retained object.
        let v = unsafe { CFRetained::from_raw(ptr) };
        cf::ax_rect(&v)
    }

    /// Query `AXRangeForPosition` with a screen point; the character range under it.
    pub(crate) fn range_for_position(&self, p: CGPoint) -> Option<CFRange> {
        let param = cf::make_ax_point(p)?;
        let key = CFString::from_str("AXRangeForPosition");
        let mut out: *const CFType = std::ptr::null();
        // SAFETY: `param` is an AXValue point, `out` a valid out pointer.
        let err = unsafe {
            self.0
                .copy_parameterized_attribute_value(&key, &param, NonNull::from(&mut out))
        };
        if err != AXError::Success {
            return None;
        }
        let ptr = NonNull::new(out.cast_mut())?;
        // SAFETY: the Copy rule hands back a +1 retained object.
        let v = unsafe { CFRetained::from_raw(ptr) };
        cf::ax_range(&v)
    }

    /// The element under a screen point; call on the system-wide element.
    pub(crate) fn element_at(&self, x: f32, y: f32) -> Result<Ax, AXError> {
        let mut out: *const AXUIElement = std::ptr::null();
        // SAFETY: `out` is a valid out pointer.
        let err = unsafe {
            self.0
                .copy_element_at_position(x, y, NonNull::from(&mut out))
        };
        if err != AXError::Success {
            return Err(err);
        }
        let ptr = NonNull::new(out.cast_mut()).ok_or(AXError::NoValue)?;
        // SAFETY: the Copy rule hands back a +1 retained element.
        Ok(Ax(unsafe { CFRetained::from_raw(ptr) }))
    }

    /// A hash that is equal for equal elements (`CFHash`), for focus-change detection.
    pub(crate) fn identity_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.0.hash(&mut h);
        h.finish()
    }
}

/// Elements out of a `CFArray` of `AXUIElement`.
pub(crate) fn elements_of(a: &CFArray) -> Vec<Ax> {
    cf::array_items(a)
        .into_iter()
        .filter_map(|c| c.downcast::<AXUIElement>().ok().map(Ax))
        .collect()
}

/// `AXCopyMultipleAttributeValues` puts an `AXValue` of type `AXError` where an attribute is
/// missing.
fn is_ax_error_value(v: &CFType) -> bool {
    v.downcast_ref::<objc2_application_services::AXValue>()
        // SAFETY: querying the type tag of a live AXValue.
        .is_some_and(|ax| unsafe { ax.r#type() } == AXValueType::AXError)
}

/// Translate an `AXError` into a short phrase for error messages.
pub(crate) fn describe(err: AXError) -> &'static str {
    match err {
        AXError::Success => "ok",
        AXError::APIDisabled => "accessibility access is disabled",
        AXError::InvalidUIElement => "the element no longer exists",
        AXError::CannotComplete => "the app did not answer (busy, hung or sandboxed)",
        AXError::AttributeUnsupported => "the element does not have that attribute",
        AXError::ActionUnsupported => "the element does not support that action",
        AXError::NotImplemented => "the app does not implement accessibility",
        AXError::NoValue => "the attribute has no value",
        AXError::IllegalArgument => "illegal argument",
        _ => "accessibility call failed",
    }
}

/// Is `AXIsProcessTrusted` true (no prompt)?
pub(crate) fn trusted() -> bool {
    // SAFETY: no arguments, no preconditions.
    unsafe { objc2_application_services::AXIsProcessTrusted() }
}
