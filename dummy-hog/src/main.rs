//! dummy-hog — intentional CPU hog for throttle testing.
//!
//! Spawns one thread per logical CPU core. Each thread computes prime numbers
//! in a tight, non-yielding loop so the process will show near-100% CPU
//! utilisation in tools like `top` or `htop`.
//!
//! Exits cleanly on SIGINT (Ctrl-C).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    println!(
        "[dummy-hog] Spawning {} threads — CPU usage will spike to ~100%.",
        num_threads
    );
    println!("[dummy-hog] Press Ctrl-C to exit cleanly.");

    // Shared flag: set to true when SIGINT arrives.
    let running = Arc::new(AtomicBool::new(true));

    // Register SIGINT handler via the `ctrlc` crate.
    {
        let r = Arc::clone(&running);
        ctrlc::set_handler(move || {
            // Move to a fresh line so the shutdown message isn't overwritten.
            println!("\n[dummy-hog] SIGINT received — shutting down.");
            r.store(false, Ordering::SeqCst);
        })
        .expect("Failed to install Ctrl-C handler");
    }

    // Spin up worker threads.
    let handles: Vec<_> = (0..num_threads)
        .map(|id| {
            let r = Arc::clone(&running);
            thread::spawn(move || {
                println!("[dummy-hog] thread-{} running", id);
                let mut candidate: u64 = 2;
                while r.load(Ordering::Relaxed) {
                    if is_prime(candidate) {
                        // Prevent the optimiser from discarding the result
                        // without yielding control to the OS scheduler.
                        std::hint::black_box(candidate);
                    }
                    candidate = candidate.wrapping_add(1);
                }
                println!("[dummy-hog] thread-{} stopped", id);
            })
        })
        .collect();

    // ── Timer thread ─────────────────────────────────────────────────────────
    // Prints elapsed seconds on a single, in-place terminal line by
    // prepending \r (carriage return) so each update overwrites the previous
    // one instead of scrolling.
    let start = Instant::now();
    {
        let r = Arc::clone(&running);
        thread::spawn(move || {
            while r.load(Ordering::Relaxed) {
                let secs = start.elapsed().as_secs();
                // \r rewinds the cursor; trailing spaces wipe any leftover chars.
                print!("\r[dummy-hog] Running for {} second{}...      ", secs,
                    if secs == 1 { "" } else { "s" });
                let _ = io::stdout().flush();
                thread::sleep(Duration::from_secs(1));
            }
        });
    }

    // Park the main thread until all workers are done.
    for h in handles {
        let _ = h.join();
    }

    println!("[dummy-hog] All threads joined — bye.");
}

/// Naïve trial-division primality test.
/// Intentionally O(√n) — just enough work to keep the CPU fully busy.
fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n == 2 {
        return true;
    }
    if n % 2 == 0 {
        return false;
    }
    let mut i = 3u64;
    while i * i <= n {
        if n % i == 0 {
            return false;
        }
        i += 2;
    }
    true
}

