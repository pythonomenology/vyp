# Dependency Overrides & Conflicts

When the resolver encounters incompatible constraints, no single version satisfies all parties. Overrides let you tell the resolver the answer. vyp's unified override system lets you force specific version constraints for any package -- including transitive dependencies -- without forking upstream.

## The Problem

Consider:

```
my-app
  ├── lib-a  (wants numpy>=1.26)
  └── lib-b  (wants numpy<1.25)
```

No single version of numpy satisfies both constraints. In pip or uv, this is a dead end -- the consumer must diagnose the failure and manually intervene.

With vyp, the solution is `[[tool.vyp.overrides]]`:

```toml
[[tool.vyp.overrides]]
package = "numpy"
constraint = ">=1.26,<2"
reason = "We tested and confirmed >=1.26 works for all our dependencies"
```

This tells the resolver: "Override all other constraints on numpy and use this range instead."

## Override Fields

All overrides use `[[tool.vyp.overrides]]` in `pyproject.toml`:

| Field | Required | Description |
|-------|----------|-------------|
| `package` | Yes | Package name to override |
| `constraint` | Yes | PEP 440 version specifier (e.g. `">=1.26,<2"` or `"==1.24.0"`) |
| `transitive` | No | If `true`, propagate to consumers (default: `false`) |
| `reason` | No | Documentation for why this override exists |

## Range Constraints

Constrain a package to a version range:

```toml
[[tool.vyp.overrides]]
package = "numpy"
constraint = ">=1.26,<2"
```

!!! example "Common patterns"
    ```toml
    constraint = ">=1.24,<1.25"   # Minor version pin
    constraint = ">=2.0"          # Minimum version
    constraint = "<3.0"            # Maximum version
    ```

## Exact Pins

Pin a package to an exact version:

```toml
[[tool.vyp.overrides]]
package = "numpy"
constraint = "==1.24.0"
reason = "Reproducible build"
```

!!! tip "Exact vs range"
    Use `==1.24.0` when you need a specific version. Use `>=1.26,<2` when you need a minimum but allow patch updates.

## The reason Field

Document why an override exists:

```toml
[[tool.vyp.overrides]]
package = "urllib3"
constraint = ">=2.0,<3"
transitive = true
reason = "CVE-2023-43804; older versions have security issues"
```

!!! tip "Best practice"
    Always add a `reason` for overrides, especially transitive ones. It helps future maintainers and consumers understand the constraint.

## Overrides Replace Other Requirements

An override replaces every requirement *other packages* place on the overridden package, so it can settle a conflict that has no solution otherwise. It never adds the package to your resolution: if nothing depends on it, the override has no effect.

## Multiple Overrides

You can define multiple overrides. Each `package` should appear once. If you need to change an override, edit `pyproject.toml` or use `vyp override remove` then `add`.

## Transitive Propagation

When `transitive = true`, the override propagates to consumers of your package via `vyp-overrides.toml`. This is how the "middle of the diamond" can resolve conflicts for the entire dependency graph:

```toml
[[tool.vyp.overrides]]
package = "numpy"
constraint = ">=1.26,<2"
transitive = true
reason = "Resolves GPU/CPU incompatibility across our stack"
```

!!! tip "Library authors"
    If you publish a library and have resolved a conflict with an override, set `transitive = true` and export with `vyp override export`. Consumers will inherit your decision automatically.

## CLI

### Add an override

```bash
vyp override add numpy ">=1.26,<2"
vyp override add scipy "==1.11.0" --transitive --reason "GPU compat"
```

### List overrides

Lists all configured overrides and marks which ones are transitive:

```bash
vyp override list
```

Output:

```
Dependency overrides:
  numpy = ">=1.26,<2" [transitive] — Security fix
  urllib3 = "==2.0.7"
```

### Remove an override

```bash
vyp override remove numpy
```

### Export transitive overrides

Libraries can export their transitive overrides (and inherited conflict declarations) for consumers:

```bash
vyp override export
vyp override export --output dist/vyp-overrides.toml
```

This writes `vyp-overrides.toml` containing all transitive dependency overrides.

Ship the file inside your wheel's `.dist-info` directory and vyp applies it for every consumer that selects your release. See [Transitive Conflicts](../concepts/transitive-conflicts.md#shipping-the-file) for how to include it and the rules that limit what inherited overrides can do.

!!! example "vyp-overrides.toml structure"
    ```toml
    overrides-version = "4.0"
    created-by = "vyp 0.1.0"
    package = "my-library"
    package_version = "1.2.0"

    [[overrides]]
    package = "numpy"
    constraint = ">=1.26,<2"
    transitive = true
    reason = "Security fix"
    ```

See [Overrides Export Format](../reference/conflict-overrides-format.md) for the full schema.

## Next Steps

- [Transitive Conflicts](../concepts/transitive-conflicts.md) -- How conflicts propagate through the dependency graph
- [Errata](errata.md) -- Correct the metadata of releases that are already published
- [PyTorch](pytorch.md) -- PyTorch index and backend selection
- [Overrides Export Format](../reference/conflict-overrides-format.md) -- Export file schema
