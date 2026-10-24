# Caching

vyp uses a **content-addressed metadata cache** with **LRU eviction** to speed up resolution while keeping disk usage bounded. Only metadata (JSON) is cached—not wheels or sdists.

## In-memory index (per resolve)

During a single resolution, providers (e.g. PyPI) use an **in-memory** index to hand off version lists and metadata from background HTTP tasks to the solver. This is separate from the **disk** metadata cache: the disk cache is content-addressed and LRU-persistent; the in-memory index is per run and discarded after resolve. The solver blocks on this index when data is not yet available; prefetch reduces how often that happens.

## Content-Addressed Storage Design

Cache entries are stored in files named by a **deterministic hash** of the cache key. The key is `{package}=={version}` (normalized: lowercase, hyphens/dots replaced with underscores).

!!! abstract "Cache key and file layout"
    ```
    Key:   numpy==1.26.4  →  Hash: a1b2c3d4e5f67890.json
    Path:  <cache_dir>/a1b2c3d4e5f67890.json
    ```

Benefits:

- **Deterministic**: Same key always maps to the same file.
- **No collisions**: Different packages/versions produce different hashes.
- **Simple lookup**: Hash the key, read the file.

## Version Lists

Each package's index page (every release and every file of it, with hashes, `Requires-Python` and upload times) is cached as `versions-<package>.pc` for 5 minutes, then revalidated with an HTTP conditional request. Each index has its own directory, so one index's answer for a package is never served for another.

These lists are the largest cache entries (megabytes for packages that ship wheels for many platforms), so they use a compact positional encoding. Next to each one, vyp keeps a **per-environment view**, `versions-<package>.<tags>.pc`: the same list reduced to the wheels the target interpreter and platform can install, plus sdists. Releases with nothing installable are left out. Warm resolves read the views, a few KiB each, instead of decoding every file for every platform. A view is rebuilt whenever the full list it came from changes.

## LRU Eviction Policy

An in-memory index tracks:

- Each cached entry's key, hash, size, and **last access time**
- Total size of all cached entries

When the cache exceeds the size limit (default **2 GB**), the **least-recently-used** entry is evicted. Eviction continues until the cache is under the limit.

!!! tip "LRU behavior"
    Frequently used metadata (e.g., popular packages) stays in the cache. Rarely used entries are evicted first.

## Cache Location

The cache directory is `{base}/vyp/cache/metadata`, where `base` is determined by:

1. **VYP_CACHE_DIR** — If set, used as base (cache at `$VYP_CACHE_DIR/vyp/cache/metadata`)
2. **XDG_CACHE_HOME** — If set, used as base (e.g., `$XDG_CACHE_HOME/vyp/cache/metadata`)
3. **Fallback** — `$HOME/.cache/vyp/cache/metadata` (or `/tmp/.cache/vyp/cache/metadata` if HOME is unset)

```bash
# Use a custom cache base directory
export VYP_CACHE_DIR=/path/to/my/cache
# Cache ends up at /path/to/my/cache/vyp/cache/metadata

# Or follow XDG (common on Linux)
export XDG_CACHE_HOME=~/.cache
# Cache ends up at ~/.cache/vyp/cache/metadata
```

!!! note "XDG compliance"
    When `XDG_CACHE_HOME` is set, vyp uses it for cache storage, following the XDG Base Directory specification.

## Bounded Disk Usage

Unlike some tools that cache indefinitely, vyp's cache has a **hard limit** (default 2 GB). When the limit is exceeded, LRU eviction removes the oldest entries until the cache is within bounds.

!!! abstract "Eviction flow"
    ```
    New metadata to store
         ↓
    Write to disk, add to index
         ↓
    total_size > max_size_bytes?
         ↓ Yes
    Find LRU entry → remove from disk and index
         ↓
    Repeat until total_size <= max_size_bytes
    ```

!!! success "Contrast with uv"
    uv's cache can grow without a fixed limit. vyp's metadata cache is capped at 2 GB by default, avoiding unbounded disk usage.

## What Is Cached

| Cached | Not cached |
|--------|------------|
| Package metadata (dependencies, versions) | Wheel files |
| Fetched from PyPI/indices | Sdist archives |
| Stored as JSON by content hash | Downloaded artifacts |

The cache accelerates **resolution** (metadata lookups), not **installation** (artifact downloads).

## Cache Invalidation

The cache is invalidated when:

- **Corrupt entry**: If a cached file fails to parse, it is removed.
- **Missing file**: If the index references a file that no longer exists, the entry is removed.
- **LRU eviction**: Entries are deleted when the cache exceeds the size limit.

There is no explicit "clear cache" command in the current design; eviction and corruption handling keep the cache self-maintaining. To force a fresh cache, you can delete the cache directory manually.

!!! example "Manual cache clear"
    ```bash
    rm -rf "${VYP_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}}/vyp"
    ```
    The path depends on `VYP_CACHE_DIR` / `XDG_CACHE_HOME` environment variables.

## Index File

The cache maintains an index file (`cache-index.json`) in the cache directory. It stores:

- Entry key (package==version)
- Hash (filename)
- Size in bytes
- Last access timestamp

The index is loaded at startup and saved after each write or eviction.

## Summary

| Aspect | Behavior |
|--------|----------|
| **Design** | Content-addressed (hash of package==version) |
| **Eviction** | LRU when over size limit |
| **Default limit** | 2 GB |
| **Location** | VYP_CACHE_DIR or XDG_CACHE_HOME/vyp/cache/metadata |
| **Contents** | Metadata only (no wheels/sdists) |
| **Bounded** | Yes—avoids unbounded disk growth |
