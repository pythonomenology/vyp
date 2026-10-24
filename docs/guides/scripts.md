# Single-File Scripts

## Running scripts with inline metadata

A script can declare its own dependencies with a [PEP 723](https://peps.python.org/pep-0723/) block:

```python
# /// script
# requires-python = ">=3.11"
# dependencies = ["rich", "httpx<1"]
#
# [tool.vyp]
# exclude-newer = "2026-01-01"
# ///
import httpx, rich
...
```

```bash
vyp run report.py --since yesterday
vyp run --with pandas report.py      # extra requirements for this run
vyp run --python 3.12 report.py
```

vyp resolves the block, installs it into an environment cached by the block's contents and interpreter, and runs the script there. The next run with unchanged metadata reuses the environment. A `[tool.vyp]` table in the block accepts the same settings as `pyproject.toml`. A script without a block just runs with a matching interpreter.

`vyp run COMMAND` (anything that isn't a `.py` file) runs a command inside the project's environment, with its `bin/` first on `PATH` and `VIRTUAL_ENV` set. It does not sync first; run `vyp sync` when dependencies change.

## Locking a script

```bash
vyp lock --script report.py
```

This resolves the script's metadata and writes the result into the script itself, as a `# /// pylock` block holding a [PEP 751](https://peps.python.org/pep-0751/) lock, following the [script-locking pre-PEP](https://discuss.python.org/t/98034). The block goes at the end of the file so the script stays readable, and running the command again replaces it. `vyp run` then installs exactly the locked versions instead of resolving, so the script behaves the same wherever it is copied.

!!! warning "Review locked scripts like code"
    A lock block is long, mechanically generated comment text. Nothing stops a script from hiding executable lines between comment blocks, so read a script you were sent before running it, as you would any other.
