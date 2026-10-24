# Run one PEP 517 build-backend hook for vyp.
#
# Usage: python backend_shim.py HOOK BACKEND BACKEND_PATH_JSON ARGS_JSON RESULT_PATH
#
# Imports BACKEND ("module:object") with the source tree as the working
# directory, calls HOOK(*args, **kwargs) and writes {"ok": true, "value": ...},
# {"ok": false, "missing": true} (hook not defined), or
# {"ok": false, "error": traceback} to RESULT_PATH as JSON.
import importlib
import json
import os
import sys
import traceback


def main():
    hook, backend_spec, backend_path_json, args_json, result_path = sys.argv[1:6]
    backend_path = json.loads(backend_path_json)
    if backend_path:
        # PEP 517 in-tree backends: resolved relative to the source tree.
        sys.path[:0] = [os.path.abspath(p) for p in backend_path]
    args = json.loads(args_json)

    try:
        module_name, _, attrs = backend_spec.partition(":")
        backend = importlib.import_module(module_name)
        for attr in filter(None, attrs.split(".")):
            backend = getattr(backend, attr)
    except Exception:
        result = {"ok": False, "error": "cannot import build backend %r:\n%s" % (backend_spec, traceback.format_exc())}
    else:
        func = getattr(backend, hook, None)
        if func is None:
            result = {"ok": False, "missing": True}
        else:
            try:
                result = {"ok": True, "value": func(*args.get("args", []), **args.get("kwargs", {}))}
            except Exception:
                result = {"ok": False, "error": traceback.format_exc()}

    with open(result_path, "w", encoding="utf-8") as f:
        json.dump(result, f)


main()
