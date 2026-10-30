//! Shared scheduler state and admission capacities.
mod loading;
mod query;

use crate::{LoadConfig, Loader, QueryBudget, Resident, RuntimeConfig, RuntimeError};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify, OnceCell, Semaphore};

type Loaded<T> = Result<Arc<Resident<T>>, RuntimeError>;
struct LoadEntry<T> {
    result: OnceCell<Loaded<T>>,
    changed: Notify,
}
impl<T> Default for LoadEntry<T> {
    fn default() -> Self {
        Self {
            result: OnceCell::new(),
            changed: Notify::new(),
        }
    }
}
type LoadCell<T> = Arc<LoadEntry<T>>;

pub struct Runtime<L: Loader> {
    loader: Arc<L>,
    registry: Arc<Mutex<HashMap<String, LoadCell<L::Engine>>>>,
    resident: Arc<Semaphore>,
    loads: Arc<Semaphore>,
    queries: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
    load_limits: LoadConfig,
    query_limits: QueryBudget,
    queue_timeout: Duration,
}
impl<L: Loader> Clone for Runtime<L> {
    fn clone(&self) -> Self {
        Self {
            loader: self.loader.clone(),
            registry: self.registry.clone(),
            resident: self.resident.clone(),
            loads: self.loads.clone(),
            queries: self.queries.clone(),
            admitted: self.admitted.clone(),
            load_limits: self.load_limits,
            query_limits: self.query_limits,
            queue_timeout: self.queue_timeout,
        }
    }
}
impl<L: Loader> Runtime<L> {
    pub fn new(loader: Arc<L>, config: RuntimeConfig) -> Self {
        Self {
            loader,
            registry: Arc::default(),
            resident: Arc::new(Semaphore::new(config.resident_datasets)),
            loads: Arc::new(Semaphore::new(config.parallel_loads)),
            queries: Arc::new(Semaphore::new(config.parallel_queries)),
            admitted: Arc::new(Semaphore::new(
                config
                    .parallel_queries
                    .saturating_add(config.queued_queries),
            )),
            load_limits: config.load,
            query_limits: config.query,
            queue_timeout: config.queue_timeout,
        }
    }
}

#[cfg(test)]
mod tests;
