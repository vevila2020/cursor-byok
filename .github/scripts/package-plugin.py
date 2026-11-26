#!/usr/bin/env python3
"""Create a deterministic zip of a distributable built-in plugin."""

import sys
import zipfile
from pathlib import Path


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: package-plugin.py <plugin-directory> <archive-path>")

    plugin_directory = Path(sys.argv[1]).resolve()
    archive_path = Path(sys.argv[2]).resolve()
    files = sorted(
        path
        for path in plugin_directory.rglob("*")
        if path.is_file()
        and not any(part.startswith(".") for part in path.relative_to(plugin_directory).parts)
        and not path.name.endswith("_test.ts")
        and path.name != "deno.json"
    )
    if not any(path.name == "plugin.json" for path in files):
        raise SystemExit("plugin.json is missing")

    archive_path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for path in files:
            archive.write(path, path.relative_to(plugin_directory).as_posix())


if __name__ == "__main__":
    main()
