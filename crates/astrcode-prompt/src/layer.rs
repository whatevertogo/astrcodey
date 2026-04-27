//! 4-layer prompt builder with independent caching TTL.

use std::collections::HashMap;

use astrcode_core::prompt::*;

pub struct LayeredPromptBuilder {
    layers: HashMap<PromptLayer, Vec<BlockSpec>>,
    ttl_secs: HashMap<PromptLayer, u64>,
}

impl LayeredPromptBuilder {
    pub fn new() -> Self {
        let mut ttl_secs = HashMap::new();
        ttl_secs.insert(PromptLayer::Stable, u64::MAX); // Never expires
        ttl_secs.insert(PromptLayer::SemiStable, 300); // 5 min
        ttl_secs.insert(PromptLayer::Inherited, 300); // 5 min
        ttl_secs.insert(PromptLayer::Dynamic, 0); // Never cached

        Self {
            layers: HashMap::new(),
            ttl_secs,
        }
    }

    pub fn add(&mut self, block: BlockSpec) {
        self.layers.entry(block.layer).or_default().push(block);
    }

    pub fn ttl(&self, layer: PromptLayer) -> u64 {
        self.ttl_secs.get(&layer).copied().unwrap_or(300)
    }
}

impl Default for LayeredPromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}
