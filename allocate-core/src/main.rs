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
use process::{compute_top_cpu, format_ram, get_frontmost_metrics, take_snapshot, CpuSnapshot, ProcessMetrics};

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
    log::set_max_level(log::LevelFilter::Error);

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

// ── Worker thread ─────────────────────────────────────────────────────────────

fn worker_loop(
    rx:          mpsc::Receiver<AppSwitchSignal>,
    broadcaster: IpcBroadcaster,
    shutdown:    Arc<AtomicBool>,
    config:      Arc<RwLock<GovernorConfig>>,
) {
    let mut governor = Governor::new(config);

    // Block on the channel. Wakes the instant the OS fires the app-switch
    // notification — no polling sleep. recv() returns Err when all senders drop.
    while let Ok(signal) = rx.recv() {
        // ── Graceful shutdown check ───────────────────────────────────────────
        // Checked at the top of each cycle so we always release suspended PIDs
        // before exiting, regardless of which signal arrived.
        if shutdown.load(Ordering::Relaxed) {
            println!("[SHUTDOWN] Signal received — releasing all suspended PIDs…");
            governor.release_all();
            println!("[SHUTDOWN] Clean exit.");
            std::process::exit(0);
        }

        let fg = ForegroundApp { name: signal.name, pid: signal.pid };

        // ── Two proc_pidinfo snapshots across a 500 ms CPU sampling window ────
        let snap1: CpuSnapshot = take_snapshot();
        let t0 = Instant::now();
        thread::sleep(CPU_SAMPLE_WINDOW);
        let snap2: CpuSnapshot = take_snapshot();
        let elapsed_ns = t0.elapsed().as_nanos() as u64;

        // compute_top_cpu excludes fg.pid so the governor never evaluates it.
        let mut hogs: Vec<ProcessMetrics> =
            compute_top_cpu(&snap1, &snap2, elapsed_ns, Some(fg.pid), TOP_N);

        // ── Active mitigation: apply throttle policy on violators ─────────────
        governor.evaluate(&hogs);

        for h in &mut hogs {
            h.is_throttled = governor.is_throttled(h.pid);
        }

        // ── Build display rows ────────────────────────────────────────────────
        // Frontmost is fetched after evaluate() so the governor never touches
        // the active app.  It is prepended so build_table renders it as rank 0.
        let mut rows: Vec<ProcessMetrics> = Vec::with_capacity(hogs.len() + 1);
        if let Some(fm) = get_frontmost_metrics(fg.pid, &snap1, &snap2, elapsed_ns) {
            rows.push(fm); // is_frontmost=true, rank 0
        }
        rows.extend(hogs); // background hogs, ranks 1..N

        // IOPowerSources: fast synchronous IOKit call (≤10 ms). None on desktops.
        let batt: Option<BatteryState> = get_battery_state();

        let table = build_table(&fg, &rows, batt.as_ref());
        print!("{}", table);
        broadcaster.broadcast(&table);
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
            "│ {:>2}. {:<24} | CPU: {:>5.1}% | RAM: {:>6} | Threads: {} | FRONTMOST\n",
            0, name, fm.cpu_pct, ram, fm.threadnum,
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
            let tag  = if h.is_throttled { "THROTTLED" } else { "OK" };
            out.push_str(&format!(
                "│ {:>2}. {:<24} | CPU: {:>5.1}% | RAM: {:>6} | Threads: {} | {}\n",
                i + 1, name, h.cpu_pct, ram, h.threadnum, tag,
            ));
        }
    }

    out.push_str(&format!("└{SEP}\n\n"));
    out
}
