//! The registry adapters add themselves to at engine startup.

use std::sync::Arc;

use sy_adapter_api::AgentAdapter;

/// Every agent adapter this engine knows, looked up by id.
#[derive(Default)]
pub struct AdapterRegistry {
    adapters: Vec<Arc<dyn AgentAdapter>>,
}

impl AdapterRegistry {
    /// Add an adapter. Ids should be unique; the first registered wins on lookup.
    pub fn register(&mut self, adapter: impl AgentAdapter) -> &mut Self {
        self.adapters.push(Arc::new(adapter));
        self
    }

    /// The ids of every registered adapter, in registration order.
    pub fn ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.adapters.iter().map(|a| a.id())
    }

    /// The adapter with this id, if registered.
    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters.iter().find(|a| a.id() == id).cloned()
    }
}
