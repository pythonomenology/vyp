# Errata: Fixing Published Metadata

Once a release is on an index its `Requires-Dist` and `Requires-Python` are frozen. When a maintainer later learns that `green<4` breaks on Python 3.12, or that `foo==1.2.0` silently needs `bar<2`, there is no way to say so: the usual workaround is a post-release plus yanking every older version, and resolvers keep backtracking into the broken releases in the meantime.

An **erratum** records that knowledge outside the release. vyp applies it during resolution as if the release had shipped with the corrected metadata.

## Using errata feeds

List feeds in `pyproject.toml`. Each entry is a URL or a path relative to the project:

```toml
[tool.vyp]
errata = [
    "https://example.com/python-errata.toml",
    "ci/errata.toml",
]
```

Feeds are read on every `vyp lock`, `install`, `sync` and `resolve`. A remote feed is cached, so locking still works offline; if it cannot be fetched, vyp warns and uses the cached copy. The SHA-256 of every feed's content is recorded in the lock file under `[tool.vyp]`, so you can tell which version of a feed produced a lock.

## Feed format

```toml
errata-version = "1"

# Older releases are known to break on Python 3.12.
[[errata]]
package = "green"
versions = "<4"
requires-python = "<3.12"
reason = "green 3.x fails with Python 3.12's unittest changes"

# A release that forgot a dependency bound.
[[errata]]
package = "foo"
versions = "==1.2.0"
add-requires = ["bar<2", "pywin32; sys_platform == 'win32'"]
reason = "foo 1.2.0 uses the bar 1.x API"

# Releases that must never be selected.
[[errata]]
package = "baz"
versions = ">=3.1,<3.1.4"
exclude = true
reason = "broken wheels; fixed in 3.1.4"
```

| Field | Required | Description |
|-------|----------|-------------|
| `package` | Yes | Package the erratum corrects. |
| `versions` | No | PEP 440 specifier of affected releases. Empty or absent means every release. |
| `requires-python` | No | Corrected `Requires-Python` for those releases. When the target interpreter is outside it, the releases are excluded. |
| `add-requires` | No | PEP 508 requirements those releases should have declared. Markers are evaluated for the target environment. A requirement on a package the release already depends on narrows that range. |
| `exclude` | No | `true` to never select those releases. |
| `reason` | No | Shown in `vyp explain` and recorded in the lock. |

Each erratum must set at least one of `exclude`, `requires-python` or `add-requires`. Unknown fields are an error, so a typo like `exlude` can't be silently ignored.

With [universal resolution](../configuration/pyproject.md#universal-resolution), errata are evaluated separately for each environment, so a `requires-python` erratum only excludes releases where it applies.

## How errata interact with overrides

Errata change what a release *declares*; [overrides](overrides.md) are your final word on a package. If you override a package, the override replaces every requirement on it, including ones an erratum added.

## Seeing what an erratum did

```bash
$ vyp explain requests
requests == 2.31.0
  Selected by: normal
  Erratum: releases >=2.32 excluded [ci/errata.toml] -- pretend 2.32+ is broken
```

Packages pulled in by an erratum show it in `Requested by`, for example `certifi==2026.10.19 (erratum)`.
