#!/usr/bin/env bash
# Prepare a release in the working tree: bump the workspace version, sync
# Cargo.lock, and record a dated release entry in the AppStream metainfo
# with the commit subjects since the last v* tag as the release notes.
#
#   scripts/cut-release.sh [patch|minor|major]     (default: patch)
#
# Prints the new version on stdout and leaves commit/tag/push to the
# caller — .github/workflows/auto-release.yml for the automated flow, or
# by hand:
#   NEW="$(scripts/cut-release.sh minor)"
#   git commit -am "Release v$NEW" && git tag "v$NEW"
#   git push origin main "v$NEW"
set -euo pipefail

cd "$(dirname "$0")/.."

APP_ID="io.github.theflyingjay.LinuxSCP"
METAINFO="data/$APP_ID.metainfo.xml"
LEVEL="${1:-patch}"

CUR="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
IFS=. read -r MAJOR MINOR PATCH <<<"$CUR"
case "$LEVEL" in
    major) NEW="$((MAJOR + 1)).0.0" ;;
    minor) NEW="$MAJOR.$((MINOR + 1)).0" ;;
    patch) NEW="$MAJOR.$MINOR.$((PATCH + 1))" ;;
    *) echo "usage: $0 [patch|minor|major]" >&2; exit 2 ;;
esac
echo ">> Version $CUR -> $NEW" >&2

sed -i "0,/^version = \"$CUR\"/s//version = \"$NEW\"/" Cargo.toml
# Sync the workspace members' own entries in Cargo.lock; leaves every
# dependency version untouched.
cargo update --workspace --quiet

# Release notes: one bullet per commit subject since the last release.
LAST_TAG="$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)"
SUBJECTS="$(git log --format='%s' "${LAST_TAG:+$LAST_TAG..}HEAD")" \
NEW_VERSION="$NEW" DATE="$(date -u +%F)" python3 - "$METAINFO" <<'EOF'
import os, re, sys
from xml.sax.saxutils import escape

path = sys.argv[1]
subjects = []
for s in os.environ["SUBJECTS"].splitlines():
    # Workflow steering markers are not release notes.
    s = re.sub(r"\s*\[(?:release:(?:major|minor|patch)|skip release)\]\s*", " ", s).strip()
    if s:
        subjects.append(s)
if subjects:
    items = "\n".join(f"          <li>{escape(s)}</li>" for s in subjects)
    body = f"        <ul>\n{items}\n        </ul>"
else:
    body = "        <p>Maintenance release.</p>"
entry = (
    f'    <release version="{os.environ["NEW_VERSION"]}" date="{os.environ["DATE"]}">\n'
    f"      <description>\n{body}\n      </description>\n"
    f"    </release>"
)
text = open(path).read()
marker = "<releases>"  # newest entry first, as AppStream expects
i = text.index(marker) + len(marker)
open(path, "w").write(text[:i] + "\n" + entry + text[i:])
EOF

# The XML must stay parseable; full AppStream validation happens in the
# .deb build, which runs appstreamcli when available.
python3 -c "import sys, xml.dom.minidom; xml.dom.minidom.parse(sys.argv[1])" "$METAINFO"

echo "$NEW"
