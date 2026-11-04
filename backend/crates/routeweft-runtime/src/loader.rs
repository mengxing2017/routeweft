//! Host-injected loading and the native routing engine adapter.
use crate::{EngineResourceIdentity, LoadConfig, RuntimeError};
use routeweft_dataset::DatasetIdentity;
use routeweft_routing::Engine;

/// Injectable loader for hosts and deterministic scheduler tests. Identity must describe loaded data.
pub trait Loader: Send + Sync + 'static {
    type Engine: Send + Sync + 'static;
    fn planning_workspace_bytes(&self) -> u64 {
        64 * 1024 * 1024
    }
    fn minimum_query_workspace_bytes(&self, _budget: crate::QueryBudget) -> u64 {
        0
    }
    fn open_paged_resources(
        &self,
        _path: &std::path::Path,
        _expected: &crate::PagedResourceIdentity,
        _config: LoadConfig,
    ) -> Result<Self::Engine, RuntimeError> {
        Err(RuntimeError::InvalidDataset)
    }
    /// Runs inside the separately reserved planning_workspace_bytes. Custom
    /// loaders must override if they need a different logical allocation model.
    fn memory_requirements(
        &self,
        _path: &std::path::Path,
        config: LoadConfig,
    ) -> Result<crate::MemoryRequirements, RuntimeError> {
        Ok(crate::MemoryRequirements {
            load_peak_bytes: config.options.max_resident_bytes,
            resident_bytes: config.options.max_resident_bytes,
        })
    }
    fn open(
        &self,
        path: &std::path::Path,
        expected: &DatasetIdentity,
        config: LoadConfig,
    ) -> Result<Self::Engine, RuntimeError>;
    fn identity(&self, engine: &Self::Engine) -> DatasetIdentity;
    /// Implementations must authenticate the complete resource identity. The
    /// default rejects exact-resource loads rather than silently ignoring MLD.
    fn open_resources(
        &self,
        _path: &std::path::Path,
        _expected: &EngineResourceIdentity,
        _config: LoadConfig,
    ) -> Result<Self::Engine, RuntimeError> {
        Err(RuntimeError::InvalidDataset)
    }
}
pub struct EngineLoader;
impl Loader for EngineLoader {
    type Engine = Engine;
    fn minimum_query_workspace_bytes(&self, budget: crate::QueryBudget) -> u64 {
        (budget.max_states as u64)
            .saturating_mul(512)
            .saturating_add((budget.max_queue as u64).saturating_mul(64))
            // Worst-case edge names (4096 bytes), guidance copies and geometry.
            .saturating_add((budget.max_geometry as u64).saturating_mul(10240))
            .saturating_add(131072)
    }
    fn memory_requirements(
        &self,
        path: &std::path::Path,
        config: LoadConfig,
    ) -> Result<crate::MemoryRequirements, RuntimeError> {
        let plan = routeweft_routing::resources::resource_plan(
            path,
            config.options,
            64 * 1024 * 1024,
            Default::default(),
        )?;
        let load_peak_bytes = plan
            .accelerated_load
            .as_ref()
            .map_or(plan.base.engine_load.estimated_bytes, |p| p.peak_bytes);
        Ok(crate::MemoryRequirements {
            load_peak_bytes,
            resident_bytes: plan.engine_resident_bytes,
        })
    }
    fn open(
        &self,
        path: &std::path::Path,
        expected: &DatasetIdentity,
        config: LoadConfig,
    ) -> Result<Engine, RuntimeError> {
        let engine = Engine::open_with_capacity(path, config.options, config.capacity)?;
        expected
            .verify(engine.manifest())
            .map_err(|_| RuntimeError::InvalidDataset)?;
        Ok(engine)
    }
    fn identity(&self, engine: &Engine) -> DatasetIdentity {
        DatasetIdentity::from(engine.manifest())
    }
    fn open_resources(
        &self,
        path: &std::path::Path,
        expected: &EngineResourceIdentity,
        config: LoadConfig,
    ) -> Result<Engine, RuntimeError> {
        expected.validate()?;
        let engine = self.open(path, &expected.dataset, config)?;
        if engine.acceleration_sha256() != expected.mld_sha256.as_deref() {
            return Err(RuntimeError::InvalidDataset);
        }
        Ok(engine)
    }
}

/// Only accepts exact paged identities; it never falls back to full decoding.
#[derive(Default)]
pub struct PagedEngineLoader {
    pub options: routeweft_routing::PagedOptions,
}
impl Loader for PagedEngineLoader {
    type Engine = routeweft_routing::PagedEngine;
    fn planning_workspace_bytes(&self) -> u64 {
        131072
    }
    fn minimum_query_workspace_bytes(&self, _: crate::QueryBudget) -> u64 {
        self.options.query_workspace_bytes
    }
    fn memory_requirements(
        &self,
        _: &std::path::Path,
        _: LoadConfig,
    ) -> Result<crate::MemoryRequirements, RuntimeError> {
        let resident = self
            .options
            .cache_bytes
            .checked_add(131072)
            .ok_or(RuntimeError::Internal)?;
        Ok(crate::MemoryRequirements {
            resident_bytes: resident,
            load_peak_bytes: resident
                .checked_add(self.options.planning_workspace_bytes)
                .ok_or(RuntimeError::Internal)?,
        })
    }
    fn open(
        &self,
        _: &std::path::Path,
        _: &DatasetIdentity,
        _: LoadConfig,
    ) -> Result<Self::Engine, RuntimeError> {
        Err(RuntimeError::InvalidDataset)
    }
    fn identity(&self, engine: &Self::Engine) -> DatasetIdentity {
        DatasetIdentity::from(engine.manifest())
    }
    fn open_paged_resources(
        &self,
        path: &std::path::Path,
        expected: &crate::PagedResourceIdentity,
        config: LoadConfig,
    ) -> Result<Self::Engine, RuntimeError> {
        expected.validate()?;
        let engine = routeweft_routing::PagedEngine::open(
            path,
            config.options,
            config.capacity,
            self.options,
        )?;
        expected
            .dataset
            .verify(engine.manifest())
            .map_err(|_| RuntimeError::InvalidDataset)?;
        if engine.query_index_sha256() != expected.query_index_sha256 {
            return Err(RuntimeError::InvalidDataset);
        }
        Ok(engine)
    }
}
