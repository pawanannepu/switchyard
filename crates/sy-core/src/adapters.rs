//! The registry adapters add themselves to at engine startup.

use std::sync::Arc;

use sy_adapter_api::AgentAdapter;

#[derive(Default)]
pub struct AdapterRegistry {
    adapters: Vec<Arc<dyn AgentAdapter>>,
}

impl AdapterRegistry {
    pub fn register(&mut self, adapter: impl AgentAdapter) -> &mut Self {
        self.adapters.push(Arc::new(adapter));
        self
    }

    pub fn ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.adapters.iter().map(|a| a.id())
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters.iter().find(|a| a.id() == id).cloned()
    }
}
