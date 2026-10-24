# Adding, Removing, and Updating Dependencies

This guide covers how to add, remove, and update dependencies with vyp using the CLI and by editing `pyproject.toml`.

## Adding Dependencies

### Add to main dependencies

```bash
vyp add requests numpy
```

This appends packages to `[project].dependencies`, resolves the graph, updates `pylock.toml`, and installs into the active virtual environment.

!!! example "With version constraints"
    Use PEP 508 specifiers for version constraints:
    ```bash
    vyp add "pandas>=2.0" "requests[security]>=2.28"
    ```

!!! example "With extras"
    Request extras (optional features) in brackets:
    ```bash
    vyp add "requests[security,socks]"
    ```

### Add to optional dependencies

Add to a PEP 621 optional group:

```bash
vyp add --optional dev pytest mypy
vyp add --optional docs mkdocs mkdocs-material
```

This writes to `[project.optional-dependencies.<name>]`.

### Add to dependency groups

Add to a PEP 735 dependency group:

```bash
vyp add --group dev pytest-cov
vyp add --group lint ruff
```

This writes to `[dependency-groups.<name>]`.

!!! tip "Create group if missing"
    If the group does not exist, vyp creates it. Ensure `[dependency-groups]` exists in `pyproject.toml` or vyp will add it.

### Add without locking or installing

```bash
vyp add --no-lock numpy        # Edit pyproject.toml only
vyp add --no-install pandas    # Edit and lock, but skip install
```

!!! warning "Rollback on failure"
    If resolution fails after `vyp add`, vyp rolls back the `pyproject.toml` changes automatically.

## Removing Dependencies

vyp does not provide a `remove` command. Remove dependencies by editing `pyproject.toml`:

1. Open `pyproject.toml`
2. Delete the package line from the appropriate array:
   - `[project].dependencies`
   - `[project.optional-dependencies.<name>]`
   - `[dependency-groups.<name>]`
3. Run `vyp lock` to regenerate the lock file
4. Run `vyp install` to sync the environment

!!! example "Before and after"
    **Before:**
    ```toml
    [project]
    dependencies = [
        "requests>=2.28",
        "numpy>=1.24",
        "pandas>=2.0",
    ]
    ```
    **After (removed pandas):**
    ```toml
    [project]
    dependencies = [
        "requests>=2.28",
        "numpy>=1.24",
    ]
    ```

!!! tip "Updating an existing dependency"
    If a package with the same normalized name already exists, `vyp add` replaces it:
    ```bash
    vyp add "numpy>=1.26"   # Replaces existing numpy entry
    ```

## Updating Dependencies

### Regenerate the lock file

After editing `pyproject.toml` (adding, removing, or changing constraints), run:

```bash
vyp lock
```

This resolves the dependency graph and writes `pylock.toml` with the new solution.

!!! note "What vyp lock does"
    - Reads `[project].dependencies` from `pyproject.toml`
    - Applies `[tool.vyp]` config (overrides, conflicts, indexes, torch-backend)
    - Resolves with PubGrub
    - Writes `pylock.toml` (PEP 751 format)

### Update a specific package

To bump a package to a newer version:

1. Edit the constraint in `pyproject.toml` (e.g. `numpy>=1.24` → `numpy>=1.26`)
2. Run `vyp lock`
3. Run `vyp install`

Or use `vyp add` to replace the existing entry:

```bash
vyp add "numpy>=1.26"
```

### Named lock files

Generate a lock file with a custom name:

```bash
vyp lock --name dev
```

This creates `pylock.dev.toml` instead of `pylock.toml`. See [Lock Files](lockfiles.md) for details.

## Local, Editable and Git Dependencies

Point a dependency at a source tree or a git repository in `[tool.vyp.sources]`:

```toml
[project]
dependencies = ["lib-b", "lib-c", "tool-d"]

[tool.vyp.sources]
lib-b = [{ path = "../lib-b", editable = true }]
lib-c = [{ git = "https://github.com/org/lib-c", tag = "v1.4.0" }]
tool-d = [{ git = "https://github.com/org/monorepo", rev = "3f2a9c1", subdirectory = "tools/d" }]
```

or as a PEP 508 direct reference in the dependency list:

```toml
dependencies = [
    "lib-c @ git+https://github.com/org/lib-c@v1.4.0",
    "lib-e @ file:///opt/src/lib-e",
]
```

| Key | Description |
|-----|-------------|
| `path` | A source tree, relative to `pyproject.toml`. |
| `editable` | With `path`: install in editable mode (PEP 660), so edits take effect without reinstalling. |
| `git` | A repository URL (anything `git clone` accepts). |
| `rev` / `tag` / `branch` | What to check out (default: the remote's HEAD). |
| `subdirectory` | The package's directory inside the repository. |

vyp reads each tree's metadata before resolving: from `[project]` directly when name, version and dependencies are static, otherwise by asking its build backend. Git references are resolved to a commit, recorded in the lock (`[packages.vcs]` with `commit-id`), and installs check out exactly that commit. Path dependencies are recorded as `[packages.directory]` relative to the project.

Repositories are cloned once into the cache and fetched on later locks, using your `git`, so SSH keys and credential helpers work as usual.

## Source Distributions

When a release has no wheel for your platform, vyp can build its sdist with the backend it declares (PEP 517), in an isolated build environment. Build requirements are installed from wheels only.

```toml
[tool.vyp]
sdists = "fallback"   # "never", "fallback" (default) or "allow"
```

| Value | Behavior |
|-------|----------|
| `never` | Only versions with a compatible wheel are considered. |
| `fallback` | Versions that need a build are considered only for a package with no wheel-installable version at all. A newer release missing wheels for your platform never triggers a build when an older release installs from a wheel. |
| `allow` | Sdist-only versions compete like any other; the newest wins. |

During resolution, an sdist's dependencies come from its `PKG-INFO` when it declares them statically (Metadata-Version 2.2+, PEP 643). Otherwise vyp asks the build backend (`prepare_metadata_for_build_wheel`). Built wheels are cached per sdist hash and interpreter, and build environments are reused across builds with the same requirements. Set `VYP_BUILD_VERBOSE=1` to see backend output.

## Workflow Summary

| Task | Command |
|------|---------|
| Add to main deps | `vyp add <pkg>` |
| Add to optional | `vyp add --optional <name> <pkg>` |
| Add to group | `vyp add --group <name> <pkg>` |
| Remove | Edit pyproject.toml, then `vyp lock` |
| Update lock | `vyp lock` |
| Install from lock | `vyp install` |

## Next Steps

- [Lock Files](lockfiles.md) — Lock file format, named locks, provenance
- [Overrides & Conflicts](overrides.md) -- Pinning, constraining transitive dependencies, and resolving conflicts
