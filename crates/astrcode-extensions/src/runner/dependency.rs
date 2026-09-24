//! Pure dependency planning over declarations; no runtime handles or I/O.
use std::collections::{BTreeMap, BTreeSet};

use astrcode_extension_sdk::extension::{DependencyKind, ServiceDependency, ServiceKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceBlockReason {
    MissingService {
        service: ServiceKey,
    },
    ProviderConflict {
        service: ServiceKey,
        providers: Vec<String>,
    },
    DependencyCycle {
        members: Vec<String>,
    },
    DependencyBlocked {
        provider: String,
    },
}
#[derive(Clone)]
pub(super) struct ServiceDeclaration {
    pub id: String,
    pub services: Vec<ServiceKey>,
    pub dependencies: Vec<ServiceDependency>,
}
pub(super) struct DependencyPlan {
    pub order: Vec<String>,
    pub blocked: BTreeMap<String, Vec<ServiceBlockReason>>,
    dependents: BTreeMap<String, BTreeSet<String>>,
}
impl DependencyPlan {
    pub fn analyze(declarations: &[ServiceDeclaration]) -> Self {
        let mut providers = BTreeMap::<&ServiceKey, Vec<String>>::new();
        let mut parents = BTreeMap::<String, BTreeSet<String>>::new();
        let mut dependents = BTreeMap::<String, BTreeSet<String>>::new();
        let mut blocked = BTreeMap::<String, Vec<ServiceBlockReason>>::new();
        for declaration in declarations {
            parents.entry(declaration.id.clone()).or_default();
            for service in &declaration.services {
                providers
                    .entry(service)
                    .or_default()
                    .push(declaration.id.clone());
            }
        }
        for (service, ids) in &providers {
            if ids.len() > 1 {
                for id in ids {
                    blocked.entry(id.clone()).or_default().push(
                        ServiceBlockReason::ProviderConflict {
                            service: (*service).clone(),
                            providers: ids.clone(),
                        },
                    );
                }
            }
        }
        for declaration in declarations {
            for dependency in declaration
                .dependencies
                .iter()
                .filter(|d| d.kind == DependencyKind::Required)
            {
                if let Some(ids) = providers.get(&dependency.service) {
                    for provider in ids {
                        parents
                            .entry(declaration.id.clone())
                            .or_default()
                            .insert(provider.clone());
                        dependents
                            .entry(provider.clone())
                            .or_default()
                            .insert(declaration.id.clone());
                    }
                } else {
                    blocked.entry(declaration.id.clone()).or_default().push(
                        ServiceBlockReason::MissingService {
                            service: dependency.service.clone(),
                        },
                    );
                }
            }
        }
        let mut assigned = BTreeSet::new();
        for id in parents.keys() {
            if assigned.contains(id) {
                continue;
            }
            let forward = reachable(&parents, [id.clone()]);
            let backward = reachable(&dependents, [id.clone()]);
            let members = forward.intersection(&backward).cloned().collect::<Vec<_>>();
            assigned.extend(members.iter().cloned());
            if members.len() > 1 || parents[id].contains(id) {
                for member in &members {
                    blocked.entry(member.clone()).or_default().push(
                        ServiceBlockReason::DependencyCycle {
                            members: members.clone(),
                        },
                    );
                }
            }
        }
        let mut pending = blocked.keys().cloned().collect::<Vec<_>>();
        while let Some(provider) = pending.pop() {
            for consumer in dependents.get(&provider).into_iter().flatten() {
                if !blocked.contains_key(consumer) {
                    blocked.insert(
                        consumer.clone(),
                        vec![ServiceBlockReason::DependencyBlocked {
                            provider: provider.clone(),
                        }],
                    );
                    pending.push(consumer.clone());
                }
            }
        }
        let mut done = BTreeSet::new();
        let mut order = Vec::new();
        while let Some(declaration) = declarations.iter().find(|declaration| {
            !blocked.contains_key(&declaration.id)
                && !done.contains(&declaration.id)
                && parents[&declaration.id].is_subset(&done)
        }) {
            done.insert(declaration.id.clone());
            order.push(declaration.id.clone());
        }
        Self {
            order,
            blocked,
            dependents,
        }
    }
    pub fn affected(&self, roots: impl IntoIterator<Item = String>) -> BTreeSet<String> {
        reachable(&self.dependents, roots)
    }
}
fn reachable(
    edges: &BTreeMap<String, BTreeSet<String>>,
    roots: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let mut pending = roots.into_iter().collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if visited.insert(id.clone()) {
            pending.extend(edges.get(&id).into_iter().flatten().cloned());
        }
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::*;
    fn declaration(
        id: &str,
        services: &[&str],
        required: &[&str],
        optional: &[&str],
    ) -> ServiceDeclaration {
        ServiceDeclaration {
            id: id.into(),
            services: services.iter().map(|key| key.parse().unwrap()).collect(),
            dependencies: required
                .iter()
                .map(|key| ServiceDependency {
                    service: key.parse().unwrap(),
                    kind: DependencyKind::Required,
                })
                .chain(optional.iter().map(|key| ServiceDependency {
                    service: key.parse().unwrap(),
                    kind: DependencyKind::Optional,
                }))
                .collect(),
        }
    }
    #[test]
    fn dependency_graph_handles_order_optional_conflicts_cycles_and_downstream() {
        let plan = DependencyPlan::analyze(&[
            declaration("client", &[], &["middle@1"], &["absent@1"]),
            declaration("middle", &["middle@1"], &["base@1"], &[]),
            declaration("base", &["base@1"], &[], &[]),
            declaration("cycle-a", &["a@1"], &["b@1"], &[]),
            declaration("cycle-b", &["b@1"], &["a@1"], &[]),
            declaration("cycle-child", &[], &["a@1"], &[]),
            declaration("duplicate-a", &["duplicate@1"], &[], &[]),
            declaration("duplicate-b", &["duplicate@1"], &[], &[]),
            declaration("missing", &[], &["absent@1"], &[]),
        ]);
        assert_eq!(plan.order, ["base", "middle", "client"]);
        assert_eq!(
            plan.affected(["base".into()]),
            BTreeSet::from(["base".into(), "middle".into(), "client".into()])
        );
        assert!(
            matches!(&plan.blocked["cycle-a"][0], ServiceBlockReason::DependencyCycle { members } if members == &["cycle-a", "cycle-b"])
        );
        assert!(matches!(
            plan.blocked["cycle-child"][0],
            ServiceBlockReason::DependencyBlocked { .. }
        ));
        assert!(matches!(
            plan.blocked["duplicate-a"][0],
            ServiceBlockReason::ProviderConflict { .. }
        ));
        assert!(matches!(
            plan.blocked["missing"][0],
            ServiceBlockReason::MissingService { .. }
        ));
    }
}
