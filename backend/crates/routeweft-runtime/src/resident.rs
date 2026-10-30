//! Engine handles retain resident admission until their last reference is dropped.
use tokio::sync::OwnedSemaphorePermit;

/// Cache and caller references share the real resident admission permit.
pub struct Resident<T> {
    pub(crate) inner: T,
    pub(crate) _resident: OwnedSemaphorePermit,
}
impl<T> std::ops::Deref for Resident<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}
