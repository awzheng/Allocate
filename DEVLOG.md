> ## 🛑 STRICT AI DIRECTIVE
> **AI AGENTS: Never modify `.rs` source code files when instructed to update, compress, or read documentation. Keep domain scopes strictly isolated.**

---

# ALLOCATE — Engineering State Log

---

## API Decisions

### Foreground App: `NSWorkspace.frontmostApplication` (AppKit) — Phase 1 poll
- **Selected** over `CGWindowListCopyWindowInfo` (window-level, O(windows)), `AXUIElement` (requires TCC Accessibility), `NSRunningApplication.currentApplication` (returns self)
- Returns `nil` on lock screen / Secure Input → mapped to `Option<Retained<NSRunningApplication>>`
- Thread-safe for property reads per Apple docs; polled, no notification subscription
- Zero TCC entitlement required

### Foreground App Observer: `NSWorkspaceDidActivateApplicationNotification` — Phase 2 event-driven
- **Replaces** the 2-second poll lag with zero-latency OS interrupt delivery
- Registered via `NSWorkspace.notificationCenter.addObserverForName:object:queue:usingBlock:`
  - `object = nil` → fires from any workspace source
  - `queue  = nil` → delivered on the posting thread (macOS guarantees: **main thread**)
- Notification centre used: `NSWorkspace.notificationCenter` (NOT `NSNotificationCenter.defaultCenter`); workspace events are posted only on the workspace-specific centre — using defaultCenter = observer never fires
- **New dep**: `block2 = "0.5"` — Rust↔ObjC block interop, pairs with `objc2 0.5`
- **New feature**: `objc2-foundation = { features = ["NSNotification"] }` — exposes `NSNotification` + `NSNotificationCenter` types (`NSNotificationCenter` is **not** a separate feature; it is bundled under `NSNotification`)
- `addObserverForName:usingBlock:` is **not** wrapped in `objc2-foundation 0.2` typed bindings (block-based methods require a crate-level `block2` feature flag that 0.2 does not expose for this class) → called via `msg_send!` directly
- **Unsafe memory guarantees** for the closure:
  1. `move` closure with zero captures → `'static`, heap-safe, no borrow lifetimes
  2. `RcBlock::new` → heap-allocates and ARC-manages the block; not freed while NC holds reference via token
  3. `NonNull<NSNotification>` parameter (not `&NSNotification`) → avoids unsupported reference-lifetime trait bounds documented in block2 0.5; pointer valid for duration of call; not stored
  4. Return value is autoreleased +0; `Retained::retain(token)` bumps to +1 before autorelease pool drain; NC also holds its own +1 reference internally — block keeps firing after caller retains
  5. Token stored as `let _observer` in `main()` (`_` prefix: suppresses unused-variable lint without dropping) → lives for the process lifetime
- **Phase 3 update**: block now captures `mpsc::Sender<AppSwitchSignal>` (moved, `'static`, `Send`); extracts `name` + `pid` from notification `userInfo[NSWorkspaceApplicationKey]` via `msg_send!`; sends `AppSwitchSignal` to worker thread over channel
- Falls back to `get_frontmost_app()` polling if userInfo extraction fails (nil guard)
- Worker thread replaced polling sleep with `rx.recv()` — wakes exactly on switch


### CPU / RAM / Threads: `proc_pidinfo(PROC_PIDTASKINFO)` (BSD libproc) — Phase 1+3
- **Selected** over `sysctl`, Mach `task_info`, shell-out to `ps`/`top`
- `PROC_PIDTASKINFO` (flavor=4) → `struct proc_taskinfo` (96 bytes) → three fields now exposed:
  - `pti_total_user + pti_total_system` → cumulative CPU ns (diff two snapshots → instantaneous %)
  - `pti_resident_size` → resident RAM bytes → formatted via `format_ram()` (MB/GB)
  - `pti_threadnum` → thread count (instantaneous)
- All three extracted in **one** `proc_pidinfo` call per process — zero extra syscalls vs Phase 1
- `CpuSnapshot` type changed: `HashMap<i32, (String, u64, u64, i32)>` (name, cpu_ns, resident_bytes, threadnum)
- `compute_top_cpu` return type changed: `Vec<ProcessMetrics>` (struct with all four fields)

### Battery: `IOPowerSources` (IOKit + CoreFoundation raw FFI) — Phase 4
- **Selected** over direct IOREG (`IORegistryEntryCreateCFProperties`) — IOPowerSources aggregates the `AppleSmartBattery` node automatically without requiring tree navigation or per-hardware key paths
- `pmset -g batt` calls the same API; we bind directly — no fork+exec
- Zero TCC requirement; works in the default sandbox
- Fields used: `kIOPSCurrentCapacityKey` (int), `kIOPSMaxCapacityKey` (int), `kIOPSIsChargingKey` (CFBoolean) → `pct = Current/Max × 100`
- Type filter: only `Type == "InternalBattery"` entries processed; UPS and AC sources skipped
- Returns `None` on desktops (no internal battery); callers display nothing
- **CFRelease memory contract (Create Rule):**

| Object | Source | Release? |
|--------|--------|----------|
| `blob` (CFTypeRef) | `IOPSCopyPowerSourcesInfo()` | ✅ `CfRelease(_blob)` RAII guard |
| `list` (CFArrayRef) | `IOPSCopyPowerSourcesList(blob)` | ✅ `CfRelease(_list)` RAII guard |
| `desc` (CFDictionaryRef) | `IOPSGetPowerSourceDescription()` | ❌ Get Rule — lifetime tied to `blob` |
| CF keys (CFStringRef ×5) | `CFStringCreateWithCString()` | ✅ each wrapped in `CfRelease(_kX)` |
| Dict values (CFTypeRef) | `CFDictionaryGetValue()` | ❌ Get Rule — do not release |

- `CfRelease` is a Rust RAII struct: `impl Drop { CFRelease(ptr) }` — releases even on early-return paths, enforced by Rust's single-owner Drop rule (prevents double-free)

### IPC Bridge: raw `libxpc` C FFI — Phase 5
- **Selected** raw `libxpc` (via `#[link(name="System")]`) over:
  - `xpc-sys` crate — adds external dep for what is available in libSystem
  - `NSXPCListener` (objc2) — requires defining a typed ObjC protocol in Rust; overly complex for Phase 5
  - Mach port direct (`bootstrap_register`) — deprecated; requires private SPI
- **Mach service name**: `com.andrewzheng.allocate.daemon`
  - Registered by launchd plist at `/Library/LaunchDaemons/`; daemon degrades to no-op if absent
- **Message format**: `xpc_dictionary_t` with single key `"payload"` → UTF-8 table string
- **Listener flow**: `xpc_connection_create_mach_service(LISTENER)` → GCD concurrent queue → `RcBlock` event handler (block2) → per-client `xpc_retain` + handler registration → `xpc_connection_resume`
- **Unsafe memory guarantees**:
  1. `XpcHandle(*mut c_void)` newtype implements `Send` — safe because libxpc serialises all operations on `xpc_connection_t` internally
  2. `xpc_retain(client)` called immediately on each incoming connection before storing in `Vec`; matching `xpc_release` in the `XPC_ERROR_CONNECTION_INVALID` handler
  3. `xpc_dictionary_create_empty` (Create Rule, +1) released via `xpc_release(msg)` after `xpc_connection_send_message` in `broadcast()`; XPC retains its own reference for delivery
  4. Listener `xpc_connection_t` assigned to `_listener_live_forever` (binding, not drop) — raw pointers are `Copy` so `mem::forget` is a no-op; explicit assignment documents the intentional process-lifetime leak

---

### Swift XPC Client: raw C XPC API from Swift — Phase 6
- **Selected** raw C XPC API (`xpc_connection_create_mach_service`, `xpc_connection_set_event_handler`) over `NSXPCConnection` — the latter requires a typed ObjC/Swift protocol; for a unidirectional listen-only channel a zero-protocol raw approach eliminates boilerplate
- `xpc_connection_create_mach_service(name, nil, 0)` — client side, no `LISTENER` flag; **safe without a launchd session** (does not trap; resolves lazily on first message)
- Event handler receives `xpc_object_t`; type-checked via `xpc_get_type()` against `XPC_TYPE_DICTIONARY` and `XPC_TYPE_ERROR` singletons
- `xpc_dictionary_get_string(event, "payload")` extracts the UTF-8 table string the Rust daemon sends
- Threading: XPC delivers events on an internal queue; `Task { @MainActor in … }` hops to the main thread before updating `@Observable` properties so SwiftUI's observation graph sees changes on the main run loop
- `XPC_ERROR_CONNECTION_INTERRUPTED` → daemon restarted; XPC reconnects automatically on next event
- `XPC_ERROR_CONNECTION_INVALID` → daemon not running; UI shows "Daemon not running" state
- App Sandbox must be **disabled** in Xcode → Signing & Capabilities; sandbox blocks third-party Mach bootstrap lookups

---

## Memory Safety Contracts

### `MaybeUninit<ProcTaskInfo>` (`process.rs::read_task_info`)
- Struct never read before kernel write; `assume_init()` only called when `ret == expected` (96 bytes)
- Without guard: UB if process exits mid-call → kernel writes partial struct
- `#[repr(C)]` enforced: Rust field reordering would misalign kernel-written bytes

### `ProcBsdShortInfo` (`process.rs`) — added in EPERM fix
- Mirrors `struct proc_bsdshortinfo`; 64 bytes; `pbsi_comm[16]` = short name NUL-terminated
- `assume_init()` guarded by `ret >= expected` (64 bytes)

### `saturating_sub` / `saturating_add`
- `delta_cpu = cpu2.saturating_sub(cpu1)` — u64 wrap impossible in 2s window but documents intent
- Eliminates both debug-mode panic and release-mode silent wrap

### `autoreleasepool` (`frontmost.rs`)
- Non-main threads have no implicit pool; without explicit drain, `frontmostApplication` auto-released objects accumulate indefinitely
- `Retained<NSRunningApplication>`: ARC-mirroring smart pointer, inc on construction, dec on Drop, no double-free

### Two-phase PID enumeration TOCTOU
- Over-allocate by 16 slots; `buf.retain(|&pid| pid > 0)` strips kernel zero-sentinels

---

## TCC Requirements

| API | Permission |
|-----|-----------|
| `NSWorkspace.frontmostApplication` | None |
| `proc_listpids(PROC_ALL_PIDS)` | None |
| `proc_name(pid)` | None (own-UID); EPERM (root-owned) |
| `proc_pidinfo(PROC_PIDTASKINFO)` | None (own-UID); EPERM (root-owned) |
| `proc_pidinfo` for ALL procs | root or `com.apple.private.cs.debugger` |

**Developer Tools** (System Settings → Privacy → Developer Tools → add Terminal): allows `proc_pidinfo` on more system processes without full root.

---

## Bug Fix Log

*Note: Phase 1-5 Rust daemon bug history (BUG-001 to BUG-005) has been archived to conserve context. Below are the active Phase 6 Swift/XPC invariants.*

### BUG-006: Swift `@MainActor` Concurrency Clash in XPCClient — Phase 6
**Symptom**: Xcode throws Swift 5.10 strict concurrency errors: `Main actor-isolated property 'connection' can not be referenced from a nonisolated context` and `Call to main actor-isolated instance method 'handleEvent' in a synchronous nonisolated context`.\
**Root cause**: `XPCClient` is marked `@MainActor` to safely drive SwiftUI `@Observable` redraws, but `xpc_connection_set_event_handler` executes its block on a background GCD queue (nonisolated). The background block attempted to synchronously hop into the `@MainActor` `handleEvent` method and the nonisolated `deinit` attempted to read the isolated `connection` property, violating Swift's strict actor isolation boundaries.\
**Fix** (`allocate-ui/XPCClient.swift`): Refactored the event handler closure to safely cross the actor boundary. `xpc_get_type` and C-string pointer parsing operations now run natively in the background closure. We explicitly hop back to the UI thread via `Task { @MainActor [weak self] in ... }` exclusively for the final state writes. To fix the `deinit` isolation error, the `connection` variable was marked `nonisolated(unsafe)`.

### BUG-007: `NSXPCConnection` vs raw `libxpc` Protocol Mismatch + Floating HUD
**Symptom**: `NSXPCConnection` connects but immediately invalidates; the UI sits stuck in the "Daemon not running" state.\
**Root cause**: The Rust daemon (Phase 5) was explicitly built using raw C `libxpc` primitives (`xpc_dictionary_t`). `NSXPCConnection` (Foundation) uses `NSInvocation` protocols under the hood and operates at a completely different abstraction layer; it cannot handshake with a raw XPC dictionary server natively.\
**Fix** (`allocate-ui/XPCClient.swift`): `NSXPCConnection` was completely ripped out. The Swift client was rewritten to use the raw C `xpc_connection_create_mach_service` API (via the `XPC` framework, imported implicitly through `Foundation`). The client now perfectly matches the Rust endpoint, listening for `XPC_TYPE_DICTIONARY` and extracting the C-string via `xpc_dictionary_get_string`.\
**HUD Pivot** (`AllocateApp.swift`): The transient `MenuBarExtra` popover was closing instantly on app switch, making live telemetry testing impossible. It was refactored into a custom `NSPanel` via an `AppDelegate`. The window level is set to `.floating` and its style mask uses `.nonactivatingPanel` — creating a persistent, transparent Floating HUD that stays on screen across all spaces without stealing system focus.

### BUG-008: Raw `libxpc` Handshake Invalidation — Phase 6
**Symptom**: The Swift `allocate-ui` app instantly drops into the "Daemon not running" state (`XPC_ERROR_CONNECTION_INVALID`). The Rust daemon logs normally but the XPC bridge never establishes.\
**Root cause**: In `ipc.rs`, the XPC listener event handler blindly assumed every incoming event was a valid `xpc_connection_t` client, immediately calling `xpc_connection_set_event_handler` and `xpc_connection_resume` on it. If `launchd` passed an error event to the listener block, the C-FFI operations operated on an invalid object, corrupting the XPC bridge and instantly invalidating the client's handshake attempt.\
**Fix** (`allocate-core/src/ipc.rs`): Added the `xpc_get_type` and `_xpc_type_connection` FFI bindings. The `listener_block` now explicitly checks `xpc_get_type(event_ptr) == &_xpc_type_connection` before treating the event as a new client connection. Furthermore, aggressive runtime logging was injected into the Swift `XPCClient` event loop to trace any future undocumented `libxpc` type anomalies.

### BUG-009: Swift FFI C-Pointer Mutability Crash — Phase 6
**Symptom**: Swift Compiler (`allocate-ui`): `Cannot convert value of type 'UnsafeMutablePointer<CChar>' to expected argument type 'UnsafePointer<CChar>'` and `Warning: 'nonisolated(unsafe)' has no effect on property...`\
**Root cause**: Swift's strict FFI C-pointer mutability rules rejected passing the mutable pointers returned by `xpc_dictionary_get_string` and `xpc_copy_description` directly into `String(cString:)`, which mandates an immutable `UnsafePointer`. Additionally, the Swift 6 compiler flagged `nonisolated(unsafe)` as redundant overkill for a generic read-only connection property.\
**Fix** (`allocate-ui/XPCClient.swift`): Explicitly wrapped the mutable C-pointers in `UnsafePointer(...)` before parsing. Downgraded the strictness tag on the connection property to just `nonisolated`.

### BUG-010: Swift Strict Concurrency & Optional Binding Rejection — Phase 6
**Symptom**: Swift Compiler (`allocate-ui`): `'nonisolated' cannot be applied to mutable stored properties` and `Initializer for conditional binding must have Optional type, not 'UnsafeMutablePointer<CChar>'`.\
**Root cause**: Swift 6 strict concurrency explicitly forbids marking a `var` (mutable state) as `nonisolated` because it cannot guarantee thread-safe writes across actor boundaries. Secondly, the C-pointers returned by `xpc_dictionary_get_string` and `xpc_copy_description` bridge into Swift as explicitly non-optional types in this specific FFI context, causing `if let` conditional unwrapping to fail compilation.\
**Fix** (`allocate-ui/XPCClient.swift`): Changed the `connection` property from `var` to `let`, making it an immutable, thread-safe constant that legally satisfies the `nonisolated` tag. Ripped out the `if let` bindings and replaced them with direct assignment and explicit `!= nil` checks before wrapping the C-pointers.

### BUG-011: Swift 6 FFI Optional Unwrapping & Actor Isolation — Phase 6
**Symptom**: Swift Compiler (`allocate-ui`): `'nonisolated' cannot be applied to mutable stored properties` and `Value of optional type 'UnsafePointer<CChar>?' must be unwrapped`.\
**Root cause**: Swift 6 strict concurrency isolation rules dictate that `xpc_connection_t` is not `Sendable`; therefore it cannot be marked `nonisolated` inside a rigorously isolated `@MainActor` class, regardless of whether it is a `let` constant. Secondly, the explicitly-cast `UnsafePointer(ptr)` in BUG-009 stripped the native optionality out of the C-pointer, causing the compiler to fail on `nil` evaluation.\
**Fix** (`allocate-ui/XPCClient.swift`): Removed the `nonisolated` tag from the `connection` property, implicitly scoping it safely to the `@MainActor`. As a result, the nonisolated `deinit` block can no longer access it, so explicit `xpc_connection_cancel` was removed (the connection drops cleanly upon process termination). For the FFI pointers, the explicit `UnsafePointer` casting was reverted because `xpc_dictionary_get_string` natively bridges back to optional `UnsafePointer<CChar>?` in this specific SDK version, allowing standard `if let` conditional bindings to function perfectly.

### BUG-012: Swift `@Observable` Macro & Static Context Trap — Phase 6
**Symptom**: Swift Compiler (`allocate-ui`): `Instance member 'payload' cannot be used on type 'XPCClient'` and further stricter mutability warnings on `connection` via the `ObservationTracked` macro expansion.\
**Root cause**: The `connect()` method was moved to a `static` context during earlier refactoring, causing `[weak self]` in the background closure to capture the class type itself instead of the instance, resulting in illegal instance property access. Concurrently, Swift's `@Observable` macro secretly rewrites all properties into implicitly mutated backing storage (`_connection`). This hidden mutability violated the STRICT concurrency boundaries established earlier for the C-pointer FFI handle, breaking the compile.\
**Fix** (`allocate-ui/XPCClient.swift`): Tagged the `connection` property with `@ObservationIgnored` to explicitly prevent the macro from rewriting it. The connection instantiation was moved out of the initializer and into an explicit `.start()` instance method. This cleanly resolved the static context capture trap, allowing the background closure to safely bind to the class instance across the actor boundary.

### BUG-014: Cursor FFI Syntax Lockdown — Phase 6
**Symptom**: Swift Compiler (`allocate-ui`): `Initializer for conditional binding must have Optional type` on `xpc_dictionary_get_string` call sites (when using a plain `let` extraction), and `Main actor-isolated property 'connection' can not be referenced from a nonisolated context` emitted from the hidden `ObservationTracked` macro expansion.\
**Root cause**: Prior agents hallucinated two things simultaneously: (1) they allowed the `@Observable` macro to reach the `connection` property — synthesising a hidden `nonisolated var _connection` ObservationTracked backing that Swift 6 strict concurrency forbids on a `@MainActor`-isolated class — and (2) they used inconsistent C-pointer extraction patterns (`let ptr` without optional guard), which crashes when the key is absent because no nil-check is in the hot path.\
**Fix** (`allocate-ui/XPCClient.swift`):
- **Observation macro bypass**: `connection` is declared `@ObservationIgnored private var connection: xpc_connection_t? = nil`. The `nonisolated` keyword is **intentionally absent** — `@ObservationIgnored` makes the property entirely invisible to the macro, eliminating all synthesised backing storage and the resulting Swift 6 actor isolation violation. This is hardcoded as a non-negotiable invariant.
- **C-pointer mutability casts hardcoded**: All `xpc_dictionary_get_string` and `xpc_copy_description` call sites now use the locked-in pattern `if let rawPtr = <fn>(...) { let s = String(cString: UnsafePointer(rawPtr)) }`. The `UnsafePointer(_:)` cast satisfies `String(cString:)`'s immutable-pointer requirement; the `if let` guard handles SDK optionality without a force-unwrap crash. `xpc_copy_description` additionally calls `free(rawPtr)` (Create Rule). These patterns are not to be altered by future agents.

### BUG-013: Claude Code FFI Rewrite — Phase 6
**Symptom**: Swift Compiler (`allocate-ui`): `Instance member 'payload' cannot be used on type 'XPCClient'`, `'nonisolated' cannot be applied to mutable stored properties` (emitted from the `ObservationTracked` macro expansion on `connection`), and `Initializer for conditional binding must have Optional type, not 'UnsafeMutablePointer<CChar>'` on every `xpc_dictionary_get_string` call site.\
**Root cause**: A prior AI agent applied a series of hallucinated incremental fixes that left the file in an internally contradictory state. Three root faults remained: (1) The `@Observable` macro was still allowed to synthesise `ObservationTracked` backing storage for `connection`, generating hidden `nonisolated var _connection` — illegal for mutable state on a `@MainActor` class under Swift 6 strict concurrency. (2) A residual `static`-context capture meant `[weak self]` inside `xpc_connection_set_event_handler` resolved to the *type* rather than the instance, making instance-member access illegal at compile time. (3) The macOS SDK imports `xpc_dictionary_get_string` as a non-optional `UnsafeMutablePointer<CChar>` (not an IUO); `if let` conditional binding is a type error on a non-optional, so every extraction call failed to compile.\
**Fix** (`allocate-ui/XPCClient.swift`): XPCClient.swift was rewritten from scratch with three precise architectural corrections:
- **Macro bypass**: `connection` is declared `@ObservationIgnored private var connection: xpc_connection_t? = nil`. The `nonisolated` keyword is intentionally absent — `@ObservationIgnored` makes the property invisible to the macro entirely, eliminating all synthesised backing storage and the resulting Swift 6 violation.
- **Static capture trap**: All connection logic lives in a `func start()` instance method. `[weak self]` in the `xpc_connection_set_event_handler` block now correctly captures the live instance, not the metatype.
- **Non-optional C FFI**: `xpc_dictionary_get_string` calls use `let ptr = xpc_dictionary_get_string(event, key)` followed by `String(cString: UnsafePointer(ptr))`. The `UnsafePointer` wrapper satisfies `String(cString:)`'s immutable-pointer signature without any conditional binding. `xpc_copy_description` retains `if let` as it is imported as `UnsafeMutablePointer<CChar>!` (IUO) — a distinct SDK signature where optional binding is legal.\
**Fix** (`allocate-ui/AllocateApp.swift`): `AppDelegate` was annotated `@MainActor`. This makes calls to `XPCClient.start()` (a `@MainActor`-isolated method) from `applicationDidFinishLaunching` legally synchronous under Swift 6. AppKit guarantees `applicationDidFinishLaunching` executes on the main thread, so the annotation is semantically correct with zero runtime cost.

---


## CPU Utilisation Formula

```
cpu_pct(proc) = ( snap2.cpu_ns - snap1.cpu_ns ) / elapsed_window_ns × 100
```
- Values >100% valid on multi-core (e.g. 800% = 8 cores saturated)
- Noise floor: `pct < 0.1` filtered; display: `{:.1}%`
- Sample window: 500 ms (only remaining sleep; polling budget eliminated in Phase 3)

---

## KPI Status (Phase 1–3)

| KPI | Target | Status |
|-----|--------|--------|
| CPU overhead | <1% | ✅ |
| Memory | <15MB | ✅ |
| Zero panics | Yes | ✅ |
| Zero leaks | Yes | ✅ |
| State-change-only output | Yes | ✅ |
| CPU readings non-zero | Yes | ✅ Fixed (BUG-001) |
| Initial app reported | Yes | ✅ Fixed (BUG-002) |
| RAM + threads in output | Yes | ✅ Phase 3 |
| Zero poll-sleep latency | Yes | ✅ Phase 3 (mpsc) |

---

## Architecture: Phase 3 Event-Driven Model

```
main():
  _app      = NSApplication::sharedApplication()   // opens Window Server Mach port
  (tx, rx)  = mpsc::channel::<AppSwitchSignal>()   // signal bridge
  _observer = register_app_switch_observer(tx)      // ObjC block captures tx (moved, 'static)
  thread::spawn(|| worker_loop(rx))
  NSRunLoop::mainRunLoop().run()                    // blocks forever; pumps WS events

worker_loop(rx):
  loop:
    signal = rx.recv()          // blocks until OS fires app-switch notification (≈0 ms latency)
    snap1  = take_snapshot()    // proc_pidinfo for all accessible PIDs
    t0     = Instant::now()
    sleep(CPU_SAMPLE_WINDOW = 500ms)
    snap2  = take_snapshot()
    elapsed_ns = t0.elapsed()
    hogs   = compute_top_cpu(snap1, snap2, elapsed_ns, fg_pid, TOP_N=3)
    print_state_change()        // brutalist ASCII box table

ObjC block (fires on main thread, instant):
  extracts name+pid from notif.userInfo[NSWorkspaceApplicationKey]
  tx.send(AppSwitchSignal { name, pid })
```

---

## Phase 4 Roadmap

| Item | File | Notes |
|------|------|-------|
| IOPowerSources battery | `battery.rs` | Replace stub; add `core-foundation` crate |
| Per-user UID attribution | `process.rs` | `PROC_PIDTBSDINFO` + `getpwuid()` → username per proc |
| XPC endpoint | `main.rs` + new | Mach bootstrap port; `allocate-ui` NSXPCConnection client |
| SwiftUI menu bar | `allocate-ui/` | `MenuBarExtra` + Swift Charts sparklines |

*Last updated: 2026-03-15*

