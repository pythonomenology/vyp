# pyproject.toml Configuration

vyp reads configuration from the `[tool.vyp]` section of `pyproject.toml`. This reference documents every supported key.

## Overview

```toml
[tool.vyp]
resolution-strategy = "highest"
pre-releases = "disallow"
index-url = "https://pypi.org/simple"
torch-backend = "auto"

[[tool.vyp.extra-index]]
name = "pytorch"
url = "https://download.pytorch.org/whl/cu128"
explicit = false

[tool.vyp.sources]
torch = [{ index = "pytorch", marker = null }]

[[tool.vyp.overrides]]
package = "numpy"
constraint = ">=1.26,<2"
reason = "Pin numpy to 1.x for CUDA compatibility"

[[tool.vyp.overrides]]
package = "scipy"
constraint = ">=1.11,<2"
transitive = true
reason = "Ensure stable scipy across all transitive deps"

[[tool.vyp.substitutions]]
provides = "opencv"
packages = ["opencv-python", "opencv-python-headless"]
prefer = "opencv-python-headless"

[tool.vyp.plugins]
search-paths = ["./vyp-plugins/"]
[[tool.vyp.plugins.load]]
name = "my-plugin"
path = "./target/release/libmy_plugin.so"
config = { key = "value" }
```

---

## Core Settings

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `resolution-strategy` | string | `"highest"` | Version selection strategy. See [Resolution Strategy](#resolution-strategy). |
| `pre-releases` | string | `"disallow"` | Pre-release version policy. See [Pre-release Policy](#pre-release-policy). |
| `index-url` | string | `"https://pypi.org/simple"` | Primary package index URL (PEP 503 Simple API). |
| `torch-backend` | string | *(none)* | PyTorch accelerator backend. See [Torch Backend](#torch-backend). |
| `environments` | array of strings | `[]` | Environment markers for universal resolution. See [Universal Resolution](#universal-resolution). |
| `fork-strategy` | string | `"requires-python"` | When using multiple environments: `"requires-python"` or `"fewest"`. See [Universal Resolution](#universal-resolution). |
| `pre-release-packages` | array of strings | `[]` | Packages whose pre-releases are allowed whatever `pre-releases` says. See [Resolution](../concepts/resolution.md#pre-releases-for-specific-packages). |
| `python` | string | *(none)* | Interpreter for the project's `.venv`: `3.12`, `>=3.11`, `3.13t` or a path. See [Python](../guides/python.md). |
| `sdists` | string | `"fallback"` | When versions that need an sdist build are considered: `"never"`, `"fallback"`, `"allow"`. See [Source Distributions](../guides/dependencies.md#source-distributions). |
| `exclude-newer` | string | *(none)* | Ignore files uploaded after a date, timestamp, or duration ago (`"P7D"`, `"7 days"`). See [Supply-Chain Safety](../guides/supply-chain.md). |
| `[tool.vyp.exclude-newer-package]` | table | `{}` | Per-package cutoffs: `pkg = "2026-01-01"`, or `pkg = false` to exempt it. |
| `startup-files` | string | `"warn"` | Packages that install code run at every interpreter start: `"warn"`, `"deny"`, `"allow"`. |
| `allow-startup-files` | array of strings | `[]` | Packages allowed to ship startup files. |

### Resolution Strategy

| Value | Description |
|-------|-------------|
| `"highest"` | Prefer the highest compatible version (default). |
| `"lowest"` | Prefer the lowest compatible version. |
| `"lowest-direct"` | Prefer lowest for direct dependencies, highest for transitive. |

### Pre-release Policy

| Value | Description |
|-------|-------------|
| `"disallow"` | Exclude pre-release versions (default). |
| `"allow"` | Include pre-release versions in resolution. |
| `"if-necessary"` | Use pre-releases only when no stable version satisfies constraints. |

### Torch Backend

| Value | Description |
|-------|-------------|
| `"auto"` | Detect GPU (NVIDIA CUDA, AMD ROCm, Intel XPU) and select matching index. |
| `"cpu"` | Use CPU-only PyTorch wheels from `https://download.pytorch.org/whl/cpu`. |
| `"cu118"` | CUDA 11.8. |
| `"cu121"` | CUDA 12.1. |
| `"cu124"` | CUDA 12.4. |
| `"cu126"` | CUDA 12.6. |
| `"cu128"` | CUDA 12.8. |
| `"cu130"` | CUDA 12.10 / 13.0. |
| `"rocm6"` | AMD ROCm 6.x. |
| `"xpu"` | Intel XPU. |

When set, torch-related packages (`torch`, `torchvision`, `torchaudio`, etc.) are fetched from the corresponding PyTorch wheel index.

---

## Universal Resolution

When you need one lockfile for **multiple Python versions** (e.g. 3.8 and 3.9), set `environments` to a list of PEP 508 environment marker strings. vyp will resolve once per environment and merge the results into a single lockfile. Each package may appear multiple times with different versions and a `marker` so installs only get the right subset for the current interpreter.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `environments` | array of strings | `[]` | List of disjoint environment markers (e.g. `python_version == "3.8"`, `python_version == "3.9"`). When empty, single-environment resolution (current interpreter). |
| `fork-strategy` | string | `"requires-python"` | How to choose versions across environments: `"requires-python"` = one version per environment (latest per Python band); `"fewest"` = minimize distinct versions (one version for all envs when possible). |

Example:

```toml
[tool.vyp]
environments = [
    "python_version == \"3.8\"",
    "python_version == \"3.9\"",
    "python_version == \"3.10\""
]
fork-strategy = "fewest"
```

On `vyp lock`, the lockfile will contain package entries with optional `marker` fields. On `vyp install`, only packages whose marker matches the current environment (or have no marker) are installed.

---

## Extra Indexes

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `[[tool.vyp.extra-index]]` | array of tables | `[]` | Additional package indexes. |

Each `[[tool.vyp.extra-index]]` entry:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | *(required)* | Index identifier (used in `[tool.vyp.sources]`). |
| `url` | string | *(required)* | Index URL (PEP 503 Simple API). |
| `explicit` | boolean | `false` | If `true`, this index is only used for packages explicitly routed via `[tool.vyp.sources]`. |

---

## Source Routing

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `[tool.vyp.sources]` | table | `{}` | Per-package index routing. |

Each key is a package name; the value is an array of source entries:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `index` | string | — | Name of an extra index (from `[[tool.vyp.extra-index]]`). |
| `path` | string | — | A local source tree, relative to `pyproject.toml`. |
| `editable` | boolean | `false` | With `path`: install it in editable mode. |
| `git` | string | — | A git repository URL. |
| `rev` / `tag` / `branch` | string | *(HEAD)* | With `git`: what to check out. |
| `subdirectory` | string | — | With `git`: the package's directory in the repository. |
| `marker` | string | *(none)* | Optional PEP 508 environment marker (e.g. `sys_platform == 'linux'`). |

Each entry sets exactly one of `index`, `path` or `git`. See [Local, Editable and Git Dependencies](../guides/dependencies.md#local-editable-and-git-dependencies).

---

## Overrides

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `[[tool.vyp.overrides]]` | array of tables | `[]` | Dependency version overrides. |

Each `[[tool.vyp.overrides]]` entry:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `package` | string | *(required)* | Package name to override. |
| `constraint` | string | *(required)* | PEP 440 version specifier (e.g. `">=1.26,<2"` or `"==1.24.0"`). |
| `transitive` | boolean | `false` | If `true`, propagate to consumers via `vyp-overrides.toml`. |
| `reason` | string | *(none)* | Human-readable reason for the override. |

### Inherited overrides

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `inherit-overrides` | boolean or array of strings | `true` | Apply overrides that selected dependencies ship in their wheels' `vyp-overrides.toml`. `false` ignores them; an array allows only the listed packages. See [Transitive Conflicts](../concepts/transitive-conflicts.md#what-consumers-do-with-it). |

---

## Errata

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `errata` | array of strings | `[]` | Errata feeds: URLs, or paths relative to the project directory. See [Errata](../guides/errata.md). |

---

## Substitutions

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `[[tool.vyp.substitutions]]` | array of tables | `[]` | Package substitution sets (interchangeable packages). |

Each `[[tool.vyp.substitutions]]` entry:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `provides` | string | *(required)* | Virtual capability name (e.g. `"opencv"`). |
| `packages` | array of strings | *(required)* | Concrete packages that fulfill this capability. |
| `prefer` | string | *(none)* | Preferred package when multiple satisfy the capability. |

Only one package from the set is installed. Use for alternatives like `opencv-python` vs `opencv-python-headless`.

---

## Plugins

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `[tool.vyp.plugins]` | table | *(none)* | Plugin configuration. |
| `[tool.vyp.plugins].search-paths` | array of strings | `[]` | Directories to scan for `.so`, `.dylib`, `.dll` plugin libraries. |
| `[[tool.vyp.plugins.load]]` | array of tables | `[]` | Explicit plugin load entries. |

Each `[[tool.vyp.plugins.load]]` entry:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | *(required)* | Plugin name (for display). |
| `path` | string | *(none)* | Path to the plugin library file. |
| `config` | table | `{}` | Plugin-specific configuration passed at load time. |
