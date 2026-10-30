use super::*;
use routeweft_dataset::DatasetIdentity;
use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Instant,
};
struct FakeLoader {
    calls: AtomicUsize,
    fail: AtomicBool,
    started: Semaphore,
    release: Semaphore,
}
impl FakeLoader {
    fn new(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            started: Semaphore::new(0),
            release: Semaphore::new(permits),
        })
    }
}
impl Loader for FakeLoader {
    type Engine = DatasetIdentity;
    fn open(
        &self,
        _: &std::path::Path,
        expected: &DatasetIdentity,
        _: LoadConfig,
    ) -> Result<DatasetIdentity, RuntimeError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.started.add_permits(1);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(permit) = self.release.try_acquire() {
                permit.forget();
                break;
            }
            assert!(Instant::now() < until, "test did not release loader");
            std::thread::sleep(Duration::from_millis(1));
        }
        if self.fail.load(Ordering::Relaxed) {
            Err(RuntimeError::InvalidDataset)
        } else {
            Ok(expected.clone())
        }
    }
    fn identity(&self, engine: &DatasetIdentity) -> DatasetIdentity {
        engine.clone()
    }
}
fn identity(id: char) -> DatasetIdentity {
    DatasetIdentity {
        dataset_id: id.to_string().repeat(64),
        format: 1,
        profile: "driving-v1".into(),
        source_sha256: "c".repeat(64),
        attribution: "Synthetic".into(),
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn runtime(loader: Arc<FakeLoader>) -> Runtime<FakeLoader> {
    Runtime::new(loader, RuntimeConfig::default())
}
#[tokio::test]
async fn single_flight_full_identity_and_evicted_active_capacity() {
    let loader = FakeLoader::new(0);
    let runtime = runtime(loader.clone());
    let one = {
        let r = runtime.clone();
        tokio::spawn(async move { r.load("unused".into(), identity('a'), deadline()).await })
    };
    loader.started.acquire().await.unwrap().forget();
    runtime.evict(&identity('a').dataset_id).await;
    let two = {
        let r = runtime.clone();
        tokio::spawn(async move { r.load("unused".into(), identity('a'), deadline()).await })
    };
    loader.release.add_permits(1);
    let one = one.await.unwrap().unwrap();
    let two = two.await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&one, &two));
    assert_eq!(loader.calls.load(Ordering::Relaxed), 1);
    for field in 0..4 {
        let mut wrong = identity('a');
        match field {
            0 => wrong.source_sha256 = "b".repeat(64),
            1 => wrong.format = 2,
            2 => wrong.attribution = "Other".into(),
            _ => wrong.profile = "walking-v1".into(),
        }
        assert!(matches!(
            runtime.load("unused".into(), wrong, deadline()).await,
            Err(RuntimeError::InvalidDataset)
        ));
    }
    runtime.evict(&identity('a').dataset_id).await;
    assert!(matches!(
        runtime
            .load("unused".into(), identity('b'), deadline())
            .await,
        Err(RuntimeError::Overloaded)
    ));
    drop((one, two));
    loader.release.add_permits(1);
    assert!(runtime
        .load("unused".into(), identity('b'), deadline())
        .await
        .is_ok());
}
struct Guard(Arc<AtomicBool>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
#[tokio::test]
async fn abandoned_waiter_keeps_load_quota_and_file_guard() {
    let loader = FakeLoader::new(0);
    let runtime = runtime(loader.clone());
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Arc::new(Guard(dropped.clone()));
    let waiter = {
        let r = runtime.clone();
        tokio::spawn(async move {
            r.load_with_lease("unused".into(), identity('a'), deadline(), Some(guard))
                .await
        })
    };
    loader.started.acquire().await.unwrap().forget();
    waiter.abort();
    let _ = waiter.await;
    assert!(!dropped.load(Ordering::Relaxed));
    assert!(matches!(
        runtime
            .load("unused".into(), identity('b'), deadline())
            .await,
        Err(RuntimeError::Overloaded)
    ));
    loader.release.add_permits(1);
    let loaded = runtime
        .load("unused".into(), identity('a'), deadline())
        .await
        .unwrap();
    assert_eq!(**loaded, identity('a'));
    assert!(dropped.load(Ordering::Relaxed));
    assert_eq!(loader.calls.load(Ordering::Relaxed), 1);
}
#[tokio::test]
async fn preparation_reopens_staged_files_and_waits_for_real_load_after_deadline() {
    let loader = FakeLoader::new(1);
    let runtime = runtime(loader.clone());
    drop(
        runtime
            .load("cached".into(), identity('a'), deadline())
            .await
            .unwrap(),
    );
    loader.started.acquire().await.unwrap().forget();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Arc::new(Guard(dropped.clone()));
    let fresh = {
        let r = runtime.clone();
        tokio::spawn(async move {
            r.validate_dataset(
                "staged".into(),
                identity('a'),
                Instant::now() + Duration::from_millis(10),
                guard,
            )
            .await
        })
    };
    loader.started.acquire().await.unwrap().forget();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!fresh.is_finished());
    assert!(!dropped.load(Ordering::Relaxed));
    loader.release.add_permits(1);
    assert!(matches!(
        fresh.await.unwrap(),
        Err(RuntimeError::DeadlineExceeded)
    ));
    assert!(dropped.load(Ordering::Relaxed));
    assert_eq!(loader.calls.load(Ordering::Relaxed), 2);
}
#[tokio::test]
async fn failed_load_is_retryable_and_idle_cache_is_evicted() {
    let loader = FakeLoader::new(3);
    loader.fail.store(true, Ordering::Relaxed);
    let runtime = runtime(loader.clone());
    assert!(matches!(
        runtime.load("bad".into(), identity('a'), deadline()).await,
        Err(RuntimeError::InvalidDataset)
    ));
    loader.fail.store(false, Ordering::Relaxed);
    drop(
        runtime
            .load("fixed".into(), identity('a'), deadline())
            .await
            .unwrap(),
    );
    drop(
        runtime
            .load("next".into(), identity('b'), deadline())
            .await
            .unwrap(),
    );
    assert_eq!(loader.calls.load(Ordering::Relaxed), 3);
}
#[tokio::test]
async fn timeout_and_abandoned_future_keep_actual_query_quota_until_exit() {
    for abort in [false, true] {
        let runtime = runtime(FakeLoader::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (exited_tx, exited_rx) = tokio::sync::oneshot::channel();
        let query = {
            let r = runtime.clone();
            let release = release.clone();
            let observed = observed.clone();
            tokio::spawn(async move {
                r.execute(Instant::now() + Duration::from_millis(100), move |budget| {
                    let _ = started_tx.send(());
                    let until = Instant::now() + Duration::from_secs(5);
                    while !release.load(Ordering::Relaxed) && Instant::now() < until {
                        if budget.cancelled.unwrap().load(Ordering::Relaxed) {
                            observed.store(true, Ordering::Relaxed);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let _ = exited_tx.send(());
                    Ok(())
                })
                .await
            })
        };
        started_rx.await.unwrap();
        if abort {
            query.abort();
            let _ = query.await;
        } else {
            assert_eq!(query.await.unwrap(), Err(RuntimeError::DeadlineExceeded));
        }
        assert_eq!(
            runtime.execute(deadline(), |_| Ok(())).await,
            Err(RuntimeError::Overloaded)
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !observed.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.store(true, Ordering::Relaxed);
        exited_rx.await.unwrap();
    }
}
#[tokio::test]
async fn bounded_queue_distinguishes_admission_queue_timeout_and_request_deadline() {
    let runtime = Runtime::new(
        FakeLoader::new(0),
        RuntimeConfig {
            queued_queries: 1,
            queue_timeout: Duration::from_millis(60),
            ..Default::default()
        },
    );
    let release = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let active = {
        let r = runtime.clone();
        let release = release.clone();
        tokio::spawn(async move {
            r.execute(deadline(), move |_| {
                let _ = started_tx.send(());
                let until = deadline();
                while !release.load(Ordering::Relaxed) && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            })
            .await
        })
    };
    started_rx.await.unwrap();
    assert_eq!(
        runtime
            .execute(Instant::now() + Duration::from_millis(10), |_| Ok(()))
            .await,
        Err(RuntimeError::DeadlineExceeded)
    );
    assert_eq!(
        runtime.execute(deadline(), |_| Ok(())).await,
        Err(RuntimeError::Overloaded)
    );
    let queued = {
        let r = runtime.clone();
        tokio::spawn(async move { r.execute(deadline(), |_| Ok(())).await })
    };
    while runtime.admitted.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        runtime.execute(deadline(), |_| Ok(())).await,
        Err(RuntimeError::Overloaded)
    );
    release.store(true, Ordering::Relaxed);
    active.await.unwrap().unwrap();
    queued.await.unwrap().unwrap();
}
