> ## STRICT AI DIRECTIVE
> **AI AGENTS: Never modify `.rs` source code files when instructed to update, compress, or read documentation. Keep domain scopes strictly isolated.**

---

# Allocate — Developer Manual & Testing Guide

---

## Core Commands

| Purpose | Command |
|---|---|
| Compile backend (release) | `cargo build --release -p allocate-core` |
| Deploy as launchd service | `sudo ./scripts/install_agent.sh` |
| Stop background service | `launchctl unload ~/Library/LaunchAgents/com.andrewzheng.allocate.daemon.plist` |
| Force-kill daemon | `sudo killall allocate-core` |
| Run daemon in foreground (live logging) | `sudo ./target/release/allocate-core` |
| Stream daemon logs | `tail -f /tmp/allocate-daemon.log` |
| Stateless E-core recovery | `sudo ./target/release/allocate-core --recover` |

---

## Throttle Mechanism

The governor calls `/usr/sbin/taskpolicy` — a SIP-compliant Apple tool — to adjust XNU QoS:

| Action | Command issued | Effect |
|---|---|---|
| Throttle | `taskpolicy -b -p <pid>` | Background QoS → process pinned to E-cores (~25% throughput) |
| Release | `taskpolicy -B -p <pid>` | Restore QoS → process returns to full P+E-core scheduling |

**Default thresholds:** throttle at **15% CPU**, release at **5% CPU** (hysteresis gap prevents oscillation). Eligibility: `uid >= 500`, not in hardcoded deny-list (`WindowServer`, `Dock`, `Finder`, `coreaudiod`, etc.).

---

## Workflow A — Synthetic Test (`dummy-hog`)

Validates the governor against a controlled 100% CPU load in a terminal environment.

1. **Terminal A — Launch the load generator:**
   ```bash
   cd /Users/andrewzheng/Allocate
   cargo run -p dummy-hog
   ```
   `dummy-hog` spins threads computing primes. CPU climbs to >100%.

2. **Terminal B — Run the daemon (foreground, live output):**
   ```bash
   cd /Users/andrewzheng/Allocate
   sudo ./target/release/allocate-core
   ```

3. **Trigger:** Press `⌘-Tab` to switch focus to any other app.

4. **Expected output in Terminal B:**
   ```
   │  1. dummy-hog               | CPU: 102.3% | RAM:  12 MB | Threads:  9 | THROTTLED
   ```
   `taskpolicy -b` fires. `dummy-hog` drops to ~25% CPU — pinned to E-cores.

5. **Cleanup:** `Ctrl+C` in Terminal B. The graceful shutdown hook calls `release_all()`, which fires `taskpolicy -B` on every throttled PID before exit. `dummy-hog` returns to full CPU.

---

## Workflow B — Real-World Test (Minecraft)

Validates the governor against a real user-space application. Minecraft's Java process (`java`) runs in windowed mode — no Metal fullscreen, no kernel graphical wake-lock.

> **Why windowed?** Fullscreen Metal apps hold a kernel display wake-lock that can bypass QoS policy changes. Always test in windowed mode to keep the throttle path clean.

1. **Deploy the daemon as a background service (if not already running):**
   ```bash
   sudo ./scripts/install_agent.sh
   tail -f /tmp/allocate-daemon.log
   ```

2. **Launch Minecraft in windowed mode.** Load a world and generate heavy CPU load: enter a dense chunk border, enable high render distance, or detonate a TNT chain. Confirm `java` exceeds 15% CPU in Activity Monitor.

3. **Trigger:** `⌘-Tab` to any other app (Terminal, Finder, etc.).

4. **Verify in the log:**
   ```
   │  1. java                    | CPU:  87.4% | RAM: 1.8 GB | Threads: 42 | THROTTLED
   ```
   Minecraft audio continues (coreaudiod is on the deny-list). Only the `java` process is pinned to E-cores.

5. **Return to Minecraft.** The frontmost-app guard immediately exempts the `java` PID from evaluation. The governor releases the throttle within one 1 Hz tick.

---

## Workflow C — Graceful Shutdown

Verifies that no process is permanently stranded on E-cores after a daemon exit.

1. **Confirm a process is throttled.** Run Workflow A or B until the THROTTLED tag appears in the table.

2. **Kill the foreground daemon with `Ctrl+C`** (or send SIGTERM to the launchd service).

3. **What happens internally:**
   - `signal_hook` sets the `AtomicBool` shutdown flag.
   - The worker loop detects it at the top of the next tick (≤100 ms).
   - `graceful_shutdown()` calls `governor.release_all()`.
   - `release_all()` fires `taskpolicy -B -p <pid>` for every PID in the `suspended` HashSet.
   - `std::process::exit(0)`.

4. **Verify in Activity Monitor:** Within one second, the previously-throttled process returns to normal scheduling. CPU% climbs back to its natural value on P+E-cores. The "Stopped" state never appears — `taskpolicy` does not suspend; it only adjusts QoS.

---

## Emergency Recovery

If the daemon was killed via `SIGKILL` before `release_all()` could execute (e.g. `sudo killall -9 allocate-core`), any throttled PIDs remain on E-cores with no running daemon to release them.

**Stateless defibrillator — no daemon required:**
```bash
sudo ./target/release/allocate-core --recover
```

Scans all running processes, fires `taskpolicy -B` on every eligible user-space PID. Idempotent — safe to run on non-throttled processes.

**Manual fallback (single PID):**
```bash
/usr/sbin/taskpolicy -B -p <pid>
```

---

## Source Map

| Layer | File | Responsibility |
|---|---|---|
| **Rust** | `src/main.rs` | Entry point, worker loop, `build_table()`, signal handling |
| | `src/frontmost.rs` | ObjC `NSWorkspace` observer, `mpsc::Sender` |
| | `src/process.rs` | `proc_pidinfo` CPU/RAM/threads, `compute_top_cpu` |
| | `src/governor.rs` | `taskpolicy` actuator, hysteresis, `GovernorConfig` |
| | `src/battery.rs` | IOKit `IOPowerSources` FFI |
| | `src/ipc.rs` | Raw `libxpc` listener, `IpcBroadcaster`, config bridge |
| **Swift** | `AllocateApp.swift` | `@main`, `NSPanel` floating HUD |
| | `XPCClient.swift` | XPC C API, `@Observable`, `@MainActor` |
| | `ContentView.swift` | `ultraThinMaterial`, SF Pro/Symbols |
| **Scripts** | `scripts/install_agent.sh` | LaunchAgent plist + `launchctl load` |

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| No table after startup | No app switch detected yet | `⌘-Tab` to any other app |
| All CPU% = 0% | Missing root | `sudo ./target/release/allocate-core` |
| THROTTLED tag never appears | Process CPU below 15% threshold | Push load past threshold, or lower it in the UI |
| Process stays throttled after daemon exit | Daemon received `SIGKILL`, skipped `release_all()` | `sudo ./target/release/allocate-core --recover` |
| allocate-ui shows "Daemon not running" | LaunchAgent not loaded | `sudo ./scripts/install_agent.sh` |
| allocate-ui shows "Daemon not running" (post-install) | App Sandbox still enabled | Xcode → Signing & Capabilities → remove App Sandbox |
| Battery badge absent | Desktop Mac, no battery | Expected — `get_battery_state()` returns `None` |
