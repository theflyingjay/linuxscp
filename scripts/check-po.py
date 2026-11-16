#!/usr/bin/env python3
"""Check that translations keep the placeholders of their message id.

    scripts/check-po.py [po/zh_CN.po ...]     (default: every po/*.po)

`msgfmt --check` validates syntax and the format directives it knows about,
but `{}` is not one of them: a translation that drops a placeholder compiles
fine and then silently loses the substituted value at run time (a file name
in the delete confirmation, for example).

Each catalog is compiled with msgfmt and the check is made on the result,
the .mo that is actually installed, rather than on the .po text. The PO
format allows a lot of layout (a tab or nothing after a keyword, a blank
line or a line break before the string, two fields on one line), and an
entry read differently here than by msgfmt would be shipped unchecked.

Every translation must have as many `{}` as its message id; a plural form
may match either its singular or its plural id. Catalogs must be UTF-8,
which is how the app reads them.

Needs msgfmt (from gettext) on PATH. Exits non-zero if any catalog has a
problem, listing each one.
"""

import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The .mo layout is described in the gettext manual, "The Format of GNU MO
# Files". The magic number tells which byte order the file was written in.
MO_MAGIC = 0x950412DE
MO_MAGIC_SWAPPED = 0xDE120495
# Separates a message context from the message id it qualifies.
CONTEXT_SEPARATOR = b"\x04"


class CatalogError(Exception):
    """A catalog that could not be compiled or read."""


def placeholders(text: str) -> int:
    """Count `{}` in a gettext string, ignoring `{{` and `}}` escapes."""
    count = 0
    i = 0
    while i < len(text):
        char = text[i]
        if char == "{" and i + 1 < len(text):
            if text[i + 1] == "{":
                i += 2
                continue
            if text[i + 1] == "}":
                count += 1
                i += 2
                continue
        if char == "}" and i + 1 < len(text) and text[i + 1] == "}":
            i += 2
            continue
        i += 1
    return count


def compile_po(po: Path, directory: Path) -> bytes:
    """Compile `po` the way the packaging does and return the .mo contents."""
    mo = directory / "catalog.mo"
    result = subprocess.run(
        ["msgfmt", "--check", "-o", str(mo), str(po.resolve())],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise CatalogError(
            result.stderr.strip() or f"msgfmt exited with status {result.returncode}"
        )
    return mo.read_bytes()


def entries(mo: bytes):
    """Yield (message ids, translations) for each entry, as raw bytes.

    A plural entry has two ids (singular, plural) and one translation per
    plural form; every other entry has one of each.
    """
    try:
        (magic,) = struct.unpack_from("<I", mo, 0)
        if magic == MO_MAGIC:
            order = "<"
        elif magic == MO_MAGIC_SWAPPED:
            order = ">"
        else:
            raise CatalogError("msgfmt did not write a .mo file")
        count, ids_at, translations_at = struct.unpack_from(order + "3I", mo, 8)
        for index in range(count):
            id_length, id_at = struct.unpack_from(order + "2I", mo, ids_at + 8 * index)
            length, at = struct.unpack_from(order + "2I", mo, translations_at + 8 * index)
            yield (
                mo[id_at : id_at + id_length].split(b"\0"),
                mo[at : at + length].split(b"\0"),
            )
    except struct.error:
        raise CatalogError("the compiled catalog is truncated") from None


def charset(header: bytes) -> str:
    """The charset named in the catalog header, lower-cased ('' if none)."""
    for line in header.decode("ascii", "replace").lower().splitlines():
        if line.startswith("content-type:") and "charset=" in line:
            return line.split("charset=", 1)[1].strip()
    return ""


def check(po: Path) -> list[str]:
    with tempfile.TemporaryDirectory() as directory:
        catalog = list(entries(compile_po(po, Path(directory))))

    declared = next((charset(strs[0]) for ids, strs in catalog if ids == [b""]), "")
    if declared not in ("utf-8", "utf8"):
        return [f"{po}: charset is {declared or 'not declared'}; catalogs must be UTF-8"]

    problems = []
    for raw_ids, raw_translations in catalog:
        if raw_ids == [b""]:
            continue  # the header entry
        try:
            # Only the id itself counts, not a context it may be prefixed with.
            ids = [raw.split(CONTEXT_SEPARATOR)[-1].decode("utf-8") for raw in raw_ids]
            translations = [raw.decode("utf-8") for raw in raw_translations]
        except UnicodeDecodeError as err:
            problems.append(f"{po}: {raw_ids[0]!r} is not valid UTF-8 ({err})")
            continue

        allowed = sorted({placeholders(msgid) for msgid in ids})
        for translation in translations:
            if not translation:
                continue  # an empty plural form; nothing to compare
            found = placeholders(translation)
            if found not in allowed:
                expected = " or ".join(str(count) for count in allowed)
                problems.append(
                    f"{po}: msgid has {expected} placeholder(s), "
                    f"translation has {found}: {ids[0]!r} -> {translation!r}"
                )
    return problems


def main():
    paths = [Path(arg) for arg in sys.argv[1:]]
    if not paths:
        paths = sorted((ROOT / "po").glob("*.po"))
    if not paths:
        print("check-po: no catalogs found", file=sys.stderr)
        return 1
    if shutil.which("msgfmt") is None:
        print("check-po: msgfmt not found; install gettext", file=sys.stderr)
        return 1

    failures = []
    for path in paths:
        try:
            failures.extend(check(path))
        except (CatalogError, OSError) as err:
            failures.append(f"{path}: {err}")

    for failure in failures:
        print(failure, file=sys.stderr)
    if failures:
        print(f"check-po: {len(failures)} problem(s)", file=sys.stderr)
        return 1
    print(f"check-po: {len(paths)} catalog(s) OK", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
