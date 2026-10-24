# Resolution

vyp uses the **PubGrub** algorithm—a modern, conflict-driven dependency resolver—to find a consistent set of package versions that satisfy all constraints in your dependency graph.

## PubGrub Algorithm Overview

PubGrub treats dependency resolution as a **Boolean satisfiability problem**. It builds a partial assignment of package versions and derives logical implications until either a complete solution is found or a conflict is proven.

### Core Mechanisms

**Unit propagation** — When only one version of a package satisfies all current constraints, that version is forced (a "unit" in SAT terms). The resolver propagates this decision through the graph.

**Decision making** — When no unit propagation applies, the resolver makes a heuristic choice (e.g., pick the highest available version). This is a "decision" that may later be backtracked.

**Conflict-driven clause learning** — When a conflict is detected (e.g., package A requires X>=2, package B requires X<2), PubGrub learns a new clause: "at least one of the prior decisions must be wrong." It backtracks and tries alternative choices until a solution is found or all options are exhausted.

!!! abstract "Algorithm flow (text diagram)"
    ```
    ┌─────────────────────────────────────────────────────────────┐
    │ 1. Start with root dependencies                            │
    │ 2. For each package in the partial solution:                 │
    │    - Fetch dependencies from metadata providers             │
    │    - Add constraints to the solver                          │
    │ 3. If unit propagation applies → assign version, continue   │
    │ 4. If conflict detected → learn clause, backtrack            │
    │ 5. Repeat until complete solution or no solution exists     │
    └─────────────────────────────────────────────────────────────┘
    ```

## Resolution Strategies

vyp determines *which* version to try first when multiple versions satisfy the constraints. This affects both the initial decision and the order in which alternatives are explored during backtracking.

| Strategy | Description | Use case |
|----------|-------------|----------|
| **highest** | Prefer the newest available version (default) | Typical: maximize features and fixes |
| **lowest** | Prefer the oldest available version | Testing compatibility with older versions |
| **lowest-direct** | Lowest for direct dependencies, highest for transitive | Balance stability of direct deps with freshness of transitive |

Configure in `pyproject.toml`:

```toml
[tool.vyp]
resolution-strategy = "highest"   # or "lowest", "lowest-direct"
```

!!! tip "Testing lowest versions"
    Use `resolution-strategy = "lowest"` when you want to verify that your project works with the minimum supported versions of your dependencies.

## Pre-release Policies

Pre-release versions (alpha, beta, rc) are excluded by default unless explicitly required or allowed.

| Policy | Behavior |
|--------|----------|
| **disallow** | Never consider pre-releases (default) |
| **allow** | Pre-releases are always considered |
| **if-necessary** | Only consider pre-releases when no stable version satisfies the constraints |

```toml
[tool.vyp]
pre-releases = "disallow"   # or "allow", "if-necessary"
```

!!! warning "Pre-releases"
    Pre-releases can be unstable and change without notice. Use `allow` only when you intentionally need bleeding-edge versions.

### Pre-releases for specific packages

Allowing pre-releases globally lets every dependency's pre-releases in too: `torch`'s nightly pulls in an `mpmath` alpha that breaks `sympy`. Name the packages instead:

```toml
[tool.vyp]
pre-release-packages = ["torch"]
```

Their dependencies still resolve to stable releases. A requirement whose specifier names a pre-release, such as `torch>=2.6.0rc1`, opts that package in automatically, as PEP 440 intends.

## Backtracking Into Old Releases

A common way for a resolution to succeed but install something broken: a library's newer releases add an upper bound on a dependency (`libx<2`), your environment needs `libx>=2`, so the resolver falls back to an older release of the library that simply never declared the bound. vyp detects this pattern after resolving and warns:

```
warning: green 3.9 was selected instead of green 4.1 (1 newer release skipped): green 4.1 requires libx<2
but libx 2.0 is selected. green 3.9 doesn't declare that limit, so it may be just as incompatible with libx 2.0.
```

It only warns when nothing else ruled the newer releases out (your own constraints or another package's), when the newer release caps the dependency below the selected version (a raised lower bound is fine), and when the older release leaves it unbounded. The warning is recorded in the lock (`[packages.tool.vyp.backtracked]`) and shown by `vyp explain`. It runs with the default `highest` strategy.

## Metadata Providers

Providers supply package metadata (versions, dependencies). Multiple providers can be registered; they are consulted in **priority order** (higher priority first).

### Priority

- **Higher priority values** are consulted first.
- **First-match wins**: the first provider that returns `Some` for a package wins.
- Explicit/extra indexes typically get higher priority than the default PyPI index.

!!! abstract "Provider consultation order"
    ```
    Provider A (priority 20) → can_provide? → get_metadata
         ↓
    Provider B (priority 10) → can_provide? → get_metadata
         ↓
    ...
    ```

### Use cases

- **Private registries**: Add a corporate registry with higher priority than PyPI.
- **Package filtering**: Limit a provider to specific packages (e.g., PyTorch index).
- **Local caches**: A cache provider can serve metadata without hitting the network.

### How metadata is fetched

Resolution runs on a dedicated thread; metadata is fetched by providers (e.g. PyPI) on background workers. The resolver **prefetches** packages it will likely need—root dependencies first, then undecided packages each round. When it actually needs versions or dependencies it may block until the prefetch completes (or use the disk cache). This keeps resolution I/O concurrent with solver work where possible: data is often ready by the time the solver asks for it.

## Overrides and Resolution Behavior

Overrides modify how constraints are resolved during conflict detection.

### Exact pins

Exact pins (e.g., `numpy==1.26.0`) are applied **before** the solver runs. The provider only returns the pinned version for that package; all other versions are effectively ignored.

### Range constraints

Range-based overrides (e.g., `numpy>=1.24,<2`) are applied by the **override conflict strategy** when a conflict occurs. The strategy rewrites the contested package's range to the override range, allowing resolution to proceed.

```toml
[[tool.vyp.overrides]]
package = "numpy"
constraint = ">=1.24,<2"
transitive = true   # optional: propagate to consumers
```

!!! note "Overrides vs. conflicts"
    Overrides are applied when the resolver *would* fail due to conflicting requirements. They do not change the initial decision order; they only affect how conflicts are resolved.

## Universal Resolution

When you need a **single lockfile for multiple Python versions** (e.g. 3.8, 3.9, 3.10), you can enable **universal resolution** by setting `[tool.vyp] environments` to a list of environment marker strings. vyp resolves once per environment and merges the results into one lockfile; package entries may have a `marker` so that `vyp install` only installs the subset applicable to the current interpreter.

### Configuration

In `pyproject.toml`:

```toml
[tool.vyp]
environments = [
    "python_version == \"3.8\"",
    "python_version == \"3.9\""
]
fork-strategy = "requires-python"   # or "fewest"
```

- **environments**: List of disjoint PEP 508 marker strings. Supported form for the first version: `python_version == "X.Y"`. When empty, resolution is single-environment (current behavior).
- **fork-strategy**: When resolving for multiple environments:
  - **requires-python** (default): Prefer the latest version *per* environment. The lockfile will have one version per package per Python band (with markers).
  - **fewest**: Prefer as **few distinct versions** as possible; when all environments resolve to the same version for a package, a single lockfile entry (no marker) is emitted; otherwise one entry per environment with a marker.

### Install behavior

When installing from a universal lockfile, vyp evaluates each package’s `marker` against the **current** marker environment (detected from the active interpreter). Only packages with no marker or whose marker evaluates to true are installed.

## Summary

| Concept | Behavior |
|---------|----------|
| **Algorithm** | PubGrub: unit propagation, decisions, conflict-driven clause learning |
| **Strategy** | `highest` (default), `lowest`, `lowest-direct` |
| **Pre-releases** | `disallow` (default), `allow`, `if-necessary` |
| **Providers** | Priority-ordered, first-match wins |
| **Overrides** | Exact pins bypass solver; range overrides apply during conflict resolution |
| **Universal resolution** | `environments` = list of markers → resolve per env, merge; `fork-strategy` = `requires-python` or `fewest`; install filters by current marker |
