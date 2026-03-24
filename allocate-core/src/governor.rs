// allocate-core/src/governor.rs
//
// Phase 9.2 — Active Mitigation Governor (dynamic thresholds + hysteresis)
//
// Actuates XNU QoS via /usr/sbin/taskpolicy (fire-and-forget subprocess).
// Thresholds are now runtime-configurable: the SwiftUI frontend sends new
// values over XPC and the IPC module writes them into GovernorConfig via an
// Arc<RwLock<GovernorConfig>> shared with this module.
//
// ── Hysteresis design ─────────────────────────────────────────────────────────
//
//   throttle_threshold  CPU% above which an eligible process is throttled.
//   release_threshold   CPU% below which a throttled process is un-throttled.
//
//   The gap between the two prevents oscillation: a process throttled at 15%
//   stays throttled until it drops below (e.g.) 5%, not back to 14.9%.
//
// ── Safety contract ───────────────────────────────────────────────────────────
//
//  Rule 1: Only processes owned by a standard user (uid >= 500) are eligible.
//  Rule 2: IGNORE_NAMES is a hardcoded deny-list of critical system processes.
//  Rule 3: A process must exceed throttle_threshold to be throttled.
//  Rule 4: A throttled process is released when CPU drops below release_threshold.
//  Rule 5: The throttle is always lifted before the governor drops (release_all).
//  Rule 6: Command::spawn() failure is logged; the daemon never panics.
//  Rule 7: GovernorConfig reads use RwLock::read() — non-blocking, concurrent-safe.

use std::collections::HashSet;
use std::process::Command;
use std::sync::{Arc, RwLock};

use crate::process::{take_snapshot, ProcessMetrics};

// ── Public config type ────────────────────────────────────────────────────────

/// Runtime-configurable governor thresholds.
///
/// Shared between the worker thread (reads) and the IPC GCD thread (writes)
/// via `Arc<RwLock<GovernorConfig>>`.  Reads are non-blocking under no
/// contention; writes are brief (two f64 assignments).
#[derive(Debug, Clone, Copy)]
pub struct GovernorConfig {
    /// CPU% at or above which an eligible process is throttled.
    pub throttle_threshold: f64,
    /// CPU% below which a currently-throttled process is released.
    /// Must be strictly less than `throttle_threshold` (hysteresis gap).
    pub release_threshold: f64,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            throttle_threshold: 15.0,
            release_threshold:  5.0,
        }
    }
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Minimum UID considered a standard user-space application.
const MIN_USER_UID: u32 = 500;

/// Critical user-space UI processes that must never be throttled.
static IGNORE_NAMES: &[&str] = &[
    "WindowServer",
    "Dock",
    "Finder",
    "loginwindow",
    "coreaudiod",
    "SystemUIServer",
    "NotificationCenter",
    "Spotlight",
    "launchd",
];

// ── Governor ──────────────────────────────────────────────────────────────────

/// Stateful workload governor.
///
/// Tracks the set of PIDs currently throttled so it can apply hysteresis:
/// throttle above `throttle_threshold`, release below `release_threshold`.
pub struct Governor {
    suspended: HashSet<i32>,
    config:    Arc<RwLock<GovernorConfig>>,
}

impl Governor {
    pub fn new(config: Arc<RwLock<GovernorConfig>>) -> Self {
        Self { suspended: HashSet::new(), config }
    }

    /// Main evaluation tick — call once per worker-loop iteration.
    ///
    /// Throttles PIDs above `throttle_threshold` and releases currently-throttled
    /// PIDs that have fallen below `release_threshold` (hysteresis).
    pub fn evaluate(&mut self, hogs: &[ProcessMetrics]) {
        // Snapshot thresholds under a brief read lock.
        let (throttle_t, release_t) = {
            let cfg = self.config.read().unwrap_or_else(|e| e.into_inner());
            (cfg.throttle_threshold, cfg.release_threshold)
        };

        // ── Throttle newly-hot PIDs ───────────────────────────────────────────
        for m in hogs {
            if !should_throttle(m, throttle_t) || self.suspended.contains(&m.pid) {
                continue;
            }
            apply_throttle(m.pid, true);
            // Insert unconditionally: release_all() will fire taskpolicy -B on
            // shutdown regardless of spawn success — idempotent and safe.
            self.suspended.insert(m.pid);
        }

        // ── Release PIDs that cooled below the release threshold ──────────────
        // A PID absent from hogs entirely has effective CPU ≈ 0 < release_t.
        let to_release: Vec<i32> = self.suspended
            .iter()
            .filter(|&&pid| {
                let cpu = hogs.iter()
                    .find(|m| m.pid == pid)
                    .map_or(0.0, |m| m.cpu_pct);
                cpu < release_t
            })
            .copied()
            .collect();

        for pid in to_release {
            apply_throttle(pid, false);
            self.suspended.remove(&pid);
        }
    }

    /// Returns true if `pid` is currently tracked as throttled.
    /// O(1) HashSet lookup — safe to call inside the worker loop per hog.
    #[inline]
    pub fn is_throttled(&self, pid: i32) -> bool {
        self.suspended.contains(&pid)
    }

    /// Lift the throttle on every suspended PID. Call on clean daemon shutdown.
    pub fn release_all(&mut self) {
        for pid in self.suspended.drain() {
            apply_throttle(pid, false);
        }
    }
}

// ── Stateless recovery ────────────────────────────────────────────────────────

/// Scans all running processes and fires `taskpolicy -B` on every eligible
/// user-space PID.  Designed for the `--recover` CLI mode.
pub fn run_recovery() -> usize {
    let snap = take_snapshot();
    let mut count = 0;

    for (&pid, (name, _, _, _, uid)) in &snap {
        if *uid < MIN_USER_UID { continue; }
        if IGNORE_NAMES.iter().any(|&b| name == b) { continue; }
        if apply_throttle(pid, false) {
            println!("[RECOVERY] Spawned taskpolicy restore for {} (PID {pid})", name);
            count += 1;
        }
    }
    count
}

// ── Private helpers ───────────────────────────────────────────────────────────

#[inline]
fn should_throttle(m: &ProcessMetrics, threshold: f64) -> bool {
    m.uid >= MIN_USER_UID
        && !IGNORE_NAMES.iter().any(|&b| m.name == b)
        && m.cpu_pct >= threshold
}

/// Fire-and-forget taskpolicy invocation.
///
/// `enable = true`  → `taskpolicy -b -p <pid>`  (background QoS)
/// `enable = false` → `taskpolicy -B -p <pid>`  (restore QoS)
///
/// Returns `true` if spawn succeeded.  On error, logs and returns `false`.
pub fn apply_throttle(pid: i32, enable: bool) -> bool {
    let flag = if enable { "-b" } else { "-B" };
    match Command::new("/usr/sbin/taskpolicy")
        .args([flag, "-p", &pid.to_string()])
        .spawn()
    {
        Ok(_child) => true,
        Err(e) => {
            log::error!("[GOV] Failed to spawn taskpolicy for PID {pid}: {e}");
            false
        }
    }
}
