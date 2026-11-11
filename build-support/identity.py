#!/usr/bin/env python3
"""Shared release identity and remote signing configuration."""
import json
from pathlib import Path
import re
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = json.loads((ROOT / 'packaging/identity.json').read_text())
CORE_VERSION = re.compile(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)')


def encode_build(version, sequence):
    """A build encodes the major.minor.patch it packages plus its sequence n of that version."""
    match = CORE_VERSION.fullmatch(version) if isinstance(version, str) else None
    if not match:
        raise ValueError(f'build version must be major.minor.patch with minor/patch <= 99, not {version!r}')
    if type(sequence) is not int or not 1 <= sequence <= 999:
        raise ValueError('build sequence must be an integer in 1..999')
    major, minor, patch = map(int, match.groups())
    build = major * 10_000_000 + minor * 100_000 + patch * 1_000 + sequence
    if build >= 2**31:
        raise ValueError('build exceeds 2147483647')
    return build


def decode_build(build):
    """Inverse of encode_build: (major.minor.patch, n)."""
    if type(build) is not int or not 1 <= build < 2**31 or build % 1_000 == 0:
        raise ValueError(f'release build must be in 1..2147483647 with sequence 1..999, not {build!r}')
    return f'{build // 10_000_000}.{build // 100_000 % 100}.{build // 1_000 % 100}', build % 1_000


def artifact_stem(version, platform, arch):
    """Public URLs use display versions; numeric builds stay in package metadata."""
    return f"{IDENTITY['name']}-{version}-{platform}-{arch}"


def windows_product_version(build):
    """MSI's 8/8/16 fields preserve build ordering across human versions."""
    decode_build(build)
    return f'{build >> 24}.{(build >> 16) & 255}.{build & 65535}'


def signpath_configuration():
    ns = 'http://signpath.io/artifact-configuration/v1'
    ET.register_namespace('', ns)
    def child(parent, tag, **attrs):
        return ET.SubElement(parent, '{' + ns + '}' + tag, attrs)
    root = ET.Element('{' + ns + '}artifact-configuration')
    archive = child(root, 'zip-file')
    installer = child(archive, 'msi-file', path=f"{IDENTITY['name']}-*-windows-x64.msi")
    directory = child(installer, 'directory', path=f"application/{IDENTITY['windows']['install_directory']}")
    for role in ['ui', 'agent', 'service', 'update_worker', 'crash_collector']:
        child(child(directory, 'pe-file', path=IDENTITY['binaries'][role] + '.exe'), 'authenticode-sign')
    child(child(directory, 'pe-file', path='OinkPasteMenu.dll'), 'authenticode-sign')
    child(child(directory, 'pe-file', path='OinkCredentialProvider.dll'), 'authenticode-sign')
    child(installer, 'authenticode-sign')
    ET.indent(root)
    return ET.tostring(root, encoding='utf-8', xml_declaration=True) + b'\n'


if __name__ == '__main__':
    (ROOT / '.signpath/artifact-configurations/windows-msi-v1.xml').write_bytes(signpath_configuration())
