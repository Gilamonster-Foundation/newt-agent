#!/usr/bin/env python3
"""Environment-selected sccache fallback; Cargo config discovery is out of scope."""
import os
import subprocess
import sys


def selected_wrapper(env):
    return env.get("RUSTC_WRAPPER", env.get("CARGO_BUILD_RUSTC_WRAPPER", ""))


def cargo_env(env, probe_ok):
    result = dict(env)
    if os.path.basename(selected_wrapper(env)) == "sccache":
        result["SCCACHE_IGNORE_SERVER_IO_ERROR"] = "1"
        if not probe_ok:
            result["RUSTC_WRAPPER"] = ""
    return result


if __name__ == "__main__":
    probe_ok = True
    if os.path.basename(selected_wrapper(os.environ)) == "sccache":
        try:
            probe_ok = subprocess.call(
                [selected_wrapper(os.environ), os.environ.get("RUSTC") or "rustc", "-vV"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) == 0
        except OSError:
            probe_ok = False
    if not probe_ok:
        print("sccache is unavailable; building without the cache", file=sys.stderr)
    os.execvpe("cargo", ["cargo"] + sys.argv[1:], cargo_env(os.environ, probe_ok))
