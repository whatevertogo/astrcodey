//! Host transport profile used for extension admission.

use std::collections::BTreeSet;

pub use crate::wire::TransportFeature;

/// Immutable features exposed by one active transport composition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportProfile {
    features: BTreeSet<TransportFeature>,
}

impl TransportProfile {
    pub fn new(features: impl IntoIterator<Item = TransportFeature>) -> Self {
        Self {
            features: features.into_iter().collect(),
        }
    }

    fn supports(&self, feature: TransportFeature) -> bool {
        self.features.contains(&feature)
    }

    pub fn missing(
        &self,
        required: impl IntoIterator<Item = TransportFeature>,
    ) -> Vec<TransportFeature> {
        required
            .into_iter()
            .filter(|feature| !self.supports(*feature))
            .collect()
    }
}
