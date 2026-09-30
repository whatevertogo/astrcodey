//! Request-owned acknowledgements bound to one pinned extension generation.

use std::sync::Arc;

use crate::extension::{ProviderContributionHandler, ProviderContributionId, ProviderResult};

/// Opaque acknowledgements paired with one prepared provider request.
///
/// Session code may only carry this value from preparation to settlement. Handler identity stays
/// inside the pinned extension generation, so a hot reload cannot redirect an acknowledgement to
/// a replacement instance.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct ProviderRequestAcknowledgements {
    entries: Vec<ProviderRequestAcknowledgement>,
}

#[derive(Clone)]
struct ProviderRequestAcknowledgement {
    extension_id: String,
    handler: Arc<dyn ProviderContributionHandler>,
    contribution_id: ProviderContributionId,
}

impl ProviderRequestAcknowledgements {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[doc(hidden)]
    pub fn push_runtime(
        &mut self,
        extension_id: String,
        handler: Arc<dyn ProviderContributionHandler>,
        contribution_id: ProviderContributionId,
    ) {
        self.entries.push(ProviderRequestAcknowledgement {
            extension_id,
            handler,
            contribution_id,
        });
    }

    #[doc(hidden)]
    pub fn into_runtime_entries(
        self,
    ) -> impl Iterator<
        Item = (
            String,
            Arc<dyn ProviderContributionHandler>,
            ProviderContributionId,
        ),
    > {
        self.entries
            .into_iter()
            .map(|entry| (entry.extension_id, entry.handler, entry.contribution_id))
    }
}

/// Aggregated request-local message effect and its opaque success acknowledgements.
#[doc(hidden)]
pub struct ProviderRequestPreparation {
    result: ProviderResult,
    acknowledgements: ProviderRequestAcknowledgements,
}

impl ProviderRequestPreparation {
    #[doc(hidden)]
    pub fn from_runtime(
        result: ProviderResult,
        acknowledgements: ProviderRequestAcknowledgements,
    ) -> Self {
        Self {
            result,
            acknowledgements,
        }
    }

    pub fn without_acknowledgements(result: ProviderResult) -> Self {
        Self::from_runtime(result, ProviderRequestAcknowledgements::default())
    }

    pub fn into_parts(self) -> (ProviderResult, ProviderRequestAcknowledgements) {
        (self.result, self.acknowledgements)
    }
}
