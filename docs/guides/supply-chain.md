# Supply-Chain Safety

## Holding back new uploads

Most malicious releases are found within days. Resolving only against files uploaded before a cutoff keeps a fresh compromise out of your lock while it is still undiscovered, and makes a lock reproducible from the date alone.

```toml
[tool.vyp]
exclude-newer = "P7D"          # or "7 days", or a date/timestamp: "2026-10-01"

[tool.vyp.exclude-newer-package]
certifi = false                # always take the newest certifi
internal-lib = "2026-10-20"    # its own cutoff
```

```bash
vyp lock --exclude-newer "2026-10-01T00:00:00Z"
VYP_EXCLUDE_NEWER=P14D vyp sync
```

The cutoff applies per file, using the upload times indexes publish (PEP 700): a version is only offered if some file was uploaded before the cutoff, and a wheel added to an old release later is ignored. The absolute cutoff used is recorded in the lock under `[tool.vyp] exclude-newer`. Indexes that don't publish upload times can't be filtered; vyp warns when that happens.

## Startup files

`site.py` runs every line of a site-packages `.pth` file that starts with `import`, and imports top-level `sitecustomize`/`usercustomize` modules on startup. A compromised package can use either to run code in **every** Python process using the environment, not just in programs that import it.

vyp inspects each wheel before installing it. Path-only `.pth` lines are ignored; the rest is reported:

```
warning: sneaky installs code that runs at every Python startup:
    sneaky.pth (runs: import os; os.system(...))
  Add "sneaky" to allow-startup-files to silence this, or set startup-files = "deny" to refuse such packages.
```

```toml
[tool.vyp]
startup-files = "deny"                       # "warn" (default), "deny" or "allow"
allow-startup-files = ["pywin32", "zope-interface"]
```

`setuptools` and `pywin32` are allowed by default, as are your project's own path and editable installs. Build environments allow everything, since they never run your code. The check is non-interactive, so it behaves the same in CI.

This doesn't make installing an untrusted package safe: a package can still run code when imported. It removes the case where merely having a package installed runs its code.
