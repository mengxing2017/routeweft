//! Host-injected loading and the native routing engine adapter.
use crate::{LoadConfig, RuntimeError};
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
}
