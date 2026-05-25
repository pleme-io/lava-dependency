//! lava-dependency — typed `LavaArchitectureDependency` CR + resolver.
//!
//! ## Shape
//!
//! ```text
//! LavaArchitectureDependency
//!   ─ from: ResourceAddress         (the dependent)
//!   ─ to:   ResourceAddress         (the upstream)
//!   ─ kind: BlocksOn | Influences
//!   ─ require_phase: Applied        (the phase the upstream must be in)
//!
//! DependencyResolver::resolve(dependent, deps, phases)
//!   ─ → ResolutionVerdict {
//!         ready:                Vec<ResourceAddress>,
//!         blocked_by:           Vec<BlockReason>,
//!         downstream_of_failed: Vec<ResourceAddress>,
//!       }
//! ```
//!
//! ## Solid abstractions
//!
//! - [`LavaArchitectureDependency`] — typed edge in the
//!   cross-architecture DAG.
//! - [`DependencyKind`] — `BlocksOn` (hard) vs `Influences` (soft;
//!   surfaces via Alert anomaly only).
//! - [`PhaseRegistry`] trait — lookup of current Phase for any
//!   ResourceAddress. Production: kube-rs informer over
//!   `LavaArchitecture` CRs. Tests: [`InMemoryPhaseRegistry`].
//! - [`DependencyResolver`] — pure function over deps + registry;
//!   returns typed [`ResolutionVerdict`].
//! - [`emit_blocked_anomaly`] — adapter that turns a block reason
//!   into a [`lava_anomaly::LavaAnomaly`] for chain emission.

#![allow(clippy::module_name_repetitions)]

use indexmap::IndexMap;
use lava_anomaly::{AnomalyKind, LavaAnomaly};
use lava_drift::Severity;
use lava_outcome_chain::ResourceAddress;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── LavaArchitectureDependency ────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LavaArchitectureDependency {
    pub from: ResourceAddress,
    pub to: ResourceAddress,
    pub kind: DependencyKind,
    #[serde(default = "default_require_phase")]
    pub require_phase: String,
}

fn default_require_phase() -> String {
    "Applied".to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DependencyKind {
    /// Hard dependency. Dependent's reconcile loop halts until
    /// upstream reaches `require_phase`.
    BlocksOn,
    /// Soft dependency. Dependent proceeds; an Alert anomaly is
    /// emitted if upstream isn't in `require_phase`.
    Influences,
}

// ── PhaseRegistry ─────────────────────────────────────────────────

/// Read-only lookup of the current Phase for any ResourceAddress.
/// Production impl: kube-rs informer; tests: [`InMemoryPhaseRegistry`].
pub trait PhaseRegistry {
    fn phase_of(&self, address: &ResourceAddress) -> Option<String>;
}

/// HashMap-backed registry for tests + small in-process scenarios.
#[derive(Debug, Default, Clone)]
pub struct InMemoryPhaseRegistry {
    pub phases: IndexMap<String, String>,
}

impl InMemoryPhaseRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, address: &ResourceAddress, phase: impl Into<String>) {
        self.phases.insert(address.dotted(), phase.into());
    }
    #[must_use]
    pub fn with(mut self, address: &ResourceAddress, phase: impl Into<String>) -> Self {
        self.insert(address, phase);
        self
    }
}

impl PhaseRegistry for InMemoryPhaseRegistry {
    fn phase_of(&self, address: &ResourceAddress) -> Option<String> {
        self.phases.get(&address.dotted()).cloned()
    }
}

// ── ResolutionVerdict + DependencyResolver ────────────────────────

/// Why a dependent is blocked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockReason {
    pub upstream: ResourceAddress,
    pub required_phase: String,
    pub observed_phase: Option<String>,
    pub kind: DependencyKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionVerdict {
    /// Dependents whose every BlocksOn upstream is in
    /// `require_phase`.
    pub ready: bool,
    /// All upstreams that fail the dependency (mixed hard + soft).
    pub blocks: Vec<BlockReason>,
    /// Upstreams in `Failed` phase that this dependent transitively
    /// depends on.
    pub downstream_of_failed: Vec<ResourceAddress>,
}

impl ResolutionVerdict {
    /// True when no `BlocksOn` upstream is blocking. `Influences`
    /// blocks don't gate readiness.
    #[must_use]
    pub fn ready_for_reconcile(&self) -> bool {
        self.ready
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DependencyResolver;

impl DependencyResolver {
    /// Compute the verdict for one dependent given its declared
    /// dependencies + a registry of current upstream phases.
    #[must_use]
    pub fn resolve<R: PhaseRegistry>(
        &self,
        dependent: &ResourceAddress,
        deps: &[LavaArchitectureDependency],
        registry: &R,
    ) -> ResolutionVerdict {
        let mut blocks = Vec::new();
        let mut downstream_of_failed = Vec::new();
        let mut hard_blocked = false;

        for d in deps {
            if &d.from != dependent {
                continue;
            }
            let observed = registry.phase_of(&d.to);
            let satisfied = observed
                .as_deref()
                .map(|p| p == d.require_phase)
                .unwrap_or(false);
            if !satisfied {
                blocks.push(BlockReason {
                    upstream: d.to.clone(),
                    required_phase: d.require_phase.clone(),
                    observed_phase: observed.clone(),
                    kind: d.kind,
                });
                if matches!(d.kind, DependencyKind::BlocksOn) {
                    hard_blocked = true;
                }
            }
            if observed.as_deref() == Some("Failed") {
                downstream_of_failed.push(d.to.clone());
            }
        }

        ResolutionVerdict {
            ready: !hard_blocked,
            blocks,
            downstream_of_failed,
        }
    }
}

// ── Anomaly emission adapter ──────────────────────────────────────

/// Build a typed [`LavaAnomaly`] from a [`BlockReason`]. The
/// severity is inferred from dependency kind: BlocksOn → Functional
/// (will reconverge once upstream catches up); Influences → Cosmetic
/// (just an alert).
#[must_use]
pub fn emit_blocked_anomaly(dependent: ResourceAddress, reason: &BlockReason) -> LavaAnomaly {
    let severity = match reason.kind {
        DependencyKind::BlocksOn => Severity::Functional,
        DependencyKind::Influences => Severity::Cosmetic,
    };
    let message = format!(
        "dependency {dep:?}: upstream {upstream} expected {required}, observed {observed}",
        dep = reason.kind,
        upstream = reason.upstream.dotted(),
        required = reason.required_phase,
        observed = reason.observed_phase.as_deref().unwrap_or("(absent)"),
    );
    LavaAnomaly::new(AnomalyKind::DependencyBlocked, severity, dependent, message)
        .with_metadata("upstream", reason.upstream.dotted())
        .with_metadata("required_phase", &reason.required_phase)
        .with_metadata("observed_phase", reason.observed_phase.as_deref().unwrap_or("absent"))
}

#[derive(Debug, Error)]
pub enum DependencyError {
    #[error("cycle detected involving {0}")]
    Cycle(String),
}

// ── DAG validation ────────────────────────────────────────────────

/// Detect cycles in the dependency DAG. Returns the first cycle
/// found (as a path of dotted addresses).
///
/// # Errors
/// Returns [`DependencyError::Cycle`] when a cycle exists.
pub fn validate_acyclic(deps: &[LavaArchitectureDependency]) -> Result<(), DependencyError> {
    use std::collections::{HashMap, HashSet};
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for d in deps {
        adj.entry(d.from.dotted())
            .or_default()
            .push(d.to.dotted());
    }
    fn dfs(
        node: &str,
        adj: &HashMap<String, Vec<String>>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
    ) -> Result<(), DependencyError> {
        if visiting.contains(node) {
            return Err(DependencyError::Cycle(node.to_string()));
        }
        if visited.contains(node) {
            return Ok(());
        }
        visiting.insert(node.to_string());
        if let Some(children) = adj.get(node) {
            for c in children {
                dfs(c, adj, visiting, visited)?;
            }
        }
        visiting.remove(node);
        visited.insert(node.to_string());
        Ok(())
    }
    let mut visiting = std::collections::HashSet::new();
    let mut visited = std::collections::HashSet::new();
    for node in adj.keys() {
        dfs(node, &adj, &mut visiting, &mut visited)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: &str) -> ResourceAddress {
        ResourceAddress::new("rio", "infra", name)
    }

    fn blocks_on(from: &ResourceAddress, to: &ResourceAddress) -> LavaArchitectureDependency {
        LavaArchitectureDependency {
            from: from.clone(),
            to: to.clone(),
            kind: DependencyKind::BlocksOn,
            require_phase: "Applied".to_string(),
        }
    }

    fn influences(from: &ResourceAddress, to: &ResourceAddress) -> LavaArchitectureDependency {
        LavaArchitectureDependency {
            from: from.clone(),
            to: to.clone(),
            kind: DependencyKind::Influences,
            require_phase: "Applied".to_string(),
        }
    }

    #[test]
    fn ready_when_no_dependencies() {
        let verdict = DependencyResolver
            .resolve(&addr("app"), &[], &InMemoryPhaseRegistry::new());
        assert!(verdict.ready_for_reconcile());
        assert!(verdict.blocks.is_empty());
    }

    #[test]
    fn ready_when_blocks_on_upstream_is_applied() {
        let app = addr("app");
        let vpc = addr("vpc");
        let registry = InMemoryPhaseRegistry::new().with(&vpc, "Applied");
        let verdict =
            DependencyResolver.resolve(&app, &[blocks_on(&app, &vpc)], &registry);
        assert!(verdict.ready_for_reconcile());
    }

    #[test]
    fn blocked_when_blocks_on_upstream_is_pending() {
        let app = addr("app");
        let vpc = addr("vpc");
        let registry = InMemoryPhaseRegistry::new().with(&vpc, "Pending");
        let verdict =
            DependencyResolver.resolve(&app, &[blocks_on(&app, &vpc)], &registry);
        assert!(!verdict.ready_for_reconcile());
        assert_eq!(verdict.blocks.len(), 1);
        assert_eq!(verdict.blocks[0].observed_phase.as_deref(), Some("Pending"));
    }

    #[test]
    fn blocked_when_blocks_on_upstream_is_missing() {
        let app = addr("app");
        let vpc = addr("vpc");
        let registry = InMemoryPhaseRegistry::new();
        let verdict =
            DependencyResolver.resolve(&app, &[blocks_on(&app, &vpc)], &registry);
        assert!(!verdict.ready_for_reconcile());
        assert_eq!(verdict.blocks[0].observed_phase, None);
    }

    #[test]
    fn influences_does_not_gate_readiness_but_surfaces_block() {
        let app = addr("app");
        let dns = addr("dns");
        let registry = InMemoryPhaseRegistry::new().with(&dns, "Pending");
        let verdict =
            DependencyResolver.resolve(&app, &[influences(&app, &dns)], &registry);
        assert!(verdict.ready_for_reconcile());
        assert_eq!(verdict.blocks.len(), 1);
        assert_eq!(verdict.blocks[0].kind, DependencyKind::Influences);
    }

    #[test]
    fn failed_upstream_is_tagged_as_downstream_of_failed() {
        let app = addr("app");
        let vpc = addr("vpc");
        let registry = InMemoryPhaseRegistry::new().with(&vpc, "Failed");
        let verdict =
            DependencyResolver.resolve(&app, &[blocks_on(&app, &vpc)], &registry);
        assert!(!verdict.ready_for_reconcile());
        assert_eq!(verdict.downstream_of_failed, vec![vpc]);
    }

    #[test]
    fn resolver_ignores_dependencies_not_originating_at_self() {
        let app = addr("app");
        let other = addr("other");
        let vpc = addr("vpc");
        let registry = InMemoryPhaseRegistry::new().with(&vpc, "Pending");
        let verdict = DependencyResolver.resolve(
            &app,
            &[blocks_on(&other, &vpc)],
            &registry,
        );
        assert!(verdict.ready_for_reconcile());
        assert!(verdict.blocks.is_empty());
    }

    #[test]
    fn emit_blocked_anomaly_carries_typed_metadata() {
        let app = addr("app");
        let vpc = addr("vpc");
        let reason = BlockReason {
            upstream: vpc.clone(),
            required_phase: "Applied".to_string(),
            observed_phase: Some("Pending".to_string()),
            kind: DependencyKind::BlocksOn,
        };
        let anomaly = emit_blocked_anomaly(app, &reason);
        assert_eq!(anomaly.kind, AnomalyKind::DependencyBlocked);
        assert_eq!(anomaly.severity, Severity::Functional);
        assert_eq!(anomaly.metadata["upstream"], "rio/infra/vpc");
        assert_eq!(anomaly.metadata["required_phase"], "Applied");
        assert_eq!(anomaly.metadata["observed_phase"], "Pending");
    }

    #[test]
    fn influences_emit_is_cosmetic_severity() {
        let app = addr("app");
        let dns = addr("dns");
        let reason = BlockReason {
            upstream: dns,
            required_phase: "Applied".to_string(),
            observed_phase: None,
            kind: DependencyKind::Influences,
        };
        let anomaly = emit_blocked_anomaly(app, &reason);
        assert_eq!(anomaly.severity, Severity::Cosmetic);
    }

    #[test]
    fn validate_acyclic_accepts_a_chain() {
        let a = addr("a");
        let b = addr("b");
        let c = addr("c");
        validate_acyclic(&[blocks_on(&a, &b), blocks_on(&b, &c)]).unwrap();
    }

    #[test]
    fn validate_acyclic_rejects_a_cycle() {
        let a = addr("a");
        let b = addr("b");
        let err = validate_acyclic(&[blocks_on(&a, &b), blocks_on(&b, &a)]).unwrap_err();
        matches!(err, DependencyError::Cycle(_));
    }

    #[test]
    fn dependency_round_trips_through_serde() {
        let d = blocks_on(&addr("app"), &addr("vpc"));
        let j = serde_json::to_string(&d).unwrap();
        assert!(j.contains(r#""kind":"BlocksOn""#));
        let parsed: LavaArchitectureDependency = serde_json::from_str(&j).unwrap();
        assert_eq!(d, parsed);
    }
}
