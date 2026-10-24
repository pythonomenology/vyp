# Virtual Environments

## Creating one

```bash
vyp venv                      # .venv next to pyproject.toml
vyp venv envs/py312 --python 3.12
vyp venv --relocatable        # can be moved or copied afterwards
vyp venv --clear              # replace an existing environment
```

`vyp sync`, `vyp install` and `vyp run` create the project's `.venv` automatically when it doesn't exist, using the `python` setting and `requires-python` to pick an [interpreter](python.md). An active `VIRTUAL_ENV` takes precedence, and `--venv` overrides both.

On macOS and Linux vyp builds the environment itself, the way the stdlib `venv` module does: `pyvenv.cfg`, `bin/python` symlinks to the base interpreter, the interpreter's own site-packages layout, and `activate`, `activate.fish` and `activate.csh` scripts. On Windows it runs `python -m venv --without-pip` and then adds its own files.

vyp refuses to replace a directory that isn't a virtual environment, even with `--clear`.

## Relocatable environments

A normal virtual environment hard-codes its location: script launchers start with `#!/absolute/path/.venv/bin/python` and `activate` sets `VIRTUAL_ENV` to the original path. Move the directory and both break.

With `--relocatable`:

- launchers for entry points and bundled scripts find the interpreter next to themselves:

    ```sh
    #!/bin/sh
    '''exec' "$(dirname -- "$(realpath -- "$0")")"/'python' "$0" "$@"
    ' '''
    ```

- `activate` and `activate.fish` compute `VIRTUAL_ENV` from their own location (`activate.csh` can't, so it keeps the absolute path),
- `pyvenv.cfg` records `relocatable = true`, so every later `vyp install`/`sync` writes relocatable launchers too.

The environment still links to its base interpreter, so it can move within a machine or to another machine with the same interpreter at the same path, such as a container image.

Even in a normal environment, vyp uses the `/bin/sh` header when the interpreter path is too long for a `#!` line or contains spaces.

## What an environment was built from

Virtual environments go stale silently: the base Python is upgraded or removed, the directory is copied to a machine with a different OS or CPU, or another tool changes it. vyp records what each environment was built from, following the [venv provenance pre-PEP](https://discuss.python.org/t/104777):

```
.venv/venv-info/
├── MANAGER                    # "vyp"
├── pylock.snapshot.toml       # the lock vyp last installed
└── vyp/environment.toml       # base interpreter, version, host OS/arch, timestamps
```

`vyp sync` and `vyp install` update it after every install, and check it first. They refuse an environment whose base interpreter is missing or reports a different version, or that was created on a different OS or architecture, and warn when `MANAGER` names another tool.

```bash
$ vyp venv --check
.venv has drifted:
  - base interpreter /usr/local/bin/python3.12 changed from Python 3.12.4 to 3.12.7
  - requests is 2.32.3 but the snapshot has 2.31.0
  - rich==13.7.1 was installed outside vyp
```

`--check` exits non-zero when anything has drifted, so it works as a CI or pre-commit gate. Environments created by other tools get a record from their `pyvenv.cfg` the first time vyp installs into them.
