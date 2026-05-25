# lava-dependency

Typed cross-architecture dependency surface for the lava-suite.
L7 of the lava-suite.

```text
LavaArchitectureDependency
  ─ from: ResourceAddress         (the dependent)
  ─ to:   ResourceAddress         (the upstream)
  ─ kind: BlocksOn | Influences
  ─ require_phase: "Applied"      (default; CR can override)

DependencyResolver::resolve(dependent, deps, registry) -> ResolutionVerdict
  ─ ready_for_reconcile()   true unless a BlocksOn upstream is unsatisfied
  ─ blocks                  every unsatisfied dependency (mixed hard + soft)
  ─ downstream_of_failed    upstreams currently in Failed phase
```

## Abstractions

| Trait / type | Purpose |
|---|---|
| `LavaArchitectureDependency` | Typed DAG edge |
| `DependencyKind` | `BlocksOn` (hard) / `Influences` (soft) |
| `PhaseRegistry` trait | Lookup of current Phase by ResourceAddress |
| `InMemoryPhaseRegistry` | HashMap impl for tests + in-process |
| `DependencyResolver` | Pure function over deps + registry |
| `ResolutionVerdict { ready, blocks, downstream_of_failed }` | Decision |
| `emit_blocked_anomaly(dependent, reason)` | Adapter → `LavaAnomaly` |
| `validate_acyclic(deps)` | DAG validation |

## Severity inference

| Dependency kind | Block severity |
|---|---|
| `BlocksOn` | `Functional` (will reconverge once upstream catches up) |
| `Influences` | `Cosmetic` (just an alert) |

## Tests

12 unit tests cover empty deps, ready/blocked transitions, missing
upstream, Influences-vs-BlocksOn semantics, failed-upstream tagging,
self-only filter, anomaly metadata, severity inference, acyclic
validation (pass + fail), serde round-trip.
