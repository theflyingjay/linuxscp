#!/usr/bin/env python3
r"""Extract translatable messages from the Rust sources into a gettext template.

    scripts/i18n-extract.py

Writes po/linuxscp.pot. Sources of message ids:

  * tr!("...")                      the message id itself
  * ngettext("...", "...", n)       singular / plural pair
  * count(n, "...", "...")          the queue's plural helper

Rust string literals are unescaped (\n, \t, \\", \u{...} and the line
continuation backslash) so the .pot holds real text. Requires nothing beyond
the Python standard library.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "linuxscp" / "src"
OUT = ROOT / "po" / "linuxscp.pot"

# A Rust string literal. Message ids are always ordinary strings; the raw
# strings in the tree hold shell scripts, never translatable text.
LIT = r'"(?:\\.|[^"\\])*"'

TR = re.compile(r'\btr!\(\s*(' + LIT + r')', re.S)
NGETTEXT = re.compile(r'\bngettext\(\s*(' + LIT + r')\s*,\s*(' + LIT + r')', re.S)
COUNT = re.compile(r'\bcount\(\s*[^,()]+\s*,\s*(' + LIT + r')\s*,\s*(' + LIT + r')', re.S)

# Files that hold no user-visible strings (or only the runtime itself).
SKIP = {"i18n.rs"}


def unescape(literal: str) -> str:
    """Turn a Rust string literal (with quotes) into the text it denotes."""
    body = literal[1:-1]
    out = []
    i = 0
    while i < len(body):
        char = body[i]
        if char != "\\":
            out.append(char)
            i += 1
            continue
        if i + 1 >= len(body):
            break
        escape = body[i + 1]
        if escape == "n":
            out.append("\n")
            i += 2
        elif escape == "t":
            out.append("\t")
            i += 2
        elif escape == "r":
            out.append("\r")
            i += 2
        elif escape in "\"'\\":
            out.append(escape)
            i += 2
        elif escape == "0":
            out.append("\0")
            i += 2
        elif escape == "x":
            out.append(chr(int(body[i + 2 : i + 4], 16)))
            i += 4
        elif escape == "u":
            end = body.index("}", i)
            out.append(chr(int(body[i + 3 : end], 16)))
            i = end + 1
        elif escape == "\n":
            # Line continuation: drop the newline and the next line's indent.
            i += 2
            while i < len(body) and body[i] in " \t":
                i += 1
        else:
            out.append(escape)
            i += 2
    return "".join(out)


def po_quote(text: str) -> str:
    return (
        text.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\t", "\\t")
    )


def collect():
    singles = {}
    plurals = {}

    for path in sorted(SRC.rglob("*.rs")):
        if path.name in SKIP:
            continue
        text = path.read_text(encoding="utf-8")
        where = str(path.relative_to(ROOT))

        for match in TR.finditer(text):
            singles.setdefault(unescape(match.group(1)), set()).add(where)

        for regex in (NGETTEXT, COUNT):
            for match in regex.finditer(text):
                pair = (unescape(match.group(1)), unescape(match.group(2)))
                plurals.setdefault(pair, set()).add(where)

    return singles, plurals


def main():
    singles, plurals = collect()
    out = sys.argv[1] if len(sys.argv) > 1 else OUT
    with open(out, "w", encoding="utf-8") as file:
        file.write(
            "# LinuxSCP translation template.\n"
            "# Regenerate with scripts/i18n-extract.py.\n"
            'msgid ""\n'
            'msgstr ""\n'
            '"Project-Id-Version: linuxscp\\n"\n'
            '"MIME-Version: 1.0\\n"\n'
            '"Content-Type: text/plain; charset=UTF-8\\n"\n'
            '"Content-Transfer-Encoding: 8bit\\n"\n'
            "\n"
        )
        for msgid in sorted(singles):
            file.write(f"#: {', '.join(sorted(singles[msgid]))}\n")
            file.write(f'msgid "{po_quote(msgid)}"\nmsgstr ""\n\n')
        for singular, plural in sorted(plurals):
            file.write(f"#: {', '.join(sorted(plurals[(singular, plural)]))}\n")
            file.write(f'msgid "{po_quote(singular)}"\n')
            file.write(f'msgid_plural "{po_quote(plural)}"\n')
            file.write('msgstr[0] ""\n\n')

    print(
        f"{len(singles)} messages, {len(plurals)} plurals -> {out}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
