//! Background CPU load: threads that burn cycles until told to stop.
//!
//! `Hogs` owns its threads: dropping it (including during unwinding, or
//! when starting only some of the threads failed) stops and joins them, so
//! no path leaves full CPU load running behind.
//!
//! Each hog publishes its running work count in its own cache line, so the
//! caller can snapshot total work at any instant and measure throughput
//! over exactly the window it cares about.

use std::hint::black_box;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

/// One counter per cache line: no false sharing between hogs.
#[repr(align(64))]
#[derive(Default)]
struct Slot(AtomicU64);

pub struct Hogs {
    stop: Arc<AtomicBool>,
    slots: Arc<[Slot]>,
    handles: Vec<JoinHandle<()>>,
}

impl Hogs {
    pub fn start(n: usize) -> io::Result<Self> {
        let mut hogs = Hogs {
            stop: Arc::new(AtomicBool::new(false)),
            slots: (0..n).map(|_| Slot::default()).collect(),
            handles: Vec::with_capacity(n),
        };
        for i in 0..n {
            let stop = Arc::clone(&hogs.stop);
            let slots = Arc::clone(&hogs.slots);
            // On error `hogs` is dropped here, stopping the ones already running.
            let h = std::thread::Builder::new()
                .name(format!("hog-{i}"))
                .spawn(move || burn(&stop, &slots[i].0, i as u64))?;
            hogs.handles.push(h);
        }
        Ok(hogs)
    }

    /// Total work units completed so far by all hogs.
    pub fn work(&self) -> u64 {
        self.slots.iter().map(|s| s.0.load(Ordering::Relaxed)).sum()
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for Hogs {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn burn(stop: &AtomicBool, slot: &AtomicU64, seed: u64) {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut units = 0u64;
    while !stop.load(Ordering::Relaxed) {
        // A few microseconds of integer mixing per unit.
        for _ in 0..4096 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
        }
        black_box(x);
        units += 1;
        slot.store(units, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn drop_stops_threads() {
        let h = Hogs::start(2).unwrap();
        let stop = Arc::clone(&h.stop);
        drop(h);
        assert!(stop.load(Ordering::Relaxed));
    }

    #[test]
    fn work_is_observable_while_running() {
        let h = Hogs::start(1).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let a = h.work();
        std::thread::sleep(Duration::from_millis(20));
        let b = h.work();
        assert!(a > 0 && b > a, "{a} -> {b}");
        h.stop();
    }
}
