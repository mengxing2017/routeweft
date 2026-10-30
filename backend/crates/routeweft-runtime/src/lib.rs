//! Optional Tokio host scheduler. Does not authorize paths or hard-interrupt synchronous loads.
//! Quotas and keepalive guards belong to actual blocking tasks, even when waiters disappear.
mod config;
mod error;
mod loader;
mod resident;
mod runtime;

pub use config::{LoadConfig, QueryBudget, RuntimeConfig};
pub use error::RuntimeError;
pub use loader::{EngineLoader, Loader};
pub use resident::Resident;
pub use runtime::Runtime;
