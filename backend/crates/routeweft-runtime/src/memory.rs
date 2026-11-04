//! Logical byte reservations tied to the allocations' actual owners, not waiters.
use crate::RuntimeError;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRequirements {
    pub load_peak_bytes: u64,
    pub resident_bytes: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    pub limit_bytes: Option<u64>,
    pub reserved_bytes: u64,
    pub peak_reserved_bytes: u64,
    pub rejections: u64,
}
/// Share one pool across all routing runtimes owned by the same host process.
pub struct MemoryBudget(Mutex<MemoryUsage>);
impl MemoryBudget {
    pub fn new(limit_bytes: Option<u64>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(MemoryUsage {
            limit_bytes,
            ..Default::default()
        })))
    }
    pub fn usage(&self) -> MemoryUsage {
        *self.0.lock().unwrap()
    }
    pub(crate) fn reserve(self: &Arc<Self>, bytes: u64) -> Result<Reservation, RuntimeError> {
        let mut reservation = Reservation {
            pool: self.clone(),
            bytes: 0,
        };
        reservation.resize(bytes)?;
        Ok(reservation)
    }
}
pub(crate) struct Reservation {
    pool: Arc<MemoryBudget>,
    bytes: u64,
}
impl Reservation {
    pub fn resize(&mut self, bytes: u64) -> Result<(), RuntimeError> {
        let mut usage = self.pool.0.lock().unwrap();
        let other = usage.reserved_bytes - self.bytes;
        let available = usage.limit_bytes.unwrap_or(u64::MAX).saturating_sub(other);
        if bytes > available {
            usage.rejections = usage.rejections.saturating_add(1);
            return Err(RuntimeError::MemoryBudgetExceeded {
                requested_bytes: bytes,
                available_bytes: available,
            });
        }
        usage.reserved_bytes = other + bytes;
        usage.peak_reserved_bytes = usage.peak_reserved_bytes.max(usage.reserved_bytes);
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.pool.0.lock().unwrap().reserved_bytes -= self.bytes;
    }
}
