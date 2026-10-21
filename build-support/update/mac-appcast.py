#!/usr/bin/env python3
"""Offline operator hook using the pinned official Sparkle tools and Keychain.

Never generates a key. Run only on approved, notarized final arm64 archives.
The caller supplies the public Sparkle distribution archive and a directory of
approved archives/current appcast; only the named new build is added.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import xml.etree.ElementTree as ET
import urllib.parse
import feed

ROOT = Path(__file__).resolve().parents[2]
SPARKLE = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
OINK = 'https://updates.oink.hachiwii.com/xml-namespaces/update'
XML = 'http://www.w3.org/XML/1998/namespace'


def embed_notes(path, build, notes):
    """Edit the generated candidate before its final official Sparkle signature."""
    feed.validate_notes(notes)
    raw = path.read_bytes()
    feed.require(len(raw) <= feed.MAX_FEED and b'<!DOCTYPE' not in raw and b'<!ENTITY' not in raw, 'appcast_xml')
    # Preserve namespace prefixes used by existing custom item fields. Sparkle
    # exposes custom fields by qualified name in propertiesDictionary.
    for _, (prefix, uri) in ET.iterparse(path, events=('start-ns',)):
        ET.register_namespace(prefix, uri)
    ET.register_namespace('sparkle', SPARKLE)
    ET.register_namespace('oink', OINK)
    root = ET.fromstring(raw)
    matches = []
    for item in root.findall('./channel/item'):
        enclosure = item.find('enclosure')
        version = item.findtext(f'{{{SPARKLE}}}version')
        if version is None and enclosure is not None:
            version = enclosure.get(f'{{{SPARKLE}}}version')
        if version == str(build): matches.append(item)
    feed.require(root.tag == 'rss' and len(matches) == 1, 'appcast_build')
    item = matches[0]
    for child in list(item):
        if child.tag in ('description', f'{{{SPARKLE}}}releaseNotesLink', f'{{{OINK}}}releaseNotes'):
            item.remove(child)
    for language in sorted(notes):
        ET.SubElement(item, 'description', {f'{{{XML}}}lang': language,
                      f'{{{SPARKLE}}}format': 'plain-text'}).text = notes[language]
    # One element prevents Sparkle's per-element locale selection from removing
    # any translation before the custom driver receives the item.
    ET.SubElement(item, f'{{{OINK}}}releaseNotes').text = feed.encode(notes).decode('utf-8')
    for retained in root.findall('./channel/item'):
        encoded = retained.findtext(f'{{{OINK}}}releaseNotes')
        feed.require(encoded is not None, 'appcast_notes_missing')
        feed.validate_notes(feed.loads(encoded.encode('utf-8')))
    output = ET.tostring(root, encoding='utf-8', xml_declaration=True)
    feed.require(len(output) <= feed.MAX_FEED, 'appcast_limit')
    feed.write(path, output)


def embed_archive_hash(path, build, archives):
    """Bind only the named new archive into the final signed XML."""
    for _, (prefix, uri) in ET.iterparse(path, events=('start-ns',)):
        ET.register_namespace(prefix, uri)
    ET.register_namespace('oink', OINK)
    root = ET.fromstring(path.read_bytes())
    items = [item for item in root.findall('./channel/item')
             if item.findtext(f'{{{SPARKLE}}}version') == str(build)]
    feed.require(len(items) == 1, 'appcast_build')
    item = items[0]
    name = Path(urllib.parse.urlsplit(item.find('enclosure').get('url')).path).name
    feed.require(re.fullmatch(r'[A-Za-z0-9_.-]+\.(zip|dmg)', name), 'asset')
    for previous in item.findall(f'{{{OINK}}}sha256'):
        item.remove(previous)
    with (archives / name).open('rb') as archive:
        ET.SubElement(item, f'{{{OINK}}}sha256').text = hashlib.file_digest(archive, 'sha256').hexdigest()
    feed.write(path, ET.tostring(root, encoding='utf-8', xml_declaration=True))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--sparkle-archive', required=True, type=Path)
    parser.add_argument('--archives', required=True, type=Path)
    parser.add_argument('--build', required=True, type=int)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--notes', required=True, type=Path, help='JSON object with required zh-Hans and en plain text')
    parser.add_argument('--channel', choices=['stable', 'beta'], default='stable')
    parser.add_argument('--account', default='Oink-mac-2026-01')
    args = parser.parse_args()
    notes = feed.validate_notes(feed.read(args.notes))
    config = json.loads((ROOT / 'packaging/update-trust.json').read_bytes())
    if not (0 < args.build < 2**31 and re.fullmatch('v[A-Za-z0-9_.-]+', args.tag)):
        raise SystemExit('release_identity')
    if hashlib.sha256(args.sparkle_archive.read_bytes()).hexdigest() != config['mac']['sparkle_archive_sha256']:
        raise SystemExit('sparkle_distribution_digest')
    folder = args.archives.resolve(strict=True)
    if not folder.is_dir() or not (list(folder.glob('*.zip')) or list(folder.glob('*.dmg'))):
        raise SystemExit('approved_archive_missing')
    with tempfile.TemporaryDirectory(prefix='oink-sparkle-') as temporary:
        tools = Path(temporary)
        # Exact official archive digest is checked before extracting any code.
        with tarfile.open(args.sparkle_archive) as archive:
            archive.extractall(tools, filter='data')
        command = [str(tools / 'bin/generate_appcast'), '--account', args.account,
                   '--maximum-deltas', '0', '--maximum-versions', '0', '--versions', str(args.build),
                   '--download-url-prefix', f'https://github.com/{config["repository"]}/releases/download/{args.tag}/']
        if args.channel == 'beta': command += ['--channel', 'beta']
        subprocess.run([*command, str(folder)], check=True)
        embed_archive_hash(folder / 'appcast.xml', args.build, folder)
        embed_notes(folder / 'appcast.xml', args.build, notes)
        # Re-sign after the final localized fields have been embedded. Only the
        # official tool writes the signature; no changes follow verification.
        subprocess.run([str(tools / 'bin/sign_update'), '--account', args.account,
                        str(folder / 'appcast.xml')], check=True)
        subprocess.run([str(tools / 'bin/sign_update'), '--verify', '--account', args.account,
                        str(folder / 'appcast.xml')], check=True)
    # Do not parse/reformat or add trailers after the official signer returns.
    print('Signed appcast verified; preserve the exact appcast and signed notes bytes.')


if __name__ == '__main__': main()
