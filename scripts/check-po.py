#!/usr/bin/env python3
"""Check that translations keep the placeholders of their message id.

    scripts/check-po.py [po/zh_CN.po ...]     (default: every po/*.po)

`msgfmt --check` validates syntax and format directives it knows about, but
`{}` is not one of them: a translation that drops a placeholder compiles
fine and then silently loses the substituted value at run time (a file name
in the delete confirmation, for example). This compares the number of `{}`
placeholders in each msgid with its msgstr, and every msgstr[N] of a plural
entry with its singular and plural ids.

Exits non-zero on the first file with problems, listing each one.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


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


def unquote(line: str) -> str:
    """Turn `msgid "text"` / `msgstr[0] "text"` into text."""
    start = line.index('"')
    body = line[start + 1 : line.rindex('"')]
    out = []
    i = 0
    while i < len(body):
        if body[i] == "\\" and i + 1 < len(body):
            escape = body[i + 1]
            out.append({"n": "\n", "t": "\t", '"': '"', "\\": "\\"}.get(escape, escape))
            i += 2
        else:
            out.append(body[i])
            i += 1
    return "".join(out)


def parse(path: Path):
    """Yield (line number, msgid, [msgstr values]) for each entry."""
    entries = []
    current = {"line": 0, "msgid": None, "plurals": [], "msgstr": []}
    field = None

    def flush():
        if current["msgid"] is not None:
            entries.append(
                (current["line"], current["msgid"], current["msgstr"] or current["plurals"])
            )

    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line:
            flush()
            current = {"line": 0, "msgid": None, "plurals": [], "msgstr": []}
            field = None
        elif line.startswith("#"):
            continue
        elif line.startswith("msgid "):
            if current["msgid"] is not None:
                flush()
                current = {"line": 0, "msgid": None, "plurals": [], "msgstr": []}
            current["line"] = number
            current["msgid"] = unquote(line)
            field = "msgid"
        elif line.startswith("msgid_plural "):
            current["plurals"].append(unquote(line))
            field = "plural"
        elif line.startswith("msgstr["):
            current["msgstr"].append(unquote(line))
            field = "msgstr"
        elif line.startswith("msgstr "):
            current["msgstr"].append(unquote(line))
            field = "msgstr"
        elif line.startswith('"'):
            if field == "msgid":
                current["msgid"] += unquote(line)
            elif field == "plural":
                current["plurals"][-1] += unquote(line)
            elif field == "msgstr":
                current["msgstr"][-1] += unquote(line)

    flush()
    return entries


def check(path: Path) -> list[str]:
    problems = []
    for line, msgid, translations in parse(path):
        if not msgid:
            continue  # the header entry
        expected = placeholders(msgid)
        for translation in translations:
            if not translation:
                continue  # untranslated entries are fine
            found = placeholders(translation)
            if found != expected:
                problems.append(
                    f'{path}:{line}: msgid has {expected} placeholder(s), '
                    f'translation has {found}: {msgid!r} -> {translation!r}'
                )
    return problems


def main():
    paths = [Path(arg) for arg in sys.argv[1:]]
    if not paths:
        paths = sorted((ROOT / "po").glob("*.po"))
    if not paths:
        print("check-po: no catalogs found", file=sys.stderr)
        return 1

    failures = []
    for path in paths:
        failures.extend(check(path))

    for failure in failures:
        print(failure, file=sys.stderr)
    if failures:
        print(f"check-po: {len(failures)} problem(s)", file=sys.stderr)
        return 1
    print(f"check-po: {len(paths)} catalog(s) OK", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
