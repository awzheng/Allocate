// allocate-core/src/governor.rs
//
// Phase 7 — Active Mitigation Governor
//
// Evaluates the current list of background CPU hogs and issues SIGSTOP /
// SIGCONT to maintain a hardcoded allowlist of target processes.
//
// ── Safety contract ───────────────────────────────────────────────────────────
//
//  1. TARGET_NAMES is the ONLY processes this module will ever signal.
//     Any PID whose name is not in that set is silently skipped.
//
//  2. SIGSTOP is reversible — we always SIGCONT before the governor drops.
//     Call Governor::release_all() on daemon shutdown for a clean teardown.
//
//  3. We call libc::kill() directly. On macOS this is a standard BSD syscall;
//     it does not trigger undefined behaviour as long as the PID is valid and
//     the process still exists. ESRCH (no such process) is handled gracefully.

use std::collections::HashSet;

use crate::process::ProcessMetrics;

// ── Tuning constants ──────────────────────────────────────────────────────────

/// CPU threshold (percent) above which an allowed process is frozen.
const FREEZE_THRESHOLD_PCT: f64 = 5.0;

/// Names the governor is authorised to signal.
/// All other processes are implicitly ignored.
const TARGET_NAMES: &[&str] = &["dummy-hog"] as &[&str];

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
    /// `hogs` is the slice returned by `compute_top_cpu` for the current cycle.
    /// The governor mutates `self.suspended` and prints a `[GOV]` line for every
    /// state transition. No other side-effects.
    pub fn evaluate(&mut self, hogs: &[ProcessMetrics]) {
        // 1. Build the set of allowed PIDs that are currently above the threshold.
        let hot: HashSet<i32> = hogs
            .iter()
            .filter(|m| is_target(&m.name) && m.cpu_pct >= FREEZE_THRESHOLD_PCT)
            .map(|m| m.pid)
            .collect();

        // 2. Resume any PID we previously froze but that is no longer hot.
        let to_resume: Vec<i32> = self
            .suspended
            .iter()
            .copied()
            .filter(|pid| !hot.contains(pid))
            .collect();

        for pid in to_resume {
            send_signal(pid, libc::SIGCONT, "SIGCONT (resume)");
            self.suspended.remove(&pid);
        }

        // 3. Freeze any newly-hot PID that is not already suspended.
        for pid in &hot {
            if !self.suspended.contains(pid) {
                send_signal(*pid, libc::SIGSTOP, "SIGSTOP (freeze)");
                self.suspended.insert(*pid);
            }
        }
    }

    /// Resume every suspended PID. Call on clean daemon shutdown so no
    /// process is left stranded in the stopped state.
    #[allow(dead_code)]
    pub fn release_all(&mut self) {
        for pid in self.suspended.drain() {
            send_signal(pid, libc::SIGCONT, "SIGCONT (release-all)");
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Returns true iff `name` is in the authorised target list.
#[inline]
fn is_target(name: &str) -> bool {
    TARGET_NAMES.iter().any(|&t| name == t)
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
