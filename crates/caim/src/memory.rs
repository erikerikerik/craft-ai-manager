//! Conservative accounting for request buffers, job working space, and retained PNGs.
//! This bounds CAIM-owned image data, not ONNX Runtime's internal session allocations.
use std::sync::{Arc, Mutex, PoisonError};

use crate::error::{Fail, Result};

pub const DEFAULT_LIMIT: usize = 2 * 1024 * 1024 * 1024;
pub const WORK_BYTES_PER_PIXEL: usize = 128;
const WORK_OVERHEAD: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct Budget {
    used: Arc<Mutex<usize>>,
    limit: usize,
}

impl Budget {
    pub fn new(limit: usize) -> Self {
        Self { used: Arc::new(Mutex::new(0)), limit }
    }

    pub fn reserve(&self, bytes: usize) -> Result<Reservation> {
        let mut reservation = Reservation { budget: self.clone(), bytes: 0 };
        reservation.resize(bytes)?;
        Ok(reservation)
    }
}

pub struct Reservation {
    budget: Budget,
    bytes: usize,
}

impl Reservation {
    pub fn resize(&mut self, bytes: usize) -> Result<()> {
        let mut used = self.budget.used.lock().unwrap_or_else(PoisonError::into_inner);
        let next = used.checked_sub(self.bytes).and_then(|n| n.checked_add(bytes));
        let Some(next) = next.filter(|next| *next <= self.budget.limit) else {
            return Err(Fail::busy("Craft AI Manager's image memory budget is busy. Read pending results or try a smaller image."));
        };
        *used = next;
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap_or_else(PoisonError::into_inner);
        *used = used.saturating_sub(self.bytes);
    }
}

pub fn work_bytes(width: u32, height: u32) -> Result<usize> {
    usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(WORK_BYTES_PER_PIXEL))
        .and_then(|bytes| bytes.checked_add(WORK_OVERHEAD))
        .ok_or_else(|| Fail::too_large("The image working space is too large for this machine."))
}

#[cfg(test)]
mod tests {
    use super::Budget;

    #[test]
    fn failed_growth_preserves_existing_reservations() {
        let budget = Budget::new(100);
        let mut first = budget.reserve(60).expect("first");
        let second = budget.reserve(40).expect("second");
        assert!(first.resize(61).is_err());
        drop(second);
        first.resize(100).expect("released bytes can be reused");
        drop(first);
        budget.reserve(100).expect("all bytes returned");
    }

    #[test]
    fn concurrent_reservations_cannot_overspend() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let budget = Budget::new(100);
        let barrier = Barrier::new(8);
        let winners = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let reservation = budget.reserve(60).ok();
                    if reservation.is_some() {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                    barrier.wait();
                    drop(reservation);
                });
            }
        });
        assert_eq!(winners.load(Ordering::SeqCst), 1);
        budget.reserve(100).expect("all concurrent reservations returned");
    }
}
