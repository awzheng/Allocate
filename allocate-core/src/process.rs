// allocate-core/src/process.rs
//
// Per-process CPU, RAM, and thread telemetry via BSD proc_pidinfo(2).
//
// ── Data model change (Phase 3) ──────────────────────────────────────────────
// CpuSnapshot now stores (name, cpu_ns, resident_bytes, threadnum) per PID.
// Both pti_resident_size and pti_threadnum are already present in the
// ProcTaskInfo struct we already query for cpu_ns — zero extra syscalls.
// compute_top_cpu now returns Vec<ProcessMetrics> exposing all three fields.

use std::collections::HashMap;
use std::ffi::CStr;
use std::mem;

use libc::c_int;

// ── FFI constants ─────────────────────────────────────────────────────────────

const PROC_ALL_PIDS:         u32   = 1;
const PROC_PIDTASKINFO:      c_int = 4;  // struct proc_taskinfo (cpu ns, rss, vss, threads)
const PROC_PIDTBSDSHORTINFO: c_int = 13; // struct proc_bsdshortinfo (name, uid, ppid)

// ── FFI declarations ──────────────────────────────────────────────────────────

#[link(name = "proc")]
extern "C" {
    fn proc_listpids(
        type_:      u32,
        typeinfo:   u32,
        buffer:     *mut libc::c_void,
        buffersize: c_int,
    ) -> c_int;

    fn proc_pidinfo(
        pid:        c_int,
        flavor:     c_int,
        arg:        u64,
        buffer:     *mut libc::c_void,
        buffersize: c_int,
    ) -> c_int;

    fn proc_name(pid: c_int, buffer: *mut u8, buffersize: u32) -> c_int;
}

// ── C struct mirrors ──────────────────────────────────────────────────────────
//
// Mirrors `struct proc_taskinfo` from <sys/proc_info.h>.
// #[repr(C)] guarantees field ordering and alignment match the C ABI.
// Total: 6×u64 (48 B) + 12×i32 (48 B) = 96 bytes.
// proc_pidinfo returns 96 on success for PROC_PIDTASKINFO.

#[repr(C)]
struct ProcTaskInfo {
    pti_virtual_size:      u64,  // bytes of virtual address space
    pti_resident_size:     u64,  // bytes of resident (physical) RAM  ← exposed Phase 3
    pti_total_user:        u64,  // cumulative nanoseconds user-space CPU
    pti_total_system:      u64,  // cumulative nanoseconds kernel-space CPU
    pti_threads_user:      u64,  // user ns for still-alive threads only
    pti_threads_system:    u64,  // system ns for still-alive threads only
    pti_policy:            i32,
    pti_faults:            i32,
    pti_pageins:           i32,
    pti_cow_faults:        i32,
    pti_messages_sent:     i32,
    pti_messages_received: i32,
    pti_syscalls_mach:     i32,
    pti_syscalls_unix:     i32,
    pti_csw:               i32,  // context switches
    pti_threadnum:         i32,  // total thread count              ← exposed Phase 3
    pti_numrunning:        i32,
    pti_priority:          i32,
}

// Mirrors `struct proc_bsdshortinfo` from <sys/proc_info.h>.
// Total: 4*12 + 16 = 64 bytes. proc_pidinfo returns 64 on success.

#[repr(C)]
struct ProcBsdShortInfo {
    pbsi_pid:    u32,
    pbsi_ppid:   u32,
    pbsi_pgid:   u32,
    pbsi_status: u32,
    pbsi_comm:   [u8; 16],  // short name, NUL-terminated
    pbsi_flags:  u32,
    pbsi_uid:    u32,
    pbsi_gid:    u32,
    pbsi_ruid:   u32,
    pbsi_rgid:   u32,
    pbsi_svuid:  u32,
    pbsi_svgid:  u32,
    pbsi_rfu:    u32,
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Point-in-time snapshot: PID → (name, cumulative_cpu_ns, resident_bytes, threadnum).
///
/// Two fields added in Phase 3 (pti_resident_size, pti_threadnum) come from the
/// same single proc_pidinfo call already used for cpu_ns — zero extra syscalls.
pub type CpuSnapshot = HashMap<i32, (String, u64, u64, i32)>;

/// Fully-expanded per-process metrics returned by compute_top_cpu.
#[derive(Debug, Clone)]
pub struct ProcessMetrics {
    pub pid:            i32,
    pub name:           String,
    pub cpu_pct:        f64,
    pub resident_bytes: u64,
    pub threadnum:      i32,
}

// ── Public formatting helper ──────────────────────────────────────────────────

/// Formats raw resident bytes as a human-readable RAM string.
///
/// Examples:
///   450_000_000  → "450 MB"
///   2_100_000_000 → "2.1 GB"
///   950_000      → "  0 MB"   (rounds below 1 MB to 0 MB rather than 0 B)
pub fn format_ram(bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;

    if bytes >= GB {
        let gb = bytes as f64 / GB as f64;
        // One decimal place: "2.1 GB"
        format!("{:.1} GB", gb)
    } else {
        let mb = bytes / MB;
        format!("{} MB", mb)
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn list_all_pids() -> Vec<i32> {
    let needed = unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }

    let capacity = (needed as usize / mem::size_of::<i32>()) + 16;
    let mut buf: Vec<i32> = vec![0i32; capacity];

    let filled = unsafe {
        proc_listpids(
            PROC_ALL_PIDS,
            0,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            (buf.len() * mem::size_of::<i32>()) as c_int,
        )
    };

    if filled <= 0 {
        return Vec::new();
    }

    let count = filled as usize / mem::size_of::<i32>();
    buf.truncate(count);
    buf.retain(|&pid| pid > 0);
    buf
}

fn read_proc_name(pid: i32) -> String {
    let mut buf = [0u8; 1024];
    let ret = unsafe { proc_name(pid, buf.as_mut_ptr(), buf.len() as u32) };
    if ret > 0 {
        if let Ok(cstr) = CStr::from_bytes_until_nul(&buf) {
            let s = cstr.to_string_lossy().into_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }

    // Fallback: PROC_PIDTBSDSHORTINFO (readable without root for most system procs)
    let mut info = mem::MaybeUninit::<ProcBsdShortInfo>::uninit();
    let expected = mem::size_of::<ProcBsdShortInfo>() as c_int;

    let ret = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTBSDSHORTINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            expected,
        )
    };

    if ret >= expected {
        // SAFETY: ret >= expected guarantees every field was written by the kernel.
        let info = unsafe { info.assume_init() };
        if let Ok(cstr) = CStr::from_bytes_until_nul(&info.pbsi_comm) {
            let s = cstr.to_string_lossy().into_owned();
            if !s.is_empty() {
                return s;
            }
        }
    }

    format!("<{}>", pid)
}

/// Reads cpu_ns, resident_bytes, and threadnum for `pid` in a single syscall.
///
/// Returns None on EPERM or ESRCH (root-owned process or process gone).
/// SAFETY: assume_init() is only called when ret == expected (96 bytes).
fn read_task_info(pid: i32) -> Option<(u64, u64, i32)> {
    let mut info = mem::MaybeUninit::<ProcTaskInfo>::uninit();
    let expected = mem::size_of::<ProcTaskInfo>() as c_int; // 96 bytes

    let ret = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            expected,
        )
    };

    if ret < expected {
        return None;
    }

    // SAFETY: ret == expected guarantees proc_pidinfo fully populated every field.
    let info = unsafe { info.assume_init() };

    let cpu_ns = info.pti_total_user.saturating_add(info.pti_total_system);
    let resident_bytes = info.pti_resident_size;
    let threadnum = info.pti_threadnum;

    Some((cpu_ns, resident_bytes, threadnum))
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Captures a point-in-time snapshot of every accessible process's CPU, RAM, and threads.
///
/// The snapshot stores cumulative CPU ns so callers can diff two snapshots to get
/// instantaneous utilisation. RAM and thread count are instantaneous already.
pub fn take_snapshot() -> CpuSnapshot {
    let pids = list_all_pids();
    let mut snap = HashMap::with_capacity(pids.len());

    for pid in pids {
        if let Some((cpu_ns, resident_bytes, threadnum)) = read_task_info(pid) {
            let name = read_proc_name(pid);
            snap.insert(pid, (name, cpu_ns, resident_bytes, threadnum));
        }
    }

    snap
}

/// Derives instantaneous metrics from two snapshots.
///
/// CPU% = (snap2.cpu_ns – snap1.cpu_ns) / elapsed_ns × 100
/// RAM and threads are taken from snap2 (current values).
/// Processes with CPU% < 0.1 are filtered as noise-floor.
/// Returns at most `top_n` entries sorted by CPU% descending.
pub fn compute_top_cpu(
    s1:          &CpuSnapshot,
    s2:          &CpuSnapshot,
    elapsed_ns:  u64,
    exclude_pid: Option<i32>,
    top_n:       usize,
) -> Vec<ProcessMetrics> {
    if elapsed_ns == 0 {
        return Vec::new();
    }

    let mut metrics: Vec<ProcessMetrics> = s2
        .iter()
        .filter_map(|(&pid, (name, cpu2, resident_bytes, threadnum))| {
            if Some(pid) == exclude_pid {
                return None;
            }

            let cpu1 = s1.get(&pid).map_or(0u64, |(_, c, _, _)| *c);
            let delta = cpu2.saturating_sub(cpu1);
            let pct = (delta as f64 / elapsed_ns as f64) * 100.0;

            if pct >= 0.1 {
                Some(ProcessMetrics {
                    pid:            pid,
                    name:           name.clone(),
                    cpu_pct:        pct,
                    resident_bytes: *resident_bytes,
                    threadnum:      *threadnum,
                })
            } else {
                None
            }
        })
        .collect();

    metrics.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct)
        .unwrap_or(std::cmp::Ordering::Equal));
    metrics.truncate(top_n);
    metrics
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_ram_mb() {
        assert_eq!(format_ram(450_000_000), "450 MB");
        assert_eq!(format_ram(0), "0 MB");
        assert_eq!(format_ram(500_000), "0 MB");
    }

    #[test]
    fn test_format_ram_gb() {
        assert_eq!(format_ram(2_100_000_000), "2.1 GB");
        assert_eq!(format_ram(1_000_000_000), "1.0 GB");
    }

    #[test]
    fn test_list_all_pids_returns_valid_pids() {
        let pids = list_all_pids();
        assert!(!pids.is_empty(), "proc_listpids returned no PIDs");
        assert!(pids.iter().all(|&p| p > 0), "all PIDs must be > 0");
    }

    #[test]
    fn test_compute_top_cpu_empty_on_zero_elapsed() {
        let s1 = CpuSnapshot::new();
        let s2 = CpuSnapshot::new();
        let result = compute_top_cpu(&s1, &s2, 0, None, 20);
        assert!(result.is_empty());
    }
}
