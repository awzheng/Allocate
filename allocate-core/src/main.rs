// allocate-core/src/main.rs
//
// Entry point for the Allocate watchdog daemon.
//
// ── Architecture (Phase 5) ────────────────────────────────────────────────────
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

mod battery;
mod frontmost;
mod governor;
mod ipc;
mod process;

use std::sync::mpsc;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread;
use std::time::{Duration, Instant};

use objc2::{class, msg_send};
use objc2::runtime::AnyObject;

use battery::{get_battery_state, BatteryState};
use frontmost::{AppSwitchSignal, ForegroundApp};
use governor::{Governor, run_recovery};
use ipc::IpcBroadcaster;
use process::{compute_top_cpu, format_ram, take_snapshot, CpuSnapshot, ProcessMetrics};

// ── Tuning constants ──────────────────────────────────────────────────────────

/// Width of the CPU sampling window.
const CPU_SAMPLE_WINDOW: Duration = Duration::from_millis(500);

/// How many background hogs to surface per report.
const TOP_N: usize = 20;

/// Separator line for the brutalist table borders.
const SEP: &str = "──────────────────────────────────────────────────────────";

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // ── --recover: stateless escape hatch ──────────────────────────────────────
    //
    // Runs before NSApplication / XPC / signal setup so it executes cleanly
    // even when the daemon is not registered with launchd.  SIGCONT on a
    // running process is a POSIX no-op, so broadcasting to all eligible PIDs
    // is safe.
    if std::env::args().any(|a| a == "--recover") {
        let n = run_recovery();
        println!("[RECOVERY] Done — sent SIGCONT to {} process(es).", n);
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

    // ── Step 3: Start the XPC listener (GCD thread pool — no Rust thread) ────
    //
    // Registers a named Mach service listener. Returns an IpcBroadcaster that
    // the worker thread uses to push table strings to connected allocate-ui
    // clients. Degrades to a no-op if the launchd plist is not installed.
    let broadcaster = ipc::start_listener();

    // ── Step 4: Create the app-switch mpsc channel ────────────────────────────
    //
    // tx → moved into the ObjC notification block (fires on main thread).
    // rx → held by the worker thread, wakes on every app switch.
    let (tx, rx) = mpsc::channel::<AppSwitchSignal>();

    // ── Step 5: Register the NSWorkspace event-driven observer ───────────────
    //
    // Passes tx into the ObjC block ('static, moved). _observer must live for
    // the process lifetime — dropping it deregisters the observer.
    let _observer = frontmost::register_app_switch_observer(tx);

    // ── Step 6: Spawn the worker thread ──────────────────────────────────────
    let shutdown_worker = Arc::clone(&shutdown);
    thread::spawn(move || worker_loop(rx, broadcaster, shutdown_worker));

    // ── Step 7: Run the main NSRunLoop forever ────────────────────────────────
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
    // will have already issued SIGCONT to all suspended PIDs).
    //
    // SAFETY: NSRunLoop is always registered once Foundation is linked.
    unsafe {
        let run_loop: *mut AnyObject = msg_send![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![run_loop, run];
    }
}

// ── Worker thread ─────────────────────────────────────────────────────────────

fn worker_loop(
    rx:         mpsc::Receiver<AppSwitchSignal>,
    broadcaster: IpcBroadcaster,
    shutdown:   Arc<AtomicBool>,
) {
    let mut governor = Governor::new();

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

        let hogs: Vec<ProcessMetrics> =
            compute_top_cpu(&snap1, &snap2, elapsed_ns, Some(fg.pid), TOP_N);

        // ── Active mitigation: freeze / resume dummy-hog as needed ────────────
        governor.evaluate(&hogs);

        // IOPowerSources: fast synchronous IOKit call (≤10 ms). None on desktops.
        let batt: Option<BatteryState> = get_battery_state();

        // Build the table string once; print to terminal AND broadcast over XPC.
        let table = build_table(&fg, &hogs, batt.as_ref());
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
fn build_table(
    fg:   &ForegroundApp,
    hogs: &[ProcessMetrics],
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

    let mut out = String::with_capacity(512);
    out.push_str(&format!("┌{SEP}\n"));
    out.push_str(&format!("│ 🟢 ACTIVE: {} (PID: {}){}\n", fg.name, fg.pid, batt_str));
    out.push_str(&format!("├{SEP}\n"));
    out.push_str("│ ⚠️  BACKGROUND HOGS\n");

    if hogs.is_empty() {
        out.push_str("│   (none above 0.1% threshold)\n");
    } else {
        for (i, h) in hogs.iter().enumerate() {
            let ram  = format_ram(h.resident_bytes);
            let name = if h.name.len() > 24 { &h.name[..24] } else { &h.name };
            out.push_str(&format!(
                "│ {:>2}. {:<24} | CPU: {:>5.1}% | RAM: {:>6} | Threads: {}\n",
                i + 1, name, h.cpu_pct, ram, h.threadnum,
            ));
        }
    }

    out.push_str(&format!("└{SEP}\n\n"));
    out
}
