//! Bounded query admission, cooperative cancellation and native query adapters.
use super::Runtime;
use crate::{EngineLoader, Loader, Resident, RuntimeError};
use routeweft_routing::{Engine, Error, SearchBudget};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

impl<L: Loader> Runtime<L> {
    pub async fn execute<T: Send + 'static>(
        &self,
        deadline: Instant,
        operation: impl FnOnce(&SearchBudget<'_>) -> Result<T, RuntimeError> + Send + 'static,
    ) -> Result<T, RuntimeError> {
        if Instant::now() >= deadline {
            return Err(RuntimeError::DeadlineExceeded);
        }
        let admitted = self
            .admitted
            .clone()
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Overloaded)?;
        let queue_deadline = deadline.min(Instant::now() + self.queue_timeout);
        let permit =
            tokio::time::timeout_at(queue_deadline.into(), self.queries.clone().acquire_owned())
                .await
                .map_err(|_| {
                    if Instant::now() >= deadline {
                        RuntimeError::DeadlineExceeded
                    } else {
                        RuntimeError::Overloaded
                    }
                })?
                .map_err(|_| RuntimeError::Internal)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let limits = self.query_limits;
        let task = tokio::task::spawn_blocking(move || {
            let (_admitted, _permit) = (admitted, permit);
            operation(&limits.search(deadline, &cancelled))
        });
        tokio::time::timeout_at(deadline.into(), task)
            .await
            .map_err(|_| RuntimeError::DeadlineExceeded)?
            .map_err(|_| RuntimeError::Internal)?
    }
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Runtime<EngineLoader> {
    /// Native query convenience API; host retains authentication and path authorization.
    pub async fn route(
        &self,
        engine: Arc<Resident<Engine>>,
        points: Vec<routeweft_dataset::Point>,
        options: routeweft_routing::QueryOptions,
        deadline: Instant,
    ) -> Result<routeweft_routing::RouteResult, RuntimeError> {
        self.execute(deadline, move |budget| {
            engine.route(&points, options, budget).map_err(|error| {
                if error == Error::BudgetExceeded && Instant::now() >= deadline {
                    RuntimeError::DeadlineExceeded
                } else {
                    error.into()
                }
            })
        })
        .await
    }
    pub async fn nearest(
        &self,
        engine: Arc<Resident<Engine>>,
        point: routeweft_dataset::Point,
        options: routeweft_routing::QueryOptions,
        deadline: Instant,
    ) -> Result<Vec<routeweft_routing::Nearest>, RuntimeError> {
        self.execute(deadline, move |budget| {
            engine.nearest(point, options, budget).map_err(|error| {
                if error == Error::BudgetExceeded && Instant::now() >= deadline {
                    RuntimeError::DeadlineExceeded
                } else {
                    error.into()
                }
            })
        })
        .await
    }
}
