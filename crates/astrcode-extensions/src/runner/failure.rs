//! Worker terminal failures are serialized with source reconciliation.
use std::sync::Arc;

use astrcode_extension_sdk::extension::{StopReason, internal::cancel_extension_tasks};
use tokio_util::sync::CancellationToken;

use super::{
    ExtensionRunner, ExtensionRuntimeState, ServiceBlockReason, dependency::DependencyPlan,
    snapshot::declaration_snapshot,
};
use crate::host_router::ExtensionInstanceId;

impl ExtensionRunner {
    pub(super) fn observe_failure(
        self: &Arc<Self>,
        id: String,
        instance: ExtensionInstanceId,
        mut failure: tokio::sync::watch::Receiver<Option<String>>,
        cancellation: CancellationToken,
    ) {
        let runner = Arc::downgrade(self);
        let mut tasks = self.failure_observers.lock();
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "extension failure observer failed");
            }
        }
        tasks.spawn(async move {
            loop {
                let message = failure.borrow_and_update().clone();
                if let Some(message) = message {
                    if let Some(runner) = runner.upgrade() {
                        runner.apply_runtime_failure(&id, instance, message).await;
                    }
                    return;
                }
                tokio::select! {
                    () = cancellation.cancelled() => return,
                    changed = failure.changed() => if changed.is_err() { return; },
                }
            }
        });
    }
    async fn apply_runtime_failure(
        &self,
        id: &str,
        instance: ExtensionInstanceId,
        message: String,
    ) {
        let _source = self.coordination.source_reconcile.lock().await;
        let (affected, gates, mut declarations) = {
            let current = self.registry.extensions.read().await;
            if !current
                .iter()
                .any(|h| h.manifest.id() == id && h.instance_id == instance)
            {
                return;
            }
            let plan = DependencyPlan::analyze(
                &current
                    .iter()
                    .map(|h| h.manifest.service_declaration())
                    .collect::<Vec<_>>(),
            );
            let affected = plan.affected([id.to_owned()]);
            let mut gates = Vec::new();
            let mut declarations = Vec::new();
            for hosted in current
                .iter()
                .filter(|h| affected.contains(h.manifest.id()))
            {
                hosted.generation_gate.deactivate();
                cancel_extension_tasks(&hosted.tasks);
                gates.push((
                    hosted.manifest.id().to_owned(),
                    hosted.operation_gate.clone(),
                ));
                let mut declaration = declaration_snapshot(
                    &hosted.manifest,
                    0,
                    if hosted.manifest.id() == id {
                        ExtensionRuntimeState::Failed
                    } else {
                        ExtensionRuntimeState::Stopped
                    },
                );
                if hosted.manifest.id() != id {
                    declaration
                        .blocked_reasons
                        .push(ServiceBlockReason::DependencyBlocked {
                            provider: id.to_owned(),
                        });
                }
                declarations.push(declaration);
            }
            (affected, gates, declarations)
        };
        let gate_map = gates
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut guards = std::collections::BTreeMap::new();
        for (id, gate) in gates {
            guards.insert(id, gate.lock_owned().await);
        }
        self.record_extension_load_failure(id, message, None);
        let _lifecycle = self.coordination.registry.lock().await;
        let removed = {
            let mut current = self.registry.extensions.write().await;
            let removed = current
                .extract_if(.., |h| affected.contains(h.manifest.id()))
                .collect::<Vec<_>>();
            let publisher = self.bindings.read().runtime_change_publisher.clone();
            self.rebuild_index_before_stable(&current, |generation| {
                for declaration in &mut declarations {
                    declaration.generation = generation;
                }
                let mut blocked = self.registry.blocked.write();
                blocked.retain(|d| !affected.contains(&d.id));
                blocked.extend(declarations);
                drop(blocked);
                if let Some(publish) = publisher {
                    publish(generation);
                }
            });
            removed
        };
        let retirement_plan = DependencyPlan::analyze(
            &removed
                .iter()
                .map(|h| h.manifest.service_declaration())
                .collect::<Vec<_>>(),
        );
        let mut removed = removed;
        removed.sort_by_key(|h| {
            std::cmp::Reverse(
                retirement_plan
                    .order
                    .iter()
                    .position(|id| id == h.manifest.id()),
            )
        });
        for hosted in removed {
            let Some(guard) = guards.remove(hosted.manifest.id()) else {
                continue;
            };
            let dependent_gates = retirement_plan
                .affected([hosted.manifest.id().to_owned()])
                .into_iter()
                .filter(|id| id != hosted.manifest.id())
                .filter_map(|id| gate_map.get(&id).cloned())
                .collect();
            self.retirements.retire_after(
                hosted,
                StopReason::Disabled,
                self.operation_timeout,
                guard,
                self.host_router(),
                dependent_gates,
            );
        }
    }
}
