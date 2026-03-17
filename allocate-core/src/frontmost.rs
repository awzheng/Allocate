// allocate-core/src/frontmost.rs
//
// Frontmost-application detection via NSWorkspace (AppKit).
//
// Phase 1: polling via NSWorkspace.frontmostApplication (get_frontmost_app)
// Phase 2: event-driven via NSWorkspaceDidActivateApplicationNotification
//          (register_app_switch_observer) — fires the instant the OS switches.
// Phase 3: observer captures name+PID from NSRunningApplication and sends them
//          over an mpsc channel to the worker thread, eliminating the 2s poll sleep.

use objc2::msg_send;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSNotification, NSString};

use block2::RcBlock;
use std::ptr::NonNull;
use std::sync::mpsc;

// ── Shared signal type ────────────────────────────────────────────────────────

/// Signal sent from the ObjC notification block → worker thread over mpsc.
///
/// The block extracts name and PID directly from the NSRunningApplication
/// object attached to the notification's `userInfo` dictionary, so the worker
/// thread never needs to call frontmostApplication() itself.
#[derive(Debug, Clone)]
pub struct AppSwitchSignal {
    pub name: String,
    pub pid:  i32,
}

// ── Phase 1: Polling (kept for compatibility / fallback) ─────────────────────

/// The currently focused application, as reported by NSWorkspace.
#[derive(Debug, Clone, PartialEq)]
pub struct ForegroundApp {
    pub name: String,
    pub pid:  i32,
}

/// Polls NSWorkspace.frontmostApplication for the current foreground app.
/// See full inline safety notes in the Phase 1 implementation.
pub fn get_frontmost_app() -> Option<ForegroundApp> {
    autoreleasepool(|_pool| {
        unsafe {
            let workspace = NSWorkspace::sharedWorkspace();
            let app: Retained<NSRunningApplication> =
                workspace.frontmostApplication()?;

            let name = app
                .localizedName()
                .map(|ns| ns.to_string())
                .unwrap_or_else(|| "Unknown".to_string());

            // processIdentifier not yet wrapped in objc2-app-kit 0.2.x.
            // SAFETY: valid NSRunningApplication; selector always exists.
            let pid: i32 = msg_send![&*app, processIdentifier];

            Some(ForegroundApp { name, pid })
        }
    })
}

// ── Phase 2/3: Event-driven observer with mpsc channel ───────────────────────

/// Registers a block observer on NSWorkspace's notification centre for
/// `NSWorkspaceDidActivateApplicationNotification` and wires it to `tx`.
///
/// When the OS switches the foreground application, NSWorkspace posts the
/// notification synchronously on the main thread. The block fires immediately
/// (< 1 ms latency), extracts the new app's name and PID from the notification's
/// `userInfo` dictionary, and sends an `AppSwitchSignal` over `tx`.
///
/// The worker thread blocks on the corresponding `rx.recv()` and wakes up
/// exactly when a switch occurs — eliminating the old 2-second polling sleep.
///
/// ### Return value
/// An opaque `Retained<AnyObject>` observer token. Keep alive for process lifetime;
/// dropping it deregisters the observer.
///
/// ### Unsafe memory guarantees
/// 1. `tx` is moved into the closure → `'static` capture, no borrow lifetimes.
///    `mpsc::Sender<T>` is `Send`, satisfying the cross-thread requirement.
/// 2. `RcBlock::new` heap-allocates and ARC-manages the block. Not freed while
///    the notification centre holds a reference via the observer token.
/// 3. `NonNull<NSNotification>` argument: pointer valid for the call duration;
///    not stored beyond the block body. `NonNull` avoids block2's unsupported
///    reference-lifetime trait bounds (documented in block2 0.5 crate docs).
/// 4. `userInfo` dictionary access: `NSWorkspaceApplicationKey` always present
///    on `NSWorkspaceDidActivateApplicationNotification` per Apple docs; we guard
///    with an Option pattern and fall back to `get_frontmost_app()` if nil.
/// 5. Return token is autoreleased (+0); Retained::retain bumps to +1 before
///    pool drain. NC holds its own +1; block keeps firing after caller retains.
pub fn register_app_switch_observer(tx: mpsc::Sender<AppSwitchSignal>) -> Retained<AnyObject> {
    autoreleasepool(|_pool| {
        unsafe {
            // ── Get NSWorkspace's dedicated notification centre ────────────────
            // MUST use NSWorkspace.notificationCenter, NOT NSNotificationCenter
            // defaultCenter — workspace notifications are only posted there.
            let workspace = NSWorkspace::sharedWorkspace();
            let nc: *mut AnyObject = msg_send![&*workspace, notificationCenter];
            assert!(!nc.is_null(), "NSWorkspace.notificationCenter returned nil");

            let name = NSString::from_str("NSWorkspaceDidActivateApplicationNotification");

            // ── Build the ObjC block ──────────────────────────────────────────
            // `tx` is moved into the closure. The closure is 'static (no borrows).
            // mpsc::Sender<T>: Send, so cross-thread delivery from main→worker is safe.
            let block = RcBlock::new(move |notif: NonNull<NSNotification>| {
                // Extract the activating NSRunningApplication from userInfo.
                // Key: "NSWorkspaceApplicationKey" (always present per Apple docs).
                let app = autoreleasepool(|_| -> Option<AppSwitchSignal> {
                    let notif_ref: &NSNotification = notif.as_ref();
                    let user_info: *mut AnyObject = msg_send![notif_ref, userInfo];
                    if user_info.is_null() {
                        return None;
                    }
                    let key = NSString::from_str("NSWorkspaceApplicationKey");
                    let running_app: *mut AnyObject =
                        msg_send![user_info, objectForKey: &*key];
                    if running_app.is_null() {
                        return None;
                    }
                    let pid: i32 = msg_send![running_app, processIdentifier];
                    let ns_name: *mut AnyObject = msg_send![running_app, localizedName];
                    let name: String = if ns_name.is_null() {
                        format!("<{}>", pid)
                    } else {
                        // SAFETY: localizedName on NSRunningApplication returns NSString or nil.
                        // nil is guarded above. Cast raw pointer to NSString so we can use
                        // objc2-foundation's safe to_string() (via Display impl) instead of
                        // the raw `msg_send![…, UTF8String]` call that panicked:
                        //   UTF8String returns const char* (ObjC encoding '*') but msg_send!
                        //   inferred return type AnyObject (encoding '@') — type code mismatch
                        //   caught by objc2's runtime type validation → panic at runtime.
                        // The pointer is valid for the duration of this autoreleasepool block.
                        let ns_str: &NSString = &*(ns_name as *const NSString);
                        ns_str.to_string()
                    };
                    Some(AppSwitchSignal { name, pid })
                });

                // Fall back to polling if userInfo extraction failed.
                let signal = app.or_else(|| {
                    get_frontmost_app().map(|fg| AppSwitchSignal { name: fg.name, pid: fg.pid })
                });

                if let Some(sig) = signal {
                    // Ignore send errors — the receiver shutting down means the
                    // process is exiting; silently discard is correct behaviour.
                    let _ = tx.send(sig);
                }
            });

            // ── Register with the notification centre ─────────────────────────
            // object=nil → any sender; queue=nil → deliver on posting thread (main).
            let token: *mut AnyObject = msg_send![
                nc,
                addObserverForName: &*name
                object: core::ptr::null_mut::<AnyObject>()
                queue:  core::ptr::null_mut::<AnyObject>()
                usingBlock: &*block
            ];

            Retained::retain(token)
                .expect("addObserverForName:object:queue:usingBlock: returned nil")
        }
    })
}
