# Environment Variables

vyp respects the following environment variables for cache location, virtual environment detection, and logging.

## Cache

| Variable | Description |
|----------|-------------|
| `VYP_CACHE_DIR` | Override the base directory for vyp's metadata cache. When set, the cache is stored at `$VYP_CACHE_DIR/vyp/cache/metadata`. |
| `XDG_CACHE_HOME` | Fallback when `VYP_CACHE_DIR` is unset. The cache base becomes `$XDG_CACHE_HOME/vyp/cache/metadata`. On Linux, this is typically `~/.cache`. |

**Resolution order:**

1. `VYP_CACHE_DIR` — if set, used directly.
2. `XDG_CACHE_HOME` — if set, used as base.
3. `$HOME/.cache` — default fallback (or `/tmp` if `HOME` is unset).

The metadata cache is content-addressed and uses LRU eviction (default max 2 GB). Only package metadata JSON is cached, not wheels or sdists.

---

## Virtual Environment

| Variable | Description |
|----------|-------------|
| `VIRTUAL_ENV` | When set, vyp auto-detects the active virtual environment. Commands like `vyp install` install into this environment when `--venv` is not specified. |

**Install target resolution order:**

1. `--venv` flag — explicit path.
2. `VIRTUAL_ENV` — active virtual environment.
3. `.venv` next to `pyproject.toml` — created if it doesn't exist.

---

## Python and Builds

| Variable | Description |
|----------|-------------|
| `VYP_PYTHON_INSTALL_DIR` | Where managed interpreters are installed (default `~/.local/share/vyp/python`). |
| `VYP_PYTHON_DOWNLOADS_URL` | GitHub API base URL used to find python-build-standalone releases. |
| `VYP_NO_PYTHON_DOWNLOADS` | `1` to never download an interpreter. |
| `GITHUB_TOKEN` | Sent to the GitHub API when listing Python builds. |
| `VYP_BUILD_VERBOSE` | `1` to show build-backend output. |
| `VYP_EXCLUDE_NEWER` | Same as `--exclude-newer`. |

---

## Logging

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Controls logging level via `tracing-subscriber`. Use `RUST_LOG=debug` or `RUST_LOG=vyp=debug` for verbose output. |

**Examples:**

```bash
# Debug output for all crates
RUST_LOG=debug vyp resolve

# Debug only for vyp crates
RUST_LOG=vyp=debug,vyp_core=debug vyp lock

# Info level (default)
RUST_LOG=info vyp add numpy
```
