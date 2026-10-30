# routeweft-runtime

Optional Tokio host scheduler over the synchronous routing/dataset crates. Hosts resolve and authorize immutable paths before calling it. `RuntimeConfig` controls resident handles, load and query concurrency, bounded admission, queue timeout, load capacity/memory budget and query work budgets. Defaults use city capacity; regional hosts must configure their own limits.

`Runtime<EngineLoader>` accepts native `DatasetIdentity`, `Point`, `QueryOptions` and returns native routing results. Custom `Loader` implementations allow host adapters and deterministic tests. `execute` exposes the same bounded scheduler for host-owned result mapping.

Concurrent loads of one dataset share initialization; every successful waiter verifies the full expected identity. Eviction removes only the cache reference. Resident admission survives while any engine handle is held. Failed loads are retryable. Optional `Arc<dyn Send + Sync>` guards keep host files alive until actual synchronous loading exits.

`load_with_lease` limits waiter time without interrupting the underlying load. `validate_dataset` always opens staged files afresh and waits for actual completion before checking the deadline. It cannot promise hard cancellation of `Engine::open`. Query cancellation is cooperative; dropping/expiring the waiter signals the blocking query, which retains CPU/admission permits until it exits.
