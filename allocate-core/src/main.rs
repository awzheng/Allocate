// allocate-core/src/main.rs
//
// Entry point for the Allocate watchdog daemon.
//
// ── Architecture (Phase 8) ────────────────────────────────────────────────────
//
//  Main thread ──► NSApplication + register_app_switch_observer(tx) + NSRunLoop.run()
//
//  Worker thread ◄─ rx.recv()           — wakes exactly on app switch
//                ► take_snapshot() ×2   — 500 ms CPU window
//                ► get_battery_state()  — IOKit IOPowerSources query
//                ► build_table()        — formats the brutalist ASCII table
//                ► print!() + broadcast — terminal + XPC push to allocate-ui
//
//  XPC listener  ← GCD thread pool (libdispatch manages, no Rust thread)
//                  Accepts connections; stores retained xpc_connection_t handles.
//                  Clients receive table string as {"payload": "…"}.
//
// ── Early-exit CLI modes (no daemon loop) ────────────────────────────────────
//
//  --recover              Fire taskpolicy -B on all eligible user-space PIDs.

mod battery;
mod frontmost;
mod governor;
mod ipc;
mod process;

use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::{Arc, RwLock, atomic::{AtomicBool, Ordering}};
use std::thread;
use std::time::{Duration, Instant};

use objc2::{class, msg_send};
use objc2::runtime::AnyObject;

use battery::{get_battery_state, BatteryState};
use frontmost::{AppSwitchSignal, ForegroundApp};
use governor::{Governor, GovernorConfig, run_recovery};
use ipc::IpcBroadcaster;
use process::{compute_system_cpu_pct, compute_top_cpu, format_ram, get_frontmost_metrics, read_host_cpu_ticks, take_snapshot, CpuSnapshot, ProcessMetrics};

// ── Minimal stderr logger ─────────────────────────────────────────────────────
//
// A two-method `log::Log` implementation that writes to stderr. Zero external
// deps: avoids pulling in env_logger or similar. Only ERROR-level messages are
// emitted by default (matching the `log::set_max_level` call in `main`).

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _meta: &log::Metadata) -> bool { true }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

// ── Tuning constants ──────────────────────────────────────────────────────────

/// Width of the CPU sampling window.
const CPU_SAMPLE_WINDOW: Duration = Duration::from_millis(500);

/// How many background hogs to surface per report.
const TOP_N: usize = 20;

/// Separator line for the brutalist table borders.
const SEP: &str = "──────────────────────────────────────────────────────────";

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // ── Logger init ───────────────────────────────────────────────────────────
    //
    // Install the inline StderrLogger before any early-exit paths so that
    // log::error! calls in governor.rs (and here) produce visible output.
    // Errors are always shown; set RUST_LOG=debug to see lower-level records
    // if a richer logger is wired in later.
    log::set_logger(&LOGGER).expect("logger already set");
    // Info lets lifecycle events (startup, shutdown) surface on stderr.
    // Errors from governor.rs are always visible regardless of this level.
    log::set_max_level(log::LevelFilter::Info);

    let args: Vec<String> = std::env::args().collect();

    // ── --recover: stateless escape hatch ─────────────────────────────────────
    //
    // Runs before NSApplication / XPC / signal setup so it executes cleanly
    // even when the daemon is not registered with launchd.  Lifting the Mach
    // background policy on a non-throttled process is idempotent.
    if args.iter().any(|a| a == "--recover") {
        let n = run_recovery();
        println!("[RECOVERY] Done — lifted throttle on {} process(es).", n);
        std::process::exit(0);
    }

    // ── Step 1: Open a Window Server session ─────────────────────────────────
    //
    // NSApplication::sharedApplication() registers the process with the macOS
    // Window Server (Quartz) and installs AppKit input sources on the main
    // NSRunLoop so that run-loop iterations drain Window Server events.
    //
    // SAFETY: NSApplication is always registered once AppKit is linked.
    // sharedApplication returns the singleton; *mut AnyObject does not bump RC.
    let _app: *mut AnyObject = unsafe { msg_send![class!(NSApplication), sharedApplication] };

    print_banner();

    // ── Step 2: Arm graceful-shutdown signal handlers ─────────────────────────
    //
    // signal_hook::flag::register atomically sets the AtomicBool on SIGINT /
    // SIGTERM. This is async-signal-safe: the only operation inside the signal
    // handler is a single atomic store (SeqCst), which is async-signal-safe on
    // all POSIX platforms.
    //
    // The flag is cloned into the worker thread; main holds the other Arc so
    // the bool outlives both threads.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT,  Arc::clone(&shutdown))
        .expect("Failed to register SIGINT handler");
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
        .expect("Failed to register SIGTERM handler");

    // ── Step 3: Create the shared governor config ─────────────────────────────
    //
    // Arc<RwLock<GovernorConfig>> is the two-way XPC bridge's shared state.
    // The IPC GCD thread writes incoming config messages from the UI here;
    // the worker thread reads it at the top of each evaluate() call.
    let config = Arc::new(RwLock::new(GovernorConfig::default()));

    // ── Step 4: Start the XPC listener (GCD thread pool — no Rust thread) ────
    //
    // Passes the config arc so the GCD event handler can update thresholds
    // when the UI sends a config message. Degrades gracefully without launchd.
    let broadcaster = ipc::start_listener(Arc::clone(&config));

    // ── Step 5: Create the app-switch mpsc channel ────────────────────────────
    //
    // tx → moved into the ObjC notification block (fires on main thread).
    // rx → held by the worker thread, wakes on every app switch.
    let (tx, rx) = mpsc::channel::<AppSwitchSignal>();

    // ── Step 6: Register the NSWorkspace event-driven observer ───────────────
    //
    // Passes tx into the ObjC block ('static, moved). _observer must live for
    // the process lifetime — dropping it deregisters the observer.
    let _observer = frontmost::register_app_switch_observer(tx);

    // ── Step 7: Spawn the worker thread ──────────────────────────────────────
    let shutdown_worker = Arc::clone(&shutdown);
    thread::spawn(move || worker_loop(rx, broadcaster, shutdown_worker, config));

    // ── Step 8: Run the main NSRunLoop forever ────────────────────────────────
    //
    // The NSRunLoop is the sole event pump for AppKit on this process. It must
    // run unblocked on the main thread. Every app switch causes the Window Server
    // to post a Mach message here; the run loop drains it and fires the observer
    // block, which calls tx.send().
    //
    // On SIGINT / SIGTERM the shutdown flag is set; the worker loop detects it,
    // calls release_all(), and exits. The NSRunLoop keeps running until the OS
    // terminates the process after the signal — this is the correct macOS daemon
    // lifecycle (launchd will SIGKILL if we do not exit promptly, but release_all
    // will have already fired taskpolicy -B on all throttled PIDs).
    //
    // SAFETY: NSRunLoop is always registered once Foundation is linked.
    unsafe {
        let run_loop: *mut AnyObject = msg_send![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![run_loop, run];
    }
}

// ── Graceful shutdown ─────────────────────────────────────────────────────────

/// Called from any point in `worker_loop` when the shutdown flag is set.
///
/// Fires `taskpolicy -B` on every throttled PID so no app is permanently
/// stranded on Efficiency cores, then exits with code 0.
///
/// Return type `!` (diverges) — the function never returns to its caller.
fn graceful_shutdown(governor: &mut Governor) -> ! {
    log::info!("Caught termination signal — releasing all throttled processes and shutting down…");
    governor.release_all();
    log::info!("Clean shutdown complete.");
    std::process::exit(0);
}

// ── Worker thread ─────────────────────────────────────────────────────────────

fn worker_loop(
    rx:          mpsc::Receiver<AppSwitchSignal>,
    broadcaster: IpcBroadcaster,
    shutdown:    Arc<AtomicBool>,
    config:      Arc<RwLock<GovernorConfig>>,
) {
    let mut governor = Governor::new(config);

    // Wait for the first foreground-app signal, polling the shutdown flag every
    // 100 ms so a SIGTERM/SIGINT that arrives before any app switch is not missed.
    let mut fg = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(s) => break ForegroundApp { name: s.name, pid: s.pid },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::Relaxed) {
                    graceful_shutdown(&mut governor);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    };

    // Track Mach CPU-tick counters across ticks to compute system-wide CPU%.
    let mut prev_cpu_ticks: Option<[u32; 4]> = None;

    // 1 Hz heartbeat loop. Each iteration:
    //   1. Drain pending app-switch signals (non-blocking try_recv).
    //   2. Sample per-process CPU across a 500 ms window.
    //   3. Diff Mach tick counters for system-wide CPU%.
    //   4. Run governor, build table, broadcast.
    //   5. Sleep the remainder of the 1-second tick.
    loop {
        let tick_start = Instant::now();

        // ── Graceful shutdown check (top of tick) ────────────────────────────
        if shutdown.load(Ordering::Relaxed) { graceful_shutdown(&mut governor); }

        // ── Drain any pending app-switch signals (non-blocking) ───────────────
        while let Ok(signal) = rx.try_recv() {
            fg = ForegroundApp { name: signal.name, pid: signal.pid };
        }

        // ── Two proc_pidinfo snapshots across a 500 ms CPU sampling window ────
        //
        // recv_timeout() replaces thread::sleep() so:
        //   • A SIGINT/SIGTERM during the wait is detected within this tick
        //     rather than waiting up to 1 s for the next top-of-loop check.
        //   • An app-switch that arrives mid-sample updates fg immediately.
        //   • elapsed_ns is the actual measured interval, not a constant,
        //     so CPU% stays accurate even when recv_timeout returns early.
        let snap1: CpuSnapshot = take_snapshot();
        let sample_t0 = Instant::now();
        match rx.recv_timeout(CPU_SAMPLE_WINDOW) {
            Ok(signal) => fg = ForegroundApp { name: signal.name, pid: signal.pid },
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if shutdown.load(Ordering::Relaxed) { graceful_shutdown(&mut governor); }
        let snap2: CpuSnapshot = take_snapshot();
        let elapsed_ns = sample_t0.elapsed().as_nanos() as u64;

        // ── System-wide CPU% via Mach host_statistics tick diff ───────────────
        let curr_ticks = read_host_cpu_ticks();
        let system_cpu = match (&prev_cpu_ticks, &curr_ticks) {
            (Some(prev), Some(curr)) => compute_system_cpu_pct(prev, curr),
            _ => 0.0,
        };
        prev_cpu_ticks = curr_ticks;

        // ── Per-process hogs + governor ───────────────────────────────────────
        // compute_top_cpu excludes fg.pid so the governor never evaluates it.
        let mut hogs: Vec<ProcessMetrics> =
            compute_top_cpu(&snap1, &snap2, elapsed_ns, Some(fg.pid), TOP_N);

        if !governor.is_enabled() {
            // Standby mode: release any throttled PIDs immediately, then skip
            // evaluation for this tick.  has_throttled() prevents redundant
            // taskpolicy -B spawns every second once the set is already empty.
            if governor.has_throttled() {
                governor.release_all();
            }
        } else {
            governor.evaluate(&hogs);
        }

        // ── Annotate hogs with governor state ─────────────────────────────────
        // forced_pid_sets() takes one read-lock; avoids per-hog lock churn.
        let (forced_e, forced_p) = governor.forced_pid_sets();
        for h in &mut hogs {
            h.is_throttled = governor.is_throttled(h.pid);
            h.is_forced_e  = forced_e.contains(&h.pid);
            h.is_forced_p  = forced_p.contains(&h.pid);
        }

        // Build comma-separated name strings for the XPC override lists.
        let e_names = build_override_list(&forced_e, &snap2);
        let p_names = build_override_list(&forced_p, &snap2);

        // ── Build display rows ────────────────────────────────────────────────
        let mut rows: Vec<ProcessMetrics> = Vec::with_capacity(hogs.len() + 1);
        if let Some(fm) = get_frontmost_metrics(fg.pid, &snap1, &snap2, elapsed_ns) {
            rows.push(fm); // is_frontmost=true, rank 0
        }
        rows.extend(hogs); // background hogs, ranks 1..N

        // IOPowerSources: fast synchronous IOKit call (≤10 ms). None on desktops.
        let batt: Option<BatteryState> = get_battery_state();

        let table = build_table(&fg, &rows, batt.as_ref());
        print!("{}", table);
        broadcaster.broadcast(&table, system_cpu, &e_names, &p_names);

        // ── Wait out the remainder of the 1-second tick ──────────────────────
        // recv_timeout serves as the sleep so a shutdown signal or an app-switch
        // that arrives during the idle window is acted on immediately.
        let remaining = Duration::from_secs(1).saturating_sub(tick_start.elapsed());
        if remaining > Duration::ZERO {
            match rx.recv_timeout(remaining) {
                Ok(signal) => fg = ForegroundApp { name: signal.name, pid: signal.pid },
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if shutdown.load(Ordering::Relaxed) { graceful_shutdown(&mut governor); }
        }
    }
}

// ── Output helpers ────────────────────────────────────────────────────────────

fn print_banner() {
    println!("═══════════════════════════════════════════════════════════");
    println!("  Allocate  ·  Hardware-Aware Workload Governor  ·  v0.1  ");
    println!("  CPU window: 500 ms  |  Top N: {}  |  Event-driven + XPC  ", TOP_N);
    println!("═══════════════════════════════════════════════════════════");
    println!("[INIT] Watchdog armed. Waiting for foreground change…\n");
}

/// Builds a comma-separated string of process names for each PID in `pids`.
/// Looks up names from the latest CPU snapshot; falls back to the PID string
/// if the process has exited between the snapshot and this call.
fn build_override_list(pids: &HashSet<i32>, snap: &CpuSnapshot) -> String {
    if pids.is_empty() { return String::new(); }
    pids.iter()
        .map(|pid| snap.get(pid)
            .map(|(name, _, _, _, _)| name.as_str())
            .unwrap_or("?"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Builds the brutalist ASCII table as a String.
/// Returned string is printed to terminal and broadcast over XPC to allocate-ui.
///
/// `rows[0]` (when `is_frontmost == true`) is rendered as rank 0 with the
/// `FRONTMOST` tag.  Background hogs follow as ranks 1..N.
///
/// Column format (5 columns):
///   │  0. <name>  | CPU: <pct>% | RAM: <ram> | Threads: <n> | FRONTMOST
///   │  1. <name>  | CPU: <pct>% | RAM: <ram> | Threads: <n> | THROTTLED | OK
fn build_table(
    fg:   &ForegroundApp,
    rows: &[ProcessMetrics],
    batt: Option<&BatteryState>,
) -> String {
    let batt_str = match batt {
        None    => String::new(),
        Some(b) => {
            let icon   = if b.is_charging { "🔌" } else { "🔋" };
            let status = if b.is_charging { "Charging" } else { "Battery" };
            format!(" | {} {}% ({})", icon, b.charge_percent, status)
        }
    };

    // Partition rows into frontmost (0 or 1) and background hogs.
    let frontmost  = rows.first().filter(|h| h.is_frontmost);
    let hog_offset = frontmost.map_or(0, |_| 1);
    let hogs       = &rows[hog_offset..];

    let mut out = String::with_capacity(640);
    out.push_str(&format!("┌{SEP}\n"));
    out.push_str(&format!("│ 🟢 ACTIVE: {} (PID: {}){}\n", fg.name, fg.pid, batt_str));
    out.push_str(&format!("├{SEP}\n"));

    // ── Rank-0 frontmost row (real CPU/RAM/threads) ───────────────────────────
    if let Some(fm) = frontmost {
        let ram  = format_ram(fm.resident_bytes);
        let name = if fm.name.len() > 24 { &fm.name[..24] } else { &fm.name };
        out.push_str(&format!(
            "│ {:>2}. {:<24} | PID: {:>5} | CPU: {:>5.1}% | RAM: {:>6} | Threads: {} | FRONTMOST\n",
            0, name, fm.pid, fm.cpu_pct, ram, fm.threadnum,
        ));
        out.push_str(&format!("├{SEP}\n"));
    }

    // ── Background hogs ───────────────────────────────────────────────────────
    out.push_str("│ ⚠️  BACKGROUND HOGS\n");
    if hogs.is_empty() {
        out.push_str("│   (none above threshold)\n");
    } else {
        for (i, h) in hogs.iter().enumerate() {
            let ram  = format_ram(h.resident_bytes);
            let name = if h.name.len() > 24 { &h.name[..24] } else { &h.name };
            let tag = if      h.is_forced_e  { "FORCE_E"   }
                      else if h.is_forced_p  { "FORCE_P"   }
                      else if h.is_throttled { "THROTTLED" }
                      else                   { "OK"        };
            out.push_str(&format!(
                "│ {:>2}. {:<24} | PID: {:>5} | CPU: {:>5.1}% | RAM: {:>6} | Threads: {} | {}\n",
                i + 1, name, h.pid, h.cpu_pct, ram, h.threadnum, tag,
            ));
        }
    }

    out.push_str(&format!("└{SEP}\n\n"));
    out
}
