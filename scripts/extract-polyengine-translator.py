#!/usr/bin/env python3
"""Extract the polyengine translator-shim wasm from a `deno info --json` module
graph, per the polyengine->JSR migration contract: the translator wasm ships
inside the pinned `@polyengine/translator` JSR package, cached on disk by
`deno info --frozen`. No network, no sha bookkeeping — the deno.lock's
package integrity already covers the bytes; this script only picks the
right cached file out and asserts the whole @polyengine graph agrees on one
pinned version.

WARNING (verified the hard way): Deno's on-disk
remote-cache file for a JSR asset is the module bytes PLUS a trailing
"\n// denoCacheMetadata={...}" line. Copying the whole file yields a
CORRUPT wasm module (fails to compile: "unexpected section <Code>" /
"section out of order" near EOF). This script truncates to the byte size
`deno info` reports and sanity-checks the result.

Usage: extract-polyengine-translator.py <deno-info.json> <out.wasm>
"""
import json
import os
import sys


def main() -> None:
    info_path, out_path, expected_pin = sys.argv[1], sys.argv[2], sys.argv[3]
    with open(info_path) as f:
        graph = json.load(f)

    modules = [m for m in graph["modules"] if "/@polyengine/" in m.get("specifier", "")]
    if not modules:
        sys.exit(f"{info_path}: no @polyengine modules found in graph")

    bad = {m["specifier"] for m in modules if expected_pin not in m["specifier"]}
    if bad:
        sys.exit(f"pin drift in translator graph (expected {expected_pin}): {bad}")

    asset = next(
        (m for m in modules if m["specifier"].endswith("/translator_shim.wasm")),
        None,
    )
    if asset is None:
        sys.exit(f"{info_path}: no translator_shim.wasm module in graph")

    with open(asset["local"], "rb") as f:
        data = f.read()

    size = asset["size"]
    body, rest = data[:size], data[size:]
    if body[:4] != b"\0asm":
        sys.exit("extracted bytes do not start with the wasm magic after truncation")
    if rest and not rest.startswith(b"\n// denoCacheMetadata="):
        sys.exit("unexpected cache-file layout after the module bytes; refusing to copy")

    os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
    with open(out_path, "wb") as f:
        f.write(body)


if __name__ == "__main__":
    main()
