#!/usr/bin/env python3
"""Read build.rustc-wrapper without requiring nightly `cargo config get`.

Environment precedence is handled by _cargo. Cargo searches from the working
directory upwards, then CARGO_HOME; extensionless config wins over config.toml.
Only the selected scalar is needed, so no general config merge is implemented.
"""
import os
from pathlib import Path
import tomllib


def configured_wrapper():
    cwd = Path.cwd()
    directories = [p / ".cargo" for p in (cwd, *cwd.parents)]
    directories.append(Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")))
    for directory in directories:
        for name in ("config", "config.toml"):
            path = directory / name
            if not path.is_file():
                continue
            with path.open("rb") as stream:
                wrapper = tomllib.load(stream).get("build", {}).get("rustc-wrapper")
            if wrapper is not None:
                # Cargo resolves paths containing separators from the directory
                # containing .cargo (including for CARGO_HOME configuration).
                if "/" in wrapper and not Path(wrapper).is_absolute():
                    wrapper = str(directory.parent / wrapper)
                return wrapper
            break
    return ""


if __name__ == "__main__":
    print(configured_wrapper())
