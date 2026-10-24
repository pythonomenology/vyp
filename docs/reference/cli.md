# CLI Reference

Complete reference for all vyp commands and their options.

## Synopsis

```bash
vyp <command> [options]
```

---

## Commands

### vyp resolve

Resolve dependencies and display the solution without writing a lock file.

```bash
vyp resolve [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |
| `--requirement` | `-r` | string (repeatable) | — | Additional requirements to resolve (e.g. `numpy>=1.20`). |
| `--torch-backend` | — | string | — | PyTorch accelerator backend: `auto`, `cpu`, `cu118`, `cu121`, `cu124`, `cu126`, `cu128`, `cu130`, `rocm6`, `xpu`. |

---

### vyp lock

Generate or update the lock file from `pyproject.toml`.

```bash
vyp lock [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |
| `--output` | `-o` | path | `pylock.toml` | Output lock file path. |
| `--name` | `-n` | string | — | Named lock file; creates `pylock.<name>.toml` (overrides `--output`). |
| `--torch-backend` | — | string | — | PyTorch accelerator backend. |
| `--exclude-newer` | — | string | — | Ignore files uploaded after a date, timestamp or duration ago (`VYP_EXCLUDE_NEWER`). |
| `--script` | — | path | — | Lock a PEP 723 script into a `# /// pylock` block in the script. |

---

### vyp install

Install resolved dependencies from a lock file into a virtual environment.

```bash
vyp install [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--lockfile` | `-l` | path | `pylock.toml` | Path to lock file. |
| `--venv` | — | path | — | Target virtual environment path (auto-detected if not specified). |
| `--dry-run` | — | flag | `false` | Print what would be installed without installing. |
| `--torch-backend` | — | string | — | PyTorch accelerator backend. |

---

### vyp venv

Create a virtual environment, or check one against what it was built from.

```bash
vyp venv [PATH] [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--python` | `-p` | string | — | Interpreter: version, specifier or path. |
| `--relocatable` | — | flag | `false` | Scripts and activation locate the environment relative to themselves. |
| `--clear` | — | flag | `false` | Replace an existing environment. |
| `--prompt` | — | string | directory name | Prompt shown when activated. |
| `--system-site-packages` | — | flag | `false` | Give access to the base interpreter's packages. |
| `--no-python-downloads` | — | flag | `false` | Don't download an interpreter when none matches. |
| `--check` | — | flag | `false` | Report drift (stale interpreter, other host, changed packages); exits non-zero if any. |

---

### vyp python

```bash
vyp python install [VERSION]
vyp python list
vyp python find [REQUEST]
vyp python uninstall VERSION
vyp python dir
```

See [Python Interpreters](../guides/python.md).

---

### vyp run

Run a PEP 723 script in an environment built from its inline metadata, or a command in the project environment.

```bash
vyp run [OPTIONS] TARGET [ARGS]...
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--with` | — | string (repeatable) | — | Extra requirements for a script. |
| `--python` | `-p` | string | — | Interpreter for a script environment. |

See [Single-File Scripts](../guides/scripts.md).

---

### vyp add

Add dependencies to `pyproject.toml`, re-lock, and optionally install.

```bash
vyp add [OPTIONS] <packages>...
```

| Argument/Flag | Short | Type | Default | Description |
|---------------|-------|------|---------|-------------|
| `packages` | — | string (required) | — | Packages to add (e.g. `numpy`, `pandas>=2.0`, `requests[security]>=2.28`). |
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |
| `--optional` | — | string | — | Add to PEP 621 optional dependency group. |
| `--group` | — | string | — | Add to PEP 735 dependency group. |
| `--no-lock` | — | flag | `false` | Only edit `pyproject.toml`; skip locking. |
| `--no-install` | — | flag | `false` | Edit and lock, but skip installation. |
| `--venv` | — | path | — | Target virtual environment for install. |
| `--torch-backend` | — | string | — | PyTorch accelerator backend. |

---

### vyp override add

Add a dependency override for a package.

```bash
vyp override add <package> <constraint> [OPTIONS]
```

| Argument/Flag | Short | Type | Default | Description |
|---------------|-------|------|---------|-------------|
| `package` | — | string (required) | — | Package name. |
| `constraint` | — | string (required) | — | Version constraint (e.g. `>=1.26,<2`, `==1.24.0`). |
| `--transitive` | — | flag | `false` | Make this override transitive (propagate to consumers). |
| `--reason` | — | string | — | Reason for the override. |
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |

---

### vyp override list

List all dependency overrides.

```bash
vyp override list [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |

---

### vyp override remove

Remove a dependency override.

```bash
vyp override remove <package> [OPTIONS]
```

| Argument/Flag | Short | Type | Default | Description |
|---------------|-------|------|---------|-------------|
| `package` | — | string (required) | — | Package name to remove override for. |
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |

---

### vyp conflict list

List conflict declarations inherited from resolved dependencies. These are not configured in `pyproject.toml` — they are propagated by dependencies that ship `vyp-overrides.toml` or embed conflict metadata.

```bash
vyp conflict list [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |

---

### vyp override export

Export transitive overrides for library consumers. Only `[[tool.vyp.overrides]]` entries with `transitive = true` are exported.

```bash
vyp override export [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |
| `--output` | `-o` | path | `vyp-overrides.toml` | Output file path. |

Produces a `vyp-overrides.toml` file that libraries can ship alongside their distribution so consumers inherit override decisions.

---

### vyp explain

Explain why a package version was chosen.

```bash
vyp explain <package> [OPTIONS]
```

| Argument/Flag | Short | Type | Default | Description |
|---------------|-------|------|---------|-------------|
| `package` | — | string (required) | — | Package name to explain. |
| `--lockfile` | `-l` | path | `pylock.toml` | Path to lock file. |

---

### vyp diff

Compare two lock files.

```bash
vyp diff <old> <new>
```

| Argument | Type | Description |
|----------|------|-------------|
| `old` | path | Path to the older lock file. |
| `new` | path | Path to the newer lock file. |

---

### vyp plugin list

List loaded plugins (built-in and external).

```bash
vyp plugin list [OPTIONS]
```

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml` (for loading configured plugins). |

---

### vyp plugin info

Show details about a specific plugin.

```bash
vyp plugin info <name> [OPTIONS]
```

| Argument/Flag | Short | Type | Default | Description |
|---------------|-------|------|---------|-------------|
| `name` | — | string (required) | — | Plugin name. |
| `--project` | `-p` | path | `pyproject.toml` | Path to `pyproject.toml`. |
