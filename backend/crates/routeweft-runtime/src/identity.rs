//! Host-authenticated identity of an immutable engine resource set.
use crate::{LoadConfig, RuntimeError};
use routeweft_dataset::{is_digest, DatasetIdentity};
use std::path::Path;

/// The base dataset identity is unchanged by offline customization. Pin the
/// complete routing.mld file digest separately, or require its absence with None.
/// Hosts obtain this identity from a trusted delivery receipt, not request input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineResourceIdentity {
    pub dataset: DatasetIdentity,
    pub mld_sha256: Option<String>,
}
/// Trusted delivery identity for the explicit paged backend. It occupies a
/// separate cache namespace from resident engines, including legacy loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PagedResourceIdentity {
    pub dataset: DatasetIdentity,
    pub query_index_sha256: String,
}
impl PagedResourceIdentity {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.dataset
            .validate()
            .map_err(|_| RuntimeError::InvalidDataset)?;
        if self.dataset.format != 2 || !is_digest(&self.query_index_sha256) {
            return Err(RuntimeError::InvalidDataset);
        }
        Ok(())
    }
}
impl EngineResourceIdentity {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.dataset
            .validate()
            .map_err(|_| RuntimeError::InvalidDataset)?;
        if self.mld_sha256.as_deref().is_some_and(|s| !is_digest(s)) {
            return Err(RuntimeError::InvalidDataset);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    pub dataset_id: String,
    // Legacy dataset-only loads occupy their own namespace. Exact absence of
    // MLD must not reuse an engine loaded by the legacy optional-MLD API.
    pub resources: ResourceKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ResourceKey {
    Legacy,
    Exact(Option<String>),
    Paged(String),
}

pub(crate) enum LoadIdentity {
    Dataset(DatasetIdentity),
    Resources(EngineResourceIdentity),
    Paged(PagedResourceIdentity),
}
impl LoadIdentity {
    pub fn dataset(&self) -> &DatasetIdentity {
        match self {
            Self::Dataset(identity) => identity,
            Self::Resources(identity) => &identity.dataset,
            Self::Paged(identity) => &identity.dataset,
        }
    }
    pub fn validate(&self) -> Result<(), RuntimeError> {
        match self {
            Self::Dataset(identity) => identity
                .validate()
                .map_err(|_| RuntimeError::InvalidDataset),
            Self::Resources(identity) => identity.validate(),
            Self::Paged(identity) => identity.validate(),
        }
    }
    pub fn key(&self) -> CacheKey {
        CacheKey {
            dataset_id: self.dataset().dataset_id.clone(),
            resources: match self {
                Self::Dataset(_) => ResourceKey::Legacy,
                Self::Resources(identity) => ResourceKey::Exact(identity.mld_sha256.clone()),
                Self::Paged(identity) => ResourceKey::Paged(identity.query_index_sha256.clone()),
            },
        }
    }
    pub fn open<L: crate::Loader>(
        &self,
        loader: &L,
        path: &Path,
        config: LoadConfig,
    ) -> Result<L::Engine, RuntimeError> {
        let engine = match self {
            Self::Dataset(identity) => loader.open(path, identity, config),
            Self::Resources(identity) => loader.open_resources(path, identity, config),
            Self::Paged(identity) => loader.open_paged_resources(path, identity, config),
        }?;
        if loader.identity(&engine) != *self.dataset() {
            return Err(RuntimeError::InvalidDataset);
        }
        Ok(engine)
    }
}
