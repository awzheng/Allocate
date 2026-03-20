> ## 🛑 STRICT AI DIRECTIVE
> **AI AGENTS: Never modify `.rs` source code files when instructed to update, compress, or read documentation. Keep domain scopes strictly isolated.**

---

# Allocate — Run Guide
**Updated**: 2026-03-16

---

## What Allocate Does

Every time you switch apps, `allocate-core` wakes up **instantly** and prints a hardware telemetry table to the terminal:

```
┌──────────────────────────────────────────────────────────
│ 🟢 ACTIVE: Xcode (PID: 25172) | 🔋 82% (Battery)
├──────────────────────────────────────────────────────────
│ ⚠️  BACKGROUND HOGS
│  1. Google Chrome Helper   | CPU:   0.5% | RAM: 450 MB | Threads: 32
│  2. UniversalControl       | CPU:   0.2% | RAM:  85 MB | Threads: 12
│  3. Docker                 | CPU:   0.1% | RAM: 2.1 GB | Threads: 18
└──────────────────────────────────────────────────────────
```

It also forwards this table string over **XPC** to any connected `allocate-ui` SwiftUI client.

---

## Mental Model: How It Works

### Three concurrent actors

```
┌─────────────────────────────────────────────────────────────────┐
│  Main thread                                                    │
│  NSApplication (opens Window Server session)                    │
│  ↓ registers ObjC block on NSWorkspace notification centre      │
│  NSRunLoop.run() — pumps Window Server events forever           │
│               │                                                 │
│         App switch happens                                      │
│               │                                                 │
│    Block fires on main thread instantly                         │
│    Extracts name + PID from notification userInfo               │
│    tx.send(AppSwitchSignal { name, pid })  ──────────────────── │
└──────────────────────────────────────────────────────│──────────┘
                                                       │ mpsc channel
┌──────────────────────────────────────────────────────▼──────────┐
│  Worker thread                                                  │
│  rx.recv() ← blocks here until signal arrives                   │
│  take_snapshot()  ← proc_pidinfo syscall for all processes      │
│  sleep 500 ms                                                   │
│  take_snapshot()  ← second syscall                              │
│  compute diff → CPU% | RAM | Threads per process                │
│  get_battery_state() ← IOKit IOPowerSources                     │
│  build_table() → String                                         │
│  print!() + broadcaster.broadcast()  ────────────────────────── │
└──────────────────────────────────────────────────────│──────────┘
                                                       │ XPC dict
┌──────────────────────────────────────────────────────▼──────────┐
│  XPC listener (GCD thread pool — no Rust thread)                │
│  com.andrewzheng.allocate.daemon                                │
│  Accepts NSXPCConnection from allocate-ui                       │
│  Sends {"payload": "<table string>"} to each client             │
└─────────────────────────────────────────────────────────────────┘
```

### Why NSRunLoop on the main thread?
`NSWorkspace.frontmostApplication` only returns live data when the main thread is
running its `NSRunLoop`. Without it, the Window Server cache freezes. So the main
thread **only** runs the event loop — all sleeping and computation happens on the worker.

### Why mpsc instead of polling?
Old design: worker thread slept 2 seconds then polled. Every switch had a ≤2 s lag.
New design: the OS fires a notification the moment the switch happens. The ObjC block
sends it to the worker via `mpsc::Sender`. No sleep between switches at all.

### The 500 ms window
CPU % requires two snapshots. We take one, sleep 500 ms, take another, then diff.
This 500 ms gap is the **only** sleep remaining in the entire architecture.

### Battery
`get_battery_state()` calls `IOPSCopyPowerSourcesInfo()` + `IOPSCopyPowerSourcesList()`
from Apple's IOKit framework — the same underlying API as `pmset -g batt`. No
TCC permission required. Returns `None` on desktops without a battery (no badge shown).

---

## Prerequisites

```bash
# Verify Rust toolchain
~/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc --version

# Add to ~/.zshrc (then: source ~/.zshrc)
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"

cargo --version   # confirms cargo is accessible
```

---

## Testing the Active Governor (Dummy Hog)

### Step 1 (The Target)
Open **Terminal A** and launch the intentional CPU hog:

```bash
cd /Users/andrewzheng/Allocate
cargo run -p dummy-hog
```

*This safely spikes CPU usage to roughly 100% on a dedicated background process without destabilizing the system.*

### Step 2 (The Governor)
Open **Terminal B** and run the core daemon with root privileges:

```bash
cd /Users/andrewzheng/Allocate
sudo cargo run -p allocate-core
```

> **Why `sudo`?** The governor requires root authority to issue `SIGSTOP` and `SIGCONT` signals to other processes, as well as to reliably read full `proc_pidinfo` telemetry across the system.

### Step 3 (The Trigger)
The daemon is now armed. **Switch focus to any other app** (e.g., click on Chrome or the Finder desktop). 

This instantly triggers the background worker loop. In **Terminal B**, the brutalist telemetry table will appear, followed immediately by the active mitigation action:

```
[GOV] SIGSTOP (freeze) → pid <N>
```

*The dummy-hog process is now suspended indefinitely via a kernel-level freeze. If you check Activity Monitor, its status will read "Stopped".*

*(To exit, press **Ctrl+C** in Terminal B. If dummy-hog is left frozen, you can manually resume it with `kill -CONT <pid>`).*

### Mode 2: LaunchAgent + XPC (production / allocate-ui integration)

Run the install script **once**:

```bash
chmod +x scripts/install_agent.sh
./scripts/install_agent.sh
```

The script:
1. Builds the release binary (if needed)
2. Writes `~/Library/LaunchAgents/com.andrewzheng.allocate.daemon.plist` with the
   `MachServices` entry XPC requires and `ALLOCATE_XPC_ENABLE=1` injected via
   `EnvironmentVariables`
3. Loads the agent via `launchctl load`

Follow live output:

```bash
tail -f /tmp/allocate-daemon.log
```

The Brutalist UI table appears there whenever you switch apps.

To uninstall:

```bash
launchctl unload ~/Library/LaunchAgents/com.andrewzheng.allocate.daemon.plist
rm ~/Library/LaunchAgents/com.andrewzheng.allocate.daemon.plist
```

---

## Building allocate-ui in Xcode

### Step 1 — Create the Xcode project

1. Open **Xcode** (Spotlight → `Xcode`)
2. Click **Create New Project…** (or **File → New → Project…**)
3. Choose **macOS** tab → **App** → click **Next**
4. Fill in:
   - **Product Name**: `allocate-ui`
   - **Organization Identifier**: `com.andrewzheng`
   - **Bundle Identifier** (auto-filled): `com.andrewzheng.allocate-ui`
   - **Interface**: `SwiftUI`
   - **Language**: `Swift`
   - Uncheck **Include Tests** (not needed)
5. Click **Next**, then save **inside** `/Users/andrewzheng/Allocate/allocate-ui/`

> Xcode will create several boilerplate files. You will replace most of them.

---

### Step 2 — Replace the boilerplate Swift files

Xcode creates `allocate_uiApp.swift` (or similar) and `ContentView.swift` by default.

**Delete the Xcode-generated stubs and add the repo files:**

1. In the **Project Navigator** (left sidebar), select the boilerplate files, right-click → **Delete** → **Move to Trash**
2. Right-click the `allocate-ui` folder in the Navigator → **Add Files to "allocate-ui"…**
3. Navigate to `/Users/andrewzheng/Allocate/allocate-ui/` and add:
   - `AllocateApp.swift`
   - `XPCClient.swift`
   - `ContentView.swift`
4. Make sure **"Copy items if needed"** is **unchecked** (they are already in the right folder)
5. Confirm **"Add to target: allocate-ui"** is checked

---

### Step 3 — Disable the App Sandbox (critical for XPC)

> **Why?** macOS's App Sandbox blocks third-party Mach bootstrap lookups. Without removing it, `xpc_connection_create_mach_service` silently fails to resolve the daemon's service name and the UI never receives any data.

1. In the Navigator, click the **blue `allocate-ui` project icon** at the very top
2. Click the **`allocate-ui` target** (under TARGETS) in the center panel
3. Click the **Signing & Capabilities** tab
4. Find the **App Sandbox** capability row — click the **`–` (minus) button** on the left to remove it entirely
5. Confirm the removal in the dialog

The entitlements file should now have **no** `com.apple.security.app-sandbox` entry (or the file will be deleted entirely).

---

### Step 4 — Set the deployment target

1. Still in the project editor, click the **Build Settings** tab
2. Search for `MACOSX_DEPLOYMENT_TARGET`
3. Set it to **`13.0`** (MenuBarExtra and `@Observable` require macOS 13+)

---

### Step 5 — Configure Info.plist for Menu Bar only

To prevent a Dock icon from appearing:

1. Click **Info** tab in the target editor
2. Add a new row: key = `Application is agent (UIElement)`, value = **YES**
   (This is the `LSUIElement` key)

---

### Step 6 — Build & Run

1. Press **⌘R** (or Product → Run)
2. Xcode builds and launches `allocate-ui`
3. A frosted-glass HUD panel appears floating on your screen.

Because it is a **Floating HUD** (`NSPanel` with `.floating` level), it will stay persistently on top of all other windows even as you switch apps — perfect for live monitoring.

**If the daemon is running** (`./scripts/install_agent.sh` has been run):
- The **● Live** indicator appears green
- Switch apps → the brutalist table updates instantly in the HUD

**If the daemon is NOT running:**
- The HUD shows "Daemon not running" with a crossed-out antenna symbol
- Start it: `./scripts/install_agent.sh`

---

## Source File Map

| Layer | File | Responsibility |
|-------|------|----------------|
| **Rust daemon** | `src/main.rs` | Entry point, broadcaster wiring, `build_table()` |
| | `src/frontmost.rs` | ObjC notification observer, mpsc sender |
| | `src/process.rs` | `proc_pidinfo` CPU/RAM/thread, `compute_top_cpu` |
| | `src/battery.rs` | IOKit IOPowerSources FFI, `CfRelease` RAII |
| | `src/ipc.rs` | Raw `libxpc` listener, `IpcBroadcaster`, env guard |
| **Swift UI** | `AllocateApp.swift` | `@main`, `NSPanel` Persistent Floating HUD |
| | `XPCClient.swift` | XPC C API, `@Observable`, `@MainActor` |
| | `ContentView.swift` | `ultraThinMaterial`, SF Pro/Symbols, vibrancy |
| **Scripts** | `scripts/install_agent.sh` | LaunchAgent plist + `launchctl load` |

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| No table after startup | No app switch yet | Switch to any other app |
| All CPU% = 0% | No root access | `sudo ./target/debug/allocate-core` |
| Battery badge absent | Desktop Mac (no battery) | Expected — `None` returned |
| `[DEBUG] XPC bypassed…` | No `ALLOCATE_XPC_ENABLE` env var | Use `scripts/install_agent.sh` for XPC mode |
| allocate-ui shows "Daemon not running" | Agent not loaded | Run `./scripts/install_agent.sh` |
| allocate-ui shows "Daemon not running" (even after install) | App Sandbox still enabled | Remove App Sandbox in Xcode → Signing & Capabilities |
| Xcode build error: `'xpc_get_type' unavailable` | Deployment target < 13 | Set `MACOSX_DEPLOYMENT_TARGET = 13.0` |


