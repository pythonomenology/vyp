# Python Interpreters

vyp finds the interpreters already on your machine and can install managed CPython builds when none match.

## How vyp picks an interpreter

When a command needs an interpreter (creating `.venv`, running a script, building a package), vyp looks in this order and takes the first match:

1. Interpreters vyp installed itself (newest first).
2. Every `python3.X`, `python3` and `python` on `PATH`.
3. pyenv's versions directory (`$PYENV_ROOT/versions`, default `~/.pyenv/versions`).

Each candidate is run once to learn its real executable, version and install scheme (the result is cached by path and modification time), so pyenv shims, Homebrew symlinks and framework builds all resolve to the binary that actually runs. Virtual environment interpreters stand for their base interpreter.

A candidate must satisfy both the request and the project's `requires-python`.

## Requesting an interpreter

Anywhere vyp takes `--python`, and in the `python` setting:

| Request | Meaning |
|---------|---------|
| `3.12` | Any 3.12.x |
| `3.12.7` | Exactly 3.12.7 |
| `3.13t` | A free-threaded 3.13 |
| `>=3.10,<3.13` | A PEP 440 specifier |
| `/usr/bin/python3`, `python3.11` | A path, or a command on `PATH` |

```toml
[tool.vyp]
python = "3.12"   # used when vyp creates the project's .venv
```

## Managed interpreters

```bash
vyp python install 3.12      # newest 3.12.x for this platform
vyp python install           # newest release
vyp python list              # every interpreter vyp can find
vyp python find ">=3.11"     # the one vyp would use
vyp python uninstall 3.12
vyp python dir               # where managed builds live
```

Builds come from [python-build-standalone](https://github.com/astral-sh/python-build-standalone). vyp lists releases through the GitHub API, picks the newest `install_only_stripped` (or `install_only`) archive for this platform that matches, and **verifies its SHA-256** against the digest the release publishes before unpacking it. An archive without a published checksum is refused.

When no installed interpreter matches, `vyp venv`, `sync`, `install` and `run` download one automatically. Turn that off with `--no-python-downloads` (on `vyp venv`) or `VYP_NO_PYTHON_DOWNLOADS=1`.

| Variable | Purpose |
|----------|---------|
| `VYP_PYTHON_INSTALL_DIR` | Where managed builds go (default `$XDG_DATA_HOME/vyp/python`, i.e. `~/.local/share/vyp/python`). |
| `VYP_PYTHON_DOWNLOADS_URL` | Base URL of the GitHub API, for a mirror. |
| `VYP_NO_PYTHON_DOWNLOADS` | `1` to never download interpreters. |
| `GITHUB_TOKEN` | Sent to the GitHub API to avoid its unauthenticated rate limit. |

Free-threaded builds can be found but not yet downloaded.
