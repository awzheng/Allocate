// allocate-core/src/ipc.rs
//
// XPC service endpoint for the Allocate daemon.
//
// Exposes telemetry via a named Mach service so allocate-ui (Swift) can connect.
// Uses raw libxpc C bindings — XPC lives in libSystem, no extra crate needed.
//
// ── Launchd registration requirement ─────────────────────────────────────────
// xpc_connection_create_mach_service(name, queue, LISTENER) requires the service
// to be registered in a launchd plist at:
//   /Library/LaunchDaemons/com.andrewzheng.allocate.daemon.plist
// Without this plist, the function returns null (bootstrap lookup fails).
// This module checks for null and degrades gracefully: IPC disabled, daemon
// continues normally.  The plist template is documented in guide.md.
//
// ── GCD / threading model ─────────────────────────────────────────────────────
// XPC event handlers are dispatched onto a GCD concurrent queue managed by
// libdispatch — independent of all Rust threads.  No Rust thread needs to spin
// waiting for XPC events.  The Rust blocks (via block2) are heap-allocated and
// ref-counted by the XPC runtime (xpc_connection_set_event_handler copies the
// block internally), so they remain alive as long as the connection is live.
//
// ── Unsafe memory guarantees ─────────────────────────────────────────────────
// 1. XpcHandle is a newtype over *mut c_void (= xpc_object_t).  Raw pointers are
//    not Send; the newtype implements Send because xpc_object_t is thread-safe
//    by XPC's API contract — all operations on an xpc_connection_t are
//    internally serialised by libxpc.
// 2. Every client connection received in the listener block is immediately
//    xpc_retain()'d before being stored.  On XPC_ERROR_CONNECTION_INVALID the
//    handler calls xpc_release() and removes it from the list.
// 3. xpc_dictionary_create_empty / xpc_dictionary_set_string / xpc_release are
//    called within broadcast() with a locally-scoped message object.  The
//    message is released immediately after send to prevent leaks.
// 4. The listener xpc_connection_t is leaked intentionally (std::mem::forget) —
//    it must remain live for the process lifetime.

use std::ffi::{c_void, CString};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;

// ── Mach service name ─────────────────────────────────────────────────────────

pub const XPC_SERVICE_NAME: &str = "com.andrewzheng.allocate.daemon";

// ── Raw type aliases ──────────────────────────────────────────────────────────

type XpcObjectT    = *mut c_void;   // xpc_object_t  (OS_xpc_object *)
type DispatchQueueT = *mut c_void;  // dispatch_queue_t

const XPC_CONNECTION_MACH_SERVICE_LISTENER: u64 = 1;

// ── FFI: libxpc + libdispatch (both in libSystem.dylib) ──────────────────────

#[link(name = "System", kind = "dylib")]
extern "C" {
    fn xpc_connection_create_mach_service(
        name:    *const libc::c_char,
        targetq: DispatchQueueT,
        flags:   u64,
    ) -> XpcObjectT;

    // handler is void (^)(xpc_object_t) — a block; XPC copies it internally.
    // Declared as *const c_void to avoid block2 generic constraints in extern "C".
    fn xpc_connection_set_event_handler(connection: XpcObjectT, handler: *const c_void);

    fn xpc_connection_resume(connection: XpcObjectT);
    fn xpc_retain(object: XpcObjectT)  -> XpcObjectT;
    fn xpc_release(object: XpcObjectT);

    fn xpc_dictionary_create_empty() -> XpcObjectT;
    fn xpc_dictionary_set_string(
        xdict: XpcObjectT,
        key:   *const libc::c_char,
        value: *const libc::c_char,
    );
    fn xpc_connection_send_message(connection: XpcObjectT, message: XpcObjectT);

    // Provides the default global concurrent queue. No release needed (Get Rule).
    fn dispatch_get_global_queue(identifier: libc::c_long, flags: libc::c_ulong)
        -> DispatchQueueT;

    fn xpc_get_type(object: XpcObjectT) -> *const c_void;

    static _xpc_type_connection: c_void;
    static _xpc_type_error: c_void;

    // XPC_ERROR_CONNECTION_INVALID singleton — compare pointer equality to detect
    // client disconnections in the client event handler.
    static _xpc_error_connection_invalid: c_void;
}

// ── Thread-safe XPC handle wrapper ───────────────────────────────────────────

/// Newtype over xpc_object_t that implements Send.
///
/// # SAFETY
/// xpc_connection_t (stored here) is thread-safe by libxpc's API contract:
/// xpc_connection_send_message and xpc_release are safe to call from any thread.
struct XpcHandle(XpcObjectT);
unsafe impl Send for XpcHandle {}

// ── Public API ────────────────────────────────────────────────────────────────

/// Handle returned by `start_listener`.  Call `broadcast` from the worker thread
/// to push a payload string to every currently-connected XPC client.
pub struct IpcBroadcaster {
    clients: Arc<Mutex<Vec<XpcHandle>>>,
}

impl IpcBroadcaster {
    /// Sends `payload` as an XPC dictionary `{"payload": "<string>"}` to every
    /// connected client.  Dead clients (already disconnected) receive the message
    /// silently; XPC drops it without panicking.
    pub fn broadcast(&self, payload: &str) {
        let clients = match self.clients.lock() {
            Ok(g)  => g,
            Err(_) => return,
        };
        if clients.is_empty() {
            return;
        }

        // Sanitise: XPC strings must be NUL-terminated; replace embedded NULs.
        let safe_payload = payload.replace('\0', "\u{FFFD}");
        let payload_cstr = match CString::new(safe_payload) {
            Ok(s)  => s,
            Err(_) => return,
        };
        let key_cstr = CString::new("payload").expect("static string");

        unsafe {
            // ── Create the XPC message dict ───────────────────────────────────
            // xpc_dictionary_create_empty → Create Rule → we own it (+1).
            let msg = xpc_dictionary_create_empty();
            if msg.is_null() {
                return;
            }

            xpc_dictionary_set_string(msg, key_cstr.as_ptr(), payload_cstr.as_ptr());

            // ── Send to every client ──────────────────────────────────────────
            // xpc_connection_send_message is a "fire-and-forget" + internally
            // retains the message until delivery.  Sending to a dead connection
            // is a no-op (XPC discards silently).
            for client in clients.iter() {
                xpc_connection_send_message(client.0, msg);
            }

            // ── Release our reference to the message ──────────────────────────
            // SAFETY: msg was obtained from xpc_dictionary_create_empty (Create
            // Rule, +1).  xpc_release drops our ref; XPC's internal delivery
            // refs keep it alive until all sends complete.
            xpc_release(msg);
        }
    }
}

// ── Listener setup ────────────────────────────────────────────────────────────

/// Returns an `IpcBroadcaster` for the worker thread to push telemetry to XPC
/// clients.  The XPC listener is **only** initialised when the environment
/// variable `ALLOCATE_XPC_ENABLE=1` is set.
///
/// # Why the env var guard?
/// `xpc_connection_create_mach_service(LISTENER)` does NOT return null on
/// failure — it issues a `SIGTRAP` (trace trap) and aborts the process if the
/// Mach service name is not provisioned in the launchd session (i.e. the daemon
/// is not launched via launchd with a MachServices plist entry).  Running
/// directly from a terminal kills the process instantly.
///
/// The guard makes the terminal development loop safe:
///   • No env var → [DEBUG] bypass message; broadcaster is a no-op; UI runs normally.
///   • `ALLOCATE_XPC_ENABLE=1` → listener starts; requires launchd plist installed.
pub fn start_listener() -> IpcBroadcaster {
    let clients: Arc<Mutex<Vec<XpcHandle>>> = Arc::new(Mutex::new(Vec::new()));

    if std::env::var("ALLOCATE_XPC_ENABLE").is_err() {
        eprintln!("[DEBUG] XPC bypassed for terminal dev \
                   (set ALLOCATE_XPC_ENABLE=1 to enable)");
        return IpcBroadcaster { clients };
    }

    // ALLOCATE_XPC_ENABLE is set — proceed with Mach service registration.
    // This WILL trap if the launchd plist is not installed.
    let connection = unsafe { create_listener(XPC_SERVICE_NAME, Arc::clone(&clients)) };

    if connection.is_null() {
        eprintln!("[XPC] Warning: create_listener returned null — plist loaded?");
    } else {
        let _listener_live_forever = connection;
        println!("[XPC] Listener active on Mach service \"{}\"", XPC_SERVICE_NAME);
    }

    IpcBroadcaster { clients }
}


// ── Internal helpers ──────────────────────────────────────────────────────────

/// Creates the XPC Mach service listener and registers event handlers.
/// Returns null if the service name is not registered in launchd.
unsafe fn create_listener(name: &str, clients: Arc<Mutex<Vec<XpcHandle>>>) -> XpcObjectT {
    let name_cstr = match CString::new(name) {
        Ok(s)  => s,
        Err(_) => return std::ptr::null_mut(),
    };

    // dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT=0, 0)
    // Returns a global concurrent queue — no retain/release required (Get Rule).
    let queue = dispatch_get_global_queue(0, 0);

    let listener = xpc_connection_create_mach_service(
        name_cstr.as_ptr(),
        queue,
        XPC_CONNECTION_MACH_SERVICE_LISTENER,
    );

    if listener.is_null() {
        return std::ptr::null_mut();
    }

    // ── Listener event handler ────────────────────────────────────────────────
    // On a listener connection, every event is a new incoming xpc_connection_t.
    // We xpc_retain the client, register its own event handler, resume it,
    // and add it to the shared client list.

    let clients_for_listener = Arc::clone(&clients);

    let listener_block = RcBlock::new(move |event: NonNull<c_void>| {
        let event_ptr: XpcObjectT = event.as_ptr();
        
        // ── Detect new connection vs listener error ───────────────────────────
        // A listener connection can receive errors (e.g. if the Mach service is
        // invalidated by the system).
        unsafe {
            let event_type = xpc_get_type(event_ptr);
            if event_type != &_xpc_type_connection as *const _ as *const c_void {
                eprintln!("[XPC] Listener received non-connection event (type: {:?})", event_type);
                return;
            }
        }
        
        let client_ptr = event_ptr; // It is safely an xpc_connection_t
        let clients_for_client = Arc::clone(&clients_for_listener);

        // ── Client event handler ──────────────────────────────────────────────
        // Receives messages from this client (Phase 5: no messages expected from
        // ui → daemon direction yet) and XPC error events.
        let client_ptr_for_error = client_ptr;

        let client_block = RcBlock::new(move |event: NonNull<c_void>| {
            let event_ptr: XpcObjectT = event.as_ptr();

            // ── Detect client disconnect ──────────────────────────────────────
            // SAFETY: _xpc_error_connection_invalid is a static singleton in
            // libxpc.  Pointer equality is the correct XPC idiom for error
            // type detection.
            let is_disconnect = unsafe {
                event_ptr == &_xpc_error_connection_invalid as *const c_void as XpcObjectT
            };

            if is_disconnect {
                // Remove from client list and release our retained reference.
                if let Ok(mut list) = clients_for_client.lock() {
                    list.retain(|h| h.0 != client_ptr_for_error);
                }
                // SAFETY: This matches the xpc_retain() call in the listener block.
                unsafe { xpc_release(client_ptr_for_error) };
            }
            // Other events (actual messages in Phase 6+): ignored for now.
        });

        unsafe {
            // Pass the block to XPC — XPC copies it internally (we don't need to
            // keep client_block alive after this call).
            xpc_connection_set_event_handler(
                client_ptr,
                &*client_block as *const _ as *const c_void,
            );

            // SAFETY: xpc_retain increments the XPC ref count on the client
            // connection.  This ref is released in the error handler above when
            // XPC_ERROR_CONNECTION_INVALID fires (i.e., when the client disconnects).
            let retained = xpc_retain(client_ptr);
            xpc_connection_resume(client_ptr);

            if let Ok(mut list) = clients_for_listener.lock() {
                list.push(XpcHandle(retained));
            }
        }
    });

    // Set event handler on the listener.
    // SAFETY: xpc_connection_set_event_handler copies the block — listener_block
    // can be dropped after this call.  The copied block lives with the connection.
    xpc_connection_set_event_handler(
        listener,
        &*listener_block as *const _ as *const c_void,
    );

    xpc_connection_resume(listener);

    listener
}
