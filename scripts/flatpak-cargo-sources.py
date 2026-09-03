#!/usr/bin/env python3
"""Regenerate build-aux/cargo-sources.json from Cargo.lock.

flatpak-builder builds with no network, so every crate the build needs is
listed up front in cargo-sources.json (URL + sha256) and vendored before
cargo runs. That file is a snapshot of Cargo.lock: whenever a dependency
changes, it has to be regenerated or the Flatpak build breaks.

    scripts/flatpak-cargo-sources.py            rewrite the file
    scripts/flatpak-cargo-sources.py --check    exit 1 if it is stale (CI)

Output is byte-identical to flatpak-cargo-generator.py from
https://github.com/flatpak/flatpak-builder-tools for crates.io
dependencies, without its aiohttp/tomlkit requirements. Git dependencies
are not supported here; if one is ever added, use the upstream tool.
"""

import argparse
import json
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    sys.exit("python 3.11+ is required (for tomllib)")

REPO = Path(__file__).resolve().parent.parent
DEFAULT_LOCK = REPO / "Cargo.lock"
DEFAULT_OUTPUT = REPO / "build-aux" / "cargo-sources.json"

CRATES_IO = "https://static.crates.io/crates"
CARGO_HOME = "cargo"
CARGO_CRATES = f"{CARGO_HOME}/vendor"

# What tomlkit.dumps() produces upstream for the vendored-sources config.
CARGO_CONFIG = (
    "[source.vendored-sources]\n"
    f'directory = "{CARGO_CRATES}"\n'
    "\n"
    "[source.crates-io]\n"
    'replace-with = "vendored-sources"\n'
)


def generate(lock_path: Path) -> list[dict]:
    with open(lock_path, "rb") as f:
        lock = tomllib.load(f)
    metadata = lock.get("metadata", {})

    sources: list[dict] = []
    for pkg in lock.get("package", []):
        name, version = pkg["name"], pkg["version"]
        source = pkg.get("source")
        if source is None:
            continue  # workspace member
        if source.startswith("git+"):
            sys.exit(
                f"{name} {version} is a git dependency ({source}); this script "
                "only handles crates.io. Use flatpak-cargo-generator.py from "
                "https://github.com/flatpak/flatpak-builder-tools instead."
            )
        if not source.startswith("registry+"):
            sys.exit(f"{name} {version}: unsupported source {source}")

        # Cargo.lock v1 kept checksums in a [metadata] table; v2+ inline them.
        checksum = metadata.get(f"checksum {name} {version} ({source})") or pkg.get(
            "checksum"
        )
        if not checksum:
            sys.exit(f"{name} {version} has no checksum in {lock_path}")

        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"{CRATES_IO}/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": f"{CARGO_CRATES}/{name}-{version}",
            }
        )
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": f"{CARGO_CRATES}/{name}-{version}",
                "dest-filename": ".cargo-checksum.json",
            }
        )

    sources.append(
        {
            "type": "inline",
            "contents": CARGO_CONFIG,
            "dest": CARGO_HOME,
            "dest-filename": "config",
        }
    )
    return sources


def crate_set(sources: list[dict]) -> set[str]:
    return {s["dest"].rsplit("/", 1)[1] for s in sources if s["type"] == "archive"}


def display(path: Path) -> str:
    try:
        return str(path.resolve().relative_to(REPO))
    except ValueError:
        return str(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--check",
        action="store_true",
        help="don't write; fail if the output file is out of date",
    )
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK, help=argparse.SUPPRESS)
    parser.add_argument(
        "--output", type=Path, default=DEFAULT_OUTPUT, help=argparse.SUPPRESS
    )
    args = parser.parse_args()

    sources = generate(args.lock)
    rendered = json.dumps(sources, indent=4)

    if not args.check:
        args.output.write_text(rendered, encoding="utf-8")
        print(f"wrote {len(crate_set(sources))} crates to {args.output}")
        return 0

    try:
        current = args.output.read_text(encoding="utf-8")
    except FileNotFoundError:
        print(f"{args.output} is missing", file=sys.stderr)
    else:
        if current == rendered:
            return 0
        have = crate_set(json.loads(current))
        want = crate_set(sources)
        for crate in sorted(have - want):
            print(f"  - {crate}", file=sys.stderr)
        for crate in sorted(want - have):
            print(f"  + {crate}", file=sys.stderr)
        if have == want:
            print("  (same crates, but content differs)", file=sys.stderr)
    print(
        f"{display(args.output)} is out of date with {display(args.lock)}; "
        "run `make flatpak-sources`",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
