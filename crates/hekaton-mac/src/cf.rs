//! Small safe wrappers over CoreFoundation values: reading strings, numbers, booleans, arrays,
//! dictionaries and AX geometry out of the untyped `CFType` the Accessibility and window-server APIs
//! hand back.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_application_services::{AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRange, CFRetained, CFString, CFType, CGPoint,
    CGRect, CGSize,
};

/// View an untyped array as one of `CFType` elements.
pub(crate) fn typed_array(a: &CFArray) -> &CFArray<CFType> {
    // SAFETY: CFArray<T> differs from CFArray only by a PhantomData marker; same layout.
    unsafe { &*(a as *const CFArray).cast::<CFArray<CFType>>() }
}

/// View an untyped dictionary as string keys and `CFType` values.
pub(crate) fn typed_dict(d: &CFDictionary) -> &CFDictionary<CFString, CFType> {
    // SAFETY: CFDictionary<K, V> differs from CFDictionary only by a PhantomData marker.
    unsafe { &*(d as *const CFDictionary).cast::<CFDictionary<CFString, CFType>>() }
}

/// Every element of an untyped array, retained.
pub(crate) fn array_items(a: &CFArray) -> Vec<CFRetained<CFType>> {
    typed_array(a).to_vec()
}

/// A string value, if `v` is a CFString.
pub(crate) fn string_of(v: &CFType) -> Option<String> {
    v.downcast_ref::<CFString>().map(ToString::to_string)
}

/// An integer, if `v` is a CFNumber that holds one (or a whole float).
pub(crate) fn i64_of(v: &CFType) -> Option<i64> {
    let n = v.downcast_ref::<CFNumber>()?;
    n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))
}

/// A float, if `v` is a CFNumber.
pub(crate) fn f64_of(v: &CFType) -> Option<f64> {
    v.downcast_ref::<CFNumber>().and_then(CFNumber::as_f64)
}

/// A boolean, if `v` is a CFBoolean.
pub(crate) fn bool_of(v: &CFType) -> Option<bool> {
    v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
}

/// The value for a string key of an untyped dictionary.
pub(crate) fn dict_get(d: &CFDictionary, key: &str) -> Option<CFRetained<CFType>> {
    typed_dict(d).get(&CFString::from_str(key))
}

/// A dictionary value as a dictionary.
pub(crate) fn dict_of(v: &CFType) -> Option<&CFDictionary> {
    v.downcast_ref::<CFDictionary>()
}

/// Read a `CGPoint` out of an `AXValue`.
pub(crate) fn ax_point(v: &CFType) -> Option<CGPoint> {
    let ax = v.downcast_ref::<AXValue>()?;
    let mut out = CGPoint { x: 0.0, y: 0.0 };
    // SAFETY: `out` is a live CGPoint, the size AXValueType::CGPoint writes.
    let ok = unsafe {
        ax.value(
            AXValueType::CGPoint,
            NonNull::from(&mut out).cast::<c_void>(),
        )
    };
    ok.then_some(out)
}

/// Read a `CGSize` out of an `AXValue`.
pub(crate) fn ax_size(v: &CFType) -> Option<CGSize> {
    let ax = v.downcast_ref::<AXValue>()?;
    let mut out = CGSize {
        width: 0.0,
        height: 0.0,
    };
    // SAFETY: `out` is a live CGSize, the size AXValueType::CGSize writes.
    let ok = unsafe {
        ax.value(
            AXValueType::CGSize,
            NonNull::from(&mut out).cast::<c_void>(),
        )
    };
    ok.then_some(out)
}

/// Read a `CGRect` out of an `AXValue`.
pub(crate) fn ax_rect(v: &CFType) -> Option<CGRect> {
    let ax = v.downcast_ref::<AXValue>()?;
    let mut out = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: 0.0,
            height: 0.0,
        },
    };
    // SAFETY: `out` is a live CGRect, the size AXValueType::CGRect writes.
    let ok = unsafe {
        ax.value(
            AXValueType::CGRect,
            NonNull::from(&mut out).cast::<c_void>(),
        )
    };
    ok.then_some(out)
}

/// Read a `CFRange` out of an `AXValue`.
pub(crate) fn ax_range(v: &CFType) -> Option<CFRange> {
    let ax = v.downcast_ref::<AXValue>()?;
    let mut out = CFRange {
        location: 0,
        length: 0,
    };
    // SAFETY: `out` is a live CFRange, the size AXValueType::CFRange writes.
    let ok = unsafe {
        ax.value(
            AXValueType::CFRange,
            NonNull::from(&mut out).cast::<c_void>(),
        )
    };
    ok.then_some(out)
}

/// Wrap a point for `AXUIElementSetAttributeValue`.
pub(crate) fn make_ax_point(p: CGPoint) -> Option<CFRetained<AXValue>> {
    let mut p = p;
    // SAFETY: `p` is a live CGPoint matching AXValueType::CGPoint.
    unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut p).cast::<c_void>()) }
}

/// Wrap a size for `AXUIElementSetAttributeValue`.
pub(crate) fn make_ax_size(s: CGSize) -> Option<CFRetained<AXValue>> {
    let mut s = s;
    // SAFETY: `s` is a live CGSize matching AXValueType::CGSize.
    unsafe { AXValue::new(AXValueType::CGSize, NonNull::from(&mut s).cast::<c_void>()) }
}

/// Wrap a range for a parameterized attribute query.
pub(crate) fn make_ax_range(r: CFRange) -> Option<CFRetained<AXValue>> {
    let mut r = r;
    // SAFETY: `r` is a live CFRange matching AXValueType::CFRange.
    unsafe { AXValue::new(AXValueType::CFRange, NonNull::from(&mut r).cast::<c_void>()) }
}

/// A value that crosses threads although its type does not say it may. Used only for immutable
/// Apple objects (event sources, `SCDisplay` snapshots, `NSRunningApplication`) whose methods the
/// frameworks document as thread-safe.
pub(crate) struct Shared<T>(pub(crate) T);

// SAFETY: see the type's doc comment; each use site names why its object is thread-safe.
unsafe impl<T> Send for Shared<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for Shared<T> {}
