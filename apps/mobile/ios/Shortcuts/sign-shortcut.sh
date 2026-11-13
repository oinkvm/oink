#!/usr/bin/env bash
# Re-signs every OinkCopy-<colour>.shortcut from its OinkCopy-<colour>.unsigned.plist for anyone
# to import, and records them in OinkCopy.signed.json. Run by hand on a Mac (about once a year:
# the signing certificate lasts ~13 months, and the packaging check fails 3 months before it
# ends), then commit the sources, the signed files and the manifest.
#
# `shortcuts sign --mode anyone` needs this Mac signed in to iCloud and online (Apple issues
# the certificate). The result carries no Apple ID: only an Apple-issued leaf certificate
# with a random name. Never use --mode people-who-know-me for the bundled files: it embeds
# the Apple ID's identifiers and only that person's contacts can import it.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

python3 -I "$here/make_shortcut.py" --check
# Signed files whose source is gone (a colour dropped from make_shortcut.py COLORS).
for signed in "$here"/OinkCopy-*.shortcut; do
  [ -e "$signed" ] || continue
  [ -e "${signed%.shortcut}.unsigned.plist" ] || rm "$signed"
done
for source in "$here"/OinkCopy-*.unsigned.plist; do
  name="$(basename "$source" .unsigned.plist)"
  plutil -convert binary1 -o "$work/$name.unsigned.shortcut" "$source"
  shortcuts sign --mode anyone --input "$work/$name.unsigned.shortcut" --output "$work/$name.shortcut"
  cp "$work/$name.shortcut" "$here/$name.shortcut"
done
python3 -I "$root/build-support/ios_package.py" manifest
python3 -I "$root/build-support/ios_package.py" shortcut
