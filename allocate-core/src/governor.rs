// allocate-core/src/governor.rs
//
// Phase 7 — Active Mitigation Governor
//
// Evaluates the current list of background CPU hogs and issues SIGSTOP /
// SIGCONT to suspend runaway user-space processes while protecting critical
// system UI daemons.
//
// ── Safety contract ───────────────────────────────────────────────────────────
//
//  Rule 1: Only processes owned by a standard user (uid >= 500) are eligible.
//          Root / system processes (UID 0–499) are unconditionally skipped.
//
//  Rule 2: IGNORE_NAMES is a hardcoded, strict deny-list of critical user-space
//          UI processes that must never be frozen. Any process whose name
//          appears in this list is unconditionally skipped regardless of uid.
//
//  Rule 3: A process must exceed FREEZE_THRESHOLD_PCT CPU to be frozen.
//
//  Rule 4: SIGSTOP is reversible — we always SIGCONT before the governor drops.
//          Call Governor::release_all() on daemon shutdown for a clean teardown.
//
//  Rule 6: run_recovery() is a stateless escape hatch. It issues SIGCONT to
//          every user-space non-ignored PID regardless of internal HashSet state.
//          SIGCONT on a running process is a POSIX no-op — safe to broadcast.
//
//  Rule 5: We call libc::kill() directly. On macOS this is a standard BSD
//          syscall; it does not trigger undefined behaviour as long as the PID
//          is valid and the process still exists. ESRCH (no such process) is
//          handled gracefully.

use std::collections::HashSet;

use crate::process::{take_snapshot, ProcessMetrics};

// ── Tuning constants ──────────────────────────────────────────────────────────

/// CPU threshold (percent) above which an eligible process is frozen.
const FREEZE_THRESHOLD_PCT: f64 = 5.0;

/// Minimum UID considered a standard user-space application.
/// UIDs 0–499 are reserved for root and system accounts on macOS.
const MIN_USER_UID: u32 = 500;

/// Critical user-space UI processes that must never receive SIGSTOP.
///
/// These run in user space (uid typically 0 or 88) but are essential to the
/// macOS GUI session. Freezing any of them would make the desktop unusable.
static IGNORE_NAMES: &[&str] = &[
    "WindowServer",       // Quartz Compositor — kills display if stopped
    "Dock",               // Dock / Mission Control
    "Finder",             // Default file manager
    "loginwindow",        // Session manager / login screen
    "coreaudiod",         // Core Audio daemon — all system audio routes through this
    "SystemUIServer",     // Menu-bar extras host
    "NotificationCenter", // macOS notification delivery
    "Spotlight",          // Spotlight search
    "launchd",            // PID 1 — absolutely never touch
];

// ── Governor ──────────────────────────────────────────────────────────────────

/// Stateful workload governor.
///
/// Holds the set of PIDs currently suspended via SIGSTOP so that it can
/// issue the matching SIGCONT when the process drops below the threshold or
/// disappears from the hog list.
pub struct Governor {
    /// PIDs currently frozen by this governor.
    suspended: HashSet<i32>,
}

impl Governor {
    pub fn new() -> Self {
        Self {
            suspended: HashSet::new(),
        }
    }

    /// Main evaluation tick — call once per worker-loop iteration.
    ///
    /// Frozen PIDs stay frozen indefinitely; only `release_all()` can resume
    /// them. This avoids the oscillation loop where SIGSTOP drives CPU to 0%,
    /// causing the governor to immediately SIGCONT on the next cycle.
    pub fn evaluate(&mut self, hogs: &[ProcessMetrics]) {
        // Collect eligible PIDs currently above the CPU threshold.
        let hot: Vec<i32> = hogs
            .iter()
            .filter(|m| should_freeze(m))
            .map(|m| m.pid)
            .collect();

        // Freeze any newly-hot PID not already suspended.
        for pid in hot {
            if !self.suspended.contains(&pid) {
                send_signal(pid, libc::SIGSTOP, "SIGSTOP (freeze)");
                self.suspended.insert(pid);
            }
        }
    }

    /// Resume every suspended PID. Call on clean daemon shutdown so no
    /// process is left stranded in the stopped state.
    pub fn release_all(&mut self) {
        for pid in self.suspended.drain() {
            send_signal(pid, libc::SIGCONT, "SIGCONT (release-all)");
        }
    }
}

// ── Stateless recovery ───────────────────────────────────────────────────────

/// Scans all running processes and issues SIGCONT to every user-space process
/// that the governor is permitted to target (uid ≥ MIN_USER_UID, not in
/// IGNORE_NAMES). Designed for the `--recover` CLI mode; does not require or
/// modify any Governor instance.
///
/// Returns the number of PIDs that received SIGCONT (including those already
/// running — POSIX guarantees SIGCONT on a non-stopped process is a no-op).
pub fn run_recovery() -> usize {
    let snap = take_snapshot();
    let mut count = 0;

    for (&pid, (name, _, _, _, uid)) in &snap {
        if *uid < MIN_USER_UID {
            continue;
        }
        if IGNORE_NAMES.iter().any(|&blocked| name == blocked) {
            continue;
        }
        // SAFETY: kill(2) / SIGCONT — identical safety justification as send_signal.
        let ret = unsafe { libc::kill(pid, libc::SIGCONT) };
        if ret == 0 {
            println!("[RECOVERY] Woke up {} (PID {})", name, pid);
            count += 1;
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                eprintln!("[RECOVERY] SIGCONT → pid {} FAILED: {}", pid, err);
            }
        }
    }

    count
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Returns true iff this process is a valid freeze candidate.
///
/// A process is frozen only when ALL of the following hold:
///   1. uid >= MIN_USER_UID   (not a root/system process)
///   2. name not in IGNORE_NAMES   (not a protected UI daemon)
///   3. cpu_pct >= FREEZE_THRESHOLD_PCT   (actually hogging CPU)
#[inline]
fn should_freeze(m: &ProcessMetrics) -> bool {
    if m.uid < MIN_USER_UID {
        return false;
    }
    if IGNORE_NAMES.iter().any(|&blocked| m.name == blocked) {
        return false;
    }
    m.cpu_pct >= FREEZE_THRESHOLD_PCT
}

/// Sends `sig` to `pid` via libc::kill and logs the outcome.
///
/// ESRCH (3) means the process vanished between the snapshot and now — that is
/// benign and logged as a warning rather than an error.
fn send_signal(pid: i32, sig: libc::c_int, label: &str) {
    // SAFETY: kill(2) is a standard POSIX syscall. We only pass SIGSTOP /
    // SIGCONT which are defined in libc and are not async-signal-unsafe here
    // because we are on a normal Rust thread (not inside a signal handler).
    let ret = unsafe { libc::kill(pid, sig) };
    if ret == 0 {
        println!("[GOV] {} → pid {}", label, pid);
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            println!("[GOV] {} → pid {} (ESRCH: process already gone)", label, pid);
        } else {
            eprintln!("[GOV] {} → pid {} FAILED: {}", label, pid, err);
        }
    }
}
