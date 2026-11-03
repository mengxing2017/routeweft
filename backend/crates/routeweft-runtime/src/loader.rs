//! Host-injected loading and the native routing engine adapter.
use crate::{EngineResourceIdentity, LoadConfig, RuntimeError};
use routeweft_dataset::DatasetIdentity;
use routeweft_routing::Engine;

/// Injectable loader for hosts and deterministic scheduler tests. Identity must describe loaded data.
pub trait Loader: Send + Sync + 'static {
    type Engine: Send + Sync + 'static;
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
