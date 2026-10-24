# Transitive Conflicts

**Transitive conflict declarations** are vyp's signature feature. They solve the diamond dependency problem in a way that existing tools cannot: by allowing the "middle of the diamond" to propagate conflict resolution decisions to consumers.

## The Diamond Dependency Problem

Consider this classic scenario:

!!! abstract "Diamond dependency diagram"
    ```
              Your App
                 │
         ┌──────┴──────┐
         │             │
      Library A    Library B
         │             │
         └──────┬──────┘
                │
              LibX
    ```

- **Library A** depends on `LibX>=2`
- **Library B** depends on `LibX<2`

There is no single version of LibX that satisfies both constraints. The resolver **fails** — it cannot find a solution. pip, uv, and other resolvers report an error and stop. The *consumer* (Your App) must diagnose the failure, figure out which version actually works, and manually add an override. But what if **Library A** already knows the right answer?

## Why Existing Tools Can't Propagate

pip and uv treat each dependency declaration in isolation. When A and B conflict on LibX:

- The resolver sees two incompatible constraints and **fails** — there is no version of LibX that satisfies both.
- It has no way to know that A's maintainers have already resolved this conflict for their users.
- The consumer must diagnose the error, determine which constraint can safely be overridden, and manually add a workaround — or give up and drop a dependency.

!!! failure "Limitation"
    Existing tools cannot propagate conflict resolution from a library to its consumers. The "middle of the diamond" is invisible to the resolver.

## How vyp's Transitive Conflicts Work

vyp allows packages to **declare** conflicts and mark them as **transitive**. When a package declares a transitive conflict, that declaration is inherited by every package that depends on it. The resolver can then use this information to fork resolution or apply overrides.

### How vyp Solves This

The key mechanism is `[[tool.vyp.overrides]]` with `transitive = true`. When a library author knows the answer to a version conflict, they declare an override and export it. Consumers inherit the override automatically.

Users interact with overrides via `[[tool.vyp.overrides]]` in `pyproject.toml`. Transitive overrides propagate to consumers via the `vyp-overrides.toml` export file.

### Example: Library A and Library B

Library A (which requires LibX>=2) can add a transitive override:

```toml
# In Library A's pyproject.toml
[[tool.vyp.overrides]]
package = "libx"
constraint = ">=2"
transitive = true
reason = "Our code requires the v2 API"
```

Library A then exports this for consumers:

```bash
vyp override export
```

When Your App depends on both A and B:

1. vyp loads A's exported `vyp-overrides.toml` and sees the transitive override on LibX.
2. The override propagates to Your App (A's consumer).
3. The resolver applies the override, restricting LibX to `>=2`, resolving the conflict in favor of A.
4. Your App's maintainer doesn't need to diagnose anything — the decision was already made upstream.

!!! success "Key insight"
    `transitive = true` means: "This conflict resolution applies not just to me, but to anyone who depends on me."

## Propagation and Inheritance

When a package declares a transitive conflict, it is **inherited** by its consumers. The inheritance chain is tracked:

- **origin**: The package that originally declared the conflict.
- **propagation_path**: The chain of packages through which the conflict was inherited.

```
Library A declares conflict (origin: A)
    → Your App inherits (propagation_path: [A])
    → Meta-App depends on Your App (propagation_path: [A, your-app])
```

## The vyp-overrides.toml Export File

Libraries can **export** their transitive overrides so that consumers can load them without re-resolving.

```bash
vyp override export --output vyp-overrides.toml
```

This produces a `vyp-overrides.toml` file that downstream consumers can use. The file contains transitive dependency overrides (package, constraint, reason, etc.).

!!! abstract "vyp-overrides.toml structure"
    ```toml
    overrides-version = "4.0"
    created-by = "vyp 0.1.0"
    package = "library-a"
    package_version = "1.0.0"

    [[overrides]]
    package = "libx"
    constraint = ">=2"
    transitive = true
    reason = "Resolved in favor of v2"
    ```

### Shipping the file

Put `vyp-overrides.toml` anywhere inside your wheel's `.dist-info` directory. With hatchling:

```toml
[tool.hatch.build.targets.wheel.extra-metadata]
"vyp-overrides.toml" = "vyp-overrides.toml"
```

### What consumers do with it

When the resolver selects a release of Library A, vyp reads `vyp-overrides.toml` from that release's wheel with HTTP range requests (a few KiB; the wheel is never downloaded) and applies its overrides. Reads start as soon as a release is selected, so they overlap the rest of resolution, and the result is cached per wheel. Because inherited overrides can change which releases are selected, vyp re-resolves until the set of inherited overrides stops changing.

Inherited overrides follow a few rules so that a dependency can't take control of your project:

- They replace requirements **between dependencies** only. Your project's own direct requirements always stand.
- Your own `[[tool.vyp.overrides]]` entry for a package always wins over inherited ones.
- If two dependencies ship incompatible overrides for the same package, resolution stops and asks you to decide with your own override.
- Every inherited override is printed when it is applied, recorded in the lock with the release it came from, and shown by `vyp explain`.

Control which dependencies may ship overrides with `inherit-overrides`:

```toml
[tool.vyp]
inherit-overrides = true                  # default: any selected dependency
# inherit-overrides = false               # ignore shipped overrides
# inherit-overrides = ["library-a"]       # only these packages
```

## Comparison with uv and pip

| Aspect | pip / uv | vyp |
|--------|----------|-----|
| **Conflict visibility** | Only direct constraints | Transitive declarations propagate |
| **Who resolves** | Consumer only | Library can pre-resolve and propagate |
| **Override scope** | Project-local | Can be transitive via vyp-overrides.toml |
| **Diamond handling** | Fail or manual override | Fork resolution, inherit decisions |

!!! tip "When to use transitive conflicts"
    Use transitive overrides when your library has resolved a known conflict (e.g., LibX version) and you want consumers to inherit that resolution automatically. Export `vyp-overrides.toml` and ship it with your package.
