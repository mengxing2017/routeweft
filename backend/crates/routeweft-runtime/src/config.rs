//! Load, query and scheduler resource limits.
use routeweft_dataset::{GraphCapacity, LoadOptions};
use routeweft_routing::SearchBudget;

#[derive(Clone, Copy, Debug, Default)]
pub struct LoadConfig {
    pub options: LoadOptions,
    pub capacity: GraphCapacity,
}
#[derive(Clone, Copy, Debug)]
pub struct QueryBudget {
    pub max_work: usize,
    pub max_states: usize,
    pub max_queue: usize,
    pub max_geometry: usize,
}
impl Default for QueryBudget {
    fn default() -> Self {
        let b = SearchBudget::default();
        Self {
            max_work: b.max_work,
            max_states: b.max_states,
            max_queue: b.max_queue,
            max_geometry: b.max_geometry,
        }
    }
}
impl QueryBudget {
    pub(crate) fn search<'a>(
        &self,
        deadline: std::time::Instant,
        cancelled: &'a std::sync::atomic::AtomicBool,
    ) -> SearchBudget<'a> {
        SearchBudget {
            max_work: self.max_work,
            max_states: self.max_states,
            max_queue: self.max_queue,
            max_geometry: self.max_geometry,
            deadline: Some(deadline),
            cancelled: Some(cancelled),
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct RuntimeConfig {
    pub resident_datasets: usize,
    pub parallel_loads: usize,
    pub parallel_queries: usize,
    pub queued_queries: usize,
    pub queue_timeout: std::time::Duration,
    pub load: LoadConfig,
    pub query: QueryBudget,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            resident_datasets: 1,
            parallel_loads: 1,
            parallel_queries: 1,
            queued_queries: 0,
            queue_timeout: std::time::Duration::from_millis(250),
            load: LoadConfig::default(),
            query: QueryBudget::default(),
        }
    }
}
