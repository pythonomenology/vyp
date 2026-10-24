#!/usr/bin/env bash
# Benchmark vyp against uv on the 20-dependency project in this directory.
#
# Scenarios, each with warm caches (default) or empty caches (--cold):
#   resolve       vyp lock            vs  uv pip compile (same single target)
#                                         and uv lock (universal, for reference)
#   install       vyp sync --lockfile vs  uv sync --frozen  (lock exists, fresh venv)
#   end-to-end    vyp sync            vs  uv sync           (no lock, no venv)
#
# Usage:
#   ./run_benchmark.sh            # warm caches
#   ./run_benchmark.sh --cold     # empty caches every run (network-bound)
#
# Environment: VYP (default: ../../target/release/vyp), UV (default: uv),
# PYTHON (interpreter both tools target; default: python3 on PATH),
# RUNS (default: 15 warm, 5 cold). Requires hyperfine.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

COLD=0
[[ "${1:-}" == "--cold" || "${COLD:-}" == "1" ]] && COLD=1

VYP="${VYP:-$SCRIPT_DIR/../../target/release/vyp}"
UV="${UV:-$(command -v uv || true)}"
PYTHON="${PYTHON:-$(python3 -c 'import os,sys; print(os.path.realpath(sys.executable))')}"
RUNS="${RUNS:-$([[ $COLD == 1 ]] && echo 5 || echo 15)}"

for cmd in hyperfine "$VYP" "$UV"; do
  if ! command -v "$cmd" >/dev/null 2>&1 && [[ ! -x "$cmd" ]]; then
    echo "missing: $cmd" >&2
    exit 1
  fi
done

# Each tool gets its own copy of the project and its own cache, so neither
# sees the other's lock file, virtual environment, or cached downloads.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
for tool in vyp uv; do
  mkdir -p "$WORK/$tool"
  cp "$SCRIPT_DIR/pyproject.toml" "$WORK/$tool/pyproject.toml"
  printf '\n[tool.uv]\npackage = false\n\n[tool.vyp]\npython = "%s"\n' "$PYTHON" >> "$WORK/$tool/pyproject.toml"
done
export VYP_CACHE_DIR="$WORK/cache-vyp" UV_CACHE_DIR="$WORK/cache-uv"
export UV_PYTHON="$PYTHON" UV_PYTHON_DOWNLOADS=never UV_NO_PROGRESS=1
unset VIRTUAL_ENV

V="cd $WORK/vyp &&"
U="cd $WORK/uv &&"
WIPE_V="" WIPE_U=""
if [[ $COLD == 1 ]]; then
  WIPE_V="rm -rf $VYP_CACHE_DIR;"
  WIPE_U="rm -rf $UV_CACHE_DIR;"
  OPTS=(--runs "$RUNS")
else
  OPTS=(--warmup 2 --runs "$RUNS")
fi

echo "vyp:    $("$VYP" --version)"
echo "uv:     $("$UV" --version)"
echo "python: $PYTHON"
echo "caches: $([[ $COLD == 1 ]] && echo 'empty for every run' || echo warm)"
echo

# Lock files for the install scenario.
(cd "$WORK/vyp" && "$VYP" lock -o pylock.toml >/dev/null 2>&1)
(cd "$WORK/uv" && "$UV" lock -q)

hyperfine --style basic "${OPTS[@]}" \
  -n "vyp lock"            --prepare "$WIPE_V true" "$V $VYP lock -o pylock.toml" \
  -n "uv pip compile"      --prepare "$WIPE_U true" "$U $UV pip compile pyproject.toml -q -o req.txt --python $PYTHON" \
  -n "uv lock (universal)" --prepare "$WIPE_U rm -f $WORK/uv/uv.lock" "$U $UV lock -q"

(cd "$WORK/vyp" && "$VYP" lock -o pylock.toml >/dev/null 2>&1)
(cd "$WORK/uv" && "$UV" lock -q)

hyperfine --style basic "${OPTS[@]}" \
  -n "vyp sync --lockfile" --prepare "$WIPE_V rm -rf $WORK/vyp/.venv" "$V $VYP sync --lockfile pylock.toml" \
  -n "uv sync --frozen"    --prepare "$WIPE_U rm -rf $WORK/uv/.venv"  "$U $UV sync --frozen -q"

hyperfine --style basic "${OPTS[@]}" \
  -n "vyp sync" --prepare "$WIPE_V rm -rf $WORK/vyp/.venv" "$V $VYP sync" \
  -n "uv sync"  --prepare "$WIPE_U rm -rf $WORK/uv/.venv $WORK/uv/uv.lock" "$U $UV sync -q"
