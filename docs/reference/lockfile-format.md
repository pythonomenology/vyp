# Lock File Format

vyp produces lock files in the **PEP 751** `pylock.toml` format. This reference documents the structure as used by vyp.

## Overview

A lock file pins exact package versions and records provenance. It is deterministic and suitable for version control.

---

## Top-Level Fields

| Field | Type | Description |
|-------|------|-------------|
| `lock-version` | string | Lock format version (e.g. `"1.0"`). |
| `created-by` | string | Tool and version that generated the lock (e.g. `"vyp 0.1.0"`). |
| `requires-python` | string (optional) | Python version requirement. |
| `environments` | array of strings | Environment identifiers. |
| `extras` | array of strings | Requested extras. |
| `dependency-groups` | array of strings | Requested dependency groups. |
| `default-groups` | array of strings | Default groups to include. |
| `packages` | array of package objects | Resolved packages. |
| `tool.vyp.errata-feeds` | array of tables (optional) | Errata feeds consulted: `source` and the `sha256` of the content used. |
| `tool.vyp.exclude-newer` | string (optional) | The upload-time cutoff the lock was resolved with. |

---

## Package Fields

Each entry in `packages`:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Package name. |
| `version` | string | Resolved version. |
| `requires-python` | string (optional) | Python version requirement for this package. |
| `dependencies` | array of strings | Dependency specifiers. |
| `wheels` | array of wheel objects | Available wheel artifacts. |
| `sdist` | object (optional) | Source distribution; vyp builds it at install time when `wheels` is empty. |
| `directory` | object (optional) | A local source tree: `path` (relative to the lock), `editable`. |
| `vcs` | object (optional) | A checkout: `type` (`git`), `url`, `requested-revision`, `commit-id`, `subdirectory`. |
| `marker` | string (optional) | PEP 508 environment marker. |
| `index` | string (optional) | Index URL this package was fetched from. |
| `variant` | object (optional) | PEP 825 variant descriptor (e.g. platform tag). |
| `tool` | object (optional) | Tool-specific metadata (see below). |

---

## Wheel Fields

Each wheel in `packages[].wheels`:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Wheel filename. |
| `url` | string (optional) | Download URL. |
| `size` | integer (optional) | Size in bytes. |
| `hashes` | table (optional) | Hash algorithm → digest (e.g. `sha256 = "..."`). |

---

## Sdist Fields

The `sdist` object:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Sdist filename. |
| `url` | string (optional) | Download URL. |
| `size` | integer (optional) | Size in bytes. |
| `hashes` | table (optional) | Hash algorithm → digest. |

---

## Tool.vyp Provenance

vyp stores provenance in `packages[].tool.vyp`. PEP 751 allows `[tool]` at the package level; it MUST NOT affect installation behavior.

| Field | Type | Description |
|-------|------|-------------|
| `selected_by` | string | How this version was selected. |
| `requested_by` | array of strings | Packages that requested this dependency. |
| `conflict_with` | array of strings | Packages this version conflicts with. |
| `resolution_path` | string (optional) | Causal chain of resolution. |
| `overrides` | array of tables (optional) | Overrides that replaced every requirement on this package: `constraint`, `origin` (`pyproject.toml` or the release that shipped it, e.g. `lib-a==1.0`), `reason`. |
| `errata` | array of tables (optional) | Errata that affected this package: `versions`, `effect` (`exclude` or `requires`), `requires`, `reason`, `source`. |
| `backtracked` | table (optional) | This release was chosen only because newer ones cap a dependency it leaves unbounded: `newest`, `skipped`, `dependency`, `newest-requires`, `message`. |

### selected_by Values

| Value | Description |
|-------|-------------|
| `normal` | Standard resolution. |
| `conflict-resolution` | Chosen by conflict strategy. |
| `override` | Forced by `[[tool.vyp.overrides]]`. |
| `substitution` | Chosen from substitution set. |
| `plugin:<name>` | Chosen by a plugin strategy. |

---

## Example

```toml
lock-version = "1.0"
created-by = "vyp 0.1.0"
requires-python = ">=3.10"
environments = []
extras = []
dependency-groups = []
default-groups = []

[[packages]]
name = "numpy"
version = "1.26.4"
dependencies = []
wheels = []
sdist = null
marker = null
index = null
variant = null

[packages.tool.vyp]
selected_by = "override"
requested_by = ["pandas", "scipy"]
conflict_with = []
resolution_path = "project -> pandas -> numpy"
```
