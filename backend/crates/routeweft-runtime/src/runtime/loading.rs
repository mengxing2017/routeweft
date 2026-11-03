//! Single-flight cached loading, staged validation and eviction.
use super::{LoadEntry, Loaded, Runtime};
use crate::identity::LoadIdentity;
use crate::{EngineResourceIdentity, Loader, Resident, RuntimeError};
use routeweft_dataset::DatasetIdentity;
use std::{path::PathBuf, sync::Arc, time::Instant};

impl<L: Loader> Runtime<L> {
    /// Preparation always opens the staged files, even if the dataset is cached.
    /// Await actual loading completion before permitting staging cleanup.
    pub async fn validate_dataset(
        &self,
        path: PathBuf,
        expected: DatasetIdentity,
        deadline: Instant,
        file_lease: Arc<dyn Send + Sync>,
    ) -> Loaded<L::Engine> {
        self.validate_inner(path, LoadIdentity::Dataset(expected), deadline, file_lease)
            .await
    }
    /// Like validate_dataset, but authenticates the optional accelerator too.
    /// This staging check deliberately does not populate the query cache.
    pub async fn validate_resources(
        &self,
        path: PathBuf,
        expected: EngineResourceIdentity,
        deadline: Instant,
        file_lease: Arc<dyn Send + Sync>,
    ) -> Loaded<L::Engine> {
        self.validate_inner(
            path,
            LoadIdentity::Resources(expected),
            deadline,
            file_lease,
        )
        .await
    }
    async fn validate_inner(
        &self,
        path: PathBuf,
        expected: LoadIdentity,
        deadline: Instant,
        file_lease: Arc<dyn Send + Sync>,
    ) -> Loaded<L::Engine> {
        expected.validate()?;
        if Instant::now() >= deadline {
            return Err(RuntimeError::DeadlineExceeded);
        }
        if self.resident.available_permits() == 0 {
            let mut registry = self.registry.lock().await;
            registry.retain(|_, cell| !matches!(cell.result.get(), Some(Ok(engine)) if Arc::strong_count(engine) == 1));
        }
        let resident = self
            .resident
            .clone()
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Overloaded)?;
        let loading = self
            .loads
            .clone()
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Overloaded)?;
        let loader = self.loader.clone();
        let limits = self.load_limits;
        let engine = tokio::task::spawn_blocking(move || {
            let _loading = loading;
            let _file_lease = file_lease;
            let inner = expected.open(loader.as_ref(), &path, limits)?;
            Ok::<_, RuntimeError>(Arc::new(Resident {
                inner,
                _resident: resident,
            }))
        })
        .await
        .map_err(|_| RuntimeError::Internal)??;
        if Instant::now() >= deadline {
            return Err(RuntimeError::DeadlineExceeded);
        }
        Ok(engine)
    }
    /// Caller supplies a path resolved from a trusted persistent storage key.
    /// A dropped waiter does not cancel initialization or release its load quota.
    pub async fn load(
        &self,
        path: PathBuf,
        expected: DatasetIdentity,
        deadline: Instant,
    ) -> Loaded<L::Engine> {
        self.load_with_lease(path, expected, deadline, None).await
    }
    pub async fn load_with_lease(
        &self,
        path: PathBuf,
        expected: DatasetIdentity,
        deadline: Instant,
        file_lease: Option<Arc<dyn Send + Sync>>,
    ) -> Loaded<L::Engine> {
        self.load_inner(path, LoadIdentity::Dataset(expected), deadline, file_lease)
            .await
    }
    /// Cache by base dataset plus exact MLD digest (or verified absence).
    /// Use during publication/startup prewarming and subsequent queries with the
    /// same trusted delivery identity. Old handles retain their resident quota.
    pub async fn load_resources(
        &self,
        path: PathBuf,
        expected: EngineResourceIdentity,
        deadline: Instant,
    ) -> Loaded<L::Engine> {
        self.load_resources_with_lease(path, expected, deadline, None)
            .await
    }
    pub async fn load_resources_with_lease(
        &self,
        path: PathBuf,
        expected: EngineResourceIdentity,
        deadline: Instant,
        file_lease: Option<Arc<dyn Send + Sync>>,
    ) -> Loaded<L::Engine> {
        self.load_inner(
            path,
            LoadIdentity::Resources(expected),
            deadline,
            file_lease,
        )
        .await
    }
    async fn load_inner(
        &self,
        path: PathBuf,
        expected: LoadIdentity,
        deadline: Instant,
        file_lease: Option<Arc<dyn Send + Sync>>,
    ) -> Loaded<L::Engine> {
        expected.validate()?;
        if Instant::now() >= deadline {
            return Err(RuntimeError::DeadlineExceeded);
        }
        let key = expected.key();
        let expected_identity = expected.dataset().clone();
        let cell = {
            let mut registry = self.registry.lock().await;
            if let Some(cell) = registry.get(&key) {
                cell.clone()
            } else {
                // Evict an idle completed entry when the bounded cache is full.
                // Active queries retain their admission permit after eviction.
                if self.resident.available_permits() == 0 {
                    let victim = registry.iter().find_map(|(key, cell)| {
                        cell.result.get().and_then(|result| match result {
                            Ok(engine) if Arc::strong_count(engine) == 1 => Some(key.clone()),
                            Err(_) => Some(key.clone()),
                            _ => None,
                        })
                    });
                    if let Some(victim) = victim {
                        registry.remove(&victim);
                    }
                }
                // Capacity belongs to the actual engine, even after cache eviction.
                let resident = self
                    .resident
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| RuntimeError::Overloaded)?;
                let loading = self
                    .loads
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| RuntimeError::Overloaded)?;
                let cell = Arc::new(LoadEntry::default());
                registry.insert(key.clone(), cell.clone());
                let runtime = self.clone();
                let target = cell.clone();
                tokio::spawn(async move {
                    let loader = runtime.loader.clone();
                    let limits = runtime.load_limits;
                    let result = tokio::task::spawn_blocking(move || {
                        let _loading = loading;
                        let _file_lease = file_lease;
                        let engine = expected.open(loader.as_ref(), &path, limits)?;
                        Ok::<_, RuntimeError>(Arc::new(Resident {
                            inner: engine,
                            _resident: resident,
                        }))
                    })
                    .await
                    .unwrap_or(Err(RuntimeError::Internal));
                    let failed = result.is_err();
                    let _ = target.result.set(result);
                    target.changed.notify_waiters();
                    if failed {
                        let mut registry = runtime.registry.lock().await;
                        if registry
                            .get(&key)
                            .is_some_and(|current| Arc::ptr_eq(current, &target))
                        {
                            registry.remove(&key);
                        }
                    }
                });
                cell
            }
        };
        // OnceCell::get_or_init cannot be used here: cancellation of its owning
        // request would start a second, non-cancellable Engine::open.
        let loaded = tokio::time::timeout_at(deadline.into(), async {
            loop {
                let changed = cell.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(result) = cell.result.get() {
                    return result.clone();
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| RuntimeError::DeadlineExceeded)??;
        // A cache hit must still check the complete expected identity.
        if self.loader.identity(&loaded.inner) != expected_identity {
            return Err(RuntimeError::InvalidDataset);
        }
        Ok(loaded)
    }
    /// Only removes the cache reference; active queries retain memory admission.
    pub async fn evict(&self, dataset_id: &str) {
        let mut registry = self.registry.lock().await;
        // Keep in-flight initialization discoverable until it completes.
        registry.retain(|key, entry| key.dataset_id != dataset_id || entry.result.get().is_none());
    }
}
