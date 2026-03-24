// allocate-core/src/ipc.rs
//
// XPC service endpoint for the Allocate daemon.
//
// Phase 9.2: two-way XPC bridge.
//   Daemon → UI  : {"payload": "<ascii-table>"}  (unchanged)
//   UI → Daemon  : {"type": "config",
//                   "throttle_threshold": <f64>,
//                   "release_threshold":  <f64>}
//
// Incoming config messages are parsed on the GCD XPC thread and written into
// the shared Arc<RwLock<GovernorConfig>>.  The worker thread reads the config
// at the top of each evaluate() call via a brief read-lock — no blocking.
//
// ── Unsafe memory guarantees ─────────────────────────────────────────────────
// 1. XpcHandle implements Send: xpc_object_t is thread-safe per libxpc contract.
// 2. Every client connection is xpc_retain()'d on arrival; released on INVALID.
// 3. xpc_dictionary_get_string returns a pointer into the dict's own storage —
//    valid only while the dict object is alive (i.e., within the event handler).
// 4. The listener xpc_connection_t is intentionally leaked (process lifetime).

use std::ffi::{c_void, CStr, CString};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, RwLock};

use block2::RcBlock;

use crate::governor::GovernorConfig;

// ── Mach service name ─────────────────────────────────────────────────────────

pub const XPC_SERVICE_NAME: &str = "com.andrewzheng.allocate.daemon";

// ── Raw type aliases ──────────────────────────────────────────────────────────

type XpcObjectT     = *mut c_void;
type DispatchQueueT = *mut c_void;

const XPC_CONNECTION_MACH_SERVICE_LISTENER: u64 = 1;

// ── FFI: libxpc + libdispatch ─────────────────────────────────────────────────

#[link(name = "System", kind = "dylib")]
extern "C" {
    fn xpc_connection_create_mach_service(
        name:    *const libc::c_char,
        targetq: DispatchQueueT,
        flags:   u64,
    ) -> XpcObjectT;

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
    /// Returns a pointer into `xdict`'s storage — valid while `xdict` is alive.
    /// Returns NULL if the key is absent or not a string.
    fn xpc_dictionary_get_string(
        xdict: XpcObjectT,
        key:   *const libc::c_char,
    ) -> *const libc::c_char;
    /// Returns 0.0 if the key is absent or not a double.
    fn xpc_dictionary_get_double(
        xdict: XpcObjectT,
        key:   *const libc::c_char,
    ) -> f64;

    fn xpc_connection_send_message(connection: XpcObjectT, message: XpcObjectT);
    fn dispatch_get_global_queue(identifier: libc::c_long, flags: libc::c_ulong)
        -> DispatchQueueT;
    fn xpc_get_type(object: XpcObjectT) -> *const c_void;

    static _xpc_type_connection:  c_void;
    static _xpc_type_dictionary:  c_void;
    static _xpc_type_error:       c_void;
    static _xpc_error_connection_invalid: c_void;
}

// ── Thread-safe XPC handle wrapper ───────────────────────────────────────────

struct XpcHandle(XpcObjectT);
/// SAFETY: xpc_connection_t is thread-safe per libxpc contract.
unsafe impl Send for XpcHandle {}

// ── Public API ────────────────────────────────────────────────────────────────

pub struct IpcBroadcaster {
    clients: Arc<Mutex<Vec<XpcHandle>>>,
}

impl IpcBroadcaster {
    pub fn broadcast(&self, payload: &str) {
        let clients = match self.clients.lock() {
            Ok(g)  => g,
            Err(_) => return,
        };
        if clients.is_empty() { return; }

        let safe_payload = payload.replace('\0', "\u{FFFD}");
        let payload_cstr = match CString::new(safe_payload) {
            Ok(s)  => s,
            Err(_) => return,
        };
        let key_cstr = CString::new("payload").expect("static string");

        unsafe {
            let msg = xpc_dictionary_create_empty();
            if msg.is_null() { return; }
            xpc_dictionary_set_string(msg, key_cstr.as_ptr(), payload_cstr.as_ptr());
            for client in clients.iter() {
                xpc_connection_send_message(client.0, msg);
            }
            xpc_release(msg);
        }
    }
}

// ── Listener setup ────────────────────────────────────────────────────────────

/// Initialises the XPC Mach service listener and returns a broadcaster.
///
/// `config` is shared with the worker thread via `Arc<RwLock<GovernorConfig>>`.
/// When the UI sends a `{"type":"config", ...}` message, the GCD event handler
/// writes new thresholds into `config` without blocking the worker.
///
/// Gated on `ALLOCATE_XPC_ENABLE=1` to prevent a SIGTRAP when running outside
/// of a launchd session (see Phase 6 notes).
pub fn start_listener(config: Arc<RwLock<GovernorConfig>>) -> IpcBroadcaster {
    let clients: Arc<Mutex<Vec<XpcHandle>>> = Arc::new(Mutex::new(Vec::new()));

    if std::env::var("ALLOCATE_XPC_ENABLE").is_err() {
        eprintln!("[DEBUG] XPC bypassed for terminal dev \
                   (set ALLOCATE_XPC_ENABLE=1 to enable)");
        return IpcBroadcaster { clients };
    }

    let connection = unsafe {
        create_listener(XPC_SERVICE_NAME, Arc::clone(&clients), config)
    };

    if connection.is_null() {
        eprintln!("[XPC] Warning: create_listener returned null — plist loaded?");
    } else {
        println!("[XPC] Listener active on Mach service \"{}\"", XPC_SERVICE_NAME);
    }

    IpcBroadcaster { clients }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

unsafe fn create_listener(
    name:    &str,
    clients: Arc<Mutex<Vec<XpcHandle>>>,
    config:  Arc<RwLock<GovernorConfig>>,
) -> XpcObjectT {
    let name_cstr = match CString::new(name) {
        Ok(s)  => s,
        Err(_) => return std::ptr::null_mut(),
    };

    let queue    = dispatch_get_global_queue(0, 0);
    let listener = xpc_connection_create_mach_service(
        name_cstr.as_ptr(),
        queue,
        XPC_CONNECTION_MACH_SERVICE_LISTENER,
    );
    if listener.is_null() { return std::ptr::null_mut(); }

    let clients_for_listener = Arc::clone(&clients);

    let listener_block = RcBlock::new(move |event: NonNull<c_void>| {
        let event_ptr: XpcObjectT = event.as_ptr();

        unsafe {
            let event_type = xpc_get_type(event_ptr);
            if event_type != &_xpc_type_connection as *const _ as *const c_void {
                eprintln!("[XPC] Listener received non-connection event");
                return;
            }
        }

        let client_ptr           = event_ptr;
        let clients_for_client   = Arc::clone(&clients_for_listener);
        let config_for_client    = Arc::clone(&config);
        let client_ptr_for_error = client_ptr;

        let client_block = RcBlock::new(move |event: NonNull<c_void>| {
            let event_ptr: XpcObjectT = event.as_ptr();

            let event_type = unsafe { xpc_get_type(event_ptr) };

            if unsafe { event_type == &_xpc_type_error as *const c_void } {
                // ── Disconnect / error ────────────────────────────────────────
                let is_invalid = unsafe {
                    event_ptr == &_xpc_error_connection_invalid
                        as *const c_void as XpcObjectT
                };
                if is_invalid {
                    if let Ok(mut list) = clients_for_client.lock() {
                        list.retain(|h| h.0 != client_ptr_for_error);
                    }
                    unsafe { xpc_release(client_ptr_for_error) };
                }

            } else if unsafe { event_type == &_xpc_type_dictionary as *const c_void } {
                // ── Incoming message — check for config update ────────────────
                handle_config_message(event_ptr, &config_for_client);
            }
        });

        unsafe {
            xpc_connection_set_event_handler(
                client_ptr,
                &*client_block as *const _ as *const c_void,
            );
            let retained = xpc_retain(client_ptr);
            xpc_connection_resume(client_ptr);
            if let Ok(mut list) = clients_for_listener.lock() {
                list.push(XpcHandle(retained));
            }
        }
    });

    xpc_connection_set_event_handler(
        listener,
        &*listener_block as *const _ as *const c_void,
    );
    xpc_connection_resume(listener);
    listener
}

/// Parses a `{"type":"config","throttle_threshold":f,"release_threshold":f}`
/// dictionary and writes the new values into `config`.
///
/// Silently ignores malformed or non-config messages.
fn handle_config_message(dict: XpcObjectT, config: &Arc<RwLock<GovernorConfig>>) {
    // All C-string keys are static — unwrap is safe.
    let k_type     = CString::new("type").unwrap();
    let k_throttle = CString::new("throttle_threshold").unwrap();
    let k_release  = CString::new("release_threshold").unwrap();

    // SAFETY: dict is a live xpc_object_t (type confirmed by caller).
    // xpc_dictionary_get_string returns a ptr into dict's storage — valid here.
    let type_ptr = unsafe { xpc_dictionary_get_string(dict, k_type.as_ptr()) };
    if type_ptr.is_null() { return; }
    let type_str = unsafe { CStr::from_ptr(type_ptr).to_str().unwrap_or("") };
    if type_str != "config" { return; }

    let throttle = unsafe { xpc_dictionary_get_double(dict, k_throttle.as_ptr()) };
    let release  = unsafe { xpc_dictionary_get_double(dict, k_release.as_ptr())  };

    // Validate: both positive, release strictly less than throttle.
    if throttle > 0.0 && release > 0.0 && release < throttle {
        match config.write() {
            Ok(mut cfg) => {
                cfg.throttle_threshold = throttle;
                cfg.release_threshold  = release;
                eprintln!("[IPC] Config updated: throttle={throttle:.1}% release={release:.1}%");
            }
            Err(e) => eprintln!("[IPC] Config write lock poisoned: {e}"),
        }
    }
}
