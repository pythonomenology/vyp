# vyp

[![CI](https://github.com/vyp-lang/vyp/actions/workflows/ci.yml/badge.svg)](https://github.com/vyp-lang/vyp/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A transitive-conflict-aware Python dependency resolver built in Rust, using the PubGrub algorithm.

**[Documentation](https://vyp.dev)**

## Overview

vyp solves dependency resolution for Python projects with features not found in existing tools:

- **Transitive Conflict Declarations** — Conflict resolution rules propagate through the entire dependency graph, not just within a single project
- **Scoped Named Indexes** — A named index attached to a dependency serves that package *and its transitive closure*, and is never consulted for unrelated dependencies (avoids stale-mirror cross-contamination, e.g. PyTorch's index)
- **Plugin System** — Extensible conflict strategies, metadata providers, and resolution filters without modifying the core solver
- **Package Substitution** — Declare interchangeable packages (e.g., `opencv-python` and `opencv-python-headless`)
- **Dependency Provenance** — Full causal chain explaining why each version was selected
- **Unified Dependency Overrides** — Range constraints and exact pins in a single `[[tool.vyp.overrides]]` array; libraries ship transitive overrides in their wheels and consumers inherit them
- **Python & Environments** — Finds installed interpreters and installs checksum-verified managed CPython builds; creates (optionally relocatable) virtual environments and records what each was built from, refusing to install into one whose interpreter or host changed (`vyp venv --check`)
- **Path, Editable, Git & Sdist Dependencies** — A PEP 517/660 build frontend with cached, isolated build environments; git refs locked to commits
- **PEP 723 Scripts** — `vyp run script.py` in a cached environment; `vyp lock --script` embeds a PEP 751 lock in the script itself
- **Supply-Chain Guards** — `exclude-newer` upload cutoffs (per package, recorded in the lock), and a gate on packages that install code run at every interpreter start (`.pth`, `sitecustomize`)
- **Backtracking Warnings** — Flags releases chosen only because newer ones cap a dependency the old one leaves unbounded
- **Errata Feeds** — Correct the `Requires-Dist`/`Requires-Python` of releases that are already published, or exclude broken ones, without waiting for a new upload
- **PEP 751 Lock Files** — Standards-compliant `pylock.toml` output, with pinned wheel integrity hashes
- **Platform-aware Resolution** — Versions with no installable wheel for the target environment are filtered out (libc-aware manylinux/musllinux tags, current-macOS deployment targets, free-threaded ABIs), so the resolver backtracks to an installable version instead of locking an unusable one
- **`Requires-Python` & Yank Enforcement** — Versions are excluded when their distributions don't match the target Python (PEP 440) or are yanked (PEP 592)
- **Spec-compliant Installs** — Entry-point launchers, `*.data/` scheme relocation, shebang rewriting, and `RECORD`/`INSTALLER` generation, plus `uninstall` and `sync`
- **Private Index Auth** — `~/.netrc` credentials applied to index and download requests
- **Resolution Explain & Diff** — Trace version selections and compare lock files
- **Space-efficient Cache** — Content-addressed metadata cache with LRU eviction

## Why vyp over pip and uv?

`pip` is the default but slow, with a backtracking resolver and no native lock
file. `uv` is a fast, excellent general-purpose installer. vyp targets the cases
those tools handle awkwardly: **conflict-aware resolution across multiple
indexes**, with an extensible core — while staying competitive on speed.

| | pip | uv | **vyp** |
|---|---|---|---|
| Implementation | Python | Rust | Rust |
| Resolver | `resolvelib` backtracking | PubGrub | PubGrub (two-watched-literals) |
| Human-readable conflict reports | basic | yes | yes (full derivation tree) |
| Lock file | none (needs pip-tools) | `uv.lock` (tool-specific) | **PEP 751 `pylock.toml`** (standard) |
| Transitive conflict declarations | ✗ | ✗ | **✓ propagate through the whole graph** |
| Named index → transitive closure | ✗ | per-package pins | **✓ scoped to the dep's whole subtree** |
| Package substitution | ✗ | ✗ | **✓ (`opencv-python` ⇄ `-headless`)** |
| Unified overrides (range + pin, transitive) | constraints files | overrides | **✓ single declaration** |
| Dependency provenance / `explain` | ✗ | partial | **✓ full causal chain** |
| Plugin system (strategies / providers / filters) | ✗ | ✗ | **✓** |

**What this buys you**

- **Multi-index resolution that doesn't cross-contaminate.** Point a named index
  (e.g. a PyTorch CUDA index) at `torch`, and vyp serves `torch` *and its entire
  transitive closure* from there — but never reaches for that index when
  resolving unrelated dependencies. No more stale mirror copies of PyPI packages
  leaking in. See [Scoped named indexes](#scoped-named-indexes).
- **Conflicts that travel.** A library can declare an incompatibility once and
  have it enforced in every project that depends on it, instead of every
  consumer rediscovering the same conflict by hand.
- **Standards-first lock files.** vyp emits PEP 751 `pylock.toml`, readable by any
  compliant tool — not a bespoke format.
- **An extensible solver.** Custom conflict strategies, metadata providers, and
  resolution filters plug in without forking the core PubGrub loop.

### Performance

vyp is built in Rust with fully parallel, lazy metadata fetching, a compact
content-addressed cache, and a two-watched-literals PubGrub core. It is **orders
of magnitude faster than pip** and **competitive with uv**.

In the repository's 20-dependency benchmark (71 packages resolved), warm/cached
resolution runs in roughly **100 ms** on our hardware — faster than `uv`'s
equivalent offline resolve — and cold resolution is on par. Numbers are
network- and machine-dependent; reproduce them yourself:

```bash
benchmarks/install-20-packages/run_benchmark.sh          # warm
benchmarks/install-20-packages/run_benchmark.sh --cold   # cold
```

uv remains the more mature general-purpose tool (universal resolution, a large
ecosystem, very fast installs); vyp's edge is the conflict- and index-aware
resolution model above.

## Installation

```bash
# From PyPI (prebuilt binary)
pip install vyp

# From source
cargo install --path crates/vyp
```

## Usage

### Add dependencies

```bash
# Add to [project].dependencies, re-lock, and install
vyp add numpy "pandas>=2.0" "requests[security]>=2.28"

# Add to an optional dependency group
vyp add --optional dev pytest pytest-cov

# Add to a PEP 735 dependency group
vyp add --group test pytest

# Add + lock but skip install
vyp add --no-install numpy

# Just edit pyproject.toml
vyp add --no-lock numpy
```

### Resolve dependencies

```bash
# Resolve from pyproject.toml (preview without writing lock file)
vyp resolve

# Resolve with additional requirements
vyp resolve -r "numpy>=1.20" -r "pandas>=2.0"
```

### Generate a lock file

```bash
vyp lock
```

### Install from lock file

```bash
vyp install                     # installs into the active or local .venv
vyp install --venv .venv        # install into a specific venv
vyp install --dry-run           # preview what would be installed
```

Installs are spec-compliant: `console_scripts`/`gui_scripts` entry points are
turned into executable launchers in the venv's `bin/`, a wheel's
`*.data/{scripts,data,headers,purelib,platlib}` payloads are relocated to the
correct scheme directories, `#!python` shebangs are rewritten to the venv
interpreter, and each distribution gets an `INSTALLER` and a PEP 376 `RECORD`
so installs are introspectable and uninstallable.

### Uninstall / sync

```bash
vyp uninstall rich pygments      # remove packages (RECORD-based, removes scripts too)
vyp sync                         # make the venv exactly match the lock (install + prune extras)
vyp sync --lockfile pylock.toml  # sync from an explicit lock
vyp sync --dry-run               # preview adds/removals
```

### Python, environments and scripts

```bash
vyp python install 3.12          # managed CPython (checksum-verified)
vyp venv --python 3.12           # create .venv (sync/install create it automatically)
vyp venv --relocatable           # movable environment
vyp venv --check                 # stale interpreter, other host, or changed packages?
vyp run script.py                # PEP 723 script in a cached environment
vyp lock --script script.py      # embed a PEP 751 lock in the script
vyp run pytest -q                # command inside the project environment
```

### Explain a package selection

```bash
vyp explain numpy
```

### Diff lock files

```bash
vyp diff old.pylock.toml pylock.toml
```

### Manage dependency overrides

```bash
vyp override add numpy ">=1.26,<2"
vyp override add numpy "==1.24.0" --transitive --reason "GPU/CPU fix"
vyp override list                 # lists all overrides, marks transitive ones
vyp override remove numpy
vyp override export               # export transitive overrides for library consumers
```

### View inherited conflicts

```bash
vyp conflict list     # list conflicts inherited from resolved dependencies
```

### Inspect plugins

```bash
vyp plugin list
```

## Configuration

Add to your `pyproject.toml`:

```toml
[tool.vyp]
resolution-strategy = "highest"  # highest | lowest | lowest-direct
pre-releases = "disallow"        # allow | disallow | if-necessary
errata = ["ci/errata.toml"]      # corrections to already-published releases

# Dependency overrides (range constraint, local only by default)
[[tool.vyp.overrides]]
package = "numpy"
constraint = ">=1.26,<2"
reason = "Pin numpy to 1.x for CUDA compatibility"

# Transitive override propagates to consumers
[[tool.vyp.overrides]]
package = "scipy"
constraint = ">=1.11,<2"
transitive = true
reason = "Ensure stable scipy across all transitive deps"

# Package substitutions
[[tool.vyp.substitutions]]
provides = "opencv"
packages = ["opencv-python", "opencv-python-headless"]
prefer = "opencv-python-headless"

# Plugin system
[tool.vyp.plugins]
search-paths = ["./vyp-plugins/"]
```

### Scoped named indexes

Declare an explicit named index and route a dependency to it. The index then
serves that package **and its direct and transitive dependencies** — but it is
**not** consulted for any other top-level dependency or its closure:

```toml
[[tool.vyp.extra-index]]
name = "torch"
url = "https://download.pytorch.org/whl/cu128"
explicit = true

# `torch` and everything only `torch` needs resolve from the torch index.
# Unrelated dependencies (and shared packages) keep resolving from PyPI.
[tool.vyp.sources]
torch = [{ index = "torch" }]
```

This solves the common PyTorch problem where an accelerator index mirrors a
subset of PyPI with out-of-date copies: those mirrored packages are only used
when they are genuinely (and exclusively) part of `torch`'s dependency tree.

**Overlap policy — the default index wins.** If a package is reachable from
both a scoped root (e.g. `torch`) and an ordinary dependency, it resolves from
the **default index (PyPI)**, not the named index. Only packages reachable
*exclusively* through the named index's roots are routed there; the declared
root packages themselves are always authoritative to their index. Membership is
discovered as the dependency graph is walked; in rare diamond-shaped graphs you
can force a decision by routing the shared package explicitly in
`[tool.vyp.sources]`.

The `torch-backend` setting (auto/cpu/cuXYZ/rocm/xpu) uses the same scoped
machinery automatically, so the matching CUDA/ROCm builds of torch's
dependencies come from the accelerator index while the rest of your project
stays on PyPI.

## Architecture

vyp is organized as a Cargo workspace with five crates under **`crates/`**:

| Crate | Location | Purpose |
|-------|----------|---------|
| `vyp-api` | `crates/vyp-api/` | Stable plugin interface: trait definitions, shared types, ABI contract |
| `vyp-resolver` | `crates/vyp-resolver/` | Pure PubGrub solver and version-set logic |
| `vyp-index` | `crates/vyp-index/` | PyPI client, in-memory index, disk cache, wheel compat |
| `vyp-core` | `crates/vyp-core/` | Orchestration: ResolverBuilder, conflict propagation, built-in strategies |
| `vyp` | `crates/vyp/` | CLI binary: config parsing, lock file I/O, plugin loading |

### Plugin System

Plugins implement traits from `vyp-api`:

- **`ConflictStrategy`** — Custom conflict resolution logic
- **`MetadataProvider`** — Alternative package metadata sources
- **`ResolutionFilter`** — Pre-filter or re-rank version candidates

The PubGrub solver loop itself is sealed — plugins influence resolution through well-defined hook points but cannot bypass the core algorithm.

## Development

```bash
# Build
cargo build

# Test
cargo test

# Run with tracing
RUST_LOG=debug cargo run -- resolve
```

## License

MIT
