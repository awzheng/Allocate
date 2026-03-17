// allocate-core/src/battery.rs
//
// IOPowerSources battery telemetry via IOKit + CoreFoundation raw FFI.
//
// ── Why IOPowerSources (not IORegistry direct)? ───────────────────────────────
// IOPSCopyPowerSourcesInfo / IOPSCopyPowerSourcesList are the public, stable
// high-level API built on top of IORegistry.  They aggregate data from the
// AppleSmartBattery IOREG node into a CFDictionary without requiring us to
// navigate the registry tree or know per-hardware key paths.
// `pmset -g batt` shells out to the same API; we call it directly.
// No TCC permission required; works in the default sandbox.
//
// ── CFRelease memory contract ─────────────────────────────────────────────────
// CoreFoundation uses manual reference counting (MRC).  The "Create Rule" states:
//   Functions whose names contain "Create" or "Copy" return an object with
//   retain count +1. The *caller* is responsible for calling CFRelease exactly once.
//
// This module's obligations:
//   • blob  (CFTypeRef) from IOPSCopyPowerSourcesInfo  → must CFRelease(blob)
//   • list  (CFArrayRef) from IOPSCopyPowerSourcesList → must CFRelease(list)
//   • desc  (CFDictionaryRef) from IOPSGetPowerSourceDescription → must NOT release
//           (it is a "Get" rule return, lifetime tied to `blob`)
//
// All releases happen via the CfRelease RAII guard below, which calls CFRelease
// in its Drop impl.  This ensures release even on early-return paths (None arms).

use std::ffi::CStr;
use libc::c_void;

// ── Raw CFTypeRef alias ───────────────────────────────────────────────────────
// CoreFoundation types are opaque pointer-to-struct in C.
// We represent them as *mut c_void.  All CF functions work on these same
// pointer-sized handles regardless of the concrete CF type.

type CFTypeRef    = *mut c_void;
type CFArrayRef   = *mut c_void;
type CFStringRef  = *mut c_void;
type CFDictionaryRef = *mut c_void;
type CFIndex      = libc::c_long;
type Boolean      = u8;

// ── FFI: CoreFoundation primitives ────────────────────────────────────────────

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: CFIndex) -> CFTypeRef;
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFStringGetCStringPtr(string: CFStringRef, encoding: u32) -> *const libc::c_char;
    fn CFStringCreateWithCString(
        alloc:  CFTypeRef,  // NULL → default allocator
        cstr:   *const libc::c_char,
        enc:    u32,
    ) -> CFStringRef;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> Boolean;
    fn CFNumberGetValue(
        number:     CFTypeRef,
        the_type:   i32,       // kCFNumberSInt32Type = 3
        value_ptr:  *mut c_void,
    ) -> Boolean;
}

// ── FFI: IOKit / IOPowerSources ───────────────────────────────────────────────

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
    fn IOPSGetPowerSourceDescription(
        blob: CFTypeRef,
        ps:   CFTypeRef,
    ) -> CFDictionaryRef;
}

// ── RAII guard for CFRelease ──────────────────────────────────────────────────

/// Wraps a `CFTypeRef` and calls `CFRelease` when dropped.
///
/// # Safety invariant
/// The pointer MUST be a valid, retained CoreFoundation object obtained from a
/// function that follows the "Create Rule" (name contains Create or Copy).
/// Never use this around CFDictionaryGetValue / CFArrayGetValueAtIndex returns
/// (those are "Get Rule" — caller does *not* own the object).
struct CfRelease(CFTypeRef);

impl Drop for CfRelease {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 is a +1 CF object. CFRelease decrements to 0 and
            // frees backing storage. Not calling this is a memory leak; calling
            // it twice (double-free) is UB — prevented by Rust's single-owner Drop.
            unsafe { CFRelease(self.0) }
        }
    }
}

// ── String constants (IOPowerSources keys) ────────────────────────────────────

// kCFStringEncodingUTF8
const CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

// kCFNumberSInt32Type
const CF_NUMBER_SINT32_TYPE: i32 = 3;

/// Converts a static Rust &str to a temporary CFStringRef.
///
/// # SAFETY
/// The returned value is a +1 CF object (CFStringCreateWithCString follows the
/// Create Rule). Wrap in CfRelease so it is released when the temporary scope ends.
unsafe fn cf_str(s: &'static str) -> CFStringRef {
    let cstr = std::ffi::CString::new(s).expect("no NUL in key");
    // SAFETY: cstr lives for the duration of this call; CF copies the bytes.
    CFStringCreateWithCString(
        std::ptr::null_mut(),
        cstr.as_ptr(),
        CF_STRING_ENCODING_UTF8,
    )
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Discriminates the power source currently supplying the system.
#[derive(Debug, Clone, PartialEq)]
pub enum PowerSource {
    Battery,
    AcPower,
    Ups,
    Unknown,
}

/// Snapshot of battery / power-source state.
#[derive(Debug, Clone)]
pub struct BatteryState {
    /// State-of-charge in percent (0 – 100).
    pub charge_percent: u8,
    /// True while the system is actively charging.
    pub is_charging: bool,
    /// Active power source (kept for Phase 5 routing logic).
    #[allow(dead_code)]
    pub source: PowerSource,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Queries IOPowerSources for the current battery state.
///
/// Returns `None` on desktops with no battery (Mac Pro, Mac mini, etc.) or if
/// IOKit returns unexpected data.  Callers should display a dash or nothing.
///
/// ### CFRelease contract (Create Rule)
/// | Object | Source function                  | Released? |
/// |--------|----------------------------------|-----------|
/// | blob   | IOPSCopyPowerSourcesInfo()       | ✅ via CfRelease(_blob)   |
/// | list   | IOPSCopyPowerSourcesList(blob)   | ✅ via CfRelease(_list)   |
/// | desc   | IOPSGetPowerSourceDescription()  | ❌ Get Rule — not owned  |
/// | keys   | CFStringCreateWithCString()      | ✅ via CfRelease on each  |
pub fn get_battery_state() -> Option<BatteryState> {
    unsafe { read_battery_state_unsafe() }
}

/// Inner implementation — all unsafe CF calls live here.
///
/// # SAFETY
/// All CFTypeRef objects obtained from Create/Copy functions are wrapped in
/// CfRelease guards immediately.  Get-rule returns (desc, dict values) are
/// read-only within the lifetime of their parent object and not released.
unsafe fn read_battery_state_unsafe() -> Option<BatteryState> {
    // ── Step 1: Copy the power sources blob ──────────────────────────────────
    // IOPSCopyPowerSourcesInfo → Create Rule → wrap in guard immediately.
    let blob = IOPSCopyPowerSourcesInfo();
    if blob.is_null() {
        return None;
    }
    let _blob = CfRelease(blob); // released when _blob drops at end of function

    // ── Step 2: Copy the list of individual power sources ────────────────────
    // IOPSCopyPowerSourcesList → Create Rule → wrap in guard.
    let list = IOPSCopyPowerSourcesList(blob);
    if list.is_null() {
        return None;
    }
    let _list = CfRelease(list); // released when _list drops

    let count = CFArrayGetCount(list);
    if count <= 0 {
        // No power sources = desktop Mac with no battery.
        return None;
    }

    // ── Step 3: Examine each source; pick the first internal battery ─────────

    // Build CF key strings for the two dictionary lookups we need.
    // CFStringCreateWithCString → Create Rule → release after use.
    let key_capacity_str  = cf_str("Current Capacity");   // kIOPSCurrentCapacityKey
    let key_max_str       = cf_str("Max Capacity");        // kIOPSMaxCapacityKey
    let key_charging_str  = cf_str("Is Charging");         // kIOPSIsChargingKey
    let key_source_str    = cf_str("Power Source State");  // kIOPSPowerSourceStateKey
    let key_type_str      = cf_str("Type");                // kIOPSTypeKey

    let _kc = CfRelease(key_capacity_str);
    let _km = CfRelease(key_max_str);
    let _ki = CfRelease(key_charging_str);
    let _ks = CfRelease(key_source_str);
    let _kt = CfRelease(key_type_str);

    for i in 0..count {
        // CFArrayGetValueAtIndex → Get Rule → do NOT release.
        let ps_ref: CFTypeRef = CFArrayGetValueAtIndex(list, i);
        if ps_ref.is_null() {
            continue;
        }

        // IOPSGetPowerSourceDescription → Get Rule → do NOT release.
        let desc: CFDictionaryRef = IOPSGetPowerSourceDescription(blob, ps_ref);
        if desc.is_null() {
            continue;
        }

        // ── Type filter: only process internal batteries ──────────────────────
        // Type = "InternalBattery" for built-in; "UPS" / "AC Power" for others.
        let type_val: CFTypeRef = CFDictionaryGetValue(desc, key_type_str);
        if type_val.is_null() {
            continue;
        }
        let type_cstr = CFStringGetCStringPtr(type_val, CF_STRING_ENCODING_UTF8);
        if type_cstr.is_null() {
            continue;
        }
        let type_str = CStr::from_ptr(type_cstr).to_string_lossy();
        if !type_str.contains("InternalBattery") {
            continue;
        }

        // ── Current capacity % ────────────────────────────────────────────────
        let cur_val: CFTypeRef = CFDictionaryGetValue(desc, key_capacity_str);
        let max_val: CFTypeRef = CFDictionaryGetValue(desc, key_max_str);

        let mut current: i32 = 0;
        let mut maximum: i32 = 100;

        if !cur_val.is_null() {
            CFNumberGetValue(cur_val, CF_NUMBER_SINT32_TYPE,
                             &mut current as *mut i32 as *mut c_void);
        }
        if !max_val.is_null() {
            CFNumberGetValue(max_val, CF_NUMBER_SINT32_TYPE,
                             &mut maximum as *mut i32 as *mut c_void);
        }

        let pct = if maximum > 0 {
            ((current as f32 / maximum as f32) * 100.0).round() as u8
        } else {
            0
        };

        // ── Is Charging ───────────────────────────────────────────────────────
        let charging_val: CFTypeRef = CFDictionaryGetValue(desc, key_charging_str);
        let is_charging = !charging_val.is_null() && CFBooleanGetValue(charging_val) != 0;

        // ── Power source state ────────────────────────────────────────────────
        let src_val: CFTypeRef = CFDictionaryGetValue(desc, key_source_str);
        let source = if src_val.is_null() {
            PowerSource::Unknown
        } else {
            let src_cstr = CFStringGetCStringPtr(src_val, CF_STRING_ENCODING_UTF8);
            if src_cstr.is_null() {
                PowerSource::Unknown
            } else {
                let src = CStr::from_ptr(src_cstr).to_string_lossy();
                if src.contains("AC Power") {
                    PowerSource::AcPower
                } else if src.contains("UPS") {
                    PowerSource::Ups
                } else {
                    PowerSource::Battery
                }
            }
        };

        // First internal battery found — return immediately.
        return Some(BatteryState { charge_percent: pct, is_charging, source });
    }

    // No internal battery found among the sources.
    None
}
