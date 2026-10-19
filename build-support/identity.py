#!/usr/bin/env python3
"""Shared release identity and remote signing configuration."""
import json
from pathlib import Path
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = json.loads((ROOT / 'packaging/identity.json').read_text())


def windows_product_version(build):
    """MSI's 8/8/16 fields preserve global build ordering across human versions."""
    if type(build) is not int or not 1 <= build < 2**31:
        raise ValueError('release build must be in 1..2147483647')
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
    child(installer, 'authenticode-sign')
    ET.indent(root)
    return ET.tostring(root, encoding='utf-8', xml_declaration=True) + b'\n'


if __name__ == '__main__':
    (ROOT / '.signpath/artifact-configurations/windows-msi-v1.xml').write_bytes(signpath_configuration())
