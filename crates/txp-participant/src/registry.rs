//! Maps `ParticipantSpec.kind` to a factory so that recovery can rebuild
//! adapters from the `Begin` record alone.

use crate::{Participant, PartError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use txp_core::ParticipantSpec;

/// Builds an adapter from its spec and the daemon's data directory.
pub type Factory = Arc<dyn Fn(&ParticipantSpec, &Path) -> Result<Arc<dyn Participant>, PartError> + Send + Sync>;

/// Kind-to-factory map used by the engine for new and recovered transactions.
#[derive(Clone, Default)]
pub struct Registry {
    factories: HashMap<String, Factory>,
    data_dir: PathBuf,
}

impl Registry {
    /// An empty registry whose adapters keep their journals under `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Registry { factories: HashMap::new(), data_dir: data_dir.into() }
    }

    /// Data directory passed to every factory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Register (or replace) the factory for `kind`.
    pub fn register(&mut self, kind: &str, f: Factory) {
        self.factories.insert(kind.to_string(), f);
    }

    /// Registered kinds, sorted.
    pub fn kinds(&self) -> Vec<String> {
        let mut k: Vec<_> = self.factories.keys().cloned().collect();
        k.sort();
        k
    }

    /// Instantiate the adapter for `spec`; `Fatal` if its kind is unknown.
    pub fn build(&self, spec: &ParticipantSpec) -> Result<Arc<dyn Participant>, PartError> {
        let f = self
            .factories
            .get(&spec.kind)
            .ok_or_else(|| PartError::Fatal(format!("no factory for participant kind {:?}", spec.kind)))?;
        f(spec, &self.data_dir)
    }
}
